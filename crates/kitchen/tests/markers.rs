//! Durable workflow markers: facts keyed by workflow, work item, and exact
//! evidence subject. Simulated with temporary stores.

mod common;

use std::{fs, num::NonZeroU64};

use common::{Fixture, TestResult, at, commit, scheduled, spec, task_id, ttl};
use kitchen::{
    ConsumerId, Error, WorkflowId,
    contracts::{
        EvidenceSubject, EvidenceVerdict, ExternalRef, Repository, ResourceKind, ResourceRef,
    },
    state::{
        Corruption, IssueRevision, MAX_MARKERS, MarkerAttempt, MarkerFact, MarkerKey,
        MarkerRecording, MarkerSubject, StateError, WorkItem,
    },
};

fn pull_request(number: u64) -> TestResult<WorkItem> {
    Ok(WorkItem::PullRequest {
        repository: Repository::new("origin89hq/km43")?,
        number: NonZeroU64::new(number).ok_or("zero")?,
    })
}

fn key(workflow: &str, item: WorkItem, head: char, base: Option<char>) -> TestResult<MarkerKey> {
    Ok(MarkerKey {
        workflow: WorkflowId::new(workflow)?,
        item,
        subject: MarkerSubject::Git(EvidenceSubject {
            head: commit(head)?,
            base: base.map(commit).transpose()?,
        }),
    })
}

fn issue_key(number: u64, updated: u64, last_comment: Option<&str>) -> TestResult<MarkerKey> {
    Ok(MarkerKey {
        workflow: WorkflowId::new("triage")?,
        item: WorkItem::Issue {
            repository: Repository::new("origin89hq/km43")?,
            number: NonZeroU64::new(number).ok_or("zero")?,
        },
        subject: MarkerSubject::Issue(IssueRevision {
            updated_at: at(updated),
            last_comment: last_comment.map(ExternalRef::new).transpose()?,
        }),
    })
}

type Expectation = fn(&Error) -> bool;

fn verdict(verdict: EvidenceVerdict) -> MarkerFact {
    MarkerFact::Verdict { verdict }
}

#[test]
fn recording_the_same_fact_again_changes_nothing() -> TestResult {
    let fixture = Fixture::new()?;
    let gate = key("merge-gate", pull_request(20)?, 'a', Some('b'))?;
    let first = fixture.store.record_marker(
        gate.clone(),
        verdict(EvidenceVerdict::Pass),
        &scheduled("gate-tick-1")?,
        at(1),
    )?;
    let MarkerRecording::Recorded(recorded) = first else {
        return Err("expected a new marker".into());
    };
    assert_eq!(recorded.recorded_at(), at(1));
    assert_eq!(
        fixture.store.record_marker(
            gate.clone(),
            verdict(EvidenceVerdict::Pass),
            &scheduled("gate-tick-2")?,
            at(2)
        )?,
        MarkerRecording::AlreadyRecorded(recorded.clone()),
        "the original marker, time, and recorder are kept"
    );
    // A different fact for the same head is refused, not overwritten.
    assert!(matches!(
        fixture.store.record_marker(
            gate.clone(),
            verdict(EvidenceVerdict::Fail),
            &scheduled("gate-tick-3")?,
            at(3)
        ),
        Err(Error::State(StateError::MarkerConflict))
    ));
    assert_eq!(fixture.store.marker(&gate)?, Some(recorded));
    Ok(())
}

#[test]
fn a_moved_head_or_base_or_another_item_is_a_distinct_key() -> TestResult {
    let fixture = Fixture::new()?;
    let recorder = scheduled("gate")?;
    let keys = [
        key("merge-gate", pull_request(20)?, 'a', Some('b'))?,
        key("merge-gate", pull_request(20)?, 'c', Some('b'))?,
        key("merge-gate", pull_request(20)?, 'a', Some('d'))?,
        key("merge-gate", pull_request(20)?, 'a', None)?,
        key("merge-gate", pull_request(21)?, 'a', Some('b'))?,
        key(
            "merge-gate",
            WorkItem::Issue {
                repository: Repository::new("origin89hq/km43")?,
                number: NonZeroU64::new(20).ok_or("zero")?,
            },
            'a',
            Some('b'),
        )?,
        key("triage", pull_request(20)?, 'a', Some('b'))?,
    ];
    for key in &keys {
        assert!(matches!(
            fixture.store.record_marker(
                key.clone(),
                verdict(EvidenceVerdict::Pass),
                &recorder,
                at(1)
            )?,
            MarkerRecording::Recorded(_)
        ));
    }
    assert_eq!(
        fixture
            .store
            .markers(&WorkflowId::new("merge-gate")?)?
            .len(),
        6
    );
    assert_eq!(fixture.store.markers(&WorkflowId::new("triage")?)?.len(), 1);
    assert_eq!(
        fixture
            .store
            .marker(&key("merge-gate", pull_request(99)?, 'a', None)?)?,
        None
    );
    Ok(())
}

#[test]
fn markers_persist_and_perform_no_effects() -> TestResult {
    let fixture = Fixture::new()?;
    let question = key("triage", pull_request(7)?, 'a', None)?;
    let asked = MarkerFact::QuestionAsked {
        question: ExternalRef::new("roger-ask-17")?,
    };
    fixture.store.record_marker(
        question.clone(),
        asked.clone(),
        &scheduled("triage-tick")?,
        at(1),
    )?;
    let reopened = fixture.reopen()?;
    let marker = reopened.marker(&question)?.ok_or("marker lost on reopen")?;
    assert_eq!(marker.fact(), &asked);
    assert_eq!(marker.recorded_by(), &scheduled("triage-tick")?);
    assert!(
        reopened.tasks()?.is_empty(),
        "a marker is not a task or an effect"
    );
    Ok(())
}

