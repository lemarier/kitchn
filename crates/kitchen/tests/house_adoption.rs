//! House isolation, immutable pins, setup diagnoses and recovery using synthetic fixtures.
use kitchen::{
    adoption::{HouseRegistry, InstructionBundle, checkout_remotes, resolve_instructions},
    contracts::{Capability, CapabilitySet, CommitId},
    house::{
        AccessStatus, DoctorCode, DoctorEvidence, HouseConfig, HouseError, LabelStatus,
        RepositoryConfig, RepositoryLabel, Workflow, doctor, preview_labels, resolve_house,
        workflow_requirements,
    },
};
use std::{collections::BTreeSet, fs, path::Path, process::Command};

type TestResult = Result<(), Box<dyn std::error::Error>>;
fn config(name: &str) -> Result<HouseConfig, Box<dyn std::error::Error>> {
    let config: HouseConfig = serde_json::from_str(match name {
        "origin89" => include_str!("fixtures/house/origin89.json"),
        _ => include_str!("fixtures/house/crabnebula.json"),
    })?;
    Ok(config)
}
fn bundle(name: &str) -> Result<InstructionBundle, Box<dyn std::error::Error>> {
    let bundle: InstructionBundle = serde_json::from_str(match name {
        "origin89" => include_str!("fixtures/house/origin89-bundle.json"),
        _ => include_str!("fixtures/house/crabnebula-bundle.json"),
    })?;
    Ok(bundle)
}
fn repo(house: &HouseConfig) -> Result<RepositoryConfig, Box<dyn std::error::Error>> {
    Ok(RepositoryConfig {
        schema: 2,
        house: house.house.clone(),
        repository: house.repositories.first().ok_or("empty fixture")?.clone(),
        workflows: BTreeSet::from([Workflow::Pickup, Workflow::Gate]),
        additional_reviewers: BTreeSet::new(),
        additional_checks: BTreeSet::new(),
    })
}
/// A Git checkout at `path` whose `origin` names `repository` on GitHub.
fn checkout(path: &Path, repository: &str) -> TestResult {
    fs::create_dir_all(path)?;
    for args in [
        vec!["init", "--quiet"],
        vec![
            "remote",
            "add",
            "origin",
            &format!("https://github.com/{repository}.git"),
        ],
    ] {
        let status = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_COMMON_DIR")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .arg("-C")
            .arg(path)
            .args(&args)
            .status()?;
        if !status.success() {
            return Err(format!("git {args:?} failed").into());
        }
    }
    Ok(())
}
#[test]
fn two_houses_resolve_without_guidance_or_authority_leakage() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = HouseRegistry::new(root.join("registry"))?;
    for name in ["origin89", "crabnebula"] {
        let house = config(name)?;
        registry.initialize(&house)?;
        registry.sync(&house.house, &bundle(name)?)?;
        let consumer = root.join(name);
        let repository = repo(&house)?;
        checkout(&consumer, repository.repository.as_str())?;
        assert!(!registry.bind_repository(&repository)?.has_conflicts());
        let resolved = registry.resolve(&consumer, CommitId::new(&"c".repeat(40))?)?;
        let guidance = fs::read_to_string(&resolved.entrypoint)?;
        if name == "crabnebula" {
            assert!(guidance.contains("Tauri"));
            assert!(!guidance.contains("embedded"));
            for role in kitchen::contracts::Role::ALL {
                assert!(
                    !fs::read_to_string(
                        resolved
                            .snapshot
                            .join(format!("roles/{}.md", role.as_str()))
                    )?
                    .contains("Origin89")
                );
            }
        } else {
            assert!(guidance.contains("bench evidence"));
        }
        assert!(!registry.private_path(&house.house)?.exists());
        assert_eq!(
            fs::read_to_string(resolved.snapshot.join("house/NOTICE.md"))?,
            "Synthetic fixture notice; retained verbatim."
        );
        assert!(
            !house.authority()?.covers(&kitchen::contracts::Grant::house(
                kitchen::contracts::Permission::Merge,
                kitchen::BackendId::new("test-backend")?,
                kitchen::CredentialId::new("test-credential")?,
            ))
        );
    }
    let crab = config("crabnebula")?;
    assert!(matches!(
        registry.sync(&crab.house, &bundle("origin89")?),
        Err(HouseError::PinMismatch)
    ));
    let origin = config("origin89")?;
    assert_ne!(
        registry.private_path(&crab.house)?,
        registry.private_path(&origin.house)?
    );
    Ok(())
}
#[test]
fn missing_ambiguous_cross_house_and_relaxation_are_rejected() -> TestResult {
    let house = config("origin89")?;
    let repository = repo(&house)?;
    assert!(matches!(
        resolve_house(&repository, &[]),
        Err(HouseError::HouseSelection)
    ));
    assert!(matches!(
        resolve_house(&repository, &[house.clone(), house.clone()]),
        Err(HouseError::HouseSelection)
    ));
    assert!(repository.validate(&config("crabnebula")?).is_err());
    let mut tightened = repository.clone();
    tightened.additional_checks.insert("more-evidence".into());
    assert!(
        tightened
            .checks(&house)?
            .is_superset(&house.required_checks)
    );
    for forbidden in [
        "credentials",
        "token",
        "grants",
        "requiredChecks",
        "privateContext",
    ] {
        let mut raw = serde_json::to_value(&repository)?;
        raw[forbidden] = serde_json::json!("DO-NOT-ECHO");
        assert!(serde_json::from_value::<RepositoryConfig>(raw).is_err());
    }
    let mut invalid = house.clone();
    invalid.posting_destinations.insert("other/repo".parse()?);
    assert!(invalid.validate().is_err());
    Ok(())
}
#[test]
fn update_preserves_old_task_pins_and_failure_preserves_current() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = HouseRegistry::new(temp.path().canonicalize()?.join("registry"))?;
    let old = config("origin89")?;
    registry.initialize(&old)?;
    let original = bundle("origin89")?;
    let active = registry.sync(&old.house, &original)?;
    let mut next = original.clone();
    next.guidance = CommitId::new(&"d".repeat(40))?;
    next.assets[0].contents = "New guidance".into();
    assert!(matches!(
        registry.sync(&old.house, &next),
        Err(HouseError::PinMismatch)
    ));
    let updated = registry.update(&old, &next)?;
    assert_ne!(updated.snapshot, active.snapshot);
    active.verify(&registry)?;
    let mut redirected_task = active.clone();
    redirected_task.snapshot = updated.snapshot.clone();
    assert!(matches!(
        redirected_task.verify(&registry),
        Err(HouseError::UnverifiedSnapshot)
    ));
    assert_eq!(
        fs::read_to_string(&active.entrypoint)?,
        original.assets[0].contents
    );
    assert_eq!(
        resolve_instructions(registry.root(), &old, None)?.snapshot,
        active.snapshot
    );
    assert!(matches!(
        registry.update(&old, &next),
        Err(HouseError::Conflict)
    ));
    let current = registry.load(&old.house)?;
    let mut corrupt = next.clone();
    corrupt.guidance = CommitId::new(&"e".repeat(40))?;
    corrupt.assets.clear();
    assert!(registry.update(&current, &corrupt).is_err());
    assert_eq!(registry.load(&old.house)?, current);
    assert_eq!(registry.sync(&old.house, &next)?.snapshot, updated.snapshot);
    fs::write(&updated.entrypoint, "local modification")?;
    assert!(matches!(
        resolve_instructions(registry.root(), &current, None),
        Err(HouseError::UnverifiedSnapshot)
    ));
    assert!(registry.sync(&old.house, &next).is_err());
    assert_eq!(
        fs::read_to_string(&updated.entrypoint)?,
        "local modification"
    );
    Ok(())
}
#[test]
fn failed_activation_and_partial_snapshot_do_not_replace_verified_pins() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = HouseRegistry::new(temp.path().canonicalize()?.join("registry"))?;
    let old = config("origin89")?;
    registry.initialize(&old)?;
    let original = bundle("origin89")?;
    let active = registry.sync(&old.house, &original)?;
    fs::write(
        registry.root().join("houses/origin89.pending"),
        "interrupted operation",
    )?;
    let mut next = original.clone();
    next.guidance = CommitId::new(&"d".repeat(40))?;
    assert!(matches!(
        registry.update(&old, &next),
        Err(HouseError::Conflict)
    ));
    assert_eq!(registry.load(&old.house)?, old);
    assert!(active.entrypoint.is_file());
    assert_eq!(
        fs::read_to_string(registry.root().join("houses/origin89.pending"))?,
        "interrupted operation"
    );
    fs::remove_file(active.snapshot.join("manifest.json"))?;
    assert!(matches!(
        resolve_instructions(registry.root(), &old, None),
        Err(HouseError::UnverifiedSnapshot)
    ));
    // Create-only retry can complete an interrupted snapshot without overwrites.
    assert_eq!(
        registry.sync(&old.house, &original)?.snapshot,
        active.snapshot
    );
    Ok(())
}
#[test]
fn labels_preview_missing_conflicting_present_and_disabled() -> TestResult {
    let declarations = [
        workflow_requirements(Workflow::Pickup),
        workflow_requirements(Workflow::Gate),
    ];
    let missing = preview_labels(&declarations, Some(&[]))?;
    assert_eq!(missing.len(), 5);
    assert!(
        missing
            .iter()
            .all(|item| item.status == LabelStatus::Missing)
    );
    assert!(
        missing
            .iter()
            .any(|item| item.requirement.name == "human-only" && item.workflows.len() == 2)
    );
    let observed: Vec<_> = missing
        .iter()
        .map(|item| RepositoryLabel {
            name: item.requirement.name.clone(),
            color: item.requirement.color.clone(),
            description: item.requirement.description.clone(),
        })
        .collect();
    assert!(
        preview_labels(&declarations, Some(&observed))?
            .iter()
            .all(|item| item.status == LabelStatus::Present)
    );
    let mut conflict = observed.clone();
    conflict[0].name = conflict[0].name.to_uppercase();
    assert!(
        preview_labels(&declarations, Some(&conflict))?
            .iter()
            .any(|item| item.status == LabelStatus::Conflict)
    );
    assert_eq!(conflict[0].name, observed[0].name.to_uppercase());
    let mut duplicates = observed.clone();
    duplicates.push(observed[0].clone());
    assert!(
        preview_labels(&declarations, Some(&duplicates))?
            .iter()
            .any(|item| item.status == LabelStatus::Conflict)
    );
    assert!(preview_labels(&[], Some(&conflict))?.is_empty());
    assert!(
        preview_labels(&declarations, None)?
            .iter()
            .all(|item| item.status == LabelStatus::Unobserved)
    );
    let mut contradictory = declarations.to_vec();
    contradictory[1]
        .labels
        .push(declarations[0].labels[0].clone());
    contradictory[1].labels.last_mut().ok_or("missing")?.color = "ffffff".into();
    assert!(
        preview_labels(&contradictory, Some(&observed))?
            .iter()
            .any(|item| item.status == LabelStatus::Conflict)
    );
    Ok(())
}
#[test]
fn doctor_unknown_is_not_success_and_scoped_evidence_can_complete_it() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = HouseRegistry::new(temp.path().canonicalize()?.join("registry"))?;
    let house = config("origin89")?;
    let repository = repo(&house)?;
    registry.initialize(&house)?;
    let unknown = doctor(&registry, &repository, None)?;
    assert!(!unknown.healthy());
    for code in [
        DoctorCode::Instructions,
        DoctorCode::Labels,
        DoctorCode::Capability,
        DoctorCode::Access,
    ] {
        assert!(
            unknown
                .findings
                .iter()
                .any(|finding| finding.code == code && !finding.next_step.is_empty())
        );
    }
    assert!(unknown.human_readable().contains("kitchn house sync"));
    registry.sync(&house.house, &bundle("origin89")?)?;
    let labels = unknown
        .labels
        .into_iter()
        .map(|item| RepositoryLabel {
            name: item.requirement.name,
            color: item.requirement.color,
            description: item.requirement.description,
        })
        .collect();
    let mut evidence = DoctorEvidence {
        house: house.house.clone(),
        repository: repository.repository.clone(),
        capabilities: CapabilitySet::supporting(Capability::ALL),
        labels: Some(labels),
        access: AccessStatus::Available,
        agent_models: None,
        stack_tool: None,
        schedules: None,
        readiness: None,
        undelivered_budget_reports: Vec::new(),
        store_capacity: None,
    };
    assert!(doctor(&registry, &repository, Some(&evidence))?.healthy());
    evidence.house = config("crabnebula")?.house;
    assert!(matches!(
        doctor(&registry, &repository, Some(&evidence)),
        Err(HouseError::HouseSelection)
    ));
    Ok(())
}
#[test]
fn registries_inside_a_checkout_are_refused() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    fs::create_dir(root.join(".git"))?;
    assert!(matches!(
        HouseRegistry::new(root.join("private")),
        Err(HouseError::InsideRepository)
    ));
    Ok(())
}

