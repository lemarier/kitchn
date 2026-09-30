//! Grant and revoke commands exercised through the real CLI and a private registry.
use kitchen::{
    BackendId, CredentialId,
    adoption::HouseRegistry,
    contracts::{ExternalRef, Grant, Permission, PostingBudget},
    house::{
        BackendBinding, BackendKind, CredentialKind, DoctorCode, FORGE_BINDING_SCHEMA,
        ForgeBinding, ForgeKind, HouseConfig, RepositoryConfig, Workflow, bind_forge, doctor,
    },
};
use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    process::{Command, Output, Stdio},
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn fixture(root: &Path) -> TestResult<HouseRegistry> {
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
    let revoked = run(&registry, &["revoke", "--workflow", "pickup", "--yes"])?;
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
    assert!(
        gate.status.success(),
        "{}",
        String::from_utf8_lossy(&gate.stderr)
    );
    let merged = registry.load(&before.house)?;
    assert!(
        merged
            .grants
            .iter()
            .any(|grant| grant.permission == Permission::Merge)
    );
    fs::remove_file(registry.private_path(&before.house)?.join("forge.json"))?;
    let revoke = run(&registry, &["revoke", "--workflow", "gate", "--yes"])?;
    assert!(
        revoke.status.success(),
        "{}",
        String::from_utf8_lossy(&revoke.stderr)
    );
    let after = registry.load(&before.house)?;
    assert!(after.grants.is_empty());
    assert!(after.policy_limits.is_empty());
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
