//! Registry-held repository bindings resolved from real Git checkouts. Kitchen
//! must leave every working tree exactly as it found it.
use kitchen::{
    HouseId,
    adoption::{
        BindingDigest, HouseRegistry, LEGACY_REPOSITORY_CONFIG, LegacyImportStatus, RemoteName,
        RepositoryMatch, legacy_binding,
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

/// Make the current branch track `remote`, without needing that remote fetched.
fn track(path: &Path, remote: &str) -> TestResult {
    let branch = git(path, &["symbolic-ref", "--short", "HEAD"])?;
    let branch = branch.trim();
    git(
        path,
        &["config", &format!("branch.{branch}.remote"), remote],
    )?;
    git(
        path,
        &[
            "config",
            &format!("branch.{branch}.merge"),
            &format!("refs/heads/{branch}"),
        ],
    )?;
    Ok(())
}
/// A house that allows only the fork repository used by the fork tests.
fn origin89_claiming_fork() -> TestResult<HouseConfig> {
    let mut house: HouseConfig =
        serde_json::from_str(include_str!("fixtures/house/origin89.json"))?;
    house.repositories.insert("someone/tauri-fixture".parse()?);
    Ok(house)
}
const FORK_REMOTES: [(&str, &str); 2] = [
    ("origin", "git@github.com:someone/tauri-fixture.git"),
    (
        "upstream",
        "https://github.com/crabnebula/tauri-fixture.git",
    ),
];

#[test]
fn a_fork_identifies_itself_by_the_remote_its_branch_tracks() -> TestResult {
    let house = crabnebula()?;
    let f = fixture(std::slice::from_ref(&house))?;
    let fork = f.root.join("fork");
    checkout(&fork, &FORK_REMOTES)?;
    // Untracked: `origin` is the fork, which no house claims, and it does not
    // borrow the identity of the claimed upstream remote.
    assert!(matches!(
        f.registry.resolve_repository(&fork),
        Err(HouseError::RemotesDisagree { remotes })
            if remotes.iter().map(ToString::to_string).collect::<Vec<_>>() == ["origin", "upstream"]
    ));
    assert!(matches!(
        f.registry.claims(&fork),
        Err(HouseError::RemotesDisagree { .. })
    ));
    track(&fork, "upstream")?;
    assert_eq!(
        f.registry.claims(&fork)?.setup_target()?,
        ("crabnebula/tauri-fixture".parse::<Repository>()?, None)
    );
    assert_eq!(
        f.registry.resolve_repository(&fork)?,
        RepositoryMatch::Unbound {
            repository: "crabnebula/tauri-fixture".parse()?,
            house: house.house.clone(),
        }
    );
    let config = binding(&house, "crabnebula/tauri-fixture")?;
    f.registry.bind_repository(&config)?;
    assert_eq!(
        f.registry.resolve_repository(&fork)?,
        RepositoryMatch::Bound(config)
    );
    // Tracking the fork instead: the bound upstream repository does not apply.
    track(&fork, "origin")?;
    assert!(matches!(
        f.registry.resolve_repository(&fork),
        Err(HouseError::RemotesDisagree { .. })
    ));
    // With no other remote the fork is simply unclaimed.
    git(&fork, &["remote", "remove", "upstream"])?;
    assert!(matches!(
        f.registry.resolve_repository(&fork),
        Err(HouseError::HouseSelection)
    ));
    Ok(())
}

#[test]
fn remotes_of_different_houses_fail_closed_and_are_named() -> TestResult {
    let crab = crabnebula()?;
    let other = origin89_claiming_fork()?;
    let f = fixture(&[crab.clone(), other.clone()])?;
    let fork = f.root.join("fork");
    checkout(&fork, &FORK_REMOTES)?;
    track(&fork, "upstream")?;
    let names = |result: Result<RepositoryMatch, HouseError>| match result {
        Err(HouseError::RemotesDisagree { remotes }) => {
            Ok(remotes.iter().map(ToString::to_string).collect::<Vec<_>>())
        }
        other => Err(format!("expected disagreeing remotes, got {other:?}")),
    };
    assert_eq!(
        names(f.registry.resolve_repository(&fork))?,
        ["upstream", "origin"]
    );
    assert!(matches!(
        f.registry.claims(&fork),
        Err(HouseError::RemotesDisagree { .. })
    ));
    // Not even a stored binding for the identifying remote overrides it.
    f.registry
        .bind_repository(&binding(&crab, "crabnebula/tauri-fixture")?)?;
    assert_eq!(
        names(f.registry.resolve_repository(&fork))?,
        ["upstream", "origin"]
    );
    let message = HouseError::RemotesDisagree { remotes: vec![] }.to_string();
    assert!(message.contains("do not agree"), "{message}");
    // The same houses agree once the other remote no longer names one.
    git(
        &fork,
        &[
            "remote",
            "set-url",
            "origin",
            "git@github.com:nobody/else.git",
        ],
    )?;
    assert!(matches!(
        f.registry.resolve_repository(&fork)?,
        RepositoryMatch::Bound(_)
    ));
    Ok(())
}

#[test]
fn the_identifying_remote_is_the_push_destination() -> TestResult {
    let house = crabnebula()?;
    let f = fixture(std::slice::from_ref(&house))?;
    let consumer = f.root.join("consumer");
    checkout(
        &consumer,
        &[("origin", "https://github.com/crabnebula/tauri-fixture.git")],
    )?;
    // A push URL elsewhere is where the branch publishes: it decides.
    git(
        &consumer,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            "git@github.com:someone/else.git",
        ],
    )?;
    assert!(matches!(
        f.registry.resolve_repository(&consumer),
        Err(HouseError::HouseSelection)
    ));
    // Two different push destinations name no single repository.
    git(
        &consumer,
        &[
            "remote",
            "set-url",
            "--add",
            "--push",
            "origin",
            "git@github.com:crabnebula/tauri-fixture.git",
        ],
    )?;
    assert!(matches!(
        f.registry.resolve_repository(&consumer),
        Err(HouseError::RepositoryUnidentified)
    ));
    Ok(())
}

#[test]
fn detached_head_and_local_upstreams_fall_back_to_origin() -> TestResult {
    let house = crabnebula()?;
    let f = fixture(std::slice::from_ref(&house))?;
    let fork = f.root.join("fork");
    checkout(&fork, &FORK_REMOTES)?;
    track(&fork, "upstream")?;
    git(&fork, &["checkout", "--quiet", "--detach"])?;
    assert!(matches!(
        f.registry.resolve_repository(&fork),
        Err(HouseError::RemotesDisagree { remotes }) if remotes.first().map(RemoteName::as_str) == Some("origin")
    ));
    // Without `origin` and without a tracked remote there is nothing to trust.
    let named = f.root.join("named");
    checkout(
        &named,
        &[("github", "https://github.com/crabnebula/tauri-fixture.git")],
    )?;
    assert!(matches!(
        f.registry.resolve_repository(&named),
        Err(HouseError::RepositoryUnidentified)
    ));
    track(&named, ".")?;
    assert!(matches!(
        f.registry.resolve_repository(&named),
        Err(HouseError::RepositoryUnidentified)
    ));
    track(&named, "github")?;
    assert!(matches!(
        f.registry.resolve_repository(&named)?,
        RepositoryMatch::Unbound { .. }
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
    let preview = f.registry.import_legacy(&consumer.join("src"), None)?;
    assert_eq!(preview.status, LegacyImportStatus::WouldCreate);
    assert_eq!(preview.binding.schema, 2);
    assert_eq!(f.registry.binding(&preview.binding.repository)?, None);
    // Approving a different digest applies nothing.
    let other: BindingDigest = "0".repeat(64).parse()?;
    assert!(matches!(
        f.registry.import_legacy(&consumer, Some(&other)),
        Err(HouseError::LegacyChanged)
    ));
    assert_eq!(f.registry.binding(&preview.binding.repository)?, None);
    let applied = f.registry.import_legacy(&consumer, Some(&preview.digest))?;
    assert_eq!(applied.digest, preview.digest);
    assert_eq!(applied.status, LegacyImportStatus::Created);
    assert_eq!(
        f.registry.resolve_repository(&consumer)?,
        RepositoryMatch::Bound(applied.binding.clone())
    );
    assert_eq!(
        f.registry
            .import_legacy(&consumer, Some(&applied.digest))?
            .status,
        LegacyImportStatus::Unchanged
    );
    let mut changed = applied.binding.clone();
    changed.workflows.clear();
    f.registry
        .configure_repository(&applied.binding, &changed)?;
    assert_eq!(
        f.registry
            .import_legacy(&consumer, Some(&applied.digest))?
            .status,
        LegacyImportStatus::Conflict
    );
    assert_eq!(f.registry.binding(&changed.repository)?, Some(changed));
    assert_eq!(fs::read(&path)?, bytes, "the legacy file is left as it was");
    Ok(())
}

fn any_digest() -> TestResult<BindingDigest> {
    Ok("1".repeat(64).parse()?)
}

#[test]
fn a_legacy_file_changed_after_the_preview_is_not_imported() -> TestResult {
    let house = crabnebula()?;
    let f = fixture(std::slice::from_ref(&house))?;
    let consumer = f.root.join("consumer");
    checkout(
        &consumer,
        &[("origin", "https://github.com/crabnebula/tauri-fixture.git")],
    )?;
    let mut legacy = serde_json::to_value(binding(&house, "crabnebula/tauri-fixture")?)?;
    legacy["schema"] = serde_json::json!(1);
    let path = consumer.join(LEGACY_REPOSITORY_CONFIG);
    fs::write(&path, serde_json::to_vec(&legacy)?)?;
    let preview = f.registry.import_legacy(&consumer, None)?;
    // A pull request edits the file between the preview and the approval.
    legacy["additionalChecks"] = serde_json::json!(["local-check", "extra-check"]);
    legacy["workflows"] = serde_json::json!(["pickup", "gate"]);
    fs::write(&path, serde_json::to_vec(&legacy)?)?;
    assert!(matches!(
        f.registry.import_legacy(&consumer, Some(&preview.digest)),
        Err(HouseError::LegacyChanged)
    ));
    assert_eq!(f.registry.binding(&preview.binding.repository)?, None);
    // Re-previewing shows the new content and its own digest, which applies.
    let again = f.registry.import_legacy(&consumer, None)?;
    assert_ne!(again.digest, preview.digest);
    assert!(again.binding.workflows.contains(&Workflow::Gate));
    let applied = f.registry.import_legacy(&consumer, Some(&again.digest))?;
    assert_eq!(applied.status, LegacyImportStatus::Created);
    assert_eq!(
        f.registry.binding(&applied.binding.repository)?,
        Some(again.binding)
    );
    // Formatting-only changes keep the digest: it covers the binding, not bytes.
    fs::write(&path, serde_json::to_vec_pretty(&legacy)?)?;
    assert_eq!(
        f.registry.import_legacy(&consumer, None)?.digest,
        again.digest
    );
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
        f.registry.import_legacy(&consumer, Some(&any_digest()?)),
        Err(HouseError::Io(std::io::ErrorKind::NotFound))
    ));
    let mut legacy = serde_json::to_value(binding(&house, "crabnebula/tauri-fixture")?)?;
    legacy["schema"] = serde_json::json!(1);
    // A file naming a repository the checkout's remotes do not name.
    fs::write(&path, serde_json::to_vec(&legacy)?)?;
    assert!(matches!(
        f.registry.import_legacy(&consumer, Some(&any_digest()?)),
        Err(HouseError::HouseSelection)
    ));
    for schema in [0, 2, 3] {
        legacy["schema"] = serde_json::json!(schema);
        fs::write(&path, serde_json::to_vec(&legacy)?)?;
        assert!(matches!(
            f.registry.import_legacy(&consumer, Some(&any_digest()?)),
            Err(HouseError::InvalidInput)
        ));
    }
    fs::write(&path, br#"{"schema":1,"token":"SECRET"}"#)?;
    assert!(matches!(
        f.registry.import_legacy(&consumer, Some(&any_digest()?)),
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
