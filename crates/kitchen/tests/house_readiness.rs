//! Repository readiness findings and the merge-grant readiness policy, using
//! synthetic observations only. None of this is live forge evidence.
mod common;

use kitchen::{
    ErrorClass,
    adoption::HouseRegistry,
    contracts::{
        AskKind, CommitId, DecisionOwner, EvidenceSubject, Grant, GrantScope, IssueNumber,
        Permission, Repository, RogerAsk, Text,
    },
    house::{
        AccessStatus, Assessed, BelowReadinessDecision, BelowReadinessRequest, CheckHistory,
        CheckOutcome, CheckRunRecord, DoctorCode, DoctorEvidence, HouseConfig, HouseError,
        MergeSubject, ReadinessEvidence, ReadinessGap, ReadinessLevel, RepositoryConfig,
        RepositoryReadiness, Workflow, accept_below_readiness, assess, below_readiness_ask, doctor,
        required_check_apps, required_check_names,
    },
    integrations::github::{
        CheckApp, CheckConclusion, CheckRun, CheckStatus, CommitStatus, RequiredCheck,
        RequiredChecks, StatusState,
    },
    state::EffectState,
};
use std::collections::{BTreeMap, BTreeSet};

use common::TestResult;

fn house() -> TestResult<HouseConfig> {
    let mut house: HouseConfig =
        serde_json::from_str(include_str!("fixtures/house/origin89.json"))?;
    house
        .merge_readiness
        .insert(work("firmware")?, ReadinessLevel::Covered);
    Ok(house)
}

fn repo(house: &HouseConfig) -> TestResult<RepositoryConfig> {
    Ok(RepositoryConfig {
        schema: kitchen::house::REPOSITORY_BINDING_SCHEMA,
        house: house.house.clone(),
        repository: house.repositories.first().ok_or("empty fixture")?.clone(),
        workflows: BTreeSet::from([Workflow::Gate]),
        additional_reviewers: BTreeSet::new(),
        additional_checks: BTreeSet::new(),
    })
}

fn work(name: &str) -> TestResult<Text> {
    Ok(Text::new(name)?)
}

fn head(digit: char) -> TestResult<CommitId> {
    Ok(CommitId::new(&digit.to_string().repeat(40))?)
}

