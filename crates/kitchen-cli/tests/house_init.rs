//! `kitchen house init` without `--config`, through the real CLI in disposable
//! roots. Standard input is piped, so these cover the non-interactive path;
//! the prompts are covered through the library's prompter in
//! `crates/kitchen/tests/house_init.rs`.
use kitchen::contracts::CommitId;
use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
};
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
const KITCHEN: &str = "4f2a9c1e0b7d3a5f6c8e9d0a1b2c3d4e5f6a7b8c";

fn git(path: &Path, args: &[&str]) -> TestResult<String> {
    let output = Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(format!("git {args:?}: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// A checkout whose `origin` is github.com/acme/app, and a separate home.
fn fixture(root: &Path) -> TestResult<(std::path::PathBuf, std::path::PathBuf)> {
    let checkout = root.join("app");
    let home = root.join("home");
    fs::create_dir_all(&checkout)?;
    fs::create_dir_all(&home)?;
    git(&checkout, &["init", "--quiet"])?;
    git(
        &checkout,
        &["remote", "add", "origin", "git@github.com:acme/app.git"],
    )?;
    Ok((checkout, home))
}

/// A verified default-guidance bundle for `house`, pinned at [`KITCHEN`]. The
/// test binary records no build commit, so a script must supply one.
fn bundle(root: &Path, house: &str) -> TestResult<String> {
    let bundle = kitchen::house::default_guidance(&house.parse()?, &CommitId::new(KITCHEN)?)?;
    let path = root.join(format!("{house}-bundle.json"));
    fs::write(&path, serde_json::to_vec(&bundle)?)?;
    Ok(path.display().to_string())
}

fn init(checkout: &Path, home: &Path, args: &[&str]) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .current_dir(checkout)
        .env("HOME", home)
        .args(["house", "init"])
        .args(args)
        .stdin(Stdio::null())
        .output()?)
}

#[test]
fn piped_input_fails_with_the_missing_flags_instead_of_blocking() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let (checkout, home) = fixture(&root)?;
    let acme = bundle(&root, "acme")?;
    let output = init(&checkout, &home, &["--bundle", &acme])?;
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.starts_with("error: standard input is not a terminal"),
        "{stderr}"
    );
    for flag in ["--house", "--required-checks", "--yes"] {
        assert!(stderr.contains(flag), "{flag}: {stderr}");
    }
    // Defaults from the checkout are not reported missing.
    assert!(!stderr.contains("--repositories"), "{stderr}");
    assert!(!home.join(".kitchn").exists());
    assert_eq!(git(&checkout, &["status", "--porcelain", "--ignored"])?, "");
    Ok(())
}

#[test]
fn flags_register_the_same_house_as_the_config_path() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let (checkout, home) = fixture(&root)?;
    let acme = bundle(&root, "acme")?;
    let output = init(
        &checkout,
        &home,
        &[
            "--house",
            "acme",
            "--required-checks",
            "test,lint",
            "--bundle",
            &acme,
            "--yes",
        ],
    )?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("and pinned the bundle's guidance at 4f2a9c1."),
        "{stdout}"
    );
    // --yes still prints the exact config before registering it.
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains(r#""house": "acme""#), "{stderr}");
    assert!(stderr.contains(r#""test""#), "{stderr}");
    assert!(stdout.contains("No authority or workflows activated."));
    let guided = home.join(".kitchn/houses/acme.json");
    assert!(stdout.contains(&format!("Saved your answers as {}.", guided.display())));
    assert_eq!(git(&checkout, &["status", "--porcelain", "--ignored"])?, "");

    // The saved file is a valid --config input and registers identically.
    let manual = root.join("manual");
    let output = init(
        &checkout,
        &home,
        &[
            "--registry",
            &manual.display().to_string(),
            "--config",
            &guided.display().to_string(),
        ],
    )?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        fs::read(&guided)?,
        fs::read(manual.join("houses/acme.json"))?
    );
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&guided)?)?;
    assert_eq!(saved["repositories"], serde_json::json!(["acme/app"]));
    assert_eq!(saved["grants"], serde_json::json!([]));
    assert_eq!(saved["agents"]["default"]["agent"], "codex");

    // Rerunning with the same answers changes nothing and succeeds.
    let rerun = init(
        &checkout,
        &home,
        &[
            "--house",
            "acme",
            "--required-checks",
            "test,lint",
            "--bundle",
            &acme,
            "--yes",
        ],
    )?;
    assert_eq!(rerun.status.code(), Some(0));
    Ok(())
}

