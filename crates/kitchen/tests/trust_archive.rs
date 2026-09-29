//! Trust ledger archival against temporary stores. Offline fixtures only: no
//! live runtime, forge, or model is exercised.
mod common;
use common::{
    Fixture, ManualClock, TestResult, at, commit, creator, holder, house, scheduled, task_id, ttl,
};
use kitchen::{
    contracts::{
        AttemptNumber, AttemptOutcome, Evidence, EvidenceKind, EvidenceSubject, EvidenceVerdict,
        ExternalRef, Fence, Grant, HouseGrants, Permission, Repository, Role, TaskAuthority,
        TaskSpec, Text,
    },
    scheduling::AgentFamily,
    selection::{AgentModel, AgentSelection, ResolvedSelection, WorkType},
    state::{Corruption, StateError, TaskState},
    trust::{
        ARCHIVE_FILE, ArchiveBatch, Attribution, AutonomyProposal, BenchResult, EvidenceMode,
        GrantAudit, Ledger, Measurement, Observation, PullRequestEvidence, StationScope,
        TrustError,
    },
    workflows::inspector::{InspectionPlan, SampleResult},
};
use sha2::{Digest, Sha256};
use std::{fs, num::NonZeroU32, path::PathBuf};

const MODEL: &str = "fixture-model-v1";
const MAX_HISTORY: usize = 4096;
/// The largest snapshot an ordinary write may produce.
const ORDINARY_LIMIT: u64 = 8 * 1024 * 1024;
const MAX_TEXT: usize = 64 * 1024;

fn source(value: &str) -> TestResult<ExternalRef> {
    Ok(ExternalRef::new(value)?)
}
fn measured<T>(value: T) -> TestResult<Measurement<T>> {
    Ok(Measurement::Observed {
        value,
        samples: NonZeroU32::MIN,
        source: source("fixture:source")?,
    })
}
fn scope() -> TestResult<StationScope> {
    Ok(StationScope {
        station: Role::StationCook,
        project: Repository::new("example/project")?,
        work_type: WorkType::new("implementation")?,
    })
}
fn spec(id: &str) -> TestResult<TaskSpec> {
    let mut task = common::spec(id)?;
    task.repository = Some(scope()?.project);
    task.work_type = Some(scope()?.work_type);
    task.agent = Some(ResolvedSelection::owner(AgentSelection {
        agent: AgentFamily::Claude,
        model: Some(AgentModel::new(MODEL)?),
        effort: None,
    }));
    Ok(task)
}
fn subject() -> TestResult<EvidenceSubject> {
    Ok(EvidenceSubject {
        head: commit('a')?,
        base: Some(commit('b')?),
    })
}
fn ledger(f: &Fixture) -> TestResult<Ledger> {
    Ok(Ledger::initialize(f.dir.path().join("trust"), house()?)?)
}
fn reopen(f: &Fixture) -> TestResult<Ledger> {
    Ok(Ledger::open(f.dir.path().join("trust"), house()?)?)
}
fn ledger_path(f: &Fixture) -> PathBuf {
    f.dir.path().join("trust/ledger.json")
}
fn archive_path(f: &Fixture) -> PathBuf {
    f.dir.path().join("trust").join(ARCHIVE_FILE)
}

