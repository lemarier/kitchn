//! Validated contract values, authority delegation, and capability checks.

mod common;

use std::time::Duration;

use common::{TestResult, commit, grants, grants_for, house, other_house, spec};
use kitchen::{
    Error, ErrorClass, HouseId, IdentifierError,
    contracts::{
        Capability, CapabilitySet, CommitId, ContractError, ExternalRef, Grant, GrantScope,
        HouseGrants, LeaseTtl, MAX_EXTERNAL_REF_BYTES, MAX_TEXT_BYTES, Permission, Repository,
        RetryPolicy, Role, Support, TaskAuthority, TaskSpec, Text, ValueKind,
    },
};

fn invalid(kind: ValueKind) -> ContractError {
    ContractError::InvalidValue { kind }
}

#[test]
fn external_refs_accept_printable_ascii_up_to_the_bound() -> TestResult {
    let longest = "r".repeat(MAX_EXTERNAL_REF_BYTES);
    assert_eq!(ExternalRef::new(&longest)?.as_str(), longest);
    assert_eq!(
        ExternalRef::new("term_1:/path#x")?.to_string(),
        "term_1:/path#x"
    );
    for rejected in [
        String::new(),
        "r".repeat(MAX_EXTERNAL_REF_BYTES + 1),
        "has space".to_owned(),
        "tab\t".to_owned(),
        "é".to_owned(),
    ] {
        assert_eq!(
            ExternalRef::new(&rejected),
            Err(invalid(ValueKind::ExternalRef))
        );
    }
    Ok(())
}

#[test]
fn commit_ids_require_full_lowercase_hex() -> TestResult {
    assert!(CommitId::new(&"0".repeat(40)).is_ok());
    assert!(CommitId::new(&"f".repeat(64)).is_ok());
    for rejected in [
        "a".repeat(39),
        "a".repeat(41),
        "A".repeat(40),
        "g".repeat(40),
    ] {
        assert_eq!(CommitId::new(&rejected), Err(invalid(ValueKind::CommitId)));
    }
    Ok(())
}

#[test]
fn repositories_split_owner_and_name() -> TestResult {
    let repository = Repository::new("origin89hq/km43")?;
    assert_eq!(
        (repository.owner(), repository.name()),
        ("origin89hq", "km43")
    );
    assert!(Repository::new(&format!("{}/{}", "o".repeat(39), "n".repeat(100))).is_ok());
    for rejected in [
        "no-slash",
        "/name",
        "owner/",
        "-owner/name",
        "owner/..",
        "owner/a/b",
        "own er/name",
    ] {
        assert_eq!(
            Repository::new(rejected),
            Err(invalid(ValueKind::Repository))
        );
    }
    assert!(Repository::new(&format!("{}/n", "o".repeat(40))).is_err());
    Ok(())
}

#[test]
fn text_is_bounded_and_redacted_in_debug_output() -> TestResult {
    let brief = Text::new("private context: token hunter2")?;
    let debug = format!("{brief:?}");
    assert_eq!(debug, "Text(30 bytes)");
    assert!(Text::new(&"x".repeat(MAX_TEXT_BYTES)).is_ok());
    for rejected in [
        String::new(),
        "x".repeat(MAX_TEXT_BYTES + 1),
        "nul\0".to_owned(),
    ] {
        assert_eq!(Text::new(&rejected), Err(invalid(ValueKind::Text)));
    }
    Ok(())
}

#[test]
fn durations_and_retry_budgets_are_bounded() -> TestResult {
    assert!(LeaseTtl::new(LeaseTtl::MIN).is_ok());
    assert!(LeaseTtl::new(LeaseTtl::MAX).is_ok());
    assert_eq!(
        LeaseTtl::new(Duration::from_millis(999)),
        Err(invalid(ValueKind::LeaseTtl))
    );
    assert_eq!(
        LeaseTtl::new(LeaseTtl::MAX + Duration::from_secs(1)),
        Err(invalid(ValueKind::LeaseTtl))
    );
    let hour = Duration::from_secs(3600);
    assert_eq!(RetryPolicy::new(1, hour)?.max_attempts(), 1);
    assert!(RetryPolicy::new(RetryPolicy::MAX_ATTEMPTS, RetryPolicy::MAX_ELAPSED).is_ok());
    for (attempts, elapsed) in [
        (0, hour),
        (RetryPolicy::MAX_ATTEMPTS + 1, hour),
        (1, Duration::from_millis(999)),
        (1, RetryPolicy::MAX_ELAPSED + Duration::from_secs(1)),
    ] {
        assert_eq!(
            RetryPolicy::new(attempts, elapsed),
            Err(invalid(ValueKind::RetryPolicy))
        );
    }
    Ok(())
}

#[test]
fn closed_names_round_trip_and_reject_unknown_text() -> TestResult {
    for capability in Capability::ALL {
        assert_eq!(capability.as_str().parse::<Capability>()?, capability);
        let json = serde_json::to_string(&capability)?;
        assert_eq!(json, format!("\"{}\"", capability.as_str()));
    }
    for role in Role::ALL {
        assert_eq!(role.to_string().parse::<Role>()?, role);
    }
    for permission in Permission::ALL {
        assert_eq!(permission.to_string().parse::<Permission>()?, permission);
    }
    assert_eq!(
        "schedule.manage ".parse::<Capability>(),
        Err(invalid(ValueKind::Capability))
    );
    assert_eq!("Chef-Owner".parse::<Role>(), Err(invalid(ValueKind::Role)));
    assert_eq!(
        "admin".parse::<Permission>(),
        Err(invalid(ValueKind::Permission))
    );
    Ok(())
}

