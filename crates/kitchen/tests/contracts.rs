//! Validated contract values, authority delegation, and capability checks.

mod common;

use std::time::Duration;

use common::{
    TestResult, backend_id, commit, credential, grant, grants, grants_for, house, other_house, spec,
};
use kitchen::{
    BackendId, CredentialId, Error, ErrorClass, HouseId, IdentifierError,
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
    // `ALL` is generated from the same list as the enum; names are unique.
    let unique = |names: Vec<&str>| {
        names
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == names.len()
    };
    assert!(unique(
        Capability::ALL.iter().map(|item| item.as_str()).collect()
    ));
    assert!(unique(
        Permission::ALL.iter().map(|item| item.as_str()).collect()
    ));
    assert!(unique(Role::ALL.iter().map(|item| item.as_str()).collect()));
    assert_eq!(
        "forge.mutation".parse::<Capability>()?,
        Capability::ForgeMutation
    );
    assert_eq!("human.ask".parse::<Capability>()?, Capability::AskHuman);
    // Every effect kind has its own lookup and idempotency capability.
    let lookups: std::collections::BTreeSet<_> = kitchen::contracts::EffectKind::ALL
        .iter()
        .map(|kind| kind.lookup_capability())
        .collect();
    let idempotency: std::collections::BTreeSet<_> = kitchen::contracts::EffectKind::ALL
        .iter()
        .map(|kind| kind.idempotency_capability())
        .collect();
    assert_eq!(lookups.len(), kitchen::contracts::EffectKind::ALL.len());
    assert_eq!(idempotency.len(), kitchen::contracts::EffectKind::ALL.len());
    assert!(lookups.is_disjoint(&idempotency));
    assert_eq!(
        "effect.lookup.message_worker".parse::<Capability>()?,
        Capability::LookupMessageWorker
    );
    for (name, permission) in [
        ("manage-schedule", Permission::ManageSchedule),
        ("activate-schedule", Permission::ActivateSchedule),
        ("trial-schedule", Permission::TrialSchedule),
        ("create-issue", Permission::CreateIssue),
        (
            "edit-issue-relationships",
            Permission::EditIssueRelationships,
        ),
        ("ask-human", Permission::AskHuman),
    ] {
        assert_eq!(name.parse::<Permission>()?, permission);
    }
    assert_eq!(
        "admin".parse::<Permission>(),
        Err(invalid(ValueKind::Permission))
    );
    Ok(())
}

#[test]
fn task_authority_is_a_subset_of_standing_grants() -> TestResult {
    let km43 = Repository::new("origin89hq/km43")?;
    let firmware = Repository::new("origin89hq/firmware")?;
    let orca = backend_id()?;
    let house_grants = HouseGrants::new(
        house()?,
        [
            Grant::house(Permission::LaunchWorker, orca.clone(), credential()?),
            Grant::repository(
                Permission::PushBranch,
                km43.clone(),
                orca.clone(),
                credential()?,
            ),
        ],
    );
    let authority = TaskAuthority::delegate(
        &house_grants,
        [
            Grant::repository(
                Permission::LaunchWorker,
                firmware.clone(),
                orca.clone(),
                credential()?,
            ),
            Grant::repository(
                Permission::PushBranch,
                km43.clone(),
                orca.clone(),
                credential()?,
            ),
        ],
    )?;
    assert_eq!(authority.grants().count(), 2);
    assert_eq!(
        authority.authorize(
            &house_grants,
            Permission::PushBranch,
            &GrantScope::Repository(km43.clone()),
            &orca,
        )?,
        credential()?
    );

    let other_backend = BackendId::new("elsewhere")?;
    let other_credential = CredentialId::new("personal-token")?;
    let expansions = [
        Grant::house(Permission::Merge, orca.clone(), credential()?),
        Grant::house(Permission::PushBranch, orca.clone(), credential()?),
        Grant::repository(
            Permission::PushBranch,
            firmware.clone(),
            orca.clone(),
            credential()?,
        ),
        Grant::house(Permission::LaunchWorker, other_backend, credential()?),
        Grant::house(Permission::LaunchWorker, orca.clone(), other_credential),
    ];
    for requested in expansions {
        let permission = requested.permission;
        let scope = requested.scope.clone();
        assert_eq!(
            TaskAuthority::delegate(&house_grants, [requested]),
            Err(ContractError::AuthorityExpansion { permission, scope })
        );
    }
    Ok(())
}

