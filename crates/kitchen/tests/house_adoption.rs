//! House isolation, immutable pins, setup diagnoses and recovery using synthetic fixtures.
use kitchen::{
    adoption::{
        HouseRegistry, InstructionBundle, adopt_repository, repository_from_path,
        resolve_instructions,
    },
    contracts::{Capability, CapabilitySet, CommitId},
    house::{
        AccessStatus, DoctorCode, DoctorEvidence, HouseConfig, HouseError, LabelStatus,
        RepositoryConfig, RepositoryLabel, Workflow, doctor, preview_labels, resolve_house,
        workflow_requirements,
    },
};
use std::{collections::BTreeSet, fs};

type TestResult = Result<(), Box<dyn std::error::Error>>;
fn config(name: &str) -> Result<HouseConfig, serde_json::Error> {
    serde_json::from_str(match name {
        "origin89" => include_str!("fixtures/house/origin89.json"),
        _ => include_str!("fixtures/house/crabnebula.json"),
    })
}
fn bundle(name: &str) -> Result<InstructionBundle, serde_json::Error> {
    serde_json::from_str(match name {
        "origin89" => include_str!("fixtures/house/origin89-bundle.json"),
        _ => include_str!("fixtures/house/crabnebula-bundle.json"),
    })
}
fn repo(house: &HouseConfig) -> Result<RepositoryConfig, Box<dyn std::error::Error>> {
    Ok(RepositoryConfig {
        schema: 1,
        house: house.house.clone(),
        repository: house.repositories.first().ok_or("empty fixture")?.clone(),
        workflows: BTreeSet::from([Workflow::Pickup, Workflow::Gate]),
        additional_reviewers: BTreeSet::new(),
        additional_checks: BTreeSet::new(),
    })
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
        fs::create_dir(&consumer)?;
        fs::create_dir(consumer.join(".git"))?;
        let repository = repo(&house)?;
        assert!(!adopt_repository(&consumer, &repository, &house)?.has_conflicts());
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
    conflict[0].color = "ffffff".into();
    assert!(
        preview_labels(&declarations, Some(&conflict))?
            .iter()
            .any(|item| item.status == LabelStatus::Conflict)
    );
    assert_eq!(conflict[0].color, "ffffff");
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
    assert!(unknown.human_readable().contains("kitchen house sync"));
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
fn repository_resolution_stops_at_nested_git_and_rejects_ambiguity() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    fs::create_dir(root.join(".git"))?;
    let house = config("origin89")?;
    let repository = repo(&house)?;
    adopt_repository(&root, &repository, &house)?;
    fs::create_dir(root.join("src"))?;
    assert_eq!(repository_from_path(&root.join("src"))?.1, repository);
    adopt_repository(&root.join("src"), &repository, &house)?;
    assert!(matches!(
        repository_from_path(&root.join("src")),
        Err(HouseError::HouseSelection)
    ));
    fs::create_dir(root.join("nested"))?;
    fs::create_dir(root.join("nested/.git"))?;
    assert!(matches!(
        repository_from_path(&root.join("nested")),
        Err(HouseError::HouseSelection)
    ));
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
    let consumer = root.join("consumer");
    adopt_repository(&consumer, &original, &house)?;
    let mut next = original.clone();
    next.workflows.clear();
    registry.configure_repository(&consumer, &original, &next)?;
    assert_eq!(kitchen::adoption::read_repository(&consumer)?, next);
    assert!(next.checks(&house)?.contains("local-check"));
    assert!(doctor(&registry, &next, None)?.labels.is_empty());
    assert!(matches!(
        registry.configure_repository(&consumer, &original, &next),
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