/// Settle `name` as a succeeded worker task, bind it, and record its live,
/// trust-eligible observation as stream `fixture:<name>`.
fn delivered(f: &Fixture, l: &Ledger, name: &str) -> TestResult<Observation> {
    let id = task_id(name)?;
    let task = spec(name)?;
    f.store.create_task(task.clone(), &creator()?, at(0))?;
    let lease = f.store.claim(&id, &scheduled("owner")?, ttl(60)?, at(1))?;
    f.store.start_attempt(&id, lease.fence(), at(2))?;
    f.store.finish_attempt(
        &id,
        lease.fence(),
        AttemptNumber::FIRST,
        AttemptOutcome::Succeeded,
        at(3),
    )?;
    l.bind_task(&task, source(&format!("fixture:{name}-binding"))?)?;
    let mut o = Observation::collect(
        &f.store,
        &id,
        source(&format!("fixture:{name}"))?,
        Attribution {
            scope: scope()?,
            agent: measured(holder("worker")?)?,
            model: measured(Text::new("claude:fixture-model-v1")?)?,
            tokens: measured(120)?,
        },
        EvidenceMode::Live,
        at(4),
    )?;
    o.pull_request = measured(PullRequestEvidence {
        house: house()?,
        task: id,
        repository: scope()?.project,
        source: source("https://example.invalid/pr/1")?,
        subject: subject()?,
        first_pass: measured(true)?,
        findings: measured(Vec::new())?,
        reverts: measured(Vec::new())?,
        regressions: measured(Vec::new())?,
        checks: measured(vec![Evidence {
            kind: EvidenceKind::Check,
            verdict: EvidenceVerdict::Pass,
            subject: subject()?,
            source: source("fixture:passing-check")?,
            observed_at: at(4),
        }])?,
    })?;
    l.record(&f.store, o.clone())?;
    Ok(o)
}
fn claim() -> TestResult<Grant> {
    Ok(Grant::repository(
        Permission::LaunchWorker,
        scope()?.project,
        common::backend_id()?,
        common::credential()?,
    ))
}
fn policy() -> TestResult<HouseGrants> {
    Ok(HouseGrants::with_limits(house()?, [claim()?], [])?)
}
/// Propose, and with `approve` issue, a grant citing stream `fixture:<name>`.
fn grant_on(l: &Ledger, grant_id: &str, name: &str, approve: bool) -> TestResult<ExternalRef> {
    let id = source(grant_id)?;
    l.propose(
        AutonomyProposal {
            id: id.clone(),
            house: house()?,
            scope: scope()?,
            claim: claim()?,
            evidence: vec![(source(&format!("fixture:{name}"))?, NonZeroU32::MIN)],
            source: source("fixture:proposal")?,
            at: at(5),
        },
        &policy()?,
    )?;
    if approve {
        l.approve(
            &id,
            holder("owner")?,
            source("fixture:decision")?,
            at(5),
            &policy()?,
        )?;
    }
    Ok(id)
}
fn revoke(l: &Ledger, id: &ExternalRef) -> TestResult<bool> {
    Ok(l.revoke(id, holder("owner")?, source("fixture:revoke")?, at(6))?)
}
/// A prospective task bound to the fixture station, to project standing for.
fn acting(l: &Ledger, name: &str) -> TestResult<TaskSpec> {
    let mut task = spec(name)?;
    task.authority = TaskAuthority::delegate(&policy()?, [])?;
    l.bind_task(&task, source(&format!("fixture:{name}-binding"))?)?;
    Ok(task)
}
fn standing_covers(f: &Fixture, l: &Ledger, task: &TaskSpec) -> TestResult<bool> {
    Ok(l.standing_for_task(&f.store, task, &policy()?)?
        .covers(&claim()?))
}
/// An ordinary write: bind one more prospective task.
fn bind_extra(l: &Ledger, name: &str) -> Result<bool, TrustError> {
    let task = spec(name).map_err(|_| TrustError::Invalid)?;
    let source = source("fixture:extra-binding").map_err(|_| TrustError::Invalid)?;
    l.bind_task(&task, source)
}
/// A valid observation of another, unbound task carrying `bytes` of bench text.
fn bulky(template: &Observation, index: usize, bytes: usize) -> TestResult<serde_json::Value> {
    let mut o = template.clone();
    o.id = source(&format!("fixture:bulk-{index}"))?;
    o.task = task_id(&format!("bulk-{index}"))?;
    if let Measurement::Observed { value, .. } = &mut o.pull_request {
        value.task = o.task.clone();
    }
    o.bench = measured(vec![BenchResult {
        subject: subject()?,
        passed: true,
        procedure: Text::new(&"b".repeat(bytes))?,
    }])?;
    Ok(serde_json::to_value(o)?)
}
/// Append `count` small bulk observations directly to the snapshot.
fn pad_entries(f: &Fixture, template: &Observation, count: usize) -> TestResult {
    let path = ledger_path(f);
    let mut document: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    let entries = document["observations"]
        .as_array_mut()
        .ok_or("observations")?;
    for index in 0..count {
        entries.push(bulky(template, index, 1)?);
    }
    fs::write(&path, serde_json::to_vec(&document)?)?;
    Ok(())
}
/// Grow the snapshot to exactly `target` bytes with bulk observations.
fn fill_bytes(f: &Fixture, template: &Observation, target: u64) -> TestResult {
    let path = ledger_path(f);
    let mut document: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    let mut size = u64::try_from(serde_json::to_vec(&document)?.len())?;
    let entries = document["observations"]
        .as_array_mut()
        .ok_or("observations")?;
    for index in 0.. {
        let missing = target.checked_sub(size).ok_or("over target")?;
        let fixed = u64::try_from(serde_json::to_vec(&bulky(template, index, 1)?)?.len())? + 1;
        let room = (missing + 1).checked_sub(fixed).ok_or("too close to pad")?;
        let last = room <= u64::try_from(MAX_TEXT)?;
        let text = if last { usize::try_from(room)? } else { 60_000 };
        entries.push(bulky(template, index, text)?);
        size += fixed - 1 + u64::try_from(text)?;
        if last {
            break;
        }
    }
    fs::write(&path, serde_json::to_vec(&document)?)?;
    assert_eq!(fs::metadata(&path)?.len(), target);
    Ok(())
}
/// Lines of the archive file, each parsed, with its digest recomputed.
fn archived(f: &Fixture) -> TestResult<Vec<(String, ArchiveBatch)>> {
    let text = fs::read_to_string(archive_path(f))?;
    assert!(text.ends_with('\n'), "every batch ends its line");
    text.lines()
        .map(|line| {
            let digest: String = Sha256::digest(line.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            Ok((digest, serde_json::from_str(line)?))
        })
        .collect()
}
fn fence(f: &Fixture) -> TestResult<Fence> {
    let id = task_id("inspector")?;
    if let Ok(task) = f.store.task(&id)
        && let TaskState::Claimed { lease } = task.state()
    {
        return Ok(lease.fence());
    }
    let mut task = spec("inspector")?;
    task.role = Role::Inspector;
    f.store.create_task(task, &creator()?, at(0))?;
    Ok(f.store
        .claim(&id, &scheduled("independent-reviewer")?, ttl(600)?, at(0))?
        .fence())
}
fn plan(name: &str) -> TestResult<InspectionPlan> {
    Ok(InspectionPlan {
        id: source("fixture:inspection")?,
        house: house()?,
        observation: source(&format!("fixture:{name}"))?,
        question: Text::new("Does the parser reject duplicate keys?")?,
        inspector: holder("independent-reviewer")?,
        task: task_id("inspector")?,
        independent: true,
        max_samples: 2,
        max_tokens: 100,
        deadline: at(60),
    })
}

#[test]
fn preview_writes_nothing_and_apply_moves_uncited_streams_with_their_bindings() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    delivered(&f, &l, "cited")?;
    let moved = delivered(&f, &l, "settled")?;
    grant_on(&l, "fixture:grant", "cited", true)?;
    let acting = acting(&l, "acting")?;
    assert!(standing_covers(&f, &l, &acting)?);
    let before = fs::read(ledger_path(&f))?;

    let preview = l.preview_archive(at(100))?;
    assert_eq!(preview.streams.len(), 1);
    let stream = preview.streams.first().ok_or("stream")?;
    assert_eq!(stream.id, moved.id);
    assert_eq!((stream.revisions, stream.binding), (1, true));
    assert_eq!(preview.kept.streams_cited_by_grants, 1);
    assert_eq!(preview.kept.unobserved_bindings, 1, "the acting task");
    assert_eq!(preview.kept.grant_audits, 1);
    assert!(preview.archival.is_none());
    assert_eq!(
        fs::read(ledger_path(&f))?,
        before,
        "a preview writes nothing"
    );
    assert!(!archive_path(&f).exists());

    let applied = l.archive(at(100))?;
    assert_eq!(applied.streams, preview.streams);
    let archival = applied.archival.ok_or("archival")?;
    assert_eq!(
        (
            archival.observations,
            archival.bindings,
            archival.inspections
        ),
        (1, 1, 0)
    );
    let l = reopen(&f)?;
    assert_eq!(l.archivals()?, vec![archival.clone()]);
    let history = l.history()?;
    assert_eq!(history.len(), 1);
    assert_eq!(history.first().map(|o| &o.task), Some(&task_id("cited")?));
    // Two observations and two bindings left; the summary took one entry.
    assert_eq!(l.capacity()?.entries, 1 + 2 + 1 + 1);

    let batches = archived(&f)?;
    let [(digest, batch)] = batches.as_slice() else {
        return Err("one batch expected".into());
    };
    assert_eq!(digest, archival.digest.as_str());
    assert_eq!(batch.house, house()?);
    assert_eq!(batch.observations, vec![moved]);
    assert_eq!(
        batch
            .bindings
            .iter()
            .map(|b| &b.spec.id)
            .collect::<Vec<_>>(),
        vec![&task_id("settled")?]
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(archive_path(&f))?.permissions().mode() & 0o777,
            0o600
        );
    }
    // The grant's evidence stayed, so its standing and revocation do not
    // need the archive.
    assert!(standing_covers(&f, &l, &acting)?);
    assert!(revoke(&l, &source("fixture:grant")?)?);
    assert!(!standing_covers(&f, &l, &acting)?);
    Ok(())
}

