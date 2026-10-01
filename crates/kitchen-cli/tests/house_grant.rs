//! Grant and revoke commands exercised through the real CLI and a private registry.
use kitchen::{
    BackendId, CredentialId,
    adoption::HouseRegistry,
    contracts::{ExternalRef, Grant, GrantScope, Permission, PostingBudget},
    house::{
        BackendBinding, BackendKind, CredentialKind, DoctorCode, FORGE_BINDING_SCHEMA,
        ForgeBinding, ForgeKind, HouseConfig, RepositoryConfig, Workflow, bind_forge, doctor,
    },
};
use std::{
    collections::BTreeSet,
    path::Path,
    process::{Command, Output, Stdio},
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn fixture(root: &Path) -> TestResult<HouseRegistry> {
    fixture_with_second_repository(root, false)
}

fn fixture_with_second_repository(
    root: &Path,
    second_repository: bool,
) -> TestResult<HouseRegistry> {
    let registry = HouseRegistry::new(root.canonicalize()?.join("registry"))?;
    let mut config: HouseConfig = serde_json::from_str(include_str!(
        "../../kitchen/tests/fixtures/house/crabnebula.json"
    ))?;
    config.backend = Some(BackendBinding {
        kind: BackendKind::Orca.into(),
        backend: BackendId::new("orca")?,
        credential: CredentialId::new("orca-host")?,
        endpoint: None,
    });
    if second_repository {
        config.repositories.insert("crabnebula/another".parse()?);
        config
            .posting_destinations
            .insert("crabnebula/another".parse()?);
    }
    registry.initialize(&config)?;
    bind_forge(
        &registry,
        &ForgeBinding {
            schema: FORGE_BINDING_SCHEMA,
            house: config.house.clone(),
            forge: ForgeKind::GitHub,
            backend: BackendId::new("github")?,
            requester: ExternalRef::new("owner")?,
            credential: CredentialId::new("github")?,
            credential_kind: CredentialKind::Token,
            bot_user_id: None,
            posting_budget: PostingBudget::new(20)?,
        },
    )?;
    Ok(registry)
}

fn run(registry: &HouseRegistry, args: &[&str]) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_kitchn"))
        .args(["house", args[0], "--registry"])
        .arg(registry.root())
        .args(&args[1..])
        .stdin(Stdio::null())
        .output()?)
}

#[test]
fn preview_confirmation_grant_and_revoke() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = fixture(temp.path())?;
    let house = "crabnebula".parse()?;
    let repository: kitchen::contracts::Repository = "crabnebula/tauri-fixture".parse()?;
    let args = ["grant", "--workflow", "pickup"];
    let preview = run(&registry, &[&args[..], &["--preview"]].concat())?;
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let shown = String::from_utf8(preview.stdout)?;
    for permission in [
        "launch-worker",
        "message-worker",
        "cancel-worker",
        "release-resource",
        "push-branch",
        "open-pull-request",
    ] {
        assert!(shown.contains(permission), "{shown}");
    }
    assert!(registry.load(&house)?.grants.is_empty());
    let before = registry.load(&house)?;
    let declined = run(&registry, &args)?;
    assert!(declined.status.success());
    assert!(registry.load(&house)?.grants.is_empty());
    let applied = run(&registry, &[&args[..], &["--yes"]].concat())?;
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let held = registry.load(&house)?.authority()?;
    assert!(matches!(
        registry.configure_grants(&before, &before),
        Err(kitchen::house::HouseError::Conflict)
    ));
    let bound = RepositoryConfig {
        schema: 2,
        house: house.clone(),
        repository: repository.clone(),
        workflows: BTreeSet::from([Workflow::Pickup]),
        additional_reviewers: BTreeSet::new(),
        additional_checks: BTreeSet::new(),
    };
    assert!(
        !doctor(&registry, &bound, None)?
            .findings
            .iter()
            .any(|finding| finding.code == DoctorCode::Authority)
    );
    let launch = Grant::repository(
        Permission::LaunchWorker,
        repository,
        BackendId::new("orca")?,
        CredentialId::new("orca-host")?,
    );
    assert!(held.covers(&launch));
    let revoked = run(
        &registry,
        &["revoke", "--workflow", "pickup", "--house-wide", "--yes"],
    )?;
    assert!(
        revoked.status.success(),
        "{}",
        String::from_utf8_lossy(&revoked.stderr)
    );
    assert!(!registry.load(&house)?.authority()?.covers(&launch));
    assert!(
        doctor(&registry, &bound, None)?
            .findings
            .iter()
            .any(|finding| finding.code == DoctorCode::Authority
                && finding.message.contains("launch-worker"))
    );
    Ok(())
}