#[test]
fn a_superseded_consumer_cannot_record_markers() -> TestResult {
    let fixture = Fixture::new()?;
    let store = &fixture.store;
    let gate = ConsumerId::new("gate-origin89")?;
    let first = store.acquire_consumer(&gate, &scheduled("tick-1")?, ttl(60)?, at(0))?;
    let tick1 = scheduled("tick-1")?.under(gate.clone(), first.fence());
    store.record_marker(
        key("merge-gate", pull_request(1)?, 'a', None)?,
        verdict(EvidenceVerdict::Pass),
        &tick1,
        at(1),
    )?;
    store.take_over_consumer(&gate, &scheduled("tick-2")?, ttl(60)?, at(61))?;
    assert!(matches!(
        store.record_marker(
            key("merge-gate", pull_request(2)?, 'a', None)?,
            verdict(EvidenceVerdict::Pass),
            &tick1,
            at(62)
        ),
        Err(Error::State(StateError::StaleFence { .. }))
    ));
    assert_eq!(store.markers(&WorkflowId::new("merge-gate")?)?.len(), 1);
    Ok(())
}

#[test]
fn the_marker_count_is_bounded() -> TestResult {
    let fixture = Fixture::new()?;
    let recorder = scheduled("gate")?;
    fixture.store.record_marker(
        key("merge-gate", pull_request(1)?, 'a', None)?,
        verdict(EvidenceVerdict::Pass),
        &recorder,
        at(1),
    )?;
    // Fill the store to its bound by copying the one valid marker.
    let path = fixture.state_path();
    let mut state: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let template = state["markers"][0].clone();
    let markers: Vec<_> = (1..=MAX_MARKERS)
        .map(|number| {
            let mut marker = template.clone();
            marker["key"]["item"]["number"] = number.into();
            marker
        })
        .collect();
    state["markers"] = markers.into();
    fs::write(&path, serde_json::to_vec(&state)?)?;
    let store = fixture.reopen()?;

    let extra = key("merge-gate", pull_request(99_999)?, 'a', None)?;
    assert!(matches!(
        store.record_marker(
            extra.clone(),
            verdict(EvidenceVerdict::Pass),
            &recorder,
            at(2)
        ),
        Err(Error::State(StateError::CapacityExceeded {
            limit: kitchen::state::Limit::Markers
        }))
    ));
    assert_eq!(store.marker(&extra)?, None);
    // Repeating an existing fact still succeeds when full.
    assert!(matches!(
        store.record_marker(
            key("merge-gate", pull_request(1)?, 'a', None)?,
            verdict(EvidenceVerdict::Pass),
            &recorder,
            at(2)
        )?,
        MarkerRecording::AlreadyRecorded(_)
    ));

    // One more than the bound is corrupt.
    let mut over = state.clone();
    let mut marker = template;
    marker["key"]["item"]["number"] = (MAX_MARKERS + 1).into();
    if let Some(list) = over["markers"].as_array_mut() {
        list.push(marker);
    }
    fs::write(&path, serde_json::to_vec(&over)?)?;
    let error = fixture.reopen().err().ok_or("over-full markers accepted")?;
    assert!(matches!(
        error.downcast::<Error>().map(|error| *error),
        Ok(Error::State(StateError::CorruptState(
            Corruption::LimitExceeded
        )))
    ));
    Ok(())
}

#[test]
fn corrupt_markers_are_rejected_without_reset() -> TestResult {
    let fixture = Fixture::new()?;
    let recorder = scheduled("gate")?;
    for number in [1, 2] {
        fixture.store.record_marker(
            key("merge-gate", pull_request(number)?, 'a', None)?,
            verdict(EvidenceVerdict::Pass),
            &recorder,
            at(1),
        )?;
    }
    let path = fixture.state_path();
    let valid: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let mut duplicate = valid.clone();
    duplicate["markers"][1]["key"] = valid["markers"][0]["key"].clone();
    let mut malformed = valid.clone();
    malformed["markers"][0]["fact"] = serde_json::json!({"type": "verdict", "verdict": "maybe"});
    let mut zero = valid.clone();
    zero["markers"][0]["key"]["item"]["number"] = 0.into();
    assert_eq!(valid["markers"][0]["key"]["subject"]["type"], "git");
    let mut bad_head = valid.clone();
    bad_head["markers"][0]["key"]["subject"]["revision"]["head"] = "not-a-commit".into();
    let cases: [(serde_json::Value, Expectation); 4] = [
        (bad_head, syntax),
        (duplicate, |error| {
            matches!(
                error,
                Error::State(StateError::CorruptState(
                    Corruption::DuplicateWorkflowMarker
                ))
            )
        }),
        (malformed, |error| {
            matches!(
                error,
                Error::State(StateError::CorruptState(Corruption::Syntax { .. }))
            )
        }),
        (zero, |error| {
            matches!(
                error,
                Error::State(StateError::CorruptState(Corruption::Syntax { .. }))
            )
        }),
    ];
    for (corrupt, expected) in cases {
        let bytes = serde_json::to_vec_pretty(&corrupt)?;
        fs::write(&path, &bytes)?;
        let error = fixture
            .reopen()
            .err()
            .ok_or("corrupt markers were accepted")?;
        let error = error
            .downcast::<Error>()
            .map_err(|_| "unexpected error type")?;
        assert!(expected(&error), "{error:?}");
        assert_eq!(fs::read(&path)?, bytes, "rejected state is not rewritten");
    }
    Ok(())
}