#[test]
fn nothing_to_archive_writes_nothing_and_a_repeat_is_a_no_op() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let before = fs::read(ledger_path(&f))?;
    let report = l.archive(at(1))?;
    assert!(report.is_empty() && report.archival.is_none());
    assert_eq!(fs::read(ledger_path(&f))?, before);
    assert!(!archive_path(&f).exists());

    delivered(&f, &l, "settled")?;
    assert!(l.archive(at(2))?.archival.is_some());
    let after = fs::read(ledger_path(&f))?;
    let report = l.archive(at(3))?;
    assert!(report.is_empty() && report.archival.is_none());
    assert_eq!(fs::read(ledger_path(&f))?, after);
    assert_eq!(archived(&f)?.len(), 1);
    Ok(())
}

#[test]
fn proposed_and_revoked_grants_keep_their_evidence_live() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    delivered(&f, &l, "proposed")?;
    delivered(&f, &l, "revoked")?;
    let pending = grant_on(&l, "fixture:pending", "proposed", false)?;
    let revoked = grant_on(&l, "fixture:revoked", "revoked", true)?;
    assert!(revoke(&l, &revoked)?);
    let report = l.archive(at(100))?;
    assert!(report.is_empty());
    assert_eq!(report.kept.streams_cited_by_grants, 2);
    assert_eq!(l.history()?.len(), 2);
    // The pending proposal can still be approved, then revoked.
    assert!(l.approve(
        &pending,
        holder("owner")?,
        source("fixture:decision")?,
        at(7),
        &policy()?
    )?);
    assert!(revoke(&l, &pending)?);
    assert!(matches!(
        reopen(&f)?.grant_history()?.as_slice(),
        [GrantAudit::Revoked { .. }, GrantAudit::Revoked { .. }]
    ));
    Ok(())
}