#[test]
fn invalid_permission_and_missing_binding_never_write() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = fixture(temp.path())?;
    let before = registry.load(&"crabnebula".parse()?)?;
    let unknown = run(
        &registry,
        &["grant", "--permission", "launch-anything", "--yes"],
    )?;
    assert_eq!(unknown.status.code(), Some(2));
    assert_eq!(registry.load(&before.house)?, before);
    let unsupported = run(&registry, &["grant", "--workflow", "triage", "--yes"])?;
    assert_eq!(unsupported.status.code(), Some(2));
    let unbound = run(&registry, &["grant", "--permission", "ask-human", "--yes"])?;
    assert_eq!(unbound.status.code(), Some(2));
    assert_eq!(registry.load(&before.house)?, before);
    let gate = run(&registry, &["grant", "--workflow", "gate", "--yes"])?;
    assert_eq!(gate.status.code(), Some(1));
    assert!(String::from_utf8(gate.stderr)?.contains("exact head"));
    let merge = run(&registry, &["grant", "--permission", "merge", "--yes"])?;
    assert_eq!(merge.status.code(), Some(1));
    assert!(String::from_utf8(merge.stderr)?.contains("exact head"));
    let after = registry.load(&before.house)?;
    assert_eq!(after, before);
    after.authority()?;
    let other: HouseConfig = serde_json::from_str(include_str!(
        "../../kitchen/tests/fixtures/house/origin89.json"
    ))?;
    registry.initialize(&other)?;
    let selected = run(
        &registry,
        &[
            "grant",
            "--permission",
            "launch-worker",
            "--repository",
            "crabnebula/tauri-fixture",
            "--yes",
        ],
    )?;
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    assert!(
        registry
            .load(&before.house)?
            .grants
            .iter()
            .any(|grant| grant.permission == Permission::LaunchWorker)
    );
    assert!(registry.load(&other.house)?.grants.is_empty());
    Ok(())
}

#[test]
fn repository_revoke_preserves_shared_limit_and_other_repository_grant() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = fixture_with_second_repository(temp.path(), true)?;
    let before = registry.load(&"crabnebula".parse()?)?;
    let a: kitchen::contracts::Repository = "crabnebula/tauri-fixture".parse()?;
    let b: kitchen::contracts::Repository = "crabnebula/another".parse()?;
    let backend = BackendId::new("orca")?;
    let credential = CredentialId::new("orca-host")?;
    let shared = Grant::house(
        Permission::MessageWorker,
        backend.clone(),
        credential.clone(),
    );
    let a_grant = Grant::repository(
        Permission::MessageWorker,
        a,
        backend.clone(),
        credential.clone(),
    );
    let b_grant = Grant::repository(Permission::MessageWorker, b, backend, credential);
    let mut configured = before.clone();
    configured.policy_limits.insert(shared.clone());
    configured.grants.extend([a_grant.clone(), b_grant.clone()]);
    registry.configure_grants(&before, &configured)?;

    let args = [
        "revoke",
        "--permission",
        "message-worker",
        "--repository",
        "crabnebula/tauri-fixture",
    ];
    let preview = run(&registry, &[&args[..], &["--preview"]].concat())?;
    assert!(preview.status.success());
    let shown = String::from_utf8(preview.stdout)?;
    assert!(shown.contains("Retained house-scoped authority for message-worker"));
    assert!(shown.contains("--house-wide"));
    assert_eq!(registry.load(&before.house)?, configured);

    let applied = run(&registry, &[&args[..], &["--yes"]].concat())?;
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let after = registry.load(&before.house)?;
    assert!(!after.grants.contains(&a_grant));
    assert!(after.grants.contains(&b_grant));
    assert!(after.policy_limits.contains(&shared));
    assert!(after.authority()?.covers(&b_grant));
    let broad = run(&registry, &[&args[..], &["--house-wide", "--yes"]].concat())?;
    assert!(broad.status.success());
    let after_broad = registry.load(&before.house)?;
    assert!(!after_broad.grants.contains(&b_grant));
    assert!(!after_broad.policy_limits.contains(&shared));
    Ok(())
}