#[test]
fn a_verdict_can_be_superseded_explicitly_for_the_same_head() -> TestResult {
    let fixture = Fixture::new()?;
    let gate = key("merge-gate", pull_request(20)?, 'a', Some('b'))?;
    let fail = verdict(EvidenceVerdict::Fail);
    let pass = verdict(EvidenceVerdict::Pass);
    fixture
        .store
        .record_marker(gate.clone(), fail.clone(), &scheduled("tick-1")?, at(1))?;

    // fail -> pass: the current fact is replaced and the old one kept.
    let MarkerRecording::Superseded(marker) =
        fixture
            .store
            .supersede_marker(&gate, &fail, pass.clone(), &scheduled("tick-2")?, at(2))?
    else {
        return Err("expected a supersession".into());
    };
    assert_eq!(marker.fact(), &pass);
    assert_eq!(marker.recorded_by(), &scheduled("tick-2")?);
    assert_eq!(marker.recorded_at(), at(2));
    assert!(matches!(
        marker.history(),
        [prior] if prior.fact == fail && prior.recorded_at == at(1) && prior.superseded_at == at(2)
            && prior.recorded_by == scheduled("tick-1")?
    ));

    // The same fact again is a no-op, even through supersede.
    assert!(matches!(
        fixture.store.supersede_marker(&gate, &pass, pass.clone(), &scheduled("tick-3")?, at(3))?,
        MarkerRecording::AlreadyRecorded(unchanged) if unchanged == marker
    ));
    // A stale writer that still expects `fail` is refused.
    assert!(matches!(
        fixture.store.supersede_marker(
            &gate,
            &fail,
            verdict(EvidenceVerdict::Unavailable),
            &scheduled("stale")?,
            at(4)
        ),
        Err(Error::State(StateError::MarkerConflict))
    ));
    // pass -> fail works the same way, and survives a reopen.
    fixture
        .store
        .supersede_marker(&gate, &pass, fail.clone(), &scheduled("tick-5")?, at(5))?;
    let reopened = fixture.reopen()?.marker(&gate)?.ok_or("marker lost")?;
    assert_eq!(reopened.fact(), &fail);
    assert_eq!(
        reopened
            .history()
            .iter()
            .map(|prior| prior.fact.clone())
            .collect::<Vec<_>>(),
        [fail.clone(), pass.clone()]
    );
    // Silent conflicting writes are still refused.
    assert!(matches!(
        fixture
            .store
            .record_marker(gate, pass, &scheduled("tick-6")?, at(6)),
        Err(Error::State(StateError::MarkerConflict))
    ));
    Ok(())
}

#[test]
fn supersession_keeps_a_bounded_history_and_never_refuses_for_capacity() -> TestResult {
    let fixture = Fixture::new()?;
    let gate = key("merge-gate", pull_request(20)?, 'a', None)?;
    let facts = [EvidenceVerdict::Pass, EvidenceVerdict::Fail];
    fixture
        .store
        .record_marker(gate.clone(), verdict(facts[0]), &scheduled("tick")?, at(0))?;
    let rounds = kitchen::state::MAX_MARKER_HISTORY + 5;
    for round in 1..=rounds {
        let before = verdict(facts[(round - 1) % 2]);
        let after = verdict(facts[round % 2]);
        let seconds = u64::try_from(round)?;
        assert!(matches!(
            fixture.store.supersede_marker(
                &gate,
                &before,
                after,
                &scheduled("tick")?,
                at(seconds)
            )?,
            MarkerRecording::Superseded(_)
        ));
    }
    let marker = fixture.store.marker(&gate)?.ok_or("marker lost")?;
    assert_eq!(marker.history().len(), kitchen::state::MAX_MARKER_HISTORY);
    assert_eq!(marker.dropped_history(), 5);
    // The oldest kept entry is the one replaced in round 6.
    assert_eq!(
        marker.history().first().map(|prior| prior.superseded_at),
        Some(at(6))
    );
    Ok(())
}

#[test]
fn questions_are_not_superseded_and_missing_markers_cannot_be() -> TestResult {
    let fixture = Fixture::new()?;
    let triage = key("triage", pull_request(7)?, 'a', None)?;
    let asked = MarkerFact::QuestionAsked {
        question: ExternalRef::new("roger-ask-1")?,
    };
    fixture
        .store
        .record_marker(triage.clone(), asked.clone(), &scheduled("tick")?, at(1))?;
    let reasked = MarkerFact::QuestionAsked {
        question: ExternalRef::new("roger-ask-2")?,
    };
    assert!(matches!(
        fixture
            .store
            .supersede_marker(&triage, &asked, reasked, &scheduled("tick")?, at(2)),
        Err(Error::State(StateError::MarkerNotSupersedable))
    ));
    let missing = key("merge-gate", pull_request(8)?, 'a', None)?;
    assert!(matches!(
        fixture.store.supersede_marker(
            &missing,
            &verdict(EvidenceVerdict::Fail),
            verdict(EvidenceVerdict::Pass),
            &scheduled("tick")?,
            at(3)
        ),
        Err(Error::State(StateError::MarkerNotFound))
    ));
    assert_eq!(fixture.store.marker(&missing)?, None);
    Ok(())
}