#[test]
fn explicit_workflow_change_preserves_strengthening_and_detects_stale_setup() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = HouseRegistry::new(root.join("registry"))?;
    let house = config("origin89")?;
    registry.initialize(&house)?;
    let mut original = repo(&house)?;
    original.additional_checks.insert("local-check".into());
    registry.bind_repository(&original)?;
    let mut next = original.clone();
    next.workflows.clear();
    registry.configure_repository(&original, &next)?;
    assert_eq!(registry.binding(&original.repository)?, Some(next.clone()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(
                registry
                    .root()
                    .join("repositories/origin89hq/firmware.json")
            )?
            .permissions()
            .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(registry.root().join("houses/origin89.json"))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert!(next.checks(&house)?.contains("local-check"));
    assert!(doctor(&registry, &next, None)?.labels.is_empty());
    assert!(matches!(
        registry.configure_repository(&original, &next),
        Err(HouseError::Conflict)
    ));
    Ok(())
}

#[test]
fn interactive_policy_limits_never_become_standing_grants() -> TestResult {
    use kitchen::contracts::{Grant, GrantScope, Permission};
    let mut house = config("crabnebula")?;
    let destination = kitchen::BackendId::new("forge")?;
    let credential = kitchen::CredentialId::new("crabnebula-forge")?;
    let grant = Grant::repository(
        Permission::PostComment,
        repo(&house)?.repository,
        destination.clone(),
        credential.clone(),
    );
    house.policy_limits.insert(grant.clone());
    let authority = house.authority()?;
    assert!(!authority.covers(&grant));
    assert_eq!(
        authority.permitted(Permission::PostComment, &grant.scope, &destination)?,
        credential
    );
    house
        .grants
        .insert(Grant::house(Permission::Merge, destination, credential));
    assert!(matches!(
        house.authority(),
        Err(HouseError::PolicyRelaxation)
    ));
    assert!(matches!(grant.scope, GrantScope::Repository(_)));
    Ok(())
}

#[test]
fn missing_role_or_guidance_file_is_an_unverified_snapshot() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = HouseRegistry::new(temp.path().canonicalize()?.join("registry"))?;
    let house = config("origin89")?;
    let bundle = bundle("origin89")?;
    registry.initialize(&house)?;
    let resolved = registry.sync(&house.house, &bundle)?;
    for path in [
        resolved.snapshot.join("roles/commis.md"),
        resolved.entrypoint.clone(),
    ] {
        fs::remove_file(&path)?;
        assert!(matches!(
            resolve_instructions(registry.root(), &house, None),
            Err(HouseError::UnverifiedSnapshot)
        ));
        registry.sync(&house.house, &bundle)?;
        assert!(path.is_file());
    }
    Ok(())
}

