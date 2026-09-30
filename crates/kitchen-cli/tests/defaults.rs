//! Scope defaults through the real CLI, with isolated registries and Git worktrees.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use kitchen::{
    adoption::HouseRegistry,
    house::{HouseConfig, RepositoryConfig},
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn git(path: &Path, args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
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
    Ok(())
}

fn cli(path: &Path, registry: &Path, args: &[&str]) -> Result<Output, Box<dyn std::error::Error>> {
    Ok(Command::new(env!("CARGO_BIN_EXE_kitchn"))
        .current_dir(path)
        .env("KITCHN_HOME", registry)
        .env("HOME", path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()?)
}

#[test]
fn origin_push_destination_must_match_fetch_repository() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = HouseRegistry::new(root.join("registry"))?;
    let crab: HouseConfig = serde_json::from_str(include_str!(
        "../../kitchen/tests/fixtures/house/crabnebula.json"
    ))?;
    let other: HouseConfig = serde_json::from_str(include_str!(
        "../../kitchen/tests/fixtures/house/origin89.json"
    ))?;
    for house in [&crab, &other] {
        registry.initialize(house)?;
        registry.initialize_store(&house.house)?;
    }
    for (house, repository) in [
        (&crab, "crabnebula/tauri-fixture"),
        (&other, "origin89hq/firmware"),
    ] {
        registry.bind_repository(&RepositoryConfig {
            schema: 2,
            house: house.house.clone(),
            repository: repository.parse()?,
            workflows: Default::default(),
            additional_reviewers: Default::default(),
            additional_checks: Default::default(),
        })?;
    }
    let checkout = root.join("checkout");
    fs::create_dir(&checkout)?;
    git(&checkout, &["init", "--quiet"])?;
    let fetch = "https://github.com/crabnebula/tauri-fixture.git";
    let other_url = "https://github.com/origin89hq/firmware.git";
    git(&checkout, &["remote", "add", "origin", fetch])?;

    git(
        &checkout,
        &["remote", "set-url", "--push", "origin", other_url],
    )?;
    let mismatched = cli(&checkout, registry.root(), &["store", "capacity"])?;
    assert_eq!(mismatched.status.code(), Some(2), "{mismatched:?}");
    let stderr = String::from_utf8(mismatched.stderr)?;
    assert!(
        stderr.contains("--house") && stderr.contains("--repository"),
        "{stderr}"
    );

    git(&checkout, &["remote", "set-url", "--push", "origin", fetch])?;
    let matching = cli(&checkout, registry.root(), &["store", "capacity"])?;
    assert_eq!(matching.status.code(), Some(0), "{matching:?}");

    git(&checkout, &["config", "--unset", "remote.origin.pushurl"])?;
    git(
        &checkout,
        &["config", &format!("url.{other_url}.pushInsteadOf"), fetch],
    )?;
    let rewritten = cli(&checkout, registry.root(), &["store", "capacity"])?;
    assert_eq!(rewritten.status.code(), Some(2), "{rewritten:?}");
    let stderr = String::from_utf8(rewritten.stderr)?;
    assert!(
        stderr.contains("--house") && stderr.contains("--repository"),
        "{stderr}"
    );

    git(
        &checkout,
        &[
            "config",
            "--unset",
            &format!("url.{other_url}.pushInsteadOf"),
        ],
    )?;
    git(
        &checkout,
        &["remote", "set-url", "origin", "alias:repo.git"],
    )?;
    git(
        &checkout,
        &[
            "config",
            &format!("url.{fetch}.insteadOf"),
            "alias:repo.git",
        ],
    )?;
    let fetch_rewritten = cli(&checkout, registry.root(), &["store", "capacity"])?;
    assert_eq!(
        fetch_rewritten.status.code(),
        Some(0),
        "{fetch_rewritten:?}"
    );
    Ok(())
}