#[test]
fn repository_revoke_preserves_shared_standing_and_house_wide_revoke_is_explicit() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = fixture_with_second_repository(temp.path(), true)?;
    let before = registry.load(&"crabnebula".parse()?)?;
    let backend = BackendId::new("orca")?;
    let credential = CredentialId::new("orca-host")?;
    let shared = Grant::house(
        Permission::CancelWorker,
        backend.clone(),
        credential.clone(),
    );
    let other = Grant::repository(
        Permission::CancelWorker,
        "crabnebula/another".parse()?,
        backend,
        credential,
    );
    let mut configured = before.clone();
    configured.grants.insert(shared.clone());
    configured.policy_limits.insert(shared.clone());
    registry.configure_grants(&before, &configured)?;
    let scoped = run(
        &registry,
        &[
            "revoke",
            "--permission",
            "cancel-worker",
            "--repository",
            "crabnebula/tauri-fixture",
            "--yes",
        ],
    )?;
    assert!(scoped.status.success());
    assert!(String::from_utf8(scoped.stdout)?.contains("Retained house-scoped authority"));
    assert_eq!(registry.load(&before.house)?, configured);
    assert!(configured.authority()?.covers(&other));

    let broad = run(
        &registry,
        &[
            "revoke",
            "--permission",
            "cancel-worker",
            "--repository",
            "crabnebula/tauri-fixture",
            "--house-wide",
            "--yes",
        ],
    )?;
    assert!(broad.status.success());
    let after = registry.load(&before.house)?;
    assert!(!after.grants.contains(&shared));
    assert!(!after.policy_limits.contains(&shared));
    assert!(!after.authority()?.covers(&other));
    Ok(())
}

#[test]
fn doctor_accepts_repository_scoped_worker_effect_grants() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = fixture_with_second_repository(temp.path(), true)?;
    let before = registry.load(&"crabnebula".parse()?)?;
    let repository: kitchen::contracts::Repository = "crabnebula/tauri-fixture".parse()?;
    let other: kitchen::contracts::Repository = "crabnebula/another".parse()?;
    let mut configured = before.clone();
    for permission in [
        Permission::LaunchWorker,
        Permission::MessageWorker,
        Permission::CancelWorker,
        Permission::ReleaseResource,
    ] {
        let grant = Grant::repository(
            permission,
            repository.clone(),
            BackendId::new("orca")?,
            CredentialId::new("orca-host")?,
        );
        configured.grants.insert(grant.clone());
        configured.policy_limits.insert(grant);
    }
    for permission in [Permission::PushBranch, Permission::OpenPullRequest] {
        let grant = Grant::repository(
            permission,
            repository.clone(),
            BackendId::new("github")?,
            CredentialId::new("github")?,
        );
        configured.grants.insert(grant.clone());
        configured.policy_limits.insert(grant);
    }
    registry.configure_grants(&before, &configured)?;
    let bound = |repository| RepositoryConfig {
        schema: 2,
        house: before.house.clone(),
        repository,
        workflows: BTreeSet::from([Workflow::Pickup]),
        additional_reviewers: BTreeSet::new(),
        additional_checks: BTreeSet::new(),
    };
    let a_findings = doctor(&registry, &bound(repository), None)?.findings;
    assert!(
        !a_findings
            .iter()
            .any(|finding| finding.code == DoctorCode::Authority)
    );
    let b_findings = doctor(&registry, &bound(other), None)?.findings;
    assert!(b_findings.iter().any(|finding| {
        finding.code == DoctorCode::Authority && finding.message.contains("message-worker")
    }));
    Ok(())
}