#[test]
fn posting_grants_cannot_bypass_repository_and_destination_allowlists() -> TestResult {
    use kitchen::contracts::{Grant, Permission};
    let mut house = config("crabnebula")?;
    let destination = kitchen::BackendId::new("forge")?;
    let credential = kitchen::CredentialId::new("crabnebula-forge")?;
    for permission in [
        Permission::PostComment,
        Permission::EditLabels,
        Permission::CreateIssue,
        Permission::EditIssueRelationships,
        Permission::Merge,
    ] {
        house.policy_limits = BTreeSet::from([Grant::house(
            permission,
            destination.clone(),
            credential.clone(),
        )]);
        assert!(matches!(
            house.validate(),
            Err(HouseError::PolicyRelaxation)
        ));
        house.policy_limits = BTreeSet::from([Grant::repository(
            permission,
            repo(&house)?.repository,
            destination.clone(),
            credential.clone(),
        )]);
        house.validate()?;
        let saved = house.posting_destinations.clone();
        house.posting_destinations.clear();
        assert!(matches!(
            house.validate(),
            Err(HouseError::PolicyRelaxation)
        ));
        house.posting_destinations = saved;
    }
    Ok(())
}

#[test]
fn pending_and_stray_files_do_not_block_other_houses() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = HouseRegistry::new(root.join("registry"))?;
    let house = config("crabnebula")?;
    registry.initialize(&house)?;
    registry.sync(&house.house, &bundle("crabnebula")?)?;
    let consumer = root.join("consumer");
    checkout(&consumer, "crabnebula/tauri-fixture")?;
    registry.bind_repository(&repo(&house)?)?;
    for stray in ["origin89.pending", ".DS_Store", "broken.json"] {
        fs::write(registry.root().join("houses").join(stray), b"interrupted")?;
        assert_eq!(
            registry
                .resolve(&consumer, CommitId::new(&"c".repeat(40))?)?
                .house,
            house.house
        );
        assert_eq!(registry.houses()?.available, vec![house.clone()]);
    }
    assert!(registry.load(&kitchen::HouseId::new("broken")?).is_err());
    let listing = registry.houses()?;
    assert_eq!(listing.unavailable.len(), 1);
    assert_eq!(listing.unavailable[0].0.as_str(), "broken");
    assert!(matches!(listing.unavailable[0].1, HouseError::InvalidInput));
    Ok(())
}

