//! Repository readiness findings and the merge-grant readiness policy, using
//! synthetic observations only. None of this is live forge evidence.
use kitchen::{
    ErrorClass, HolderId,
    adoption::HouseRegistry,
    contracts::{
        CommitId, ExternalRef, Grant, GrantScope, Permission, Repository, Text, Timestamp,
    },
    house::{
        AccessStatus, Assessed, BelowReadinessDecision, CheckHistory, CheckOutcome, CheckRunRecord,
        DoctorCode, DoctorEvidence, HouseConfig, HouseError, ReadinessClearance, ReadinessEvidence,
        ReadinessGap, ReadinessLevel, RepositoryConfig, Workflow, assess, doctor, merge_readiness,
        required_check_names,
    },
    integrations::github::{
        CheckConclusion, CheckRun, CheckStatus, CommitStatus, RequiredCheck, RequiredChecks,
        StatusState,
    },
};
use std::collections::{BTreeMap, BTreeSet};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

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
        schema: 1,
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
    })
}

/// Required checks, stable passing history, instructions, and a covered
/// firmware work type: the strongest synthetic observation.
fn complete() -> TestResult<ReadinessEvidence> {
    Ok(ReadinessEvidence {
        required_checks: Some(names(&["check", "bench"])),
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

fn decision(
    house: &HouseConfig,
    assessed: ReadinessLevel,
    reason: &str,
) -> TestResult<BelowReadinessDecision> {
    Ok(BelowReadinessDecision {
        house: house.house.clone(),
        repository: house.repositories.first().ok_or("empty fixture")?.clone(),
        work_type: work("firmware")?,
        assessed,
        required: ReadinessLevel::Covered,
        decided_by: HolderId::new("owner")?,
        reason: Text::new(reason)?,
        decision: ExternalRef::new("roger:merge:firmware-readiness")?,
        at: Timestamp::from_unix_millis(1_000),
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
fn grant_below_required_level_needs_a_matching_recorded_decision() -> TestResult {
    let house = house()?;
    let firmware = work("firmware")?;
    let mut evidence = complete()?;
    evidence.check_history = None;
    let readiness = assess(&house, &repo(&house)?, Some(&evidence))?;
    assert_eq!(readiness.level_for(&firmware), ReadinessLevel::Checked);

    let refused = merge_readiness(&house, &readiness, &firmware, None);
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

    let accepted = decision(&house, ReadinessLevel::Checked, "Bench runs weekly by hand")?;
    assert_eq!(
        merge_readiness(&house, &readiness, &firmware, Some(&accepted))?,
        ReadinessClearance::AcceptedBelow(accepted.clone())
    );

    let mut stale = accepted.clone();
    stale.assessed = ReadinessLevel::Reliable;
    let mut elsewhere = accepted.clone();
    elsewhere.repository = Repository::new("origin89hq/other")?;
    let mut other_work = accepted.clone();
    other_work.work_type = work("docs")?;
    let mut weaker_policy = accepted.clone();
    weaker_policy.required = ReadinessLevel::Reliable;
    let blank = decision(&house, ReadinessLevel::Checked, "   ")?;
    for mismatched in [stale, elsewhere, other_work, weaker_policy, blank] {
        assert!(matches!(
            merge_readiness(&house, &readiness, &firmware, Some(&mismatched)),
            Err(HouseError::ReadinessDecision)
        ));
    }

    let met = assess(&house, &repo(&house)?, Some(&complete()?))?;
    assert_eq!(
        merge_readiness(&house, &met, &firmware, None)?,
        ReadinessClearance::Met {
            required: ReadinessLevel::Covered,
            assessed: ReadinessLevel::Covered,
        }
    );
    assert_eq!(
        merge_readiness(&house, &readiness, &work("docs")?, None)?,
        ReadinessClearance::NotRequired {
            assessed: ReadinessLevel::Checked
        }
    );
    Ok(())
}

#[test]
fn readiness_never_grants_merge_authority() -> TestResult {
    let house = house()?;
    let repository = repo(&house)?;
    let readiness = assess(&house, &repository, Some(&complete()?))?;
    let clearance = merge_readiness(&house, &readiness, &work("firmware")?, None)?;
    assert!(matches!(clearance, ReadinessClearance::Met { .. }));
    // The best assessment leaves house authority exactly as configured.
    let authority = house.authority()?;
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

    let mut other = house.clone();
    other.house = kitchen::HouseId::new("crabnebula")?;
    assert!(matches!(
        merge_readiness(&other, &readiness, &work("firmware")?, None),
        Err(HouseError::HouseSelection)
    ));
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