#[test]
fn corrupt_marker_history_is_rejected_without_reset() -> TestResult {
    let fixture = Fixture::new()?;
    let gate = key("merge-gate", pull_request(20)?, 'a', None)?;
    let fail = verdict(EvidenceVerdict::Fail);
    fixture
        .store
        .record_marker(gate.clone(), fail.clone(), &scheduled("tick")?, at(1))?;
    fixture.store.supersede_marker(
        &gate,
        &fail,
        verdict(EvidenceVerdict::Pass),
        &scheduled("tick")?,
        at(2),
    )?;
    let path = fixture.state_path();
    let valid: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let entry = valid["markers"][0]["history"][0].clone();
    let mut over = valid.clone();
    over["markers"][0]["history"] =
        vec![entry.clone(); kitchen::state::MAX_MARKER_HISTORY + 1].into();
    let mut malformed = valid.clone();
    malformed["markers"][0]["history"][0]["supersededAt"] = "yesterday".into();
    let cases: [(serde_json::Value, Expectation); 2] = [
        (over, |error| {
            matches!(
                error,
                Error::State(StateError::CorruptState(Corruption::LimitExceeded))
            )
        }),
        (malformed, |error| {
            matches!(
                error,
                Error::State(StateError::CorruptState(Corruption::Syntax { .. }))
            )
        }),
    ];
    for (corrupt, expected) in cases {
        let bytes = serde_json::to_vec_pretty(&corrupt)?;
        fs::write(&path, &bytes)?;
        let error = fixture
            .reopen()
            .err()
            .ok_or("corrupt history was accepted")?;
        let error = error
            .downcast::<Error>()
            .map_err(|_| "unexpected error type")?;
        assert!(expected(&error), "{error:?}");
        assert_eq!(fs::read(&path)?, bytes, "rejected state is not rewritten");
    }
    // An exactly full history is valid.
    let mut full = valid;
    full["markers"][0]["history"] = vec![entry; kitchen::state::MAX_MARKER_HISTORY].into();
    fs::write(&path, serde_json::to_vec_pretty(&full)?)?;
    let marker = fixture.reopen()?.marker(&gate)?.ok_or("marker lost")?;
    assert_eq!(marker.history().len(), kitchen::state::MAX_MARKER_HISTORY);
    Ok(())
}

/// A gate's own typed verdict, as #9 would define it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct GateVerdict {
    ready: bool,
    missing_checks: Vec<String>,
}

fn gate_schema(version: u32) -> TestResult<kitchen::state::MarkerSchema> {
    Ok(kitchen::state::MarkerSchema::new(
        "gate.verdict",
        std::num::NonZeroU32::new(version).ok_or("zero")?,
    )?)
}

#[test]
fn workflow_owned_facts_round_trip_through_their_schema() -> TestResult {
    let fixture = Fixture::new()?;
    let gate = key("merge-gate", pull_request(20)?, 'a', Some('b'))?;
    let pending = GateVerdict {
        ready: false,
        missing_checks: vec!["ci".to_owned()],
    };
    let fact = MarkerFact::workflow(gate_schema(1)?, &pending)?;
    fixture
        .store
        .record_marker(gate.clone(), fact.clone(), &scheduled("gate")?, at(1))?;
    assert!(matches!(
        fixture
            .store
            .record_marker(gate.clone(), fact.clone(), &scheduled("gate")?, at(2))?,
        MarkerRecording::AlreadyRecorded(_)
    ));
    let stored = fixture.reopen()?.marker(&gate)?.ok_or("marker lost")?;
    assert_eq!(
        stored.fact().decode::<GateVerdict>(&gate_schema(1)?)?,
        pending
    );
    assert_eq!(
        "gate.verdict/1".parse::<kitchen::state::MarkerSchema>()?,
        gate_schema(1)?
    );

    // A different version, or a core fact, is an explicit mismatch.
    assert!(matches!(
        stored.fact().decode::<GateVerdict>(&gate_schema(2)?),
        Err(StateError::MarkerSchemaMismatch { found: Some(found), .. }) if found == gate_schema(1)?
    ));
    assert!(matches!(
        verdict(EvidenceVerdict::Pass).decode::<GateVerdict>(&gate_schema(1)?),
        Err(StateError::MarkerSchemaMismatch { found: None, .. })
    ));
    // Same schema, wrong shape: the owning area sees a decode error.
    assert!(matches!(
        stored.fact().decode::<Vec<u8>>(&gate_schema(1)?),
        Err(StateError::MarkerPayloadInvalid)
    ));

    // The explicit supersede keeps the prior workflow fact in history.
    let ready = MarkerFact::workflow(
        gate_schema(1)?,
        &GateVerdict {
            ready: true,
            missing_checks: Vec::new(),
        },
    )?;
    let MarkerRecording::Superseded(updated) =
        fixture
            .store
            .supersede_marker(&gate, &fact, ready.clone(), &scheduled("gate")?, at(3))?
    else {
        return Err("expected a supersession".into());
    };
    assert_eq!(updated.fact(), &ready);
    assert!(matches!(updated.history(), [prior] if prior.fact == fact));
    Ok(())
}