#[test]
fn mismatched_role_digest_is_refused_before_snapshot_or_pin_write() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = HouseRegistry::new(temp.path().canonicalize()?.join("registry"))?;
    let house = config("origin89")?;
    registry.initialize(&house)?;
    let mut wrong = bundle("origin89")?;
    wrong.kitchen = CommitId::new(&"f".repeat(40))?;
    wrong.role_cards_digest = kitchen::adoption::RoleCardsDigest::new(&"0".repeat(64))?;
    assert!(matches!(
        registry.update(&house, &wrong),
        Err(HouseError::PinMismatch)
    ));
    assert_eq!(registry.load(&house.house)?, house);
    assert!(!registry.root().join("snapshots").exists());
    Ok(())
}

#[test]
fn label_metadata_drift_is_informational_in_preview_and_doctor() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = HouseRegistry::new(temp.path().canonicalize()?.join("registry"))?;
    let house = config("origin89")?;
    registry.initialize(&house)?;
    registry.sync(&house.house, &bundle("origin89")?)?;
    let repository = repo(&house)?;
    let declarations: Vec<_> = repository
        .workflows
        .iter()
        .copied()
        .map(workflow_requirements)
        .collect();
    let labels: Vec<_> = preview_labels(&declarations, Some(&[]))?
        .into_iter()
        .map(|item| RepositoryLabel {
            name: item.requirement.name,
            color: "ffffff".into(),
            description: "local meaning".into(),
        })
        .collect();
    let previews = preview_labels(&declarations, Some(&labels))?;
    assert!(previews.iter().all(|p| p.status == LabelStatus::Drift));
    let evidence = DoctorEvidence {
        house: house.house,
        repository: repository.repository.clone(),
        capabilities: CapabilitySet::supporting(Capability::ALL),
        labels: Some(labels.clone()),
        access: AccessStatus::Available,
        agent_models: None,
        stack_tool: None,
        schedules: None,
        readiness: None,
        undelivered_budget_reports: Vec::new(),
        store_capacity: None,
    };
    let report = doctor(&registry, &repository, Some(&evidence))?;
    assert!(report.healthy());
    assert!(report.human_readable().contains("drift"));
    assert_eq!(evidence.labels, Some(labels));
    Ok(())
}