fn names(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn run(check: &str, digit: char, outcome: CheckOutcome) -> TestResult<CheckRunRecord> {
    Ok(CheckRunRecord {
        check: check.into(),
        head: head(digit)?,
        outcome,
        app_id: None,
    })
}

/// Required checks, stable passing history, instructions, and a covered
/// firmware work type: the strongest synthetic observation.
fn complete() -> TestResult<ReadinessEvidence> {
    Ok(ReadinessEvidence {
        required_checks: Some(names(&["check", "bench"])),
        required_check_apps: BTreeMap::new(),
        check_history: Some(vec![
            run("check", 'a', CheckOutcome::Passed)?,
            run("check", 'b', CheckOutcome::Failed)?,
            run("bench", 'a', CheckOutcome::Passed)?,
            run("bench", 'b', CheckOutcome::Inconclusive)?,
        ]),
        instruction_files: Some(names(&["AGENTS.md"])),
        acceptance_checks: Some(BTreeMap::from([(work("firmware")?, names(&["bench"]))])),
    })
}

#[test]
fn complete_evidence_reaches_reliable_and_covers_only_work_with_required_acceptance() -> TestResult
{
    let house = house()?;
    let mut evidence = complete()?;
    evidence
        .acceptance_checks
        .as_mut()
        .ok_or("acceptance")?
        .extend([
            (work("docs")?, BTreeSet::new()),
            (work("web")?, names(&["preview"])),
        ]);
    let readiness = assess(&house, &repo(&house)?, Some(&evidence))?;
    assert_eq!(readiness.level, ReadinessLevel::Reliable);
    assert_eq!(
        readiness.check_history.get("check"),
        Some(&Assessed::Known(CheckHistory {
            passed: 1,
            failed: 1,
            inconclusive: 0,
            flaky_heads: 0,
        }))
    );
    assert_eq!(
        readiness.level_for(&work("firmware")?),
        ReadinessLevel::Covered
    );
    assert_eq!(
        readiness.level_for(&work("docs")?),
        ReadinessLevel::Reliable
    );
    assert_eq!(readiness.level_for(&work("web")?), ReadinessLevel::Reliable);
    assert_eq!(
        readiness.level_for(&work("unlisted")?),
        ReadinessLevel::Reliable
    );
    assert_eq!(
        readiness.gaps,
        [
            ReadinessGap::NoAcceptanceCheck {
                work_type: work("docs")?
            },
            ReadinessGap::AcceptanceNotRequired {
                work_type: work("web")?,
                check: "preview".into(),
            },
        ]
    );
    Ok(())
}

#[test]
fn repository_without_required_checks_is_unready() -> TestResult {
    let house = house()?;
    let mut evidence = complete()?;
    evidence.required_checks = Some(BTreeSet::new());
    let readiness = assess(&house, &repo(&house)?, Some(&evidence))?;
    assert_eq!(readiness.level, ReadinessLevel::Unready);
    // Coverage never lifts a repository whose checks are not enforced.
    assert_eq!(
        readiness.level_for(&work("firmware")?),
        ReadinessLevel::Unready
    );
    assert!(readiness.gaps.contains(&ReadinessGap::NoRequiredChecks));
    for check in ["bench", "check"] {
        assert!(readiness.gaps.contains(&ReadinessGap::CheckNotRequired {
            check: check.into()
        }));
    }
    assert!(
        readiness
            .gaps
            .contains(&ReadinessGap::AcceptanceNotRequired {
                work_type: work("firmware")?,
                check: "bench".into(),
            })
    );

    // Enforcing only one of the configured checks is still unready.
    evidence.required_checks = Some(names(&["check"]));
    let partial = assess(&house, &repo(&house)?, Some(&evidence))?;
    assert_eq!(partial.level, ReadinessLevel::Unready);
    assert_eq!(
        partial
            .gaps
            .iter()
            .filter(|gap| matches!(gap, ReadinessGap::CheckNotRequired { .. }))
            .count(),
        1
    );

    evidence = complete()?;
    evidence.instruction_files = Some(BTreeSet::new());
    let uninstructed = assess(&house, &repo(&house)?, Some(&evidence))?;
    assert_eq!(uninstructed.level, ReadinessLevel::Unready);
    assert_eq!(uninstructed.gaps, [ReadinessGap::NoInstructions]);
    Ok(())
}

#[test]
fn large_forge_required_check_sets_are_accepted() -> TestResult {
    let house = house()?;
    let mut evidence = complete()?;
    let mut required: BTreeSet<String> = (0..100).map(|index| format!("matrix-{index}")).collect();
    required.extend(names(&["check", "bench"]));
    evidence.required_checks = Some(required);
    let readiness = assess(&house, &repo(&house)?, Some(&evidence))?;
    // Accepted, but the extra checks have no history and hold the level down.
    assert_eq!(readiness.level, ReadinessLevel::Checked);
    assert_eq!(
        readiness.check_history.get("matrix-0"),
        Some(&Assessed::Unknown)
    );
    Ok(())
}

#[test]
fn flaky_or_never_passing_history_holds_the_level_at_checked() -> TestResult {
    let house = house()?;
    let mut evidence = complete()?;
    let history = evidence.check_history.as_mut().ok_or("history")?;
    history.push(run("check", 'b', CheckOutcome::Passed)?);
    history.retain(|record| record.check != "bench" || record.outcome != CheckOutcome::Passed);
    history.push(run("bench", 'c', CheckOutcome::Failed)?);
    let readiness = assess(&house, &repo(&house)?, Some(&evidence))?;
    assert_eq!(readiness.level, ReadinessLevel::Checked);
    assert_eq!(
        readiness.level_for(&work("firmware")?),
        ReadinessLevel::Checked
    );
    assert_eq!(
        readiness.check_history.get("check"),
        Some(&Assessed::Known(CheckHistory {
            passed: 2,
            failed: 1,
            inconclusive: 0,
            flaky_heads: 1,
        }))
    );
    assert_eq!(
        readiness.gaps,
        [
            ReadinessGap::NeverPassed {
                check: "bench".into()
            },
            ReadinessGap::Flaky {
                check: "check".into(),
                heads: 1,
            },
        ]
    );
    Ok(())
}

#[test]
fn missing_history_and_observations_are_unknown_not_passes() -> TestResult {
    let house = house()?;
    let mut evidence = complete()?;
    evidence.check_history = None;
    let readiness = assess(&house, &repo(&house)?, Some(&evidence))?;
    assert_eq!(readiness.level, ReadinessLevel::Checked);
    assert!(
        readiness
            .check_history
            .values()
            .all(|history| history == &Assessed::Unknown)
    );
    assert_eq!(
        readiness.gaps,
        [
            ReadinessGap::HistoryUnknown {
                check: "bench".into()
            },
            ReadinessGap::HistoryUnknown {
                check: "check".into()
            },
        ]
    );

    // Observed history without runs of a check is no sample, not a pass.
    evidence.check_history = Some(vec![run("check", 'a', CheckOutcome::Passed)?]);
    let partial = assess(&house, &repo(&house)?, Some(&evidence))?;
    assert_eq!(partial.check_history.get("bench"), Some(&Assessed::Unknown));
    assert_eq!(partial.level, ReadinessLevel::Checked);

    let unobserved = assess(&house, &repo(&house)?, None)?;
    assert_eq!(unobserved.level, ReadinessLevel::Unready);
    assert_eq!(unobserved.required_checks, Assessed::Unknown);
    assert_eq!(unobserved.instruction_files, Assessed::Unknown);
    assert_eq!(
        unobserved.acceptance_checks.get(&work("firmware")?),
        Some(&Assessed::Unknown)
    );
    assert_eq!(
        unobserved.gaps,
        [
            ReadinessGap::RequiredChecksUnknown,
            ReadinessGap::InstructionsUnknown,
            ReadinessGap::HistoryUnknown {
                check: "bench".into()
            },
            ReadinessGap::HistoryUnknown {
                check: "check".into()
            },
            ReadinessGap::AcceptanceUnknown {
                work_type: work("firmware")?
            },
        ]
    );
    let json = serde_json::to_value(&unobserved)?;
    assert_eq!(
        json["requiredChecks"],
        serde_json::json!({"status": "unknown"})
    );
    assert_eq!(json["level"], "unready");
    Ok(())
}

#[test]
fn below_readiness_ask_binds_the_exact_scope_and_refuses_what_is_not_below() -> TestResult {
    let house = house()?;
    let firmware = work("firmware")?;
    let readiness = checked_only(&house)?;
    assert_eq!(readiness.level_for(&firmware), ReadinessLevel::Checked);
    let task = kitchen::TaskId::new("gate-7")?;
    let revision = kitchen::contracts::EvidenceRevision::INITIAL;

    let ask = below_readiness_ask(
        &house,
        &readiness,
        &request(&house, "firmware", "Bench runs weekly by hand")?,
        &task,
        revision,
    )?;
    let binding = &ask.binding;
    assert_eq!(binding.house, house.house);
    assert_eq!(binding.task, task);
    assert_eq!(binding.owner, DecisionOwner::Merge);
    assert_eq!(binding.action, Permission::Merge);
    assert_eq!(binding.target.as_str(), "pr:origin89hq/firmware#7");
    assert_eq!(
        binding.subject,
        Some(EvidenceSubject {
            head: head('c')?,
            base: Some(head('d')?),
        })
    );
    assert_eq!(
        binding.limits.as_str(),
        "Merge below readiness for work type firmware: assessed checked, required covered. Reason: Bench runs weekly by hand"
    );
    assert_eq!(ask.kind, AskKind::Approval);

    // Nothing to accept: the level is met, or policy sets none.
    let met = assess(&house, &repo(&house)?, Some(&complete()?))?;
    let docs = request(&house, "docs", "No policy")?;
    let blank = request(&house, "firmware", "   ")?;
    for (readiness, request) in [
        (&met, request(&house, "firmware", "Already covered")?),
        (&readiness, docs),
        (&readiness, blank),
    ] {
        assert!(matches!(
            below_readiness_ask(&house, readiness, &request, &task, revision),
            Err(HouseError::ReadinessDecision)
        ));
    }
    // Another house's assessment, or a pull request in another repository.
    let mut other = house.clone();
    other.house = kitchen::HouseId::new("crabnebula")?;
    let mut elsewhere = request(&house, "firmware", "Bench runs weekly by hand")?;
    elsewhere.subject.repository = Repository::new("origin89hq/other")?;
    for (house, request) in [
        (
            &other,
            request(&house, "firmware", "Bench runs weekly by hand")?,
        ),
        (&house, elsewhere),
    ] {
        assert!(matches!(
            below_readiness_ask(house, &readiness, &request, &task, revision),
            Err(HouseError::HouseSelection)
        ));
    }
    Ok(())
}

#[test]
fn readiness_never_grants_merge_authority() -> TestResult {
    let house = house()?;
    let repository = repo(&house)?;
    let readiness = assess(&house, &repository, Some(&complete()?))?;
    // The best assessment leaves house authority exactly as configured.
    let issued = house.issue_authority(&[readiness], &[])?;
    for authority in [house.authority()?, issued.grants().clone()] {
        assert!(
            authority
                .permitted(
                    Permission::Merge,
                    &GrantScope::Repository(repository.repository.clone()),
                    &kitchen::BackendId::new("github")?,
                )
                .is_err()
        );
        assert!(!authority.covers(&Grant::house(
            Permission::Merge,
            kitchen::BackendId::new("github")?,
            kitchen::CredentialId::new("forge")?,
        )));
    }
    Ok(())
}

#[test]
fn invalid_or_oversized_readiness_input_is_rejected() -> TestResult {
    let house = house()?;
    let repository = repo(&house)?;
    let mut oversized = complete()?;
    let record = run("check", 'a', CheckOutcome::Passed)?;
    oversized.check_history = Some(vec![record; kitchen::house::MAX_CHECK_RECORDS + 1]);
    let mut unnamed = complete()?;
    unnamed.check_history = Some(vec![run("", 'a', CheckOutcome::Passed)?]);
    let mut many_required = complete()?;
    many_required.required_checks = Some(
        (0..=kitchen::house::MAX_REQUIRED_CHECKS)
            .map(|index| format!("check-{index}"))
            .collect(),
    );
    let mut control = complete()?;
    control.instruction_files = Some(names(&["AGENTS\n.md"]));
    let mut bad_work = complete()?;
    bad_work.acceptance_checks = Some(BTreeMap::from([(
        work("fire\u{7}ware")?,
        names(&["bench"]),
    )]));
    for evidence in [oversized, unnamed, many_required, control, bad_work] {
        assert!(matches!(
            assess(&house, &repository, Some(&evidence)),
            Err(HouseError::InvalidInput)
        ));
    }

    let mut policy = house.clone();
    policy
        .merge_readiness
        .insert(work(&"w".repeat(129))?, ReadinessLevel::Checked);
    assert!(matches!(policy.validate(), Err(HouseError::InvalidInput)));
    let mut raw = serde_json::to_value(&house)?;
    raw["mergeReadiness"]["firmware"] = serde_json::json!("certified");
    assert!(serde_json::from_value::<HouseConfig>(raw).is_err());
    let mut raw = serde_json::to_value(complete()?)?;
    raw["passed"] = serde_json::json!(true);
    assert!(serde_json::from_value::<ReadinessEvidence>(raw).is_err());
    Ok(())
}

#[test]
fn app_bound_checks_count_only_runs_from_their_app() -> TestResult {
    let house = house()?;
    let repository = repo(&house)?;
    let firmware = work("firmware")?;
    let from = |record: CheckRunRecord, app: Option<i64>| CheckRunRecord {
        app_id: app,
        ..record
    };
    let mut evidence = complete()?;
    evidence.required_check_apps = BTreeMap::from([("bench".to_owned(), 15)]);
    let history = |evidence: &ReadinessEvidence| -> TestResult<_> {
        let readiness = assess(&house, &repository, Some(evidence))?;
        Ok((
            readiness.level_for(&firmware),
            readiness.check_history.get("bench").cloned(),
        ))
    };
    // Another app's passing run and a same-named status do not count.
    evidence.check_history = Some(vec![
        run("check", 'a', CheckOutcome::Passed)?,
        from(run("bench", 'a', CheckOutcome::Passed)?, Some(99)),
        from(run("bench", 'b', CheckOutcome::Passed)?, None),
    ]);
    assert_eq!(
        history(&evidence)?,
        (ReadinessLevel::Checked, Some(Assessed::Unknown))
    );
    // The bound app's run does.
    evidence.check_history = Some(vec![
        run("check", 'a', CheckOutcome::Passed)?,
        from(run("bench", 'a', CheckOutcome::Passed)?, Some(15)),
        from(run("bench", 'b', CheckOutcome::Failed)?, Some(99)),
    ]);
    let (level, bench) = history(&evidence)?;
    assert_eq!(level, ReadinessLevel::Covered);
    assert!(matches!(
        bench,
        Some(Assessed::Known(CheckHistory {
            passed: 1,
            failed: 0,
            ..
        }))
    ));
    // An app identity must be a positive id for a named check.
    for apps in [
        BTreeMap::from([("bench".to_owned(), 0)]),
        BTreeMap::from([("bench".to_owned(), -1)]),
        BTreeMap::from([(String::new(), 15)]),
    ] {
        evidence.required_check_apps = apps;
        assert!(matches!(
            assess(&house, &repository, Some(&evidence)),
            Err(HouseError::InvalidInput)
        ));
    }
    Ok(())
}

#[test]
fn github_observations_become_check_records() -> TestResult {
    let sha = head('a')?;
    let check = |status, conclusion| CheckRun {
        name: "check".into(),
        head_sha: sha.clone(),
        status,
        conclusion,
        app: None,
    };
    for (status, conclusion, outcome) in [
        (
            CheckStatus::Completed,
            Some(CheckConclusion::Success),
            CheckOutcome::Passed,
        ),
        (
            CheckStatus::Completed,
            Some(CheckConclusion::TimedOut),
            CheckOutcome::Failed,
        ),
        (
            CheckStatus::Completed,
            Some(CheckConclusion::Cancelled),
            CheckOutcome::Inconclusive,
        ),
        (
            CheckStatus::InProgress,
            Some(CheckConclusion::Success),
            CheckOutcome::Inconclusive,
        ),
        (CheckStatus::Completed, None, CheckOutcome::Inconclusive),
    ] {
        let record = CheckRunRecord::from_check_run(&check(status, conclusion));
        assert_eq!((record.check.as_str(), record.outcome), ("check", outcome));
        assert_eq!(record.head, head('a')?);
    }
    for (state, outcome) in [
        (StatusState::Success, CheckOutcome::Passed),
        (StatusState::Error, CheckOutcome::Failed),
        (StatusState::Pending, CheckOutcome::Inconclusive),
    ] {
        let status = CommitStatus {
            context: "ci/legacy".into(),
            state,
            sha: head('b')?,
        };
        assert_eq!(CheckRunRecord::from_commit_status(&status).outcome, outcome);
    }
    let required = RequiredChecks {
        contexts: vec!["ci/legacy".into()],
        checks: vec![
            RequiredCheck {
                context: "check".into(),
                app_id: Some(15),
            },
            RequiredCheck {
                context: "ci/legacy".into(),
                app_id: None,
            },
        ],
    };
    assert_eq!(
        required_check_names(&required),
        names(&["check", "ci/legacy"])
    );
    // Only a named app binds a check; -1 means any source.
    let mut any_source = required.clone();
    any_source.checks.push(RequiredCheck {
        context: "lint".into(),
        app_id: Some(-1),
    });
    assert_eq!(
        required_check_apps(&any_source),
        BTreeMap::from([("check".to_owned(), 15)])
    );
    let mut from_app = check(CheckStatus::Completed, Some(CheckConclusion::Success));
    from_app.app = Some(CheckApp { id: 15 });
    assert_eq!(CheckRunRecord::from_check_run(&from_app).app_id, Some(15));
    assert!(
        required_check_names(&RequiredChecks {
            contexts: Vec::new(),
            checks: Vec::new(),
        })
        .is_empty()
    );
    Ok(())
}

#[test]
fn doctor_reports_readiness_and_flags_policy_below_the_required_level() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = HouseRegistry::new(temp.path().canonicalize()?.join("registry"))?;
    let house = house()?;
    let repository = repo(&house)?;
    registry.initialize(&house)?;

    let unknown = doctor(&registry, &repository, None)?;
    assert_eq!(unknown.readiness.level, ReadinessLevel::Unready);
    assert!(
        unknown
            .findings
            .iter()
            .any(|finding| finding.code == DoctorCode::Readiness
                && finding.message.contains("firmware requires covered")
                && finding.message.contains("assessed unready"))
    );
    let text = unknown.human_readable();
    assert!(text.contains("Readiness: unready (diagnostic; grants no merge authority)"));
    assert!(text.contains("Required checks: unknown"));
    assert!(text.contains("Check bench: history unknown"));

    let evidence = DoctorEvidence {
        house: house.house.clone(),
        repository: repository.repository.clone(),
        capabilities: kitchen::contracts::CapabilitySet::new(),
        labels: None,
        access: AccessStatus::Unobserved,
        agent_models: None,
        stack_tool: None,
        schedules: None,
        readiness: Some(complete()?),
    };
    let observed = doctor(&registry, &repository, Some(&evidence))?;
    assert_eq!(
        observed.readiness.level_for(&work("firmware")?),
        ReadinessLevel::Covered
    );
    assert!(
        !observed
            .findings
            .iter()
            .any(|finding| finding.code == DoctorCode::Readiness)
    );
    let text = observed.human_readable();
    assert!(text.contains("Check check: 1 passed, 1 failed, 0 inconclusive, 0 flaky head(s)"));
    assert!(text.contains("Work type firmware: covered"));

    // Without a policy, readiness gaps are reported but are not setup findings.
    let mut unconstrained = house.clone();
    unconstrained.merge_readiness.clear();
    let registry = HouseRegistry::new(temp.path().canonicalize()?.join("plain"))?;
    registry.initialize(&unconstrained)?;
    let plain = doctor(&registry, &repository, None)?;
    assert!(!plain.readiness.gaps.is_empty());
    assert!(
        !plain
            .findings
            .iter()
            .any(|finding| finding.code == DoctorCode::Readiness)
    );
    Ok(())
}