#[test]
fn pickup_grant_and_revoke_are_repository_scoped() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = fixture_with_second_repository(temp.path(), true)?;
    let house = "crabnebula".parse()?;
    let a: kitchen::contracts::Repository = "crabnebula/tauri-fixture".parse()?;
    let b: kitchen::contracts::Repository = "crabnebula/another".parse()?;
    for repository in [&a, &b] {
        let result = run(
            &registry,
            &[
                "grant",
                "--workflow",
                "pickup",
                "--repository",
                repository.as_str(),
                "--yes",
            ],
        )?;
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        if repository == &a {
            let configured = registry.load(&house)?;
            let authority = configured.authority()?;
            for permission in [
                Permission::LaunchWorker,
                Permission::MessageWorker,
                Permission::CancelWorker,
                Permission::ReleaseResource,
                Permission::PushBranch,
                Permission::OpenPullRequest,
            ] {
                let (backend, credential) = if matches!(
                    permission,
                    Permission::PushBranch | Permission::OpenPullRequest
                ) {
                    (BackendId::new("github")?, CredentialId::new("github")?)
                } else {
                    (BackendId::new("orca")?, CredentialId::new("orca-host")?)
                };
                let a_grant =
                    Grant::repository(permission, a.clone(), backend.clone(), credential.clone());
                let b_grant = Grant::repository(permission, b.clone(), backend.clone(), credential);
                assert!(configured.grants.contains(&a_grant));
                assert!(configured.policy_limits.contains(&a_grant));
                assert!(authority.covers(&a_grant));
                assert!(!authority.covers(&b_grant));
                assert!(
                    authority
                        .permitted(permission, &b_grant.scope, &backend)
                        .is_err()
                );
            }
        }
    }
    let configured = registry.load(&house)?;
    let authority = configured.authority()?;
    for permission in [
        Permission::LaunchWorker,
        Permission::MessageWorker,
        Permission::CancelWorker,
        Permission::ReleaseResource,
        Permission::PushBranch,
        Permission::OpenPullRequest,
    ] {
        let (backend, credential) = if matches!(
            permission,
            Permission::PushBranch | Permission::OpenPullRequest
        ) {
            (BackendId::new("github")?, CredentialId::new("github")?)
        } else {
            (BackendId::new("orca")?, CredentialId::new("orca-host")?)
        };
        let a_grant = Grant::repository(permission, a.clone(), backend.clone(), credential.clone());
        let b_grant = Grant::repository(permission, b.clone(), backend, credential);
        assert!(configured.grants.contains(&a_grant));
        assert!(configured.policy_limits.contains(&a_grant));
        assert!(configured.grants.contains(&b_grant));
        assert!(configured.policy_limits.contains(&b_grant));
        assert!(authority.covers(&a_grant));
        assert!(authority.covers(&b_grant));
    }
    assert!(
        configured
            .grants
            .iter()
            .all(|grant| matches!(grant.scope, GrantScope::Repository(_)))
    );
    assert!(
        configured
            .policy_limits
            .iter()
            .all(|grant| matches!(grant.scope, GrantScope::Repository(_)))
    );

    let revoked = run(
        &registry,
        &[
            "revoke",
            "--workflow",
            "pickup",
            "--repository",
            a.as_str(),
            "--yes",
        ],
    )?;
    assert!(
        revoked.status.success(),
        "{}",
        String::from_utf8_lossy(&revoked.stderr)
    );
    let after = registry.load(&house)?;
    assert!(
        after
            .grants
            .iter()
            .all(|grant| grant.scope == GrantScope::Repository(b.clone()))
    );
    assert!(
        after
            .policy_limits
            .iter()
            .all(|grant| grant.scope == GrantScope::Repository(b.clone()))
    );
    assert_eq!(after.grants.len(), configured.grants.len() / 2);
    assert_eq!(
        after.policy_limits.len(),
        configured.policy_limits.len() / 2
    );
    let after_authority = after.authority()?;
    for permission in [
        Permission::MessageWorker,
        Permission::CancelWorker,
        Permission::ReleaseResource,
    ] {
        let a_grant = Grant::repository(
            permission,
            a.clone(),
            BackendId::new("orca")?,
            CredentialId::new("orca-host")?,
        );
        let b_grant = Grant::repository(
            permission,
            b.clone(),
            BackendId::new("orca")?,
            CredentialId::new("orca-host")?,
        );
        assert!(!after_authority.covers(&a_grant));
        assert!(after_authority.covers(&b_grant));
    }
    Ok(())
}