#[test]
fn doctor_reports_a_store_table_near_its_limit() -> TestResult {
    use kitchen::state::{MAX_MARKERS, MAX_TASKS, StoreCapacity, TableUsage};
    let temp = tempfile::tempdir()?;
    let registry = HouseRegistry::new(temp.path().canonicalize()?.join("registry"))?;
    let house = config("origin89")?;
    registry.initialize(&house)?;
    registry.sync(&house.house, &bundle("origin89")?)?;
    let repository = repo(&house)?;
    let gate = kitchen::WorkflowId::new("merge-gate")?;
    let capacity = |markers: usize| StoreCapacity {
        tasks: TableUsage {
            used: 10,
            limit: MAX_TASKS,
        },
        markers: TableUsage {
            used: markers,
            limit: MAX_MARKERS,
        },
        consumers: TableUsage {
            used: 0,
            limit: 256,
        },
        markers_by_workflow: [(gate.clone(), markers)].into_iter().collect(),
        settled_tasks: 4,
    };
    let findings = |store_capacity: Option<StoreCapacity>| -> Result<Vec<kitchen::house::DoctorFinding>, Box<dyn std::error::Error>> {
        let mut evidence =
            DoctorEvidence::unobserved(house.house.clone(), repository.repository.clone());
        evidence.store_capacity = store_capacity;
        Ok(doctor(&registry, &repository, Some(&evidence))?
            .findings
            .into_iter()
            .filter(|finding| finding.code == DoctorCode::StoreCapacity)
            .collect())
    };
    // Unread, or below the threshold: nothing to report.
    assert!(findings(None)?.is_empty());
    assert!(findings(Some(capacity(MAX_MARKERS * 79 / 100)))?.is_empty());
    // At the threshold, before the limit, one finding names the table.
    let near = findings(Some(capacity(MAX_MARKERS * 80 / 100 + 1)))?;
    assert_eq!(near.len(), 1);
    assert!(near[0].message.contains("workflow markers"));
    assert!(near[0].message.contains("merge-gate"));
    assert!(near[0].next_step.contains("kitchn store retain"));
    Ok(())
}