/// The house with a repository-scoped merge grant inside its policy limits.
fn merge_house() -> TestResult<(HouseConfig, Grant)> {
    let mut house = house()?;
    let repository = house.repositories.first().ok_or("empty fixture")?.clone();
    let grant = Grant {
        permission: Permission::Merge,
        scope: GrantScope::Repository(repository),
        destination: kitchen::BackendId::new("github")?,
        credential: kitchen::CredentialId::new("forge")?,
    };
    house.policy_limits.insert(grant.clone());
    house.grants.insert(grant.clone());
    Ok((house, grant))
}

fn checked_only(house: &HouseConfig) -> TestResult<RepositoryReadiness> {
    let mut evidence = complete()?;
    evidence.check_history = None;
    Ok(assess(house, &repo(house)?, Some(&evidence))?)
}

/// PR 7 of the fixture repository at head `c` and base `d`.
fn subject(house: &HouseConfig) -> TestResult<MergeSubject> {
    Ok(MergeSubject {
        repository: house.repositories.first().ok_or("empty fixture")?.clone(),
        number: IssueNumber::new(7)?,
        head: head('c')?,
        base: head('d')?,
    })
}

fn request(
    house: &HouseConfig,
    work_type: &str,
    reason: &str,
) -> TestResult<BelowReadinessRequest> {
    Ok(BelowReadinessRequest {
        work_type: work(work_type)?,
        subject: subject(house)?,
        reason: Text::new(reason)?,
    })
}