#[test]
fn bound_worktrees_share_house_store_and_explicit_house_wins() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = HouseRegistry::new(root.join(".kitchn"))?;
    let crab: HouseConfig = serde_json::from_str(include_str!(
        "../../kitchen/tests/fixtures/house/crabnebula.json"
    ))?;
    let other: HouseConfig = serde_json::from_str(include_str!(
        "../../kitchen/tests/fixtures/house/origin89.json"
    ))?;
    registry.initialize(&crab)?;
    registry.initialize(&other)?;
    registry.initialize_store(&crab.house)?;
    registry.initialize_store(&other.house)?;
    let first = root.join("first");
    fs::create_dir(&first)?;
    git(&first, &["init", "--quiet"])?;
    git(
        &first,
        &["commit", "--quiet", "--allow-empty", "-m", "init"],
    )?;
    git(
        &first,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/crabnebula/tauri-fixture.git",
        ],
    )?;
    registry.bind_repository(&RepositoryConfig {
        schema: 2,
        house: crab.house.clone(),
        repository: "crabnebula/tauri-fixture".parse()?,
        workflows: Default::default(),
        additional_reviewers: Default::default(),
        additional_checks: Default::default(),
    })?;
    let second = root.join("second");
    git(
        &first,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "second",
            second.to_str().ok_or("path")?,
        ],
    )?;
    for worktree in [&first, &second] {
        let output = cli(worktree, registry.root(), &["store", "capacity"])?;
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let explicit = Command::new(env!("CARGO_BIN_EXE_kitchn"))
            .current_dir(worktree)
            .env("KITCHN_HOME", root.join("unused"))
            .args(["store", "capacity", "--registry"])
            .arg(registry.root())
            .args(["--house", "crabnebula"])
            .output()?;
        assert_eq!(output.stdout, explicit.stdout);
    }
    let home_fallback = Command::new(env!("CARGO_BIN_EXE_kitchn"))
        .current_dir(&first)
        .env_remove("KITCHN_HOME")
        .env("HOME", &root)
        .args(["store", "capacity"])
        .output()?;
    assert_eq!(home_fallback.status.code(), Some(0), "{home_fallback:?}");
    let override_house = cli(
        &first,
        registry.root(),
        &["store", "capacity", "--house", "origin89"],
    )?;
    assert_eq!(override_house.status.code(), Some(0), "{override_house:?}");
    Ok(())
}

#[test]
fn unbound_and_missing_remote_name_house_flag() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = HouseRegistry::new(root.join("registry"))?;
    let crab: HouseConfig = serde_json::from_str(include_str!(
        "../../kitchen/tests/fixtures/house/crabnebula.json"
    ))?;
    registry.initialize(&crab)?;
    let checkout = root.join("checkout");
    fs::create_dir(&checkout)?;
    git(&checkout, &["init", "--quiet"])?;
    for remote in [false, true] {
        if remote {
            git(
                &checkout,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/crabnebula/tauri-fixture.git",
                ],
            )?;
        }
        let output = cli(&checkout, registry.root(), &["store", "capacity"])?;
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(String::from_utf8(output.stderr)?.contains("--house"));
    }
    Ok(())
}

#[test]
fn ambiguous_origin_is_refused_without_overriding_an_explicit_house() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = HouseRegistry::new(root.join("registry"))?;
    let crab: HouseConfig = serde_json::from_str(include_str!(
        "../../kitchen/tests/fixtures/house/crabnebula.json"
    ))?;
    registry.initialize(&crab)?;
    registry.initialize_store(&crab.house)?;
    registry.bind_repository(&RepositoryConfig {
        schema: 2,
        house: crab.house.clone(),
        repository: "crabnebula/tauri-fixture".parse()?,
        workflows: Default::default(),
        additional_reviewers: Default::default(),
        additional_checks: Default::default(),
    })?;
    let checkout = root.join("checkout");
    fs::create_dir(&checkout)?;
    git(&checkout, &["init", "--quiet"])?;
    git(
        &checkout,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/crabnebula/tauri-fixture.git",
        ],
    )?;
    git(
        &checkout,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            "https://github.com/crabnebula/tauri-fixture.git",
        ],
    )?;
    git(
        &checkout,
        &[
            "remote",
            "set-url",
            "--add",
            "--push",
            "origin",
            "https://github.com/other/repository.git",
        ],
    )?;
    let inferred = cli(&checkout, registry.root(), &["store", "capacity"])?;
    assert_eq!(inferred.status.code(), Some(2), "{inferred:?}");
    assert!(String::from_utf8(inferred.stderr)?.contains("--house"));
    let explicit = cli(
        &checkout,
        registry.root(),
        &["store", "capacity", "--house", "crabnebula"],
    )?;
    assert_eq!(explicit.status.code(), Some(0), "{explicit:?}");
    Ok(())
}

#[test]
fn commands_without_a_registry_option_keep_explicit_house_scope() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let output = Command::new(env!("CARGO_BIN_EXE_kitchn"))
        .current_dir(&root)
        .env_remove("HOME")
        .env_remove("KITCHN_HOME")
        .args(["trust", "capacity", "--house", "crabnebula", "--ledger"])
        .arg(root.join("missing-ledger"))
        .output()?;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(!String::from_utf8(output.stderr)?.contains("--registry"));
    Ok(())
}
