//! Verification environments: target declarations, policy checks at
//! activation, exact-revision evidence, and explicit access grants.
//!
//! Everything here runs against in-memory values and the fake backend. No VM
//! or device is started or operated; these are not live verification evidence.

mod common;

use std::collections::BTreeSet;

use common::{TestResult, backend_id, commit, credential, grant, grants_for, house, other_house};
use kitchen::{
    Error, ErrorClass, HouseId,
    contracts::{
        BackendDescriptor, BackendUnavailable, ContractError, DeviceClass, EffectExecutor,
        EffectFailure, EffectRequest, Evidence, EvidenceKind, EvidenceSubject, EvidenceVerdict,
        ExternalRef, GrantScope, HouseGrants, Lookup, MAX_DEVICE_CLASS_BYTES,
        MAX_TARGETS_PER_WORK_TYPE, MAX_VERIFICATION_ENVIRONMENTS, OperatingSystem, Permission,
        Receipt, Repository, Support, TargetStatus, TaskAuthority, Text, ValueKind,
        VerificationEnvironments, VerificationError, VerificationPolicy, VerificationReport,
        VerificationTarget, authorize_access, fake::FakeBackend,
    },
};

fn target(value: &str) -> TestResult<VerificationTarget> {
    Ok(value.parse()?)
}

fn work(value: &str) -> TestResult<Text> {
    Ok(Text::new(value)?)
}

fn repo(value: &str) -> TestResult<Repository> {
    Ok(Repository::new(value)?)
}

fn environments(declared: &[(&str, Support)]) -> TestResult<VerificationEnvironments> {
    let mut environments = VerificationEnvironments::new();
    for (name, support) in declared {
        environments = environments.with(target(name)?, *support)?;
    }
    Ok(environments)
}

/// The fake backend, declaring `declared` verification environments.
struct Declaring {
    inner: FakeBackend,
    environments: VerificationEnvironments,
}

impl EffectExecutor for Declaring {
    fn descriptor(&self) -> &BackendDescriptor {
        self.inner.descriptor()
    }

    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        self.inner.execute(request)
    }

    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        self.inner.lookup(request)
    }

    fn verification_environments(&self) -> &VerificationEnvironments {
        &self.environments
    }
}

fn declaring(house: HouseId, declared: &[(&str, Support)]) -> TestResult<Declaring> {
    Ok(Declaring {
        inner: FakeBackend::fully_capable(backend_id()?, house),
        environments: environments(declared)?,
    })
}

fn subject(head: char, base: Option<char>) -> TestResult<EvidenceSubject> {
    Ok(EvidenceSubject {
        head: commit(head)?,
        base: base.map(commit).transpose()?,
    })
}

fn verification(
    on: &str,
    verdict: EvidenceVerdict,
    subject: EvidenceSubject,
) -> TestResult<Evidence> {
    Ok(Evidence {
        kind: EvidenceKind::Verification(target(on)?),
        verdict,
        subject,
        source: ExternalRef::new("run:verification-1")?,
        observed_at: kitchen::contracts::Timestamp::from_unix_millis(1),
    })
}

fn invalid() -> ContractError {
    ContractError::InvalidValue {
        kind: ValueKind::VerificationTarget,
    }
}

#[test]
fn targets_round_trip_through_text_and_serde() -> TestResult {
    let cases = [
        (
            "host:linux",
            VerificationTarget::Host(OperatingSystem::Linux),
        ),
        (
            "vm:windows",
            VerificationTarget::Vm(OperatingSystem::Windows),
        ),
        (
            "device:km43-controller",
            VerificationTarget::Device(DeviceClass::new("km43-controller")?),
        ),
    ];
    for (text, expected) in cases {
        let parsed: VerificationTarget = text.parse()?;
        assert_eq!(parsed, expected);
        assert_eq!(parsed.to_string(), text);
        assert_eq!(serde_json::to_string(&parsed)?, format!("\"{text}\""));
        assert_eq!(
            serde_json::from_str::<VerificationTarget>(&format!("\"{text}\""))?,
            expected
        );
    }
    Ok(())
}

