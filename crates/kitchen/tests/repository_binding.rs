//! Registry-held repository bindings resolved from real Git checkouts. Kitchen
//! must leave every working tree exactly as it found it.
use kitchen::{
    HouseId,
    adoption::{
        HouseRegistry, LEGACY_REPOSITORY_CONFIG, LegacyImportStatus, RepositoryMatch,
        legacy_binding,
    },
    contracts::{CommitId, Repository},
    house::{HouseConfig, HouseError, RepositoryConfig, Workflow},
};
use std::{collections::BTreeSet, fs, path::Path, process::Command};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn crabnebula() -> TestResult<HouseConfig> {
    Ok(serde_json::from_str(include_str!(
        "fixtures/house/crabnebula.json"
    ))?)
}
/// A second house that also allows the crabnebula fixture repository.
fn origin89_claiming_crabnebula() -> TestResult<HouseConfig> {
    let mut house: HouseConfig =
        serde_json::from_str(include_str!("fixtures/house/origin89.json"))?;
    house
        .repositories
        .insert("crabnebula/tauri-fixture".parse()?);
    Ok(house)
}
fn binding(house: &HouseConfig, repository: &str) -> TestResult<RepositoryConfig> {
    Ok(RepositoryConfig {
        schema: 2,
        house: house.house.clone(),
        repository: repository.parse()?,
        workflows: BTreeSet::from([Workflow::Pickup]),
        additional_reviewers: BTreeSet::new(),
        additional_checks: BTreeSet::from(["local-check".to_owned()]),
    })
}
fn git(path: &Path, args: &[&str]) -> TestResult<String> {
    // Fixture setup ignores the person's Git configuration, such as signing.
    let output = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "user.name=Kitchen Test",
            "-c",
            "user.email=test@example.com",
        ])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(format!("git {args:?}: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}
/// A checkout with one commit and the given remotes.
fn checkout(path: &Path, remotes: &[(&str, &str)]) -> TestResult {
    fs::create_dir_all(path)?;
    git(path, &["init", "--quiet"])?;
    git(path, &["commit", "--quiet", "--allow-empty", "-m", "init"])?;
    for (name, url) in remotes {
        git(path, &["remote", "add", name, url])?;
    }
    Ok(())
}
/// Every path Git sees in the working tree, including ignored ones.
fn tree_status(path: &Path) -> TestResult<String> {
    git(
        path,
        &[
            "status",
            "--porcelain",
            "--ignored",
            "--untracked-files=all",
        ],
    )
}
struct Fixture {
    _temp: tempfile::TempDir,
    root: std::path::PathBuf,
    registry: HouseRegistry,
}
fn fixture(houses: &[HouseConfig]) -> TestResult<Fixture> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = HouseRegistry::new(root.join("registry"))?;
    for house in houses {
        registry.initialize(house)?;
    }
    Ok(Fixture {
        _temp: temp,
        root,
        registry,
    })
}

#[test]
fn binding_and_resolution_leave_the_working_tree_unchanged() -> TestResult {
    let house = crabnebula()?;
    let f = fixture(std::slice::from_ref(&house))?;
    let consumer = f.root.join("consumer");
    checkout(
        &consumer,
        &[("origin", "git@github.com:crabnebula/tauri-fixture.git")],
    )?;
    let before = tree_status(&consumer)?;
    assert_eq!(before, "");
    assert_eq!(
        f.registry.resolve_repository(&consumer)?,
        RepositoryMatch::Unbound {
            repository: "crabnebula/tauri-fixture".parse()?,
            house: house.house.clone(),
        }
    );
    let config = binding(&house, "crabnebula/tauri-fixture")?;
    f.registry.bind_repository(&config)?;
    // An identical rerun is accepted; a different binding is not overwritten.
    f.registry.bind_repository(&config)?;
    let mut other = config.clone();
    other.workflows.clear();
    let refused = f.registry.bind_repository(&other);
    assert!(
        matches!(refused, Err(HouseError::Conflicts(_))),
        "{refused:?}"
    );
    assert_eq!(
        f.registry.resolve_repository(&consumer)?,
        RepositoryMatch::Bound(config.clone())
    );
    f.registry.configure_repository(&config, &other)?;
    assert_eq!(
        f.registry.resolve_repository(&consumer)?,
        RepositoryMatch::Bound(other)
    );
    assert_eq!(tree_status(&consumer)?, before);
    assert!(!consumer.join(LEGACY_REPOSITORY_CONFIG).exists());
    Ok(())
}

#[test]
fn subdirectories_and_other_worktrees_resolve_the_same_binding() -> TestResult {
    let house = crabnebula()?;
    let f = fixture(std::slice::from_ref(&house))?;
    let main = f.root.join("main");
    checkout(
        &main,
        &[("origin", "https://github.com/crabnebula/tauri-fixture.git")],
    )?;
    fs::create_dir_all(main.join("src/deep"))?;
    let second = f.root.join("second");
    git(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "other",
            second.to_str().ok_or("path")?,
        ],
    )?;
    fs::create_dir_all(second.join("nested"))?;
    let config = binding(&house, "crabnebula/tauri-fixture")?;
    f.registry.bind_repository(&config)?;
    for start in [
        main.clone(),
        main.join("src/deep"),
        second.clone(),
        second.join("nested"),
    ] {
        assert_eq!(
            f.registry.resolve_repository(&start)?,
            RepositoryMatch::Bound(config.clone()),
            "{}",
            start.display()
        );
    }
    // A nested checkout with its own remote never inherits the parent's binding.
    let nested = main.join("vendor/other");
    checkout(
        &nested,
        &[("origin", "https://github.com/someone/else.git")],
    )?;
    assert!(matches!(
        f.registry.resolve_repository(&nested),
        Err(HouseError::HouseSelection)
    ));
    Ok(())
}