#[test]
fn inspections_leave_only_after_their_deadline_with_every_sample_answered() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    delivered(&f, &l, "inspected")?;
    let p = plan("inspected")?;
    l.start_inspection(
        &f.store,
        p.clone(),
        fence(&f)?,
        &ManualClock::starting_at(5),
    )?;
    l.reserve_sample(
        &f.store,
        &p.id,
        fence(&f)?,
        1,
        10,
        &ManualClock::starting_at(6),
    )?;

    // Open: before the deadline the stream and the inspection stay.
    let open = l.preview_archive(at(30))?;
    assert!(open.is_empty());
    assert_eq!(
        (
            open.kept.streams_under_inspection,
            open.kept.open_inspections
        ),
        (1, 1)
    );
    // Past the deadline, an unanswered sample still keeps both.
    assert!(l.preview_archive(at(60))?.is_empty());

    l.finish_sample(
        &f.store,
        &p.id,
        fence(&f)?,
        1,
        SampleResult::NoFinding {
            source: source("fixture:check")?,
        },
        &ManualClock::starting_at(61),
    )?;
    // Every sample answered but the deadline not reached: still open.
    assert!(l.preview_archive(at(59))?.is_empty());
    let report = l.archive(at(60))?;
    assert_eq!(report.inspections, vec![p.id.clone()]);
    assert_eq!(report.streams.len(), 1);
    let archival = report.archival.ok_or("archival")?;
    assert_eq!(
        (
            archival.observations,
            archival.bindings,
            archival.inspections
        ),
        (1, 1, 1)
    );
    assert!(matches!(l.inspection(&p.id), Err(TrustError::Incomplete)));
    let [(_, batch)] = archived(&f)?.try_into().map_err(|_| "one batch")?;
    assert_eq!(batch.inspections.first().map(|i| i.id()), Some(&p.id));
    // The plan cannot restart with a fresh budget against archived evidence.
    assert!(
        l.start_inspection(&f.store, p, fence(&f)?, &ManualClock::starting_at(61))
            .is_err()
    );
    Ok(())
}