#[test]
fn role_digest_rejects_invalid_input_and_changed_manifest_role_bytes() -> TestResult {
    use kitchen::adoption::{RoleCardsDigest, role_cards_digest};
    for invalid in ["", "1234", &"x".repeat(64), &"0".repeat(65)] {
        assert!(matches!(
            RoleCardsDigest::new(invalid),
            Err(HouseError::InvalidInput)
        ));
    }
    assert_eq!(
        RoleCardsDigest::new(&role_cards_digest().as_str().to_uppercase())?,
        role_cards_digest()
    );
    let bundle = bundle("origin89")?;
    assert_eq!(bundle.role_cards_digest, role_cards_digest()); // Independently generated fixture digest.
    let mut raw = serde_json::to_value(&bundle)?;
    raw.as_object_mut()
        .ok_or("not an object")?
        .remove("roleCardsDigest");
    assert!(serde_json::from_value::<InstructionBundle>(raw).is_err());
    let temp = tempfile::tempdir()?;
    let registry = HouseRegistry::new(temp.path().canonicalize()?.join("registry"))?;
    let house = config("origin89")?;
    registry.initialize(&house)?;
    let resolved = registry.sync(&house.house, &bundle)?;
    assert_eq!(resolved.role_cards_digest, bundle.role_cards_digest);
    let mut wrong_reference = resolved.clone();
    wrong_reference.role_cards_digest = RoleCardsDigest::new(&"0".repeat(64))?;
    assert!(matches!(
        wrong_reference.verify(&registry),
        Err(HouseError::UnverifiedSnapshot)
    ));
    let mut wrong = bundle.clone();
    wrong.role_cards_digest = RoleCardsDigest::new(&"0".repeat(64))?;
    assert!(matches!(
        registry.sync(&house.house, &wrong),
        Err(HouseError::PinMismatch)
    ));
    let path = resolved.snapshot.join("manifest.json");
    let mut manifest: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    let role_path = manifest["roles"][0]["path"]
        .as_str()
        .ok_or("missing path")?
        .to_owned();
    manifest["roles"][0]["contents"] = serde_json::json!("altered but self-consistent bytes");
    fs::write(
        resolved.snapshot.join(role_path),
        "altered but self-consistent bytes",
    )?;
    fs::write(path, serde_json::to_vec(&manifest)?)?;
    assert!(matches!(
        resolved.verify(&registry),
        Err(HouseError::UnverifiedSnapshot)
    ));
    Ok(())
}