#[test]
fn workflow_payloads_and_schemas_are_bounded_and_validated() -> TestResult {
    let at_bound = "x".repeat(kitchen::state::MAX_MARKER_PAYLOAD_BYTES - 2);
    assert!(
        MarkerFact::workflow(gate_schema(1)?, &at_bound).is_ok(),
        "quoted string at the bound"
    );
    let over = "x".repeat(kitchen::state::MAX_MARKER_PAYLOAD_BYTES);
    assert!(matches!(
        MarkerFact::workflow(gate_schema(1)?, &over),
        Err(StateError::MarkerPayloadInvalid)
    ));
    for invalid in [
        "gate.verdict",
        "gate.verdict/0",
        "gate.verdict/01",
        "Gate/1",
        "gate verdict/1",
        "/1",
        "gate/x",
    ] {
        assert!(
            matches!(
                invalid.parse::<kitchen::state::MarkerSchema>(),
                Err(StateError::MarkerSchemaInvalid)
            ),
            "{invalid}"
        );
    }
    assert!(kitchen::state::MarkerSchema::new(&"a".repeat(65), std::num::NonZeroU32::MIN).is_err());

    // Oversized or mis-named persisted values are rejected on load.
    let fixture = Fixture::new()?;
    let gate = key("merge-gate", pull_request(20)?, 'a', None)?;
    fixture.store.record_marker(
        gate,
        MarkerFact::workflow(gate_schema(1)?, &"ok")?,
        &scheduled("gate")?,
        at(1),
    )?;
    let path = fixture.state_path();
    let valid: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let mut oversized = valid.clone();
    oversized["markers"][0]["fact"]["payload"] = "x"
        .repeat(kitchen::state::MAX_MARKER_PAYLOAD_BYTES + 1)
        .into();
    let mut misnamed = valid;
    misnamed["markers"][0]["fact"]["schema"] = "Gate/1".into();
    for corrupt in [oversized, misnamed] {
        let bytes = serde_json::to_vec_pretty(&corrupt)?;
        fs::write(&path, &bytes)?;
        let error = fixture
            .reopen()
            .err()
            .ok_or("invalid workflow fact was accepted")?;
        let error = error
            .downcast::<Error>()
            .map_err(|_| "unexpected error type")?;
        assert!(
            matches!(
                *error,
                Error::State(StateError::CorruptState(Corruption::Syntax { .. }))
            ),
            "{error:?}"
        );
        assert_eq!(fs::read(&path)?, bytes);
    }
    Ok(())
}

#[test]
fn an_edited_issue_is_a_new_question_key() -> TestResult {
    let fixture = Fixture::new()?;
    let asked = MarkerFact::QuestionAsked {
        question: ExternalRef::new("roger-ask-1")?,
    };
    let revision = issue_key(7, 100, Some("comment-41"))?;
    fixture.store.record_marker(
        revision.clone(),
        asked.clone(),
        &scheduled("triage")?,
        at(1),
    )?;
    // The same issue revision is the same key: the question is not asked again.
    assert!(matches!(
        fixture.store.record_marker(
            issue_key(7, 100, Some("comment-41"))?,
            asked.clone(),
            &scheduled("triage")?,
            at(2)
        )?,
        MarkerRecording::AlreadyRecorded(_)
    ));
    // An edit or a new comment is a new key, whatever the repository head.
    for edited in [
        issue_key(7, 101, Some("comment-41"))?,
        issue_key(7, 100, Some("comment-42"))?,
        issue_key(7, 100, None)?,
    ] {
        assert_eq!(fixture.store.marker(&edited)?, None, "{edited:?}");
    }
    // A Git subject never collides with an issue revision.
    let git = key("triage", revision.item.clone(), 'a', None)?;
    assert_eq!(fixture.store.marker(&git)?, None);
    assert!(fixture.reopen()?.marker(&revision)?.is_some());
    Ok(())
}

#[test]
fn corrupt_issue_subjects_are_rejected_without_reset() -> TestResult {
    let fixture = Fixture::new()?;
    let asked = |id: &str| -> TestResult<MarkerFact> {
        Ok(MarkerFact::QuestionAsked {
            question: ExternalRef::new(id)?,
        })
    };
    fixture.store.record_marker(
        issue_key(7, 100, None)?,
        asked("q-1")?,
        &scheduled("triage")?,
        at(1),
    )?;
    fixture.store.record_marker(
        issue_key(7, 101, None)?,
        asked("q-2")?,
        &scheduled("triage")?,
        at(2),
    )?;
    let path = fixture.state_path();
    let valid: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    assert_eq!(valid["markers"][0]["key"]["subject"]["type"], "issue");
    let mut duplicate = valid.clone();
    duplicate["markers"][1]["key"] = valid["markers"][0]["key"].clone();
    let mut unknown_kind = valid.clone();
    unknown_kind["markers"][0]["key"]["subject"]["type"] = "wiki".into();
    let mut missing_time = valid.clone();
    missing_time["markers"][0]["key"]["subject"]["revision"] = serde_json::json!({});
    let mut bad_comment = valid.clone();
    bad_comment["markers"][0]["key"]["subject"]["revision"]["lastComment"] = "has space".into();
    let mut git_as_issue = valid;
    git_as_issue["markers"][0]["key"]["subject"]["type"] = "git".into();
    let cases: [(serde_json::Value, Expectation); 5] = [
        (duplicate, |error| {
            matches!(
                error,
                Error::State(StateError::CorruptState(
                    Corruption::DuplicateWorkflowMarker
                ))
            )
        }),
        (unknown_kind, syntax),
        (missing_time, syntax),
        (bad_comment, syntax),
        (git_as_issue, syntax),
    ];
    for (corrupt, expected) in cases {
        let bytes = serde_json::to_vec_pretty(&corrupt)?;
        fs::write(&path, &bytes)?;
        let error = fixture
            .reopen()
            .err()
            .ok_or("corrupt issue subject was accepted")?;
        let error = error
            .downcast::<Error>()
            .map_err(|_| "unexpected error type")?;
        assert!(expected(&error), "{error:?}");
        assert_eq!(fs::read(&path)?, bytes, "rejected state is not rewritten");
    }
    Ok(())
}

fn syntax(error: &Error) -> bool {
    matches!(
        error,
        Error::State(StateError::CorruptState(Corruption::Syntax { .. }))
    )
}

fn resource_key(handle: &str, digest: &str) -> TestResult<MarkerKey> {
    Ok(MarkerKey {
        workflow: WorkflowId::new("dishwasher")?,
        item: WorkItem::Resource {
            resource: ResourceRef {
                kind: ResourceKind::Worktree,
                backend: kitchen::BackendId::new("fake")?,
                handle: ExternalRef::new(handle)?,
            },
        },
        subject: MarkerSubject::Observation(ExternalRef::new(digest)?),
    })
}