#[test]
fn two_claiming_houses_fail_closed_until_a_choice_is_stored() -> TestResult {
    let crab = crabnebula()?;
    let origin = origin89_claiming_crabnebula()?;
    let f = fixture(&[crab.clone(), origin.clone()])?;
    let consumer = f.root.join("consumer");
    checkout(
        &consumer,
        &[("origin", "https://github.com/crabnebula/tauri-fixture")],
    )?;
    match f.registry.resolve_repository(&consumer) {
        Err(HouseError::AmbiguousHouse { houses }) => {
            assert_eq!(houses, vec![crab.house.clone(), origin.house.clone()]);
        }
        other => return Err(format!("expected an ambiguous house, got {other:?}").into()),
    }
    assert!(
        f.registry
            .resolve(&consumer, CommitId::new(&"c".repeat(40))?)
            .is_err()
    );
    // The person's choice lives in the registry and settles resolution.
    let chosen = binding(&origin, "crabnebula/tauri-fixture")?;
    f.registry.bind_repository(&chosen)?;
    assert_eq!(
        f.registry.resolve_repository(&consumer)?,
        RepositoryMatch::Bound(chosen)
    );
    assert_eq!(tree_status(&consumer)?, "");
    Ok(())
}

#[test]
fn unclaimed_unidentified_and_damaged_cases_are_refused() -> TestResult {
    let house = crabnebula()?;
    let f = fixture(std::slice::from_ref(&house))?;
    let unclaimed = f.root.join("unclaimed");
    checkout(&unclaimed, &[("origin", "https://github.com/someone/else")])?;
    assert!(matches!(
        f.registry.resolve_repository(&unclaimed),
        Err(HouseError::HouseSelection)
    ));
    let no_remote = f.root.join("no-remote");
    checkout(&no_remote, &[])?;
    assert!(matches!(
        f.registry.resolve_repository(&no_remote),
        Err(HouseError::RepositoryUnidentified)
    ));
    let elsewhere = f.root.join("elsewhere");
    checkout(
        &elsewhere,
        &[
            ("origin", "git@gitlab.com:crabnebula/tauri-fixture.git"),
            ("mirror", "/srv/git/tauri-fixture.git"),
        ],
    )?;
    assert!(matches!(
        f.registry.resolve_repository(&elsewhere),
        Err(HouseError::RepositoryUnidentified)
    ));
    let plain = f.root.join("plain");
    fs::create_dir(&plain)?;
    assert!(matches!(
        f.registry.resolve_repository(&plain),
        Err(HouseError::RepositoryUnidentified)
    ));
    // An unreadable house might also claim an unbound repository.
    let claimed = f.root.join("claimed");
    checkout(
        &claimed,
        &[("origin", "https://github.com/crabnebula/tauri-fixture")],
    )?;
    fs::write(f.registry.root().join("houses/broken.json"), "damaged")?;
    assert!(matches!(
        f.registry.resolve_repository(&claimed),
        Err(HouseError::HouseSelection)
    ));
    // A stored choice still resolves while another house is damaged.
    let config = binding(&house, "crabnebula/tauri-fixture")?;
    f.registry.bind_repository(&config)?;
    assert_eq!(
        f.registry.resolve_repository(&claimed)?,
        RepositoryMatch::Bound(config)
    );
    Ok(())
}