#[test]
fn repository_lookup_rejects_relative_input() -> TestResult {
    assert!(matches!(
        checkout_remotes(Path::new(".")),
        Err(HouseError::InvalidInput)
    ));
    Ok(())
}

#[test]
fn doctor_reports_a_configured_stack_tool_that_is_missing() -> TestResult {
    use kitchen::house::{StackTool, StackToolStatus};
    let temp = tempfile::tempdir()?;
    let registry = HouseRegistry::new(temp.path().canonicalize()?.join("registry"))?;
    let mut value: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/house/origin89.json"))?;
    value["stackTool"] = serde_json::json!("gh-stack");
    let house: HouseConfig = serde_json::from_value(value)?;
    assert_eq!(house.stack_tool, Some(StackTool::GhStack));
    // Houses written before the field keep working without a stack tool.
    assert_eq!(config("origin89")?.stack_tool, None);
    let repository = repo(&house)?;
    registry.initialize(&house)?;
    registry.sync(&house.house, &bundle("origin89")?)?;
    let labels: Vec<RepositoryLabel> = doctor(&registry, &repository, None)?
        .labels
        .into_iter()
        .map(|item| RepositoryLabel {
            name: item.requirement.name,
            color: item.requirement.color,
            description: item.requirement.description,
        })
        .collect();
    let mut evidence = DoctorEvidence {
        house: house.house.clone(),
        repository: repository.repository.clone(),
        capabilities: CapabilitySet::supporting(Capability::ALL),
        labels: Some(labels),
        access: AccessStatus::Available,
        agent_models: None,
        stack_tool: Some(StackToolStatus::Missing),
        schedules: None,
        readiness: None,
        undelivered_budget_reports: Vec::new(),
        store_capacity: None,
    };
    let report = doctor(&registry, &repository, Some(&evidence))?;
    assert!(!report.healthy());
    assert_eq!(
        report
            .findings
            .iter()
            .map(|finding| finding.code)
            .collect::<Vec<_>>(),
        vec![DoctorCode::StackTool]
    );
    assert!(report.human_readable().contains("gh stack"));
    evidence.stack_tool = Some(StackToolStatus::Installed {
        version: "0.1.0".into(),
    });
    assert!(doctor(&registry, &repository, Some(&evidence))?.healthy());
    Ok(())
}

#[test]
fn house_policy_may_set_a_disk_pressure_threshold() -> TestResult {
    let mut json: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/house/origin89.json"))?;
    // Absent by default: free space is not watched.
    assert_eq!(config("origin89")?.disk_pressure, None);
    json["diskPressure"] = serde_json::json!({ "minFreeBytes": 21_474_836_480_u64 });
    let house: HouseConfig = serde_json::from_value(json.clone())?;
    house.validate()?;
    assert_eq!(
        house
            .disk_pressure
            .map(|policy| policy.min_free_bytes.get()),
        Some(21_474_836_480)
    );
    // A zero threshold or an unknown key is refused.
    for invalid in [
        serde_json::json!({ "minFreeBytes": 0 }),
        serde_json::json!({ "minFreeBytes": 1, "path": "/tmp" }),
    ] {
        json["diskPressure"] = invalid;
        assert!(serde_json::from_value::<HouseConfig>(json.clone()).is_err());
    }
    Ok(())
}