#[test]
fn resource_observations_are_distinct_keys_and_persist() -> TestResult {
    let fixture = Fixture::new()?;
    let fact = verdict(EvidenceVerdict::Pass);
    let recorder = scheduled("dishwasher")?;
    let first = resource_key("wt-1", "sha256:aa")?;
    fixture
        .store
        .record_marker(first.clone(), fact.clone(), &recorder, at(1))?;
    // Changed evidence or another resource is a different key.
    for other in [
        resource_key("wt-1", "sha256:bb")?,
        resource_key("wt-2", "sha256:aa")?,
    ] {
        assert_eq!(fixture.store.marker(&other)?, None);
    }
    let reopened = fixture.reopen()?;
    let stored = reopened.marker(&first)?.ok_or("marker lost")?;
    assert_eq!(stored.key(), &first);
    let valid: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture.state_path())?)?;
    assert_eq!(valid["markers"][0]["key"]["item"]["type"], "resource");
    assert_eq!(valid["markers"][0]["key"]["subject"]["type"], "observation");
    Ok(())
}

#[test]
fn corrupt_resource_markers_are_rejected_without_reset() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.store.record_marker(
        resource_key("wt-1", "sha256:aa")?,
        verdict(EvidenceVerdict::Pass),
        &scheduled("dishwasher")?,
        at(1),
    )?;
    let path = fixture.state_path();
    let valid: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let mut bad_kind = valid.clone();
    bad_kind["markers"][0]["key"]["item"]["resource"]["kind"] = "disk".into();
    let mut bad_handle = valid.clone();
    bad_handle["markers"][0]["key"]["item"]["resource"]["handle"] = "has space".into();
    let mut bad_digest = valid.clone();
    bad_digest["markers"][0]["key"]["subject"]["revision"] = "has space".into();
    let mut observation_as_issue = valid;
    observation_as_issue["markers"][0]["key"]["subject"]["type"] = "issue".into();
    for corrupt in [bad_kind, bad_handle, bad_digest, observation_as_issue] {
        let bytes = serde_json::to_vec_pretty(&corrupt)?;
        fs::write(&path, &bytes)?;
        let error = fixture
            .reopen()
            .err()
            .ok_or("corrupt resource marker was accepted")?;
        let error = error
            .downcast::<Error>()
            .map_err(|_| "unexpected error type")?;
        assert!(syntax(&error), "{error:?}");
        assert_eq!(fs::read(&path)?, bytes, "rejected state is not rewritten");
    }
    Ok(())
}

#[test]
fn a_guarded_record_reads_and_writes_in_one_transaction() -> TestResult {
    let fixture = Fixture::new()?;
    let recorder = scheduled("guard-tick")?;
    let first = key("gate", pull_request(20)?, 'a', None)?;
    let second = key("gate", pull_request(20)?, 'b', None)?;
    let other_workflow = key("triage", pull_request(20)?, 'c', None)?;

    // Nothing to object to: recorded, and the guard saw no siblings.
    let attempt = fixture.store.record_marker_unless(
        first.clone(),
        verdict(EvidenceVerdict::Pass),
        &recorder,
        at(1),
        |siblings| {
            assert!(siblings.is_empty());
            Ok(None::<()>)
        },
    )?;
    assert!(matches!(attempt, MarkerAttempt::Recorded(_)));

    // The guard sees the marker recorded just before, and only this
    // workflow's: another workflow's marker is not a sibling.
    fixture.store.record_marker(
        other_workflow,
        verdict(EvidenceVerdict::Pass),
        &recorder,
        at(2),
    )?;
    let attempt = fixture.store.record_marker_unless(
        second.clone(),
        verdict(EvidenceVerdict::Pass),
        &recorder,
        at(3),
        |siblings| Ok((siblings.len() == 1).then_some(siblings[0].key().clone())),
    )?;
    assert_eq!(attempt, MarkerAttempt::Blocked(first.clone()));
    assert_eq!(fixture.store.marker(&second)?, None, "nothing was written");
    assert_eq!(fixture.store.markers(&first.workflow)?.len(), 1);
    Ok(())
}

#[test]
fn a_guarded_record_of_an_existing_key_is_never_blocked() -> TestResult {
    let fixture = Fixture::new()?;
    let recorder = scheduled("guard-tick")?;
    let gate = key("gate", pull_request(20)?, 'a', None)?;
    fixture.store.record_marker(
        gate.clone(),
        verdict(EvidenceVerdict::Pass),
        &recorder,
        at(1),
    )?;
    let same = fixture.store.record_marker_unless(
        gate.clone(),
        verdict(EvidenceVerdict::Pass),
        &recorder,
        at(2),
        |_| Ok(Some(())),
    )?;
    assert!(matches!(same, MarkerAttempt::AlreadyRecorded(_)));
    let different = fixture.store.record_marker_unless(
        gate,
        verdict(EvidenceVerdict::Fail),
        &recorder,
        at(3),
        |_| Ok(Some(())),
    );
    assert!(matches!(
        different,
        Err(Error::State(StateError::MarkerConflict))
    ));
    Ok(())
}