/// A store holding a gate task whose firmware below-readiness Ask for
/// [`subject`] was persisted and acknowledged by Roger.
struct Asked {
    fixture: common::Fixture,
    task: kitchen::TaskId,
    ask: RogerAsk,
}

fn asked(house: &HouseConfig, reason: &str) -> TestResult<Asked> {
    let fixture = common::Fixture::new()?;
    let subject = subject(house)?;
    let (task, fence, revision, grants) = common::asking_task(
        &fixture,
        "gate-7",
        &subject.repository,
        &subject.head,
        &subject.base,
    )?;
    let ask = below_readiness_ask(
        house,
        &checked_only(house)?,
        &request(house, "firmware", reason)?,
        &task,
        revision,
    )?;
    let record = common::persist_ask(&fixture, &task, fence, &grants, ask.clone())?;
    assert!(matches!(record.state(), EffectState::Applied { .. }));
    Ok(Asked { fixture, task, ask })
}

impl Asked {
    fn accept(
        &self,
        house: &HouseConfig,
        request: &BelowReadinessRequest,
        answer: &[u8],
    ) -> Result<BelowReadinessDecision, HouseError> {
        let scope = common::roger_scope(&request.subject.repository)
            .map_err(|_| HouseError::InvalidInput)?;
        accept_below_readiness(
            house,
            &checked_only(house).map_err(|_| HouseError::InvalidInput)?,
            request,
            &self.fixture.store,
            &self.task,
            &scope,
            answer,
        )
    }
}

