//! `kitchen forge` and the forge binding offered by guided `house init`,
//! through the real CLI in disposable roots with a fake `gh`. Simulated: no
//! GitHub account or token is used.
use kitchen::contracts::CommitId;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
const KITCHEN: &str = "4f2a9c1e0b7d3a5f6c8e9d0a1b2c3d4e5f6a7b8c";
const TOKEN: &str = "fixture-token-never-printed";

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

struct Fixture {
    root: PathBuf,
    checkout: PathBuf,
    home: PathBuf,
    /// A `PATH` whose `gh` prints `login`, or has no `gh`.
    path: String,
    bundle: String,
}

fn fixture(root: &Path, login: Option<&str>) -> TestResult<Fixture> {
    let checkout = root.join("app");
    let home = root.join("home");
    let bin = root.join("bin");
    fs::create_dir_all(&checkout)?;
    fs::create_dir_all(&home)?;
    fs::create_dir_all(&bin)?;
    git(&checkout, &["init", "--quiet"])?;
    git(
        &checkout,
        &["remote", "add", "origin", "git@github.com:acme/app.git"],
    )?;
    if let Some(login) = login {
        let gh = bin.join("gh");
        fs::write(&gh, format!("#!/bin/sh\necho {login}\n"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&gh, fs::Permissions::from_mode(0o755))?;
        }
    }
    let bundle = kitchen::house::default_guidance(&"acme".parse()?, &CommitId::new(KITCHEN)?)?;
    let bundle_path = root.join("acme-bundle.json");
    fs::write(&bundle_path, serde_json::to_vec(&bundle)?)?;
    Ok(Fixture {
        root: root.to_path_buf(),
        checkout,
        home,
        path: format!("{}:/usr/bin:/bin", bin.display()),
        bundle: bundle_path.display().to_string(),
    })
}

fn kitchen(fixture: &Fixture, args: &[&str]) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .current_dir(&fixture.checkout)
        .env("HOME", &fixture.home)
        .env("PATH", &fixture.path)
        .args(args)
        .stdin(Stdio::null())
        .output()?)
}