#[test]
fn a_guarded_record_checks_an_existing_key_only_until_its_task_exists() -> TestResult {
    let fixture = Fixture::new()?;
    let recorder = scheduled("guard-tick")?;
    let gate = key("gate", pull_request(20)?, 'a', None)?;
    let pending = task_id("gate-task")?;
    fixture.store.record_marker(
        gate.clone(),
        verdict(EvidenceVerdict::Pass),
        &recorder,
        at(1),
    )?;

    // The task is missing: the guard runs for the recorded key and objects.
    let blocked = fixture.store.record_marker_unless_created(
        gate.clone(),
        verdict(EvidenceVerdict::Pass),
        &recorder,
        at(2),
        &pending,
        |siblings| Ok(Some(siblings.len())),
    )?;
    assert_eq!(blocked, MarkerAttempt::Blocked(1));

    // The guard's silence settles the key as usual.
    let settled = fixture.store.record_marker_unless_created(
        gate.clone(),
        verdict(EvidenceVerdict::Pass),
        &recorder,
        at(3),
        &pending,
        |_| Ok(None::<()>),
    )?;
    assert!(matches!(settled, MarkerAttempt::AlreadyRecorded(_)));

    // Once the task exists, the recorded key is never blocked again.
    fixture
        .store
        .create_task(spec(pending.as_str())?, &recorder, at(4))?;
    let unguarded = fixture.store.record_marker_unless_created(
        gate,
        verdict(EvidenceVerdict::Pass),
        &recorder,
        at(5),
        &pending,
        |_| Ok(Some(())),
    )?;
    assert!(matches!(unguarded, MarkerAttempt::AlreadyRecorded(_)));
    Ok(())
}

#[test]
fn a_guarded_record_refuses_a_stale_consumer_and_propagates_guard_errors() -> TestResult {
    let fixture = Fixture::new()?;
    let consumer = ConsumerId::new("gate-consumer")?;
    let first = scheduled("first")?;
    let second = scheduled("second")?;
    let old = fixture
        .store
        .acquire_consumer(&consumer, &first, ttl(60)?, at(0))?
        .fence();
    let new = fixture
        .store
        .take_over_consumer(&consumer, &second, ttl(60)?, at(61))?
        .fence();
    let gate = key("gate", pull_request(20)?, 'a', None)?;

    // The fence is checked before the guard runs.
    let refused = fixture.store.record_marker_unless(
        gate.clone(),
        verdict(EvidenceVerdict::Pass),
        &first.clone().under(consumer.clone(), old),
        at(62),
        |_| -> kitchen::Result<Option<()>> { Err(StateError::MarkerConflict.into()) },
    );
    assert!(matches!(
        refused,
        Err(Error::State(StateError::StaleFence { .. }))
    ));

    // A guard failure aborts the transaction without writing.
    let failed = fixture.store.record_marker_unless(
        gate.clone(),
        verdict(EvidenceVerdict::Pass),
        &second.under(consumer, new),
        at(63),
        |_| -> kitchen::Result<Option<()>> { Err(StateError::MarkerNotFound.into()) },
    );
    assert!(matches!(
        failed,
        Err(Error::State(StateError::MarkerNotFound))
    ));
    assert_eq!(fixture.store.marker(&gate)?, None);
    Ok(())
}

#[test]
fn retiring_removes_only_markers_whose_fact_is_unchanged() -> TestResult {
    let fixture = Fixture::new()?;
    let recorder = scheduled("gate")?;
    let spent = key("merge-gate", pull_request(20)?, 'a', None)?;
    let renewed = key("merge-gate", pull_request(21)?, 'a', None)?;
    let absent = key("merge-gate", pull_request(22)?, 'a', None)?;
    for marker in [&spent, &renewed] {
        fixture.store.record_marker(
            marker.clone(),
            verdict(EvidenceVerdict::Pass),
            &recorder,
            at(1),
        )?;
    }
    // A renewal after the caller read the marker keeps it.
    fixture.store.supersede_marker(
        &renewed,
        &verdict(EvidenceVerdict::Pass),
        verdict(EvidenceVerdict::Fail),
        &recorder,
        at(2),
    )?;
    let retired = fixture.store.retire_markers(&[
        (spent.clone(), verdict(EvidenceVerdict::Pass)),
        (renewed.clone(), verdict(EvidenceVerdict::Pass)),
        (absent, verdict(EvidenceVerdict::Pass)),
    ])?;
    assert_eq!(retired, std::slice::from_ref(&spent));
    assert_eq!(fixture.store.marker(&spent)?, None);
    assert!(fixture.store.marker(&renewed)?.is_some());
    // Retiring again is harmless.
    assert!(
        fixture
            .store
            .retire_markers(&[(spent, verdict(EvidenceVerdict::Pass))])?
            .is_empty()
    );
    Ok(())
}

#[test]
fn an_asked_question_is_never_retired() -> TestResult {
    let fixture = Fixture::new()?;
    let question = issue_key(7, 1, None)?;
    let asked = MarkerFact::QuestionAsked {
        question: ExternalRef::new("decision-1")?,
    };
    let spent = key("merge-gate", pull_request(20)?, 'a', None)?;
    fixture.store.record_marker(
        question.clone(),
        asked.clone(),
        &scheduled("triage")?,
        at(1),
    )?;
    fixture.store.record_marker(
        spent.clone(),
        verdict(EvidenceVerdict::Pass),
        &scheduled("gate")?,
        at(1),
    )?;
    assert!(matches!(
        fixture.store.retire_markers(&[
            (spent.clone(), verdict(EvidenceVerdict::Pass)),
            (question.clone(), asked),
        ]),
        Err(Error::State(StateError::MarkerNotSupersedable))
    ));
    // The refusal removed nothing, not even the marker before it.
    assert!(fixture.store.marker(&question)?.is_some());
    assert!(fixture.store.marker(&spent)?.is_some());
    Ok(())
}