#[test]
fn task_authority_is_a_subset_of_house_grants() -> TestResult {
    let km43 = Repository::new("origin89hq/km43")?;
    let firmware = Repository::new("origin89hq/firmware")?;
    let house_grants = HouseGrants::new(
        house()?,
        [
            Grant::house(Permission::LaunchWorker),
            Grant::repository(Permission::PushBranch, km43.clone()),
        ],
    );
    let authority = TaskAuthority::delegate(
        &house_grants,
        [
            Grant::repository(Permission::LaunchWorker, firmware.clone()),
            Grant::repository(Permission::PushBranch, km43.clone()),
        ],
    )?;
    assert_eq!(authority.grants().count(), 2);
    authority.authorize(
        &house_grants,
        Permission::PushBranch,
        &GrantScope::Repository(km43.clone()),
    )?;

    let expansions = [
        (
            Grant::house(Permission::Merge),
            Permission::Merge,
            GrantScope::House,
        ),
        (
            Grant::house(Permission::PushBranch),
            Permission::PushBranch,
            GrantScope::House,
        ),
        (
            Grant::repository(Permission::PushBranch, firmware.clone()),
            Permission::PushBranch,
            GrantScope::Repository(firmware.clone()),
        ),
    ];
    for (requested, permission, scope) in expansions {
        assert_eq!(
            TaskAuthority::delegate(&house_grants, [requested]),
            Err(ContractError::AuthorityExpansion { permission, scope })
        );
    }
    Ok(())
}

#[test]
fn authorization_rechecks_current_grants_and_house() -> TestResult {
    let original = grants()?;
    let authority = TaskAuthority::delegate(&original, [Grant::house(Permission::LaunchWorker)])?;
    authority.authorize(&original, Permission::LaunchWorker, &GrantScope::House)?;

    assert_eq!(
        authority.authorize(&original, Permission::CancelWorker, &GrantScope::House),
        Err(ContractError::PermissionDenied {
            permission: Permission::CancelWorker
        })
    );
    let revoked = grants_for(house()?, &[Permission::CancelWorker]);
    assert_eq!(
        authority.authorize(&revoked, Permission::LaunchWorker, &GrantScope::House),
        Err(ContractError::AuthorityExpansion {
            permission: Permission::LaunchWorker,
            scope: GrantScope::House
        })
    );
    let foreign = grants_for(other_house()?, &[Permission::LaunchWorker]);
    assert_eq!(
        authority.authorize(&foreign, Permission::LaunchWorker, &GrantScope::House),
        Err(ContractError::CrossHouse {
            expected: house()?,
            found: other_house()?
        })
    );
    Ok(())
}

#[test]
fn backend_missing_required_capability_rejected() {
    let declared =
        CapabilitySet::supporting([Capability::ScheduleManage, Capability::WorkerLaunchIsolated])
            .with(Capability::SchedulePrecheck, Support::Partial);
    assert_eq!(
        declared.require([Capability::ScheduleManage, Capability::WorkerLaunchIsolated]),
        Ok(())
    );
    assert_eq!(declared.require([]), Ok(()));

    let error = declared
        .require([
            Capability::ScheduleSingleConsumer,
            Capability::SchedulePrecheck,
            Capability::ScheduleManage,
            Capability::ScheduleSingleConsumer,
        ])
        .err();
    assert_eq!(
        error,
        Some(ContractError::UnsupportedCapabilities {
            missing: vec![Capability::ScheduleSingleConsumer],
            partial: vec![Capability::SchedulePrecheck],
        })
    );
    let message = error.map(|error| error.to_string()).unwrap_or_default();
    assert!(message.contains("schedule.single_consumer"), "{message}");
    assert!(message.contains("partial: schedule.precheck"), "{message}");
}

#[test]
fn persisted_contracts_round_trip_and_revalidate() -> TestResult {
    let original = spec("task-1")?;
    let json = serde_json::to_string(&original)?;
    assert_eq!(serde_json::from_str::<TaskSpec>(&json)?, original);

    let secret = "secret/../value";
    let tampered = json.replace("\"task-1\"", &format!("\"{secret}\""));
    let error = serde_json::from_str::<TaskSpec>(&tampered)
        .err()
        .map(|e| e.to_string());
    assert!(
        error.as_deref().is_some_and(|text| !text.contains(secret)),
        "{error:?}"
    );

    let zero_attempts = json.replace("\"maxAttempts\":3", "\"maxAttempts\":0");
    assert!(serde_json::from_str::<TaskSpec>(&zero_attempts).is_err());
    let unknown_field = json.replacen('{', "{\"extra\":1,", 1);
    assert!(serde_json::from_str::<TaskSpec>(&unknown_field).is_err());
    assert!(serde_json::from_str::<CommitId>(&format!("\"{}\"", "A".repeat(40))).is_err());
    assert_eq!(
        serde_json::from_str::<CommitId>(&format!("\"{}\"", "a".repeat(40)))?,
        commit('a')?
    );
    Ok(())
}

#[test]
fn errors_expose_a_handling_class() -> TestResult {
    let identifier = HouseId::new("").err().map(Error::from);
    assert_eq!(
        identifier.map(|error| error.class()),
        Some(ErrorClass::InvalidInput)
    );
    assert!(matches!(
        HouseId::new("a b"),
        Err(IdentifierError::Characters)
    ));
    let refused = Error::from(ContractError::CrossHouse {
        expected: house()?,
        found: other_house()?,
    });
    assert_eq!(refused.class(), ErrorClass::Refused);
    assert_eq!(
        Error::from(invalid(ValueKind::Text)).class(),
        ErrorClass::InvalidInput
    );
    Ok(())
}