/// A grant keeps the stream live, so its finished inspection stays too.
/// Archived, the inspection's record would be gone while its evidence is
/// still live, and an archival clock ahead of the inspector's would let the
/// same plan start over with no samples and a fresh budget.
#[test]
fn a_finished_inspection_of_a_grant_cited_stream_stays_live() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    delivered(&f, &l, "inspected")?;
    let p = plan("inspected")?;
    l.start_inspection(
        &f.store,
        p.clone(),
        fence(&f)?,
        &ManualClock::starting_at(5),
    )?;
    l.reserve_sample(
        &f.store,
        &p.id,
        fence(&f)?,
        1,
        10,
        &ManualClock::starting_at(6),
    )?;
    l.finish_sample(
        &f.store,
        &p.id,
        fence(&f)?,
        1,
        SampleResult::NoFinding {
            source: source("fixture:check")?,
        },
        &ManualClock::starting_at(7),
    )?;
    grant_on(&l, "fixture:grant", "inspected", false)?;
    let before = fs::read(ledger_path(&f))?;

    let report = l.archive(at(60))?;
    assert!(report.is_empty());
    assert_eq!(
        (
            report.kept.streams_cited_by_grants,
            report.kept.inspections_of_kept_streams,
            report.kept.open_inspections,
        ),
        (1, 1, 0)
    );
    assert_eq!(fs::read(ledger_path(&f))?, before);
    assert!(!archive_path(&f).exists());
    // Before the deadline by the inspector's clock, the same plan returns the
    // recorded inspection with its spent budget.
    let again = l.start_inspection(&f.store, p, fence(&f)?, &ManualClock::starting_at(59))?;
    assert_eq!(again.samples().len(), 1);
    Ok(())
}

#[test]
fn a_ledger_full_by_entries_accepts_new_records_after_archival() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let template = delivered(&f, &l, "cited")?;
    grant_on(&l, "fixture:grant", "cited", true)?;
    // Observation, binding, and grant, padded to the entry limit.
    pad_entries(&f, &template, MAX_HISTORY - 3)?;
    assert_eq!(l.capacity()?.entries, MAX_HISTORY);
    assert!(matches!(
        bind_extra(&l, "extra"),
        Err(TrustError::Exhausted)
    ));

    let report = l.archive(at(100))?;
    assert_eq!(report.streams.len(), MAX_HISTORY - 3);
    let capacity = l.capacity()?;
    assert_eq!(capacity.entries, 4, "three kept records and one summary");
    assert!(!capacity.near_limit());
    assert!(bind_extra(&l, "extra")?);
    let acting = acting(&l, "acting")?;
    assert!(standing_covers(&f, &l, &acting)?);
    assert!(revoke(&l, &source("fixture:grant")?)?);
    assert_eq!(archived(&f)?.len(), 1);
    Ok(())
}

#[test]
fn a_ledger_full_by_bytes_accepts_new_records_after_archival() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let template = delivered(&f, &l, "cited")?;
    grant_on(&l, "fixture:grant", "cited", true)?;
    fill_bytes(&f, &template, ORDINARY_LIMIT - 8)?;
    assert!(matches!(
        bind_extra(&l, "extra"),
        Err(TrustError::Storage(StateError::StateTooLarge {
            limit_bytes: ORDINARY_LIMIT
        }))
    ));
    assert!(l.capacity()?.near_limit());

    let archival = l.archive(at(100))?.archival.ok_or("archival")?;
    assert!(archival.observations > 100);
    let capacity = l.capacity()?;
    assert!(
        capacity.bytes < 64 * 1024,
        "ledger is {} bytes",
        capacity.bytes
    );
    assert!(bind_extra(&l, "extra")?);
    assert!(fs::metadata(archive_path(&f))?.len() > ORDINARY_LIMIT - 64 * 1024);
    assert!(revoke(&l, &source("fixture:grant")?)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_redirected_or_public_archive_file_is_refused_without_changing_the_ledger() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    delivered(&f, &l, "settled")?;
    let before = fs::read(ledger_path(&f))?;

    let elsewhere = f.dir.path().join("elsewhere.jsonl");
    fs::write(&elsewhere, b"")?;
    std::os::unix::fs::symlink(&elsewhere, archive_path(&f))?;
    assert!(matches!(
        l.archive(at(100)),
        Err(TrustError::Storage(StateError::RedirectedPath))
    ));
    assert_eq!(fs::read(&elsewhere)?, b"");
    assert_eq!(fs::read(ledger_path(&f))?, before);

    fs::remove_file(archive_path(&f))?;
    fs::write(archive_path(&f), b"")?;
    fs::set_permissions(archive_path(&f), fs::Permissions::from_mode(0o644))?;
    assert!(matches!(
        l.archive(at(100)),
        Err(TrustError::Storage(StateError::PublicPath))
    ));
    assert_eq!(fs::read(ledger_path(&f))?, before);
    assert_eq!(fs::read(archive_path(&f))?, b"");
    Ok(())
}