#[test]
fn rewritten_and_differently_cased_remotes_match_the_allowlist() -> TestResult {
    let house = crabnebula()?;
    let f = fixture(std::slice::from_ref(&house))?;
    let consumer = f.root.join("consumer");
    checkout(&consumer, &[("origin", "gh:CrabNebula/Tauri-Fixture")])?;
    git(
        &consumer,
        &["config", "url.git@github.com:.insteadOf", "gh:"],
    )?;
    assert_eq!(
        f.registry.resolve_repository(&consumer)?,
        RepositoryMatch::Unbound {
            repository: "crabnebula/tauri-fixture".parse()?,
            house: house.house.clone(),
        }
    );
    Ok(())
}

#[test]
fn forks_resolve_the_claimed_remote_and_refuse_two_bindings() -> TestResult {
    let house = crabnebula()?;
    let f = fixture(std::slice::from_ref(&house))?;
    let fork = f.root.join("fork");
    checkout(
        &fork,
        &[
            ("origin", "git@github.com:someone/tauri-fixture.git"),
            (
                "upstream",
                "https://github.com/crabnebula/tauri-fixture.git",
            ),
        ],
    )?;
    let claims = f.registry.claims(&fork)?;
    assert_eq!(
        claims.setup_target()?,
        ("crabnebula/tauri-fixture".parse::<Repository>()?, None)
    );
    let mut house = house;
    house.repositories.insert("someone/tauri-fixture".parse()?);
    fs::write(
        f.registry.root().join("houses/crabnebula.json"),
        serde_json::to_vec(&house)?,
    )?;
    assert!(matches!(
        f.registry.resolve_repository(&fork),
        Err(HouseError::AmbiguousRepository { .. })
    ));
    assert!(matches!(
        f.registry.claims(&fork)?.setup_target(),
        Err(HouseError::AmbiguousRepository { .. })
    ));
    f.registry
        .bind_repository(&binding(&house, "crabnebula/tauri-fixture")?)?;
    assert!(matches!(
        f.registry.resolve_repository(&fork)?,
        RepositoryMatch::Bound(config) if config.repository.as_str() == "crabnebula/tauri-fixture"
    ));
    f.registry
        .bind_repository(&binding(&house, "someone/tauri-fixture")?)?;
    assert!(matches!(
        f.registry.resolve_repository(&fork),
        Err(HouseError::AmbiguousRepository { repositories }) if repositories.len() == 2
    ));
    Ok(())
}

#[test]
fn legacy_file_is_imported_only_explicitly_and_never_touched() -> TestResult {
    let house = crabnebula()?;
    let f = fixture(std::slice::from_ref(&house))?;
    let consumer = f.root.join("consumer");
    checkout(
        &consumer,
        &[("origin", "https://github.com/crabnebula/tauri-fixture.git")],
    )?;
    fs::create_dir(consumer.join("src"))?;
    let mut legacy = serde_json::to_value(binding(&house, "crabnebula/tauri-fixture")?)?;
    legacy["schema"] = serde_json::json!(1);
    let bytes = serde_json::to_vec(&legacy)?;
    let path = consumer.join(LEGACY_REPOSITORY_CONFIG);
    fs::write(&path, &bytes)?;
    assert_eq!(legacy_binding(&consumer.join("src"))?, Some(path.clone()));
    // Legacy files are never read into a resolution.
    assert!(matches!(
        f.registry.resolve_repository(&consumer)?,
        RepositoryMatch::Unbound { .. }
    ));
    let preview = f.registry.import_legacy(&consumer.join("src"), false)?;
    assert_eq!(preview.status, LegacyImportStatus::WouldCreate);
    assert_eq!(preview.binding.schema, 2);
    assert_eq!(f.registry.binding(&preview.binding.repository)?, None);
    let applied = f.registry.import_legacy(&consumer, true)?;
    assert_eq!(applied.status, LegacyImportStatus::Created);
    assert_eq!(
        f.registry.resolve_repository(&consumer)?,
        RepositoryMatch::Bound(applied.binding.clone())
    );
    assert_eq!(
        f.registry.import_legacy(&consumer, true)?.status,
        LegacyImportStatus::Unchanged
    );
    let mut changed = applied.binding.clone();
    changed.workflows.clear();
    f.registry
        .configure_repository(&applied.binding, &changed)?;
    assert_eq!(
        f.registry.import_legacy(&consumer, true)?.status,
        LegacyImportStatus::Conflict
    );
    assert_eq!(f.registry.binding(&changed.repository)?, Some(changed));
    assert_eq!(fs::read(&path)?, bytes, "the legacy file is left as it was");
    Ok(())
}