#[test]
fn plain_authority_refuses_a_configured_merge_grant() -> TestResult {
    let (house, _) = merge_house()?;
    assert!(matches!(
        house.authority(),
        Err(HouseError::MergeNeedsReadiness)
    ));
    Ok(())
}

#[test]
fn merge_at_the_required_level_is_cleared() -> TestResult {
    let (house, grant) = merge_house()?;
    let covered = assess(&house, &repo(&house)?, Some(&complete()?))?;
    let issued = house.issue_authority(&[covered], &[])?;
    assert!(issued.grants().covers(&grant));
    assert!(issued.merge_clearance(&subject(&house)?)?.is_empty());
    assert!(issued.accepted_below().is_empty());
    Ok(())
}

#[test]
fn merge_below_policy_is_refused_naming_both_levels() -> TestResult {
    let (house, grant) = merge_house()?;
    let issued = house.issue_authority(&[checked_only(&house)?], &[])?;
    // The grants are issued; only the merge is held back.
    assert!(issued.grants().covers(&grant));
    let refused = issued.merge_clearance(&subject(&house)?);
    assert!(matches!(
        refused,
        Err(HouseError::BelowReadiness {
            required: ReadinessLevel::Covered,
            assessed: ReadinessLevel::Checked,
        })
    ));
    assert_eq!(
        refused.err().map(|error| error.class()),
        Some(ErrorClass::Refused)
    );
    Ok(())
}