#[test]
fn malformed_targets_are_rejected_without_echo() {
    for rejected in [
        "",
        "linux",
        "host:",
        "host:Linux",
        "vm:beos",
        "container:linux",
        "device:",
        "device:Phone",
        "device:9phone",
        "device:phone-",
        "device:pho--ne",
        "device:phone board",
    ] {
        assert_eq!(
            rejected.parse::<VerificationTarget>(),
            Err(invalid()),
            "{rejected:?}"
        );
    }
    assert!(serde_json::from_str::<VerificationTarget>("\"vm:beos\"").is_err());
    assert_eq!(
        Error::from(invalid()).class(),
        ErrorClass::InvalidInput,
        "a malformed target is invalid input"
    );
}

#[test]
fn device_class_length_is_bounded() -> TestResult {
    let longest = format!("d{}", "1".repeat(MAX_DEVICE_CLASS_BYTES - 1));
    assert_eq!(DeviceClass::new(&longest)?.as_str(), longest);
    assert_eq!(DeviceClass::new(&format!("{longest}1")), Err(invalid()));
    assert_eq!(DeviceClass::new("a")?.as_str(), "a");
    Ok(())
}

#[test]
fn declared_environments_require_full_support_and_name_every_gap() -> TestResult {
    let declared = environments(&[
        ("host:linux", Support::Supported),
        ("vm:windows", Support::Partial),
    ])?;
    assert_eq!(declared.require([&target("host:linux")?]), Ok(()));
    assert_eq!(declared.require([]), Ok(()));
    assert_eq!(
        declared.require([
            &target("device:phone")?,
            &target("vm:windows")?,
            &target("host:linux")?,
            &target("vm:macos")?,
        ]),
        Err(VerificationError::UnsupportedTargets {
            missing: vec![target("vm:macos")?, target("device:phone")?],
            partial: vec![target("vm:windows")?],
        })
    );
    let refused = Error::from(VerificationError::UnsupportedTargets {
        missing: vec![target("vm:macos")?],
        partial: vec![],
    });
    assert_eq!(refused.class(), ErrorClass::Refused);
    assert!(
        refused
            .to_string()
            .contains("missing: vm:macos; partial: none")
    );
    Ok(())
}