#[test]
fn a_stored_summary_that_is_empty_or_malformed_is_corrupt() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    delivered(&f, &l, "settled")?;
    l.archive(at(100))?;
    let path = ledger_path(&f);
    let original: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    for (field, value) in [
        ("observations", serde_json::json!(0)),
        ("digest", serde_json::json!("not-hex")),
        ("bytes", serde_json::json!(0)),
    ] {
        let mut document = original.clone();
        if field == "observations" {
            document["archivals"][0]["bindings"] = serde_json::json!(0);
        }
        document["archivals"][0][field] = value;
        fs::write(&path, serde_json::to_vec(&document)?)?;
        assert!(
            matches!(
                Ledger::open(f.dir.path().join("trust"), house()?),
                Err(TrustError::Corrupt | TrustError::Storage(_))
            ),
            "{field} must be refused"
        );
    }
    // Record counts whose sum overflows are refused, not summed.
    let mut counted = original.clone();
    counted["archivals"][0]["observations"] = serde_json::json!(usize::MAX);
    counted["archivals"][0]["bindings"] = serde_json::json!(1);
    fs::write(&path, serde_json::to_vec(&counted)?)?;
    assert!(matches!(
        Ledger::open(f.dir.path().join("trust"), house()?),
        Err(TrustError::Corrupt)
    ));
    // Two summaries whose lengths overflow a file length.
    let mut overflowing = original.clone();
    let summaries = overflowing["archivals"].as_array_mut().ok_or("archivals")?;
    let mut second = summaries.first().cloned().ok_or("summary")?;
    second["digest"] = serde_json::json!("0".repeat(64));
    summaries.push(second);
    for summary in summaries.iter_mut() {
        summary["bytes"] = serde_json::json!(u64::MAX);
    }
    fs::write(&path, serde_json::to_vec(&overflowing)?)?;
    assert!(matches!(
        Ledger::open(f.dir.path().join("trust"), house()?),
        Err(TrustError::Corrupt)
    ));
    let mut duplicated = original;
    let summaries = duplicated["archivals"].as_array_mut().ok_or("archivals")?;
    let first = summaries.first().cloned().ok_or("summary")?;
    summaries.push(first);
    fs::write(&path, serde_json::to_vec(&duplicated)?)?;
    assert!(matches!(
        Ledger::open(f.dir.path().join("trust"), house()?),
        Err(TrustError::Corrupt)
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_hard_linked_archive_file_is_refused_without_writing_either_file() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    delivered(&f, &l, "settled")?;
    let before = fs::read(ledger_path(&f))?;

    // Another house's private archive, linked in under this house's name.
    let elsewhere = f.dir.path().join("other-house-archive.jsonl");
    fs::write(&elsewhere, b"{\"other\":true}\n")?;
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o600))?;
    fs::hard_link(&elsewhere, archive_path(&f))?;
    assert!(matches!(
        l.archive(at(100)),
        Err(TrustError::Storage(StateError::RedirectedPath))
    ));
    assert_eq!(fs::read(&elsewhere)?, b"{\"other\":true}\n");
    assert_eq!(fs::read(ledger_path(&f))?, before);

    // Once the link is gone, the house gets its own file.
    fs::remove_file(archive_path(&f))?;
    l.archive(at(100))?;
    assert_eq!(archived(&f)?.len(), 1);
    assert_eq!(fs::read(&elsewhere)?, b"{\"other\":true}\n");
    Ok(())
}

