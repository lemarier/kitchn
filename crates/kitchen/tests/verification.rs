//! Verification environments: target declarations, policy checks at
//! activation, exact-revision evidence bound to authorized access, and
//! explicit access grants scoped to named targets.
//!
//! Everything here runs against in-memory values and the fake backend. No VM
//! or device is started or operated; these are not live verification evidence.

mod common;

use std::collections::BTreeSet;

use common::{TestResult, backend_id, commit, credential, grant, house, other_house};
use kitchen::{
    Error, ErrorClass, HouseId,
    contracts::{
        BackendDescriptor, BackendUnavailable, ContractError, DeviceClass, EffectExecutor,
        EffectFailure, EffectRequest, Evidence, EvidenceKind, EvidenceSubject, EvidenceVerdict,
        ExternalRef, Grant, GrantScope, HouseGrants, Lookup, MAX_DEVICE_CLASS_BYTES,
        MAX_TARGETS_PER_WORK_TYPE, MAX_VERIFICATION_ENVIRONMENTS, OperatingSystem, Permission,
        Receipt, Repository, Support, TargetStatus, TaskAuthority, Text, ValueKind,
        VerificationAccess, VerificationEnvironments, VerificationError, VerificationPolicy,
        VerificationReport, VerificationTarget, authorize_access, fake::FakeBackend,
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

/// A house-wide grant of `permission` naming `targets`.
fn targeted(permission: Permission, targets: &[&str]) -> TestResult<Grant> {
    let targets = targets
        .iter()
        .map(|name| target(name))
        .collect::<TestResult<Vec<_>>>()?;
    Ok(grant(permission)?.with_targets(targets)?)
}

/// The house granting `standing`, and a task delegated all of it.
fn delegated(standing: Vec<Grant>) -> TestResult<(HouseGrants, TaskAuthority)> {
    let current = HouseGrants::new(house()?, standing.clone());
    let authority = TaskAuthority::delegate(&current, standing)?;
    Ok((current, authority))
}

/// House-wide access to `on` through a backend declaring it, authorized by
/// grants naming `on`.
fn access_to(on: &str) -> TestResult<VerificationAccess> {
    let on_target = target(on)?;
    let standing = on_target
        .required_permissions()
        .iter()
        .map(|permission| targeted(*permission, &[on]))
        .collect::<TestResult<Vec<_>>>()?;
    let (current, authority) = delegated(standing)?;
    let backend = declaring(house()?, &[(on, Support::Supported)])?;
    Ok(authorize_access(
        &authority,
        &current,
        &backend,
        &on_target,
        &GrantScope::House,
    )?)
}

fn evidence(
    kind: EvidenceKind,
    verdict: EvidenceVerdict,
    subject: EvidenceSubject,
) -> TestResult<Evidence> {
    Ok(Evidence {
        kind,
        verdict,
        subject,
        source: ExternalRef::new("run:verification-1")?,
        observed_at: kitchen::contracts::Timestamp::from_unix_millis(1),
    })
}

/// Verification evidence recording the access that produced it.
fn verification(
    access: &VerificationAccess,
    verdict: EvidenceVerdict,
    subject: EvidenceSubject,
) -> TestResult<Evidence> {
    evidence(
        EvidenceKind::AuthorizedVerification(access.clone()),
        verdict,
        subject,
    )
}

/// Verification evidence naming only its target, as any producer can build.
fn unbound(on: &str, verdict: EvidenceVerdict, subject: EvidenceSubject) -> TestResult<Evidence> {
    evidence(EvidenceKind::Verification(target(on)?), verdict, subject)
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
        .require_in_house(work("audit")?, [target("host:linux")?])?
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
        policy.check_activation(&firmware, &work("release")?, &backend),
        Err(VerificationError::UnsupportedTargets {
            missing: vec![target("device:km43-controller")?],
            partial: vec![],
        })
    );
    // Other repositories get only the house requirement.
    assert_eq!(
        policy.check_activation(&repo("origin89hq/km43")?, &work("release")?, &backend)?,
        BTreeSet::from([target("host:linux")?])
    );
    // House-level work is checked against the house requirement alone.
    assert_eq!(
        policy.check_house_activation(&work("audit")?, &VerificationEnvironments::new()),
        Err(VerificationError::UnsupportedTargets {
            missing: vec![target("host:linux")?],
            partial: vec![],
        })
    );
    // A work type without requirements activates on any backend.
    assert_eq!(
        policy.check_activation(&firmware, &work("docs")?, &VerificationEnvironments::new())?,
        BTreeSet::new()
    );
    let complete = backend.with(target("device:km43-controller")?, Support::Supported)?;
    assert_eq!(
        policy
            .check_activation(&firmware, &work("release")?, &complete)?
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
        policy.required_targets(&app, &work("ui")?),
        BTreeSet::from([target("host:macos")?, target("vm:windows")?])
    );
    Ok(())
}

#[test]
fn backends_declare_no_environments_by_default() -> TestResult {
    let policy = VerificationPolicy::new().require_in_house(work("ui")?, [target("vm:macos")?])?;
    let plain = FakeBackend::fully_capable(backend_id()?, house()?);
    assert!(matches!(
        policy.check_house_activation(&work("ui")?, plain.verification_environments()),
        Err(VerificationError::UnsupportedTargets { .. })
    ));
    let declaring = declaring(house()?, &[("vm:macos", Support::Supported)])?;
    assert!(
        policy
            .check_house_activation(&work("ui")?, declaring.verification_environments())
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
        policy.required_targets(&repo("origin89hq/firmware")?, &work("release")?),
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
    let access = access_to("vm:windows")?;
    let authorized = [access.clone()];
    let evidence = [
        verification(&access, EvidenceVerdict::Pass, subject('c', Some('b'))?)?,
        verification(&access, EvidenceVerdict::Pass, subject('a', Some('d'))?)?,
    ];
    let report = VerificationReport::evaluate(&required, &current, &evidence, &authorized);
    assert_eq!(
        report.status(&target("vm:windows")?),
        Some(TargetStatus::Stale)
    );
    assert!(!report.is_satisfied());

    let mut current_evidence = evidence.to_vec();
    current_evidence.push(verification(
        &access,
        EvidenceVerdict::Pass,
        current.clone(),
    )?);
    let report = VerificationReport::evaluate(&required, &current, &current_evidence, &authorized);
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
    let other = access_to("vm:linux")?;
    let check = evidence(EvidenceKind::Check, EvidenceVerdict::Pass, current.clone())?;
    let worker_report = evidence(
        EvidenceKind::WorkerReport,
        EvidenceVerdict::Pass,
        current.clone(),
    )?;
    let other_environment = verification(&other, EvidenceVerdict::Pass, current.clone())?;
    let evaluated = VerificationReport::evaluate(
        &required,
        &current,
        &[check, worker_report, other_environment],
        &[other, access_to("host:linux")?],
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
    assert!(VerificationReport::evaluate(&BTreeSet::new(), &current, &[], &[]).is_satisfied());
    Ok(())
}

#[test]
fn a_failure_on_the_subject_outweighs_a_pass() -> TestResult {
    let current = subject('a', None)?;
    let required = BTreeSet::from([target("host:macos")?]);
    let access = access_to("host:macos")?;
    let status = |evidence: &[Evidence]| -> TestResult<Option<TargetStatus>> {
        Ok(VerificationReport::evaluate(
            &required,
            &current,
            evidence,
            std::slice::from_ref(&access),
        )
        .status(&target("host:macos")?))
    };
    let pass = verification(&access, EvidenceVerdict::Pass, current.clone())?;
    let fail = verification(&access, EvidenceVerdict::Fail, current.clone())?;
    let unavailable = verification(&access, EvidenceVerdict::Unavailable, current.clone())?;
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
fn evidence_counts_only_under_an_access_the_task_holds() -> TestResult {
    let current = subject('a', None)?;
    let windows = target("vm:windows")?;
    let required = BTreeSet::from([windows.clone()]);
    let access = access_to("vm:windows")?;
    let status = |evidence: &[Evidence], authorized: &[VerificationAccess]| {
        VerificationReport::evaluate(&required, &current, evidence, authorized).status(&windows)
    };
    let pass = verification(&access, EvidenceVerdict::Pass, current.clone())?;
    assert_eq!(
        status(std::slice::from_ref(&pass), std::slice::from_ref(&access)),
        Some(TargetStatus::Verified)
    );

    // The same record when the task holds no matching access.
    assert_eq!(
        status(std::slice::from_ref(&pass), &[]),
        Some(TargetStatus::Unauthenticated)
    );
    assert_eq!(
        status(std::slice::from_ref(&pass), &[access_to("vm:linux")?]),
        Some(TargetStatus::Unauthenticated)
    );

    // A record naming another executor, scope, or credential, as a producer
    // could write it, does not match the access the task holds.
    let recorded = serde_json::to_value(&access)?;
    for (field, forged) in [
        ("backend", serde_json::json!("other-executor")),
        (
            "scope",
            serde_json::json!({ "type": "repository", "repository": "origin89hq/kitchen" }),
        ),
        (
            "credentials",
            serde_json::json!({ "use-verification-environment": "borrowed" }),
        ),
    ] {
        let mut value = recorded.clone();
        value[field] = forged;
        let forged: VerificationAccess = serde_json::from_value(value)?;
        let evidence = verification(&forged, EvidenceVerdict::Pass, current.clone())?;
        assert_eq!(
            status(&[evidence], std::slice::from_ref(&access)),
            Some(TargetStatus::Unauthenticated),
            "{field}"
        );
    }

    // Unauthenticated evidence neither verifies nor blocks: an authenticated
    // pass still verifies, and a forged failure does not fail the target.
    let unbound_fail = unbound("vm:windows", EvidenceVerdict::Fail, current.clone())?;
    assert_eq!(
        status(&[unbound_fail, pass], std::slice::from_ref(&access)),
        Some(TargetStatus::Verified)
    );
    Ok(())
}

#[test]
fn evidence_bound_to_another_target_does_not_verify() -> TestResult {
    let current = subject('a', None)?;
    let windows = target("vm:windows")?;
    let linux = access_to("vm:linux")?;
    let report = VerificationReport::evaluate(
        &BTreeSet::from([windows.clone()]),
        &current,
        &[verification(
            &linux,
            EvidenceVerdict::Pass,
            current.clone(),
        )?],
        &[linux, access_to("vm:windows")?],
    );
    assert_eq!(report.status(&windows), Some(TargetStatus::Missing));
    Ok(())
}

#[test]
fn unbound_verification_evidence_stays_readable_but_never_verifies() -> TestResult {
    let current = subject('a', None)?;
    // The form written before evidence recorded its access.
    let legacy = serde_json::json!({
        "kind": { "verification": "vm:windows" },
        "verdict": "pass",
        "subject": { "head": commit('a')?.as_str() },
        "source": "run:verification-1",
        "observedAt": 1,
    });
    let read: Evidence = serde_json::from_value(legacy.clone())?;
    assert_eq!(
        read,
        unbound("vm:windows", EvidenceVerdict::Pass, current.clone())?
    );
    assert_eq!(serde_json::to_value(&read)?, legacy);
    let report = VerificationReport::evaluate(
        &BTreeSet::from([target("vm:windows")?]),
        &current,
        &[read],
        &[access_to("vm:windows")?],
    );
    assert_eq!(
        report.status(&target("vm:windows")?),
        Some(TargetStatus::Unauthenticated)
    );
    assert!(!report.is_satisfied());
    Ok(())
}

#[test]
fn verification_evidence_records_its_access_on_the_wire() -> TestResult {
    let access = access_to("vm:windows")?;
    let evidence = verification(&access, EvidenceVerdict::Pass, subject('a', None)?)?;
    let json = serde_json::to_value(&evidence)?;
    assert_eq!(
        json["kind"],
        serde_json::json!({ "authorized-verification": {
            "house": "origin89",
            "backend": "fake",
            "scope": { "type": "house" },
            "target": "vm:windows",
            "credentials": { "use-verification-environment": "origin89-orca" },
        } })
    );
    assert_eq!(serde_json::from_value::<Evidence>(json.clone())?, evidence);
    // Existing kinds keep their serialized form.
    assert_eq!(serde_json::to_value(EvidenceKind::Check)?, "check");
    assert!(
        serde_json::from_value::<EvidenceKind>(serde_json::json!({ "verification": "vm:beos" }))
            .is_err()
    );
    let mut unknown = json["kind"]["authorized-verification"].clone();
    unknown["granted"] = serde_json::json!(true);
    assert!(serde_json::from_value::<VerificationAccess>(unknown).is_err());
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
    let (none, authority) = delegated(Vec::new())?;
    assert_eq!(
        access(&authority, &none),
        Err(VerificationError::Contract(
            ContractError::PermissionDenied {
                permission: Permission::UseVerificationEnvironment,
            }
        ))
    );

    // Using a verification environment never implies operating equipment.
    let (environment_only, authority) = delegated(vec![targeted(
        Permission::UseVerificationEnvironment,
        &["device:km43-controller"],
    )?])?;
    assert_eq!(
        access(&authority, &environment_only),
        Err(VerificationError::Contract(
            ContractError::PermissionDenied {
                permission: Permission::OperateEquipment,
            }
        ))
    );

    let (granted, authority) = delegated(vec![
        targeted(
            Permission::UseVerificationEnvironment,
            &["device:km43-controller"],
        )?,
        targeted(Permission::OperateEquipment, &["device:km43-controller"])?,
    ])?;
    let allowed = access(&authority, &granted)?;
    assert_eq!(allowed.target(), &device);
    assert_eq!(allowed.house(), &house()?);
    assert_eq!(allowed.backend(), &backend_id()?);
    assert_eq!(allowed.scope(), &GrantScope::House);
    assert_eq!(
        allowed.credentials().keys().copied().collect::<Vec<_>>(),
        vec![
            Permission::OperateEquipment,
            Permission::UseVerificationEnvironment
        ]
    );
    let token = credential()?;
    assert!(allowed.credentials().values().all(|used| used == &token));
    Ok(())
}

#[test]
fn vm_access_needs_a_grant_and_a_current_declaration() -> TestResult {
    let vm = target("vm:windows")?;
    let (granted, authority) = delegated(vec![targeted(
        Permission::UseVerificationEnvironment,
        &["vm:windows"],
    )?])?;

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
    assert_eq!(access.credentials().len(), 1);

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
    let (none, authority) = delegated(Vec::new())?;
    let access = authorize_access(
        &authority,
        &none,
        &declaring(house()?, &declared)?,
        &host,
        &GrantScope::House,
    )?;
    assert!(access.credentials().is_empty());

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

#[test]
fn grants_authorize_only_the_targets_they_name() -> TestResult {
    let backend = declaring(
        house()?,
        &[
            ("vm:windows", Support::Supported),
            ("vm:linux", Support::Supported),
            ("device:phone", Support::Supported),
            ("device:km43-controller", Support::Supported),
        ],
    )?;
    let (current, authority) = delegated(vec![
        targeted(
            Permission::UseVerificationEnvironment,
            &["vm:windows", "device:phone", "device:km43-controller"],
        )?,
        targeted(Permission::OperateEquipment, &["device:phone"])?,
    ])?;
    let access = |on: &str| -> TestResult<Result<VerificationAccess, VerificationError>> {
        Ok(authorize_access(
            &authority,
            &current,
            &backend,
            &target(on)?,
            &GrantScope::House,
        ))
    };
    assert_eq!(access("vm:windows")??.target(), &target("vm:windows")?);
    assert_eq!(access("device:phone")??.credentials().len(), 2);

    // A grant for one VM does not cover another.
    assert_eq!(
        access("vm:linux")?,
        Err(VerificationError::Contract(
            ContractError::PermissionDenied {
                permission: Permission::UseVerificationEnvironment,
            }
        ))
    );
    // Equipment granted for one device class does not cover another.
    assert_eq!(
        access("device:km43-controller")?,
        Err(VerificationError::Contract(
            ContractError::PermissionDenied {
                permission: Permission::OperateEquipment,
            }
        ))
    );
    Ok(())
}

#[test]
fn task_targets_stay_within_the_house_targets() -> TestResult {
    let windows = targeted(Permission::UseVerificationEnvironment, &["vm:windows"])?;
    let current = HouseGrants::new(house()?, [windows.clone()]);
    let wider = targeted(
        Permission::UseVerificationEnvironment,
        &["vm:windows", "vm:linux"],
    )?;
    let expansion = ContractError::AuthorityExpansion {
        permission: Permission::UseVerificationEnvironment,
        scope: GrantScope::House,
    };
    assert_eq!(
        TaskAuthority::delegate(&current, [wider.clone()]),
        Err(expansion.clone())
    );

    // Narrowing the house grant after delegation refuses the next use.
    let house_wide = HouseGrants::new(house()?, [wider.clone()]);
    let authority = TaskAuthority::delegate(&house_wide, [wider])?;
    let backend = declaring(house()?, &[("vm:windows", Support::Supported)])?;
    assert_eq!(
        authorize_access(
            &authority,
            &current,
            &backend,
            &target("vm:windows")?,
            &GrantScope::House
        ),
        Err(VerificationError::Contract(expansion))
    );
    Ok(())
}

#[test]
fn untargeted_legacy_grants_stay_readable_and_authorize_nothing() -> TestResult {
    // A grant stored before grants named targets.
    let legacy = serde_json::json!({
        "permission": "use-verification-environment",
        "scope": { "type": "house" },
        "destination": "fake",
        "credential": "origin89-orca",
    });
    let read: Grant = serde_json::from_value(legacy.clone())?;
    assert_eq!(read, grant(Permission::UseVerificationEnvironment)?);
    assert!(read.targets.is_empty());
    assert_eq!(serde_json::to_value(&read)?, legacy);

    let (current, authority) = delegated(vec![read])?;
    let backend = declaring(house()?, &[("vm:windows", Support::Supported)])?;
    let denied = ContractError::PermissionDenied {
        permission: Permission::UseVerificationEnvironment,
    };
    assert_eq!(
        authorize_access(
            &authority,
            &current,
            &backend,
            &target("vm:windows")?,
            &GrantScope::House
        ),
        Err(VerificationError::Contract(denied.clone()))
    );

    // No path authorizes a target-scoped permission without a target, even
    // from a grant that names one.
    let (current, authority) = delegated(vec![targeted(
        Permission::UseVerificationEnvironment,
        &["vm:windows"],
    )?])?;
    assert_eq!(
        authority.authorize(
            &current,
            Permission::UseVerificationEnvironment,
            &GrantScope::House,
            &backend_id()?
        ),
        Err(denied)
    );
    assert_eq!(
        current.permitted(
            Permission::UseVerificationEnvironment,
            &GrantScope::House,
            &backend_id()?
        ),
        Err(ContractError::AuthorityExpansion {
            permission: Permission::UseVerificationEnvironment,
            scope: GrantScope::House,
        })
    );
    Ok(())
}

#[test]
fn grant_targets_are_validated() -> TestResult {
    let invalid_targets = Err(invalid());
    assert_eq!(
        grant(Permission::Merge)?.with_targets([target("vm:windows")?]),
        invalid_targets
    );
    let too_many = (0..=MAX_VERIFICATION_ENVIRONMENTS)
        .map(|index| {
            Ok(VerificationTarget::Device(DeviceClass::new(&format!(
                "d{index}"
            ))?))
        })
        .collect::<TestResult<Vec<_>>>()?;
    assert_eq!(
        grant(Permission::OperateEquipment)?.with_targets(too_many),
        invalid_targets
    );

    // The same rules hold for stored grants.
    let stored = |targets: serde_json::Value, permission: &str| {
        serde_json::from_value::<Grant>(serde_json::json!({
            "permission": permission,
            "scope": { "type": "house" },
            "destination": "fake",
            "credential": "origin89-orca",
            "targets": targets,
        }))
    };
    assert!(stored(serde_json::json!(["vm:windows"]), "merge").is_err());
    assert!(stored(serde_json::json!(["vm:beos"]), "operate-equipment").is_err());
    let read = stored(serde_json::json!(["device:phone"]), "operate-equipment")?;
    assert_eq!(
        read,
        targeted(Permission::OperateEquipment, &["device:phone"])?
    );
    // Grants without targets keep their stored form.
    assert!(
        serde_json::to_value(grant(Permission::Merge)?)?
            .get("targets")
            .is_none()
    );
    Ok(())
}

#[test]
fn house_only_activation_refuses_when_a_repository_adds_requirements() -> TestResult {
    let policy = VerificationPolicy::new()
        .require_in_house(work("release")?, [target("host:linux")?])?
        .require_in_repository(
            repo("origin89hq/firmware")?,
            work("release")?,
            [target("device:km43-controller")?],
        )?;
    let backend = environments(&[("host:linux", Support::Supported)])?;

    // Omitting the repository must not drop the repository's device target.
    let refused = Err(VerificationError::RepositoryRequired {
        work_type: work("release")?,
    });
    assert_eq!(policy.required_house_targets(&work("release")?), refused);
    assert_eq!(
        policy.check_house_activation(&work("release")?, &backend),
        refused
    );
    // A work type no repository touches still activates house-only.
    let policy = policy.require_in_house(work("docs")?, [target("host:linux")?])?;
    assert_eq!(
        policy.check_house_activation(&work("docs")?, &backend)?,
        BTreeSet::from([target("host:linux")?])
    );
    Ok(())
}