fn init(fixture: &Fixture, extra: &[&str]) -> TestResult<Output> {
    let mut args = vec![
        "house",
        "init",
        "--house",
        "acme",
        "--required-checks",
        "none",
        "--bundle",
        &fixture.bundle,
        "--yes",
    ];
    args.extend_from_slice(extra);
    kitchen(fixture, &args)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn guided_init_binds_the_logged_in_gh_account_outside_the_checkout() -> TestResult {
    let temp = tempfile::tempdir()?;
    let fixture = fixture(&temp.path().canonicalize()?, Some("octo-cat"))?;
    let output = init(&fixture, &[])?;
    let stdout = text(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let registry = fixture.home.join(".kitchn");
    let token = registry.join("private/acme/credentials/github");
    assert!(
        stdout.contains("Bound the house to GitHub as octo-cat."),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("Token file {} is missing.", token.display())),
        "{stdout}"
    );
    assert!(
        stdout.contains("gh auth token --user 'octo-cat'"),
        "{stdout}"
    );
    // --yes prints the binding with the config before registering.
    assert!(text(&output.stderr).contains("Forge: GitHub as octo-cat, credential github"));
    let stored: serde_json::Value =
        serde_json::from_slice(&fs::read(registry.join("private/acme/forge.json"))?)?;
    assert_eq!(stored["requester"], "octo-cat");
    assert_eq!(stored["credential"], "github");
    assert_eq!(stored["postingBudget"], 20);
    assert!(!token.exists());
    assert_eq!(
        git(&fixture.checkout, &["status", "--porcelain", "--ignored"])?,
        ""
    );

    // The same answers resume; the binding is kept.
    let rerun = init(&fixture, &[])?;
    assert_eq!(rerun.status.code(), Some(0), "{}", text(&rerun.stderr));
    Ok(())
}

#[test]
fn guided_init_without_gh_binds_nothing_and_show_names_the_missing_binding() -> TestResult {
    let temp = tempfile::tempdir()?;
    let fixture = fixture(&temp.path().canonicalize()?, None)?;
    let output = init(&fixture, &[])?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert!(!text(&output.stdout).contains("Bound the house"));
    let registry = fixture.home.join(".kitchn");
    assert!(!registry.join("private").exists());

    let registry = registry.display().to_string();
    let show = kitchen(
        &fixture,
        &["forge", "show", "--registry", &registry, "--house", "acme"],
    )?;
    assert_eq!(show.status.code(), Some(1));
    assert_eq!(
        text(&show.stderr),
        "error: house acme has no forge binding, so kitchen cannot write to its forge; bind one with `kitchen forge bind --house acme` or `kitchen house init`\n"
    );
    Ok(())
}

#[test]
fn bind_then_show_reports_the_token_file_without_reading_it() -> TestResult {
    let temp = tempfile::tempdir()?;
    let fixture = fixture(&temp.path().canonicalize()?, None)?;
    assert_eq!(
        init(&fixture, &["--forge-requester", "none"])?
            .status
            .code(),
        Some(0)
    );
    let registry = fixture.home.join(".kitchn");
    let registry_arg = registry.display().to_string();
    let bind = |requester: &str, budget: &str| {
        kitchen(
            &fixture,
            &[
                "forge",
                "bind",
                "--registry",
                &registry_arg,
                "--house",
                "acme",
                "--requester",
                requester,
                "--posting-budget",
                budget,
            ],
        )
    };
    let show = || {
        kitchen(
            &fixture,
            &[
                "forge",
                "show",
                "--registry",
                &registry_arg,
                "--house",
                "acme",
            ],
        )
    };

    // Out-of-range budgets are invalid input and store nothing.
    assert_eq!(bind("acme-bot", "101")?.status.code(), Some(2));
    assert!(!registry.join("private/acme/forge.json").exists());

    let output = bind("acme-bot", "3")?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert!(text(&output.stdout).starts_with("Bound house acme to GitHub as acme-bot."));
    let output = bind("acme-bot", "3")?;
    assert!(text(&output.stdout).starts_with("Already bound house acme to GitHub as acme-bot."));
    let output = bind("someone-else", "3")?;
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stderr).contains("already has a different forge binding; it was kept"));

    let output = show()?;
    assert_eq!(output.status.code(), Some(1), "token not ready");
    assert!(text(&output.stdout).contains(
        "House acme writes to GitHub as acme-bot with credential github, at most 3 writes per task."
    ));
    let token = registry.join("private/acme/credentials/github");
    fs::create_dir_all(token.parent().ok_or("no parent")?)?;
    fs::write(&token, TOKEN)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&token, fs::Permissions::from_mode(0o644))?;
        let output = show()?;
        assert_eq!(output.status.code(), Some(1));
        assert!(text(&output.stdout).contains("readable by other users"));
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;

        // A credentials directory linked elsewhere is refused, with no
        // command that would write through the link.
        let credentials = token.parent().ok_or("no parent")?.to_path_buf();
        let moved = fixture.root.join("moved-credentials");
        fs::rename(&credentials, &moved)?;
        std::os::unix::fs::symlink(&moved, &credentials)?;
        let output = show()?;
        assert_eq!(output.status.code(), Some(1));
        let stdout = text(&output.stdout);
        assert!(
            stdout.contains("is behind a link or non-directory on its path"),
            "{stdout}"
        );
        assert!(!stdout.contains("gh auth token"), "{stdout}");
        fs::remove_file(&credentials)?;
        fs::rename(&moved, &credentials)?;
    }
    let output = show()?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(stdout.contains(&format!("Token file {} is ready.", token.display())));
    for output in [&stdout, &text(&output.stderr)] {
        assert!(!output.contains(TOKEN));
    }
    assert!(!fs::read_to_string(registry.join("private/acme/forge.json"))?.contains(TOKEN));
    assert!(fixture.root.join("app").exists());
    Ok(())
}