/// A failed append leaves either part of a batch (the write stopped) or a
/// whole batch whose ledger commit failed. Neither was committed, and neither
/// holds a record the ledger lacks, so the next archival drops it and every
/// line stays one committed batch.
#[test]
fn an_uncommitted_tail_is_dropped_before_the_next_batch() -> TestResult {
    let empty = empty_batch(&house()?.to_string());
    let tails: [&[u8]; 3] = [b"{\"schema\":1,\"house\":\"exa", empty.as_bytes(), b"\n"];
    for (index, tail) in tails.into_iter().enumerate() {
        let f = Fixture::new()?;
        let l = ledger(&f)?;
        // A tail on a file with nothing committed yet.
        fs::write(archive_path(&f), tail)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(archive_path(&f), fs::Permissions::from_mode(0o600))?;
        }
        delivered(&f, &l, "first")?;
        l.archive(at(100))?;
        // A tail after a committed batch.
        let mut bytes = fs::read(archive_path(&f))?;
        bytes.extend_from_slice(tail);
        fs::write(archive_path(&f), &bytes)?;
        delivered(&f, &l, "second")?;
        l.archive(at(200))?;

        let batches = archived(&f)?;
        let digests: Vec<&str> = batches.iter().map(|(d, _)| d.as_str()).collect();
        let committed: Vec<String> = reopen(&f)?
            .archivals()?
            .iter()
            .map(|a| a.digest.as_str().to_owned())
            .collect();
        assert_eq!(digests, committed, "tail {index}");
        let tasks: Vec<_> = batches
            .iter()
            .flat_map(|(_, b)| b.observations.iter().map(|o| o.task.clone()))
            .collect();
        assert_eq!(tasks, vec![task_id("first")?, task_id("second")?]);
    }
    Ok(())
}

/// The archive append synced but the ledger write did not: the ledger still
/// holds every record of that batch, so the next archival drops it and
/// writes the batch again.
#[test]
fn a_batch_whose_ledger_commit_failed_is_replaced() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    delivered(&f, &l, "first")?;
    let uncommitted = fs::read(ledger_path(&f))?;
    l.archive(at(100))?;
    // The ledger as it was before the failed commit.
    fs::write(ledger_path(&f), &uncommitted)?;

    let reopened = reopen(&f)?;
    let report = reopened.archive(at(200))?;
    assert_eq!(report.streams.len(), 1);
    let [(digest, batch)] = archived(&f)?.try_into().map_err(|_| "one batch")?;
    assert_eq!(batch.at, at(200));
    let committed = reopened.archivals()?;
    assert_eq!(
        committed
            .iter()
            .map(|a| a.digest.as_str())
            .collect::<Vec<_>>(),
        vec![digest.as_str()]
    );
    Ok(())
}

/// A ledger restored from a copy older than an archival records fewer
/// batches than the file holds. A batch with a record the ledger lacks is
/// its only copy, so the next archival refuses rather than cut it off.
#[test]
fn a_restored_older_ledger_is_refused_unchanged() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    delivered(&f, &l, "first")?;
    let older = fs::read(ledger_path(&f))?;
    delivered(&f, &l, "second")?;
    l.archive(at(100))?;
    let archive = fs::read(archive_path(&f))?;
    let newer = fs::read(ledger_path(&f))?;

    // The backup still holds "first", but not "second".
    fs::write(ledger_path(&f), &older)?;
    let restored = reopen(&f)?;
    assert_eq!(restored.preview_archive(at(200))?.streams.len(), 1);
    let refused = restored.archive(at(200));
    assert!(matches!(
        refused,
        Err(TrustError::Storage(StateError::CorruptState(
            Corruption::UnreconciledAppend
        )))
    ));
    assert_eq!(fs::read(archive_path(&f))?, archive);
    assert_eq!(fs::read(ledger_path(&f))?, older);

    // Restoring the ledger that committed the batch recovers.
    fs::write(ledger_path(&f), &newer)?;
    let recovered = reopen(&f)?;
    delivered(&f, &recovered, "third")?;
    recovered.archive(at(300))?;
    let tasks: Vec<_> = archived(&f)?
        .iter()
        .flat_map(|(_, b)| b.observations.iter().map(|o| o.task.clone()))
        .collect();
    assert_eq!(
        tasks,
        vec![task_id("first")?, task_id("second")?, task_id("third")?]
    );
    Ok(())
}

/// An archive line holding no records, as the given house.
fn empty_batch(house: &str) -> String {
    format!(
        "{{\"schema\":1,\"house\":\"{house}\",\"at\":1,\"observations\":[],\"bindings\":[],\"inspections\":[]}}\n"
    )
}