#[test]
fn authorization_binds_destination_credential_and_current_grants() -> TestResult {
    let original = grants()?;
    let orca = backend_id()?;
    let authority = TaskAuthority::delegate(&original, [grant(Permission::LaunchWorker)?])?;
    assert_eq!(
        authority.authorize(
            &original,
            Permission::LaunchWorker,
            &GrantScope::House,
            &orca
        )?,
        credential()?
    );
    assert_eq!(
        authority.authorize(
            &original,
            Permission::LaunchWorker,
            &GrantScope::House,
            &BackendId::new("elsewhere")?
        ),
        Err(ContractError::PermissionDenied {
            permission: Permission::LaunchWorker
        }),
        "a grant names its destination backend"
    );
    assert_eq!(
        authority.authorize(
            &original,
            Permission::CancelWorker,
            &GrantScope::House,
            &orca
        ),
        Err(ContractError::PermissionDenied {
            permission: Permission::CancelWorker
        })
    );
    let revoked = grants_for(house()?, &[Permission::CancelWorker])?;
    assert_eq!(
        authority.authorize(
            &revoked,
            Permission::LaunchWorker,
            &GrantScope::House,
            &orca
        ),
        Err(ContractError::AuthorityExpansion {
            permission: Permission::LaunchWorker,
            scope: GrantScope::House
        })
    );
    let foreign = grants_for(other_house()?, &[Permission::LaunchWorker])?;
    assert_eq!(
        authority.authorize(
            &foreign,
            Permission::LaunchWorker,
            &GrantScope::House,
            &orca
        ),
        Err(ContractError::CrossHouse {
            expected: house()?,
            found: other_house()?
        })
    );
    Ok(())
}

#[test]
fn the_most_specific_grant_selects_the_credential() -> TestResult {
    let km43 = Repository::new("origin89hq/km43")?;
    let orca = backend_id()?;
    let deploy = CredentialId::new("km43-deploy")?;
    let house_wide = Grant::house(Permission::PushBranch, orca.clone(), credential()?);
    let specific = Grant::repository(
        Permission::PushBranch,
        km43.clone(),
        orca.clone(),
        deploy.clone(),
    );
    let grants = HouseGrants::new(house()?, [house_wide.clone(), specific.clone()]);
    let authority = TaskAuthority::delegate(&grants, [house_wide, specific])?;
    let km43_scope = GrantScope::Repository(km43);
    assert_eq!(
        authority.authorize(&grants, Permission::PushBranch, &km43_scope, &orca)?,
        deploy
    );
    assert_eq!(
        authority.authorize(&grants, Permission::PushBranch, &GrantScope::House, &orca)?,
        credential()?
    );

    let ambiguous = HouseGrants::new(
        house()?,
        [
            Grant::house(Permission::PushBranch, orca.clone(), credential()?),
            Grant::house(Permission::PushBranch, orca.clone(), deploy),
        ],
    );
    assert_eq!(
        ambiguous.permitted(Permission::PushBranch, &GrantScope::House, &orca),
        Err(ContractError::AmbiguousCredential {
            permission: Permission::PushBranch
        })
    );
    Ok(())
}