#[test]
fn merge_with_unobserved_readiness_fails_closed() -> TestResult {
    let (house, _) = merge_house()?;
    let unobserved = assess(&house, &repo(&house)?, None)?;
    for readiness in [Vec::new(), vec![unobserved]] {
        let issued = house.issue_authority(&readiness, &[])?;
        assert!(matches!(
            issued.merge_clearance(&subject(&house)?),
            Err(HouseError::BelowReadiness {
                required: ReadinessLevel::Covered,
                assessed: ReadinessLevel::Unready,
            })
        ));
    }
    Ok(())
}

#[test]
fn persisted_owner_approval_clears_one_pull_request_and_records_the_reason() -> TestResult {
    let (house, _) = merge_house()?;
    let reason = "Bench runs weekly by hand";
    let asked = asked(&house, reason)?;
    let request = request(&house, "firmware", reason)?;
    let decision = asked.accept(&house, &request, &common::roger_answer(&asked.ask, true)?)?;
    assert_eq!(decision.reason().as_str(), reason);
    assert_eq!(decision.ask().as_str(), common::ROGER_ASK);
    assert_eq!(decision.task(), &asked.task);
    assert_eq!(decision.subject(), &request.subject);
    assert_eq!(decision.assessed(), ReadinessLevel::Checked);
    assert_eq!(decision.required(), ReadinessLevel::Covered);

    let issued =
        house.issue_authority(&[checked_only(&house)?], std::slice::from_ref(&decision))?;
    assert_eq!(issued.accepted_below(), std::slice::from_ref(&decision));
    assert_eq!(issued.merge_clearance(&request.subject)?, [&decision]);
    // The approval covers only that pull request at that head and base.
    let mut moved = request.subject.clone();
    moved.head = head('e')?;
    let mut other_pr = request.subject.clone();
    other_pr.number = IssueNumber::new(8)?;
    for subject in [moved, other_pr] {
        assert!(matches!(
            issued.merge_clearance(&subject),
            Err(HouseError::BelowReadiness { .. })
        ));
    }
    // A later assessment at another level makes the approval stale.
    let unready = assess(&house, &repo(&house)?, None)?;
    let stale = house.issue_authority(&[unready], &[decision])?;
    assert!(stale.accepted_below().is_empty());
    assert!(matches!(
        stale.merge_clearance(&request.subject),
        Err(HouseError::BelowReadiness {
            assessed: ReadinessLevel::Unready,
            ..
        })
    ));
    Ok(())
}