/// A whole batch of another house is not this ledger's to drop, even empty.
#[test]
fn an_uncommitted_batch_of_another_house_is_refused_unchanged() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let foreign = empty_batch("other-house");
    fs::write(archive_path(&f), &foreign)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(archive_path(&f), fs::Permissions::from_mode(0o600))?;
    }
    delivered(&f, &l, "first")?;
    let before = fs::read(ledger_path(&f))?;
    assert!(matches!(
        l.archive(at(100)),
        Err(TrustError::Storage(StateError::CorruptState(
            Corruption::UnreconciledAppend
        )))
    ));
    assert_eq!(fs::read(archive_path(&f))?, foreign.as_bytes());
    assert_eq!(fs::read(ledger_path(&f))?, before);
    Ok(())
}

#[test]
fn an_archive_shorter_than_its_summaries_is_refused() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    delivered(&f, &l, "first")?;
    l.archive(at(100))?;
    let committed = fs::read(archive_path(&f))?;
    delivered(&f, &l, "second")?;
    let before = fs::read(ledger_path(&f))?;

    let truncated = committed.get(..committed.len() - 1).ok_or("line")?;
    fs::write(archive_path(&f), truncated)?;
    assert!(matches!(
        l.archive(at(200)),
        Err(TrustError::Storage(StateError::CorruptState(
            Corruption::TruncatedAppend
        )))
    ));
    assert_eq!(fs::read(archive_path(&f))?, truncated);
    assert_eq!(fs::read(ledger_path(&f))?, before);

    fs::remove_file(archive_path(&f))?;
    assert!(matches!(
        l.archive(at(200)),
        Err(TrustError::Storage(StateError::CorruptState(
            Corruption::TruncatedAppend
        )))
    ));
    assert_eq!(fs::read(ledger_path(&f))?, before);
    Ok(())
}

/// A summary whose length or digest was altered still loads, but the next
/// archival reads the committed lines back and refuses before it cuts off or
/// appends anything. Otherwise a shortened length would cut the last byte of
/// a committed batch and join it to the next one.
#[test]
fn committed_lines_that_differ_from_their_summaries_are_refused_unchanged() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    delivered(&f, &l, "first")?;
    l.archive(at(100))?;
    delivered(&f, &l, "second")?;
    l.archive(at(200))?;
    delivered(&f, &l, "third")?;
    let archive = fs::read(archive_path(&f))?;
    let original: serde_json::Value = serde_json::from_slice(&fs::read(ledger_path(&f))?)?;
    let lengths: Vec<u64> = original["archivals"]
        .as_array()
        .ok_or("archivals")?
        .iter()
        .map(|a| a["bytes"].as_u64().ok_or("bytes"))
        .collect::<Result<_, _>>()?;
    let [first, second] = lengths[..] else {
        return Err("two summaries".into());
    };
    assert_eq!(first + second, u64::try_from(archive.len())?);

    let cases = [
        // The first batch's last byte would read as an uncommitted tail.
        ("first shortened", Some((first - 1, second)), None),
        // Same total length, so only per-line framing catches it.
        ("byte moved to second", Some((first - 1, second + 1)), None),
        ("byte moved to first", Some((first + 1, second - 1)), None),
        ("second digest", None, Some("0".repeat(64))),
    ];
    for (case, bytes, digest) in cases {
        let mut document = original.clone();
        if let Some((a, b)) = bytes {
            document["archivals"][0]["bytes"] = serde_json::json!(a);
            document["archivals"][1]["bytes"] = serde_json::json!(b);
        }
        if let Some(digest) = digest {
            document["archivals"][1]["digest"] = serde_json::json!(digest);
        }
        let altered = serde_json::to_vec(&document)?;
        fs::write(ledger_path(&f), &altered)?;
        let reopened = reopen(&f)?;
        assert!(
            matches!(
                reopened.archive(at(300)),
                Err(TrustError::Storage(StateError::CorruptState(
                    Corruption::AppendMismatch
                )))
            ),
            "{case} must be refused"
        );
        assert_eq!(fs::read(archive_path(&f))?, archive, "{case}: archive");
        assert_eq!(fs::read(ledger_path(&f))?, altered, "{case}: ledger");
    }

    // With the true summaries back, the next batch lands after the others.
    fs::write(ledger_path(&f), serde_json::to_vec(&original)?)?;
    reopen(&f)?.archive(at(300))?;
    let tasks: Vec<_> = archived(&f)?
        .iter()
        .flat_map(|(_, b)| b.observations.iter().map(|o| o.task.clone()))
        .collect();
    assert_eq!(
        tasks,
        vec![task_id("first")?, task_id("second")?, task_id("third")?]
    );
    Ok(())
}