fn task_key(task: &kitchen::TaskId, subject: &str) -> TestResult<MarkerKey> {
    Ok(MarkerKey {
        workflow: WorkflowId::new("held-follow-up")?,
        item: WorkItem::Task { task: task.clone() },
        subject: MarkerSubject::Observation(ExternalRef::new(subject)?),
    })
}

#[test]
fn a_task_marker_is_recorded_only_under_the_tasks_current_live_claim() -> TestResult {
    let fixture = Fixture::new()?;
    let store = &fixture.store;
    let task = task_id("held-task")?;
    store.create_task(spec(task.as_str())?, &common::creator()?, at(0))?;
    let first = store.claim(&task, &scheduled("first")?, ttl(60)?, at(0))?;
    let fact = || verdict(EvidenceVerdict::Pass);
    let unblocked = |_: &[&kitchen::state::WorkflowMarker]| Ok(None::<()>);

    // The claim's owner records it, and the guard can still block a write.
    let attempt = store.record_task_marker_unless(
        task_key(&task, "a")?,
        fact(),
        &task,
        first.fence(),
        at(1),
        unblocked,
    )?;
    let MarkerAttempt::Recorded(marker) = attempt else {
        return Err(format!("not recorded: {attempt:?}").into());
    };
    assert_eq!(marker.recorded_by().holder.as_str(), "first");
    let blocked = store.record_task_marker_unless(
        task_key(&task, "b")?,
        fact(),
        &task,
        first.fence(),
        at(2),
        |_| Ok(Some("full")),
    )?;
    assert_eq!(blocked, MarkerAttempt::Blocked("full"));
    assert_eq!(store.marker(&task_key(&task, "b")?)?, None);

    // An expired claim records nothing, even before anyone took it over.
    let expired = store.record_task_marker_unless(
        task_key(&task, "b")?,
        fact(),
        &task,
        first.fence(),
        at(61),
        unblocked,
    );
    assert!(
        matches!(expired, Err(Error::State(StateError::LeaseExpired { .. }))),
        "{expired:?}"
    );

    // Taken over: the old fence is stale, even for a key already recorded.
    let second = store.take_over(&task, &scheduled("second")?, ttl(60)?, at(62))?;
    for subject in ["a", "b"] {
        let stale = store.record_task_marker_unless(
            task_key(&task, subject)?,
            fact(),
            &task,
            first.fence(),
            at(63),
            unblocked,
        );
        assert!(
            matches!(stale, Err(Error::State(StateError::StaleFence { .. }))),
            "{stale:?}"
        );
    }

    // Settled: even the current owner records nothing more.
    store.request_cancel(&task, &common::holder("second")?, at(64))?;
    store.settle_cancelled(&task, second.fence(), at(64))?;
    let settled = store.record_task_marker_unless(
        task_key(&task, "b")?,
        fact(),
        &task,
        second.fence(),
        at(65),
        unblocked,
    );
    assert!(
        matches!(settled, Err(Error::State(StateError::TaskSettled { .. }))),
        "{settled:?}"
    );
    let missing = task_id("no-such-task")?;
    let unknown = store.record_task_marker_unless(
        task_key(&missing, "a")?,
        fact(),
        &missing,
        second.fence(),
        at(65),
        unblocked,
    );
    assert!(
        matches!(unknown, Err(Error::State(StateError::TaskNotFound(_)))),
        "{unknown:?}"
    );
    assert_eq!(store.markers(&WorkflowId::new("held-follow-up")?)?.len(), 1);
    Ok(())
}

#[test]
fn a_task_marker_is_superseded_only_under_the_tasks_current_live_claim() -> TestResult {
    let fixture = Fixture::new()?;
    let store = &fixture.store;
    let task = task_id("briefed-task")?;
    store.create_task(spec(task.as_str())?, &common::creator()?, at(0))?;
    let first = store.claim(&task, &scheduled("first")?, ttl(60)?, at(0))?;
    let key = task_key(&task, "a")?;
    let waiting = verdict(EvidenceVerdict::Pass);
    let briefed = verdict(EvidenceVerdict::Fail);
    store.record_marker(key.clone(), waiting.clone(), &scheduled("first")?, at(1))?;

    // Taken over: the old owner changes nothing.
    let second = store.take_over(&task, &scheduled("second")?, ttl(60)?, at(61))?;
    let stale = store.supersede_task_marker(
        &key,
        &waiting,
        briefed.clone(),
        &task,
        first.fence(),
        at(62),
    );
    assert!(
        matches!(stale, Err(Error::State(StateError::StaleFence { .. }))),
        "{stale:?}"
    );
    assert_eq!(
        store.marker(&key)?.map(|marker| marker.fact().clone()),
        Some(waiting.clone())
    );

    // The current owner supersedes it, recorded as itself.
    let recorded = store.supersede_task_marker(
        &key,
        &waiting,
        briefed.clone(),
        &task,
        second.fence(),
        at(63),
    )?;
    let MarkerRecording::Superseded(marker) = recorded else {
        return Err(format!("not superseded: {recorded:?}").into());
    };
    assert_eq!(marker.fact(), &briefed);
    assert_eq!(marker.recorded_by().holder.as_str(), "second");

    // Settled: nothing more, even from the owner that settled it.
    store.request_cancel(&task, &common::holder("second")?, at(64))?;
    store.settle_cancelled(&task, second.fence(), at(64))?;
    let settled =
        store.supersede_task_marker(&key, &briefed, waiting, &task, second.fence(), at(65));
    assert!(
        matches!(settled, Err(Error::State(StateError::TaskSettled { .. }))),
        "{settled:?}"
    );
    assert_eq!(
        store.marker(&key)?.map(|marker| marker.fact().clone()),
        Some(briefed)
    );
    Ok(())
}