#[test]
fn forged_or_unapproved_decisions_are_refused() -> TestResult {
    let (house, _) = merge_house()?;
    let reason = "Bench runs weekly by hand";
    let asked = asked(&house, reason)?;
    let approved = common::roger_answer(&asked.ask, true)?;
    let request = request(&house, "firmware", reason)?;

    // An approval-shaped answer for a reason the owner was never asked about.
    let mut other_reason = asked.ask.clone();
    other_reason.binding.limits = Text::new(
        "Merge below readiness for work type firmware: assessed checked, required covered. Reason: Trust me",
    )?;
    let forged = common::roger_answer(&other_reason, true)?;
    assert!(matches!(
        asked.accept(
            &house,
            &self::request(&house, "firmware", "Trust me")?,
            &forged
        ),
        Err(HouseError::ReadinessNotApproved)
    ));
    // The same forged answer against the persisted Ask does not match it.
    assert!(matches!(
        asked.accept(&house, &request, &forged),
        Err(HouseError::ReadinessNotApproved)
    ));
    // A rejection, an unanswered Ask, and an approval without a passkey.
    let rejected = common::roger_answer(&asked.ask, false)?;
    let mut open: serde_json::Value = serde_json::from_slice(&approved)?;
    open["state"] = "open".into();
    open["answer"] = serde_json::Value::Null;
    let mut no_passkey: serde_json::Value = serde_json::from_slice(&approved)?;
    no_passkey["answer"]["passkey"] = false.into();
    for answer in [
        rejected,
        serde_json::to_vec(&open)?,
        serde_json::to_vec(&no_passkey)?,
    ] {
        assert!(matches!(
            asked.accept(&house, &request, &answer),
            Err(HouseError::ReadinessNotApproved)
        ));
    }
    // A task that never persisted an Ask cannot be approved, and a task the
    // store does not hold cannot be read.
    let fixture = common::Fixture::new()?;
    let subject = subject(&house)?;
    let (bare, ..) = common::asking_task(
        &fixture,
        "gate-8",
        &subject.repository,
        &subject.head,
        &subject.base,
    )?;
    let scope = common::roger_scope(&subject.repository)?;
    let readiness = checked_only(&house)?;
    assert!(matches!(
        accept_below_readiness(
            &house,
            &readiness,
            &request,
            &fixture.store,
            &bare,
            &scope,
            &approved
        ),
        Err(HouseError::ReadinessNotApproved)
    ));
    let missing = accept_below_readiness(
        &house,
        &readiness,
        &request,
        &fixture.store,
        &kitchen::TaskId::new("gate-9")?,
        &scope,
        &approved,
    );
    assert!(matches!(missing, Err(HouseError::DecisionRecord)));
    assert_eq!(
        missing.err().map(|error| error.class()),
        Some(ErrorClass::Execution)
    );
    Ok(())
}