#[test]
fn environment_declarations_are_bounded() -> TestResult {
    let mut declared = VerificationEnvironments::new();
    for index in 0..MAX_VERIFICATION_ENVIRONMENTS {
        declared = declared.with(target(&format!("device:d{index}"))?, Support::Supported)?;
    }
    // Redeclaring an existing target at the bound is still accepted.
    declared = declared.with(target("device:d0")?, Support::Partial)?;
    assert_eq!(
        declared.support(&target("device:d0")?),
        Some(Support::Partial)
    );
    let json = serde_json::to_string(&declared)?;
    assert_eq!(
        serde_json::from_str::<VerificationEnvironments>(&json)?,
        declared
    );
    assert_eq!(
        declared.with(target("host:linux")?, Support::Supported),
        Err(VerificationError::TooMany)
    );
    let over: serde_json::Map<String, serde_json::Value> = (0..=MAX_VERIFICATION_ENVIRONMENTS)
        .map(|index| (format!("device:d{index}"), "supported".into()))
        .collect();
    assert!(
        serde_json::from_value::<VerificationEnvironments>(serde_json::Value::Object(over))
            .is_err()
    );
    assert!(
        serde_json::from_str::<VerificationEnvironments>(r#"{"vm:beos":"supported"}"#).is_err()
    );
    assert_eq!(
        Error::from(VerificationError::TooMany).class(),
        ErrorClass::InvalidInput
    );
    Ok(())
}

#[test]
fn activation_fails_when_a_required_target_is_missing() -> TestResult {
    let firmware = repo("origin89hq/firmware")?;
    let policy = VerificationPolicy::new()
        .require_in_house(work("release")?, [target("host:linux")?])?
        .require_in_repository(
            firmware.clone(),
            work("release")?,
            [target("device:km43-controller")?, target("vm:windows")?],
        )?;
    let backend = environments(&[
        ("host:linux", Support::Supported),
        ("vm:windows", Support::Supported),
    ])?;

    assert_eq!(
        policy.check_activation(Some(&firmware), &work("release")?, &backend),
        Err(VerificationError::UnsupportedTargets {
            missing: vec![target("device:km43-controller")?],
            partial: vec![],
        })
    );
    // Other repositories get only the house requirement.
    assert_eq!(
        policy.check_activation(Some(&repo("origin89hq/km43")?), &work("release")?, &backend)?,
        BTreeSet::from([target("host:linux")?])
    );
    // House-level work is checked against the house requirement alone.
    assert_eq!(
        policy.check_activation(None, &work("release")?, &VerificationEnvironments::new()),
        Err(VerificationError::UnsupportedTargets {
            missing: vec![target("host:linux")?],
            partial: vec![],
        })
    );
    // A work type without requirements activates on any backend.
    assert_eq!(
        policy.check_activation(
            Some(&firmware),
            &work("docs")?,
            &VerificationEnvironments::new()
        )?,
        BTreeSet::new()
    );
    let complete = backend.with(target("device:km43-controller")?, Support::Supported)?;
    assert_eq!(
        policy
            .check_activation(Some(&firmware), &work("release")?, &complete)?
            .len(),
        3
    );
    Ok(())
}

#[test]
fn a_repository_entry_adds_to_but_never_removes_house_requirements() -> TestResult {
    let app = repo("lemarier/app")?;
    let policy = VerificationPolicy::new()
        .require_in_house(work("ui")?, [target("host:macos")?])?
        .require_in_repository(app.clone(), work("ui")?, [target("vm:windows")?])?;
    assert_eq!(
        policy.required_targets(Some(&app), &work("ui")?),
        BTreeSet::from([target("host:macos")?, target("vm:windows")?])
    );
    Ok(())
}

#[test]
fn backends_declare_no_environments_by_default() -> TestResult {
    let policy = VerificationPolicy::new().require_in_house(work("ui")?, [target("vm:macos")?])?;
    let plain = FakeBackend::fully_capable(backend_id()?, house()?);
    assert!(matches!(
        policy.check_activation(None, &work("ui")?, plain.verification_environments()),
        Err(VerificationError::UnsupportedTargets { .. })
    ));
    let declaring = declaring(house()?, &[("vm:macos", Support::Supported)])?;
    assert!(
        policy
            .check_activation(None, &work("ui")?, declaring.verification_environments())
            .is_ok()
    );
    Ok(())
}

#[test]
fn policy_bounds_and_empty_requirements_are_rejected() -> TestResult {
    assert_eq!(
        VerificationPolicy::new().require_in_house(work("ui")?, []),
        Err(VerificationError::EmptyRequirement)
    );
    let many = |count: usize| -> TestResult<Vec<VerificationTarget>> {
        (0..count)
            .map(|index| target(&format!("device:d{index}")))
            .collect()
    };
    let full = VerificationPolicy::new()
        .require_in_house(work("ui")?, many(MAX_TARGETS_PER_WORK_TYPE)?)?;
    assert_eq!(
        full.clone()
            .require_in_house(work("ui")?, [target("host:linux")?]),
        Err(VerificationError::TooMany)
    );
    // Re-adding targets already present stays within the bound.
    assert!(full.require_in_house(work("ui")?, many(1)?).is_ok());
    Ok(())
}

#[test]
fn policies_deserialize_strictly() -> TestResult {
    let policy: VerificationPolicy = serde_json::from_str(
        r#"{"house":{"release":["host:linux"]},"repositories":{"origin89hq/firmware":{"release":["device:km43-controller"]}}}"#,
    )?;
    assert_eq!(
        policy.required_targets(Some(&repo("origin89hq/firmware")?), &work("release")?),
        BTreeSet::from([target("host:linux")?, target("device:km43-controller")?])
    );
    assert_eq!(
        serde_json::from_str::<VerificationPolicy>(&serde_json::to_string(&policy)?)?,
        policy
    );
    assert_eq!(
        serde_json::from_str::<VerificationPolicy>("{}")?,
        VerificationPolicy::new()
    );
    for rejected in [
        r#"{"house":{"release":[]}}"#,
        r#"{"house":{"release":["vm:beos"]}}"#,
        r#"{"repositories":{"not a repo":{"release":["host:linux"]}}}"#,
        r#"{"houses":{}}"#,
    ] {
        assert!(
            serde_json::from_str::<VerificationPolicy>(rejected).is_err(),
            "{rejected}"
        );
    }
    let too_many: Vec<String> = (0..=MAX_TARGETS_PER_WORK_TYPE)
        .map(|index| format!("device:d{index}"))
        .collect();
    let json = serde_json::json!({ "house": { "release": too_many } });
    assert!(serde_json::from_value::<VerificationPolicy>(json).is_err());
    Ok(())
}

#[test]
fn evidence_from_the_wrong_revision_does_not_verify() -> TestResult {
    let current = subject('a', Some('b'))?;
    let required = BTreeSet::from([target("vm:windows")?]);
    let evidence = [
        verification(
            "vm:windows",
            EvidenceVerdict::Pass,
            subject('c', Some('b'))?,
        )?,
        verification(
            "vm:windows",
            EvidenceVerdict::Pass,
            subject('a', Some('d'))?,
        )?,
    ];
    let report = VerificationReport::evaluate(&required, &current, &evidence);
    assert_eq!(
        report.status(&target("vm:windows")?),
        Some(TargetStatus::Stale)
    );
    assert!(!report.is_satisfied());

    let mut current_evidence = evidence.to_vec();
    current_evidence.push(verification(
        "vm:windows",
        EvidenceVerdict::Pass,
        current.clone(),
    )?);
    let report = VerificationReport::evaluate(&required, &current, &current_evidence);
    assert_eq!(
        report.status(&target("vm:windows")?),
        Some(TargetStatus::Verified)
    );
    assert!(report.is_satisfied());
    Ok(())
}

#[test]
fn only_verification_on_the_named_environment_counts() -> TestResult {
    let current = subject('a', None)?;
    let required = BTreeSet::from([target("device:phone")?, target("host:linux")?]);
    let check = Evidence {
        kind: EvidenceKind::Check,
        ..verification("host:linux", EvidenceVerdict::Pass, current.clone())?
    };
    let worker_report = kitchen::contracts::Evidence {
        kind: EvidenceKind::WorkerReport,
        ..check.clone()
    };
    let other_environment = verification("vm:linux", EvidenceVerdict::Pass, current.clone())?;
    let evaluated = VerificationReport::evaluate(
        &required,
        &current,
        &[check, worker_report, other_environment],
    );
    assert_eq!(
        evaluated.unsatisfied().collect::<Vec<_>>(),
        vec![
            (&target("host:linux")?, TargetStatus::Missing),
            (&target("device:phone")?, TargetStatus::Missing),
        ]
    );
    assert_eq!(evaluated.status(&target("vm:linux")?), None);
    // Nothing required is trivially satisfied.
    assert!(VerificationReport::evaluate(&BTreeSet::new(), &current, &[]).is_satisfied());
    Ok(())
}

#[test]
fn a_failure_on_the_subject_outweighs_a_pass() -> TestResult {
    let current = subject('a', None)?;
    let required = BTreeSet::from([target("host:macos")?]);
    let status = |evidence: &[Evidence]| -> TestResult<Option<TargetStatus>> {
        Ok(VerificationReport::evaluate(&required, &current, evidence)
            .status(&target("host:macos")?))
    };
    let pass = verification("host:macos", EvidenceVerdict::Pass, current.clone())?;
    let fail = verification("host:macos", EvidenceVerdict::Fail, current.clone())?;
    let unavailable = verification("host:macos", EvidenceVerdict::Unavailable, current.clone())?;
    assert_eq!(
        status(&[pass.clone(), fail.clone()])?,
        Some(TargetStatus::Failed)
    );
    assert_eq!(status(&[fail, pass.clone()])?, Some(TargetStatus::Failed));
    assert_eq!(
        status(std::slice::from_ref(&unavailable))?,
        Some(TargetStatus::Unavailable)
    );
    assert_eq!(status(&[unavailable, pass])?, Some(TargetStatus::Verified));
    Ok(())
}

#[test]
fn verification_evidence_names_its_environment_on_the_wire() -> TestResult {
    let evidence = verification("vm:windows", EvidenceVerdict::Pass, subject('a', None)?)?;
    let json = serde_json::to_value(&evidence)?;
    assert_eq!(
        json["kind"],
        serde_json::json!({ "verification": "vm:windows" })
    );
    assert_eq!(serde_json::from_value::<Evidence>(json)?, evidence);
    // Existing kinds keep their serialized form.
    assert_eq!(serde_json::to_value(EvidenceKind::Check)?, "check");
    assert!(
        serde_json::from_value::<EvidenceKind>(serde_json::json!({ "verification": "vm:beos" }))
            .is_err()
    );
    Ok(())
}

#[test]
fn a_device_target_without_a_grant_is_refused() -> TestResult {
    let device = target("device:km43-controller")?;
    let backend = declaring(house()?, &[("device:km43-controller", Support::Supported)])?;
    let access = |authority: &TaskAuthority, current: &HouseGrants| {
        authorize_access(authority, current, &backend, &device, &GrantScope::House)
    };

    // Nothing granted: the declared device grants nothing.
    let none = grants_for(house()?, &[])?;
    assert_eq!(
        access(&TaskAuthority::delegate(&none, [])?, &none),
        Err(VerificationError::Contract(
            ContractError::PermissionDenied {
                permission: Permission::UseVerificationEnvironment,
            }
        ))
    );

    // Using a verification environment never implies operating equipment.
    let environment_only = grants_for(house()?, &[Permission::UseVerificationEnvironment])?;
    let authority = TaskAuthority::delegate(
        &environment_only,
        [grant(Permission::UseVerificationEnvironment)?],
    )?;
    assert_eq!(
        access(&authority, &environment_only),
        Err(VerificationError::Contract(
            ContractError::PermissionDenied {
                permission: Permission::OperateEquipment,
            }
        ))
    );

    let both = [
        Permission::UseVerificationEnvironment,
        Permission::OperateEquipment,
    ];
    let granted = grants_for(house()?, &both)?;
    let authority = TaskAuthority::delegate(
        &granted,
        both.iter()
            .map(|permission| grant(*permission))
            .collect::<TestResult<Vec<_>>>()?,
    )?;
    let allowed = access(&authority, &granted)?;
    assert_eq!(allowed.target, device);
    assert_eq!(
        allowed.credentials.keys().copied().collect::<Vec<_>>(),
        vec![
            Permission::OperateEquipment,
            Permission::UseVerificationEnvironment
        ]
    );
    let token = credential()?;
    assert!(allowed.credentials.values().all(|used| used == &token));
    Ok(())
}

#[test]
fn vm_access_needs_a_grant_and_a_current_declaration() -> TestResult {
    let vm = target("vm:windows")?;
    let permission = [Permission::UseVerificationEnvironment];
    let granted = grants_for(house()?, &permission)?;
    let authority = TaskAuthority::delegate(&granted, [grant(permission[0])?])?;

    // Granted, but the backend does not fully offer the VM.
    for declared in [&[][..], &[("vm:windows", Support::Partial)][..]] {
        let backend = declaring(house()?, declared)?;
        assert!(matches!(
            authorize_access(&authority, &granted, &backend, &vm, &GrantScope::House),
            Err(VerificationError::UnsupportedTargets { .. })
        ));
    }

    let backend = declaring(house()?, &[("vm:windows", Support::Supported)])?;
    let access = authorize_access(&authority, &granted, &backend, &vm, &GrantScope::House)?;
    assert_eq!(access.credentials.len(), 1);

    // Revoking the house grant refuses the next use.
    let revoked = HouseGrants::new(house()?, []);
    assert_eq!(
        authorize_access(&authority, &revoked, &backend, &vm, &GrantScope::House),
        Err(VerificationError::Contract(
            ContractError::AuthorityExpansion {
                permission: Permission::UseVerificationEnvironment,
                scope: GrantScope::House,
            }
        ))
    );
    Ok(())
}

#[test]
fn host_access_needs_no_extra_grant_but_stays_in_the_house() -> TestResult {
    let host = target("host:linux")?;
    let declared = [("host:linux", Support::Supported)];
    let none = grants_for(house()?, &[])?;
    let authority = TaskAuthority::delegate(&none, [])?;
    let access = authorize_access(
        &authority,
        &none,
        &declaring(house()?, &declared)?,
        &host,
        &GrantScope::House,
    )?;
    assert!(access.credentials.is_empty());

    assert_eq!(
        authorize_access(
            &authority,
            &none,
            &declaring(other_house()?, &declared)?,
            &host,
            &GrantScope::House
        ),
        Err(VerificationError::Contract(ContractError::CrossHouse {
            expected: house()?,
            found: other_house()?,
        }))
    );
    Ok(())
}