#[test]
fn house_wide_grant_requires_explicit_flag() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = fixture_with_second_repository(temp.path(), true)?;
    let house = "crabnebula".parse()?;
    let result = run(
        &registry,
        &[
            "grant",
            "--permission",
            "message-worker",
            "--repository",
            "crabnebula/tauri-fixture",
            "--house-wide",
            "--yes",
        ],
    )?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let configured = registry.load(&house)?;
    let shared = Grant::house(
        Permission::MessageWorker,
        BackendId::new("orca")?,
        CredentialId::new("orca-host")?,
    );
    assert_eq!(configured.grants, BTreeSet::from([shared.clone()]));
    assert_eq!(configured.policy_limits, BTreeSet::from([shared.clone()]));
    assert!(configured.authority()?.covers(&Grant::repository(
        Permission::MessageWorker,
        "crabnebula/another".parse()?,
        shared.destination,
        shared.credential
    )));
    let review = run(
        &registry,
        &[
            "grant",
            "--permission",
            "review-pull-request",
            "--repository",
            "crabnebula/tauri-fixture",
            "--house-wide",
            "--yes",
        ],
    )?;
    assert!(
        review.status.success(),
        "{}",
        String::from_utf8_lossy(&review.stderr)
    );
    let review_grant = Grant::repository(
        Permission::ReviewPullRequest,
        "crabnebula/tauri-fixture".parse()?,
        BackendId::new("github")?,
        CredentialId::new("github")?,
    );
    let after = registry.load(&house)?;
    assert!(after.grants.contains(&review_grant));
    assert!(after.policy_limits.contains(&review_grant));
    Ok(())
}

#[test]
fn doctor_points_missing_merge_authority_at_the_gate_path() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = fixture(temp.path())?;
    let house = registry.load(&"crabnebula".parse()?)?;
    let bound = RepositoryConfig {
        schema: 2,
        house: house.house.clone(),
        repository: "crabnebula/tauri-fixture".parse()?,
        workflows: BTreeSet::from([Workflow::Gate]),
        additional_reviewers: BTreeSet::new(),
        additional_checks: BTreeSet::new(),
    };
    let findings = doctor(&registry, &bound, None)?.findings;
    let merge = findings
        .iter()
        .find(|finding| finding.code == DoctorCode::Authority && finding.message.contains("merge"))
        .ok_or("no merge authority finding")?;
    assert!(
        merge.next_step.contains("kitchn gate attest"),
        "{}",
        merge.next_step
    );
    assert!(
        !merge.next_step.contains("--permission merge"),
        "{}",
        merge.next_step
    );
    Ok(())
}

#[test]
fn grant_without_registry_uses_kitchn_home() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = fixture(temp.path())?;
    let before = registry.load(&"crabnebula".parse()?)?;
    let output = Command::new(env!("CARGO_BIN_EXE_kitchn"))
        .args([
            "house",
            "grant",
            "--house",
            "crabnebula",
            "--workflow",
            "pickup",
            "--yes",
        ])
        .env("KITCHN_HOME", registry.root())
        .env("HOME", temp.path().join("elsewhere"))
        .current_dir(temp.path())
        .stdin(Stdio::null())
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = registry.load(&"crabnebula".parse()?)?;
    assert!(after.grants.len() > before.grants.len());
    assert!(!temp.path().join("elsewhere/.kitchn").exists());
    Ok(())
}

#[test]
fn review_thread_resolution_is_granted_on_the_forge_binding() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = fixture(temp.path())?;
    let output = run(
        &registry,
        &[
            "grant",
            "--house",
            "crabnebula",
            "--repository",
            "crabnebula/tauri-fixture",
            "--permission",
            "resolve-review-thread",
            "--yes",
        ],
    )?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let house = registry.load(&"crabnebula".parse()?)?;
    let grant = house
        .grants
        .iter()
        .find(|grant| grant.permission == Permission::ResolveReviewThread)
        .ok_or("no resolve-review-thread grant")?;
    assert_eq!(grant.destination, BackendId::new("github")?);
    assert_eq!(grant.credential, CredentialId::new("github")?);
    assert_eq!(
        grant.scope,
        GrantScope::Repository("crabnebula/tauri-fixture".parse()?)
    );
    Ok(())
}