#[test]
fn invalid_flags_and_mixed_modes_exit_2() -> TestResult {
    let temp = tempfile::tempdir()?;
    let (checkout, home) = fixture(&temp.path().canonicalize()?)?;
    let output = init(
        &checkout,
        &home,
        &["--house", "acme", "--station-cook", "gemini", "--yes"],
    )?;
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr)?.contains("invalid answer for --station-cook"));
    let output = init(
        &checkout,
        &home,
        &[
            "--registry",
            "/tmp/r",
            "--config",
            "house.json",
            "--house",
            "acme",
        ],
    )?;
    assert_eq!(output.status.code(), Some(2));
    // GitHub access flags come as a set.
    let output = init(&checkout, &home, &["--github-requester", "octocat"])?;
    assert_eq!(output.status.code(), Some(2));
    assert!(!home.join(".kitchn").exists());
    Ok(())
}

/// A fake `gh` answering as `octocat`; `protection` is its branch-protection
/// response, or empty to fail that read.
#[cfg(unix)]
fn fake_gh(root: &Path, protection: &str) -> TestResult<(std::path::PathBuf, std::path::PathBuf)> {
    use std::os::unix::fs::PermissionsExt;
    let gh = root.join("fake-gh");
    let response = if protection.is_empty() {
        "exit 1".to_owned()
    } else {
        format!("printf '%s' '{protection}'")
    };
    fs::write(
        &gh,
        format!(
            r#"#!/bin/sh
[ "$GH_TOKEN" = fixture-token ] || exit 2
for arg in "$@"; do
  case "$arg" in
    user) printf '%s' '{{"login":"octocat"}}'; exit 0 ;;
    *protection/required_status_checks*) {response}; exit 0 ;;
    repos/acme/app*) printf '%s' '{{"default_branch":"trunk"}}'; exit 0 ;;
  esac
done
exit 1
"#
        ),
    )?;
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o700))?;
    let token = root.join("token");
    fs::write(&token, "fixture-token")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    Ok((gh, token))
}

#[cfg(unix)]
#[test]
fn house_scoped_github_access_offers_the_branch_required_checks() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let (checkout, home) = fixture(&root)?;
    let (acme, other) = (bundle(&root, "acme")?, bundle(&root, "other")?);
    let (gh, token) = fake_gh(
        &root,
        r#"{"contexts":["test"],"checks":[{"context":"lint","app_id":null}]}"#,
    )?;
    let (gh, token) = (gh.display().to_string(), token.display().to_string());
    let access = [
        "--github-requester",
        "octocat",
        "--github-credential",
        "acme-read",
        "--github-credential-file",
        &token,
        "--gh",
        &gh,
    ];
    let mut args = vec!["--house", "acme", "--bundle", &acme, "--yes"];
    args.extend(access);
    let output = init(&checkout, &home, &args)?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join(".kitchn/houses/acme.json"))?)?;
    assert_eq!(saved["requiredChecks"], serde_json::json!(["lint", "test"]));

    // Unreadable protection is not "no checks": a script must answer.
    let (gh, _) = fake_gh(&root, "")?;
    let gh = gh.display().to_string();
    let mut args = vec!["--house", "other", "--bundle", &other, "--yes"];
    args.extend(access);
    // The last value is --gh.
    args.pop();
    args.push(&gh);
    let output = init(&checkout, &home, &args)?;
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr)?.contains("pass --required-checks"));
    assert!(!home.join(".kitchn/houses/other.json").exists());
    Ok(())
}

#[test]
fn the_built_in_guidance_is_refused_without_a_recorded_build_commit() -> TestResult {
    // A build that recorded its commit (`just install`) can label the
    // embedded guidance; this scenario only exists without one.
    if option_env!("KITCHEN_COMMIT").is_some_and(|commit| CommitId::new(commit).is_ok()) {
        return Ok(());
    }
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let (checkout, home) = fixture(&root)?;
    // No commit can be claimed for the embedded guidance, whatever --kitchen
    // says.
    for extra in [&[][..], &["--kitchen", KITCHEN][..]] {
        let mut args = vec!["--house", "acme", "--required-checks", "none", "--yes"];
        args.extend(extra);
        let output = init(&checkout, &home, &args)?;
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr)?;
        assert!(
            stderr.contains("did not record the commit it was built from")
                && stderr.contains("just install")
                && stderr.contains("--bundle <path>"),
            "{stderr}"
        );
        assert!(!home.join(".kitchn").exists());
    }
    Ok(())
}