#[test]
fn legacy_import_refuses_other_repositories_and_schemas() -> TestResult {
    let house = crabnebula()?;
    let f = fixture(std::slice::from_ref(&house))?;
    let consumer = f.root.join("consumer");
    checkout(
        &consumer,
        &[("origin", "https://github.com/someone/else.git")],
    )?;
    let path = consumer.join(LEGACY_REPOSITORY_CONFIG);
    assert_eq!(legacy_binding(&consumer)?, None);
    assert!(matches!(
        f.registry.import_legacy(&consumer, true),
        Err(HouseError::Io(std::io::ErrorKind::NotFound))
    ));
    let mut legacy = serde_json::to_value(binding(&house, "crabnebula/tauri-fixture")?)?;
    legacy["schema"] = serde_json::json!(1);
    // A file naming a repository the checkout's remotes do not name.
    fs::write(&path, serde_json::to_vec(&legacy)?)?;
    assert!(matches!(
        f.registry.import_legacy(&consumer, true),
        Err(HouseError::HouseSelection)
    ));
    for schema in [0, 2, 3] {
        legacy["schema"] = serde_json::json!(schema);
        fs::write(&path, serde_json::to_vec(&legacy)?)?;
        assert!(matches!(
            f.registry.import_legacy(&consumer, true),
            Err(HouseError::InvalidInput)
        ));
    }
    fs::write(&path, br#"{"schema":1,"token":"SECRET"}"#)?;
    assert!(matches!(
        f.registry.import_legacy(&consumer, true),
        Err(HouseError::InvalidInput)
    ));
    assert!(
        fs::read_dir(f.registry.root())?
            .all(|entry| { entry.is_ok_and(|entry| entry.file_name() != "repositories") })
    );
    Ok(())
}

#[test]
fn stored_bindings_reject_old_schemas_and_foreign_keys() -> TestResult {
    let house = crabnebula()?;
    let f = fixture(std::slice::from_ref(&house))?;
    let repository: Repository = "crabnebula/tauri-fixture".parse()?;
    let stored = f
        .registry
        .root()
        .join("repositories/crabnebula/tauri-fixture.json");
    fs::create_dir_all(stored.parent().ok_or("parent")?)?;
    let mut old = serde_json::to_value(binding(&house, "crabnebula/tauri-fixture")?)?;
    old["schema"] = serde_json::json!(1);
    fs::write(&stored, serde_json::to_vec(&old)?)?;
    assert!(matches!(
        f.registry.binding(&repository),
        Err(HouseError::InvalidInput)
    ));
    // A binding for another repository filed under this key is refused.
    let foreign = binding(&house, "crabnebula/tauri-fixture")?;
    let mut foreign = serde_json::to_value(foreign)?;
    foreign["repository"] = serde_json::json!("someone/else");
    fs::write(&stored, serde_json::to_vec(&foreign)?)?;
    assert!(matches!(
        f.registry.binding(&repository),
        Err(HouseError::InvalidInput)
    ));
    // Repository keys ignore case.
    fs::write(
        &stored,
        serde_json::to_vec(&binding(&house, "crabnebula/tauri-fixture")?)?,
    )?;
    assert!(
        f.registry
            .binding(&"CrabNebula/Tauri-Fixture".parse()?)?
            .is_some()
    );
    // Binding a repository the house does not allow writes nothing.
    assert!(matches!(
        f.registry
            .bind_repository(&binding(&house, "someone/else")?),
        Err(HouseError::HouseSelection)
    ));
    assert!(!f.registry.root().join("repositories/someone").exists());
    let unknown = RepositoryConfig {
        house: HouseId::new("unknown")?,
        ..binding(&house, "crabnebula/tauri-fixture")?
    };
    assert!(f.registry.bind_repository(&unknown).is_err());
    Ok(())
}

#[test]
fn a_held_registry_lock_is_busy_after_a_bounded_wait() -> TestResult {
    let house = crabnebula()?;
    let f = fixture(std::slice::from_ref(&house))?;
    let holder = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(f.registry.root().join("installation.lock"))?;
    holder.try_lock()?;
    let config = binding(&house, "crabnebula/tauri-fixture")?;
    let started = std::time::Instant::now();
    assert!(matches!(
        f.registry.bind_repository(&config),
        Err(HouseError::Busy)
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(f.registry.binding(&config.repository)?, None);
    drop(holder);
    f.registry.bind_repository(&config)?;
    Ok(())
}