#[test]
fn house_limits_bound_standing_grants() -> TestResult {
    let orca = backend_id()?;
    let limits = [
        grant(Permission::LaunchWorker)?,
        grant(Permission::CancelWorker)?,
    ];
    let house_grants =
        HouseGrants::with_limits(house()?, limits.clone(), [grant(Permission::CancelWorker)?])?;
    assert_eq!(
        house_grants.permitted(Permission::LaunchWorker, &GrantScope::House, &orca)?,
        credential()?
    );
    assert_eq!(
        house_grants.permitted(Permission::Merge, &GrantScope::House, &orca),
        Err(ContractError::AuthorityExpansion {
            permission: Permission::Merge,
            scope: GrantScope::House
        })
    );
    // Standing grants, not limits, bound delegation.
    assert_eq!(
        TaskAuthority::delegate(&house_grants, [grant(Permission::LaunchWorker)?]),
        Err(ContractError::AuthorityExpansion {
            permission: Permission::LaunchWorker,
            scope: GrantScope::House
        })
    );
    assert_eq!(
        HouseGrants::with_limits(house()?, limits, [grant(Permission::Merge)?]),
        Err(ContractError::AuthorityExpansion {
            permission: Permission::Merge,
            scope: GrantScope::House
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

#[test]
fn closing_an_issue_needs_its_own_explicit_grant() -> TestResult {
    let close: Permission = "close-issue".parse()?;
    assert_eq!(close.as_str(), "close-issue");
    let orca = backend_id()?;
    // Every other issue permission is granted, closing is not.
    let token = credential()?;
    let others: Vec<_> = Permission::ALL
        .into_iter()
        .filter(|permission| permission.as_str() != "close-issue")
        .map(|permission| Grant::house(permission, orca.clone(), token.clone()))
        .collect();
    let house_grants = HouseGrants::new(house()?, others.clone());
    let authority = TaskAuthority::delegate(&house_grants, others)?;
    assert_eq!(
        authority.authorize(&house_grants, close, &GrantScope::House, &orca),
        Err(ContractError::PermissionDenied { permission: close })
    );
    let explicit = HouseGrants::new(house()?, [Grant::house(close, orca.clone(), credential()?)]);
    let granted = TaskAuthority::delegate(
        &explicit,
        [Grant::house(close, orca.clone(), credential()?)],
    )?;
    assert_eq!(
        granted.authorize(&explicit, close, &GrantScope::House, &orca)?,
        credential()?
    );
    Ok(())
}

#[test]
fn standing_grants_can_be_extended_only_within_house_limits() -> TestResult {
    let orca = backend_id()?;
    let token = credential()?;
    let limits = [
        Grant::house(Permission::LaunchWorker, orca.clone(), token.clone()),
        Grant::house(Permission::PostComment, orca.clone(), token.clone()),
        Grant::house(Permission::Merge, orca.clone(), token.clone()),
    ];
    let original = HouseGrants::with_limits(house()?, limits, [grant(Permission::LaunchWorker)?])?;
    let extended = original.with_added_standing([grant(Permission::PostComment)?])?;
    assert!(extended.covers(&grant(Permission::PostComment)?));
    assert!(extended.covers(&grant(Permission::LaunchWorker)?));
    assert!(
        !original.covers(&grant(Permission::PostComment)?),
        "the original is unchanged"
    );

    // Outside the limits: refused, never silently dropped.
    for outside in [
        Permission::Publish,
        Permission::OperateEquipment,
        Permission::CloseIssue,
    ] {
        assert_eq!(
            original.with_added_standing([grant(Permission::PostComment)?, grant(outside)?]),
            Err(ContractError::AuthorityExpansion {
                permission: outside,
                scope: GrantScope::House
            })
        );
    }
    // Merge is added only because house policy already permits it.
    assert!(
        original
            .with_added_standing([grant(Permission::Merge)?])?
            .covers(&grant(Permission::Merge)?)
    );
    let narrow = HouseGrants::new(house()?, [grant(Permission::LaunchWorker)?]);
    assert!(
        narrow
            .with_added_standing([grant(Permission::Merge)?])
            .is_err()
    );
    assert_eq!(narrow.with_added_standing([])?, narrow);
    Ok(())
}

#[test]
fn branch_names_follow_git_ref_rules() -> TestResult {
    for valid in [
        "main",
        "lemarier/core-contracts",
        "fix-42",
        "a.b",
        "release/v1.2",
    ] {
        assert_eq!(kitchen::contracts::BranchName::new(valid)?.as_str(), valid);
    }
    let too_long = "b".repeat(kitchen::contracts::MAX_BRANCH_NAME_BYTES + 1);
    for invalid in [
        "",
        "-leading",
        "@",
        "a..b",
        "a@{1}",
        "trailing.",
        "trailing/",
        "/leading",
        "a//b",
        ".hidden",
        "a/.hidden",
        "x.lock",
        "a/x.lock/b",
        "has space",
        "tilde~",
        "caret^",
        "colon:",
        "q?",
        "star*",
        "bracket[",
        "back\\slash",
        "tab\t",
        too_long.as_str(),
    ] {
        assert_eq!(
            kitchen::contracts::BranchName::new(invalid),
            Err(invalid_kind(ValueKind::BranchName)),
            "{invalid:?}"
        );
    }
    let longest = "b".repeat(kitchen::contracts::MAX_BRANCH_NAME_BYTES);
    assert!(kitchen::contracts::BranchName::new(&longest).is_ok());
    Ok(())
}

fn invalid_kind(kind: ValueKind) -> ContractError {
    ContractError::InvalidValue { kind }
}
