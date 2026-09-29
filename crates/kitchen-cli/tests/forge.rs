//! `kitchn forge` and the forge binding offered by guided `house init`,
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
    Ok(Command::new(env!("CARGO_BIN_EXE_kitchn"))
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
        "error: house acme has no forge binding, so kitchn cannot write to its forge; bind one with `kitchn forge bind --house acme` or `kitchn house init`\n"
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

/// `issue apply`, `decompose apply`, and the acknowledgement re-read through
/// the real CLI. The fake `gh` answers only the identity check and logs every
/// call; any other call fails, so nothing is ever posted.
mod apply {
    use super::{TOKEN, TestResult, git, text};
    use kitchen::{
        BackendId, CredentialId, HouseId,
        adoption::HouseRegistry,
        contracts::{CommitId, ExternalRef, Grant, Permission, PostingBudget, Repository},
        house::{
            FORGE_BINDING_SCHEMA, ForgeBinding, ForgeKind, HouseConfig, RepositoryConfig,
            bind_forge, credential_path,
        },
        state::{HouseStore, StoreOptions},
    };
    use std::{
        collections::BTreeSet,
        fs,
        path::{Path, PathBuf},
        process::{Command, Output},
    };

    const DRAFT: &str = r#"{"repository":"acme/app","target":{"type":"new","title":"Add flash retry","body":"Retries a failed flash once."},"blockedBy":[7]}"#;

    struct Desk {
        _temp: tempfile::TempDir,
        root: PathBuf,
        registry: HouseRegistry,
        checkout: PathBuf,
        store: PathBuf,
        path: String,
        log: PathBuf,
    }

    impl Desk {
        /// House `acme` with checkout `acme/app`, bound to the forge as
        /// `acme-bot` when `bound`, and a `gh` on `PATH`.
        fn new(bound: bool) -> TestResult<Self> {
            let temp = tempfile::tempdir()?;
            let root = temp.path().canonicalize()?;
            let app = Repository::new("acme/app")?;
            let kitchen = CommitId::new(super::KITCHEN)?;
            let limit = |permission| -> TestResult<Grant> {
                Ok(Grant::repository(
                    permission,
                    app.clone(),
                    BackendId::new("github")?,
                    CredentialId::new("github")?,
                ))
            };
            let config = HouseConfig {
                schema: 1,
                house: HouseId::new("acme")?,
                kitchen: kitchen.clone(),
                guidance: kitchen,
                repositories: [app.clone()].into(),
                posting_destinations: [app.clone()].into(),
                required_reviewers: BTreeSet::new(),
                required_checks: BTreeSet::new(),
                policy_limits: [
                    limit(Permission::CreateIssue)?,
                    limit(Permission::EditIssueRelationships)?,
                ]
                .into(),
                grants: BTreeSet::new(),
                agents: None,
                stack_tool: None,
                schedules: None,
                merge_readiness: Default::default(),
                disk_pressure: None,
            };
            let registry = HouseRegistry::new(root.join("registry"))?;
            registry.initialize(&config)?;
            registry.bind_repository(&RepositoryConfig {
                schema: 2,
                house: config.house.clone(),
                repository: app,
                workflows: BTreeSet::new(),
                additional_reviewers: BTreeSet::new(),
                additional_checks: BTreeSet::new(),
            })?;
            if bound {
                bind_forge(
                    &registry,
                    &ForgeBinding {
                        schema: FORGE_BINDING_SCHEMA,
                        house: config.house.clone(),
                        forge: ForgeKind::GitHub,
                        backend: BackendId::new("github")?,
                        requester: ExternalRef::new("acme-bot")?,
                        credential: CredentialId::new("github")?,
                        posting_budget: PostingBudget::new(5)?,
                    },
                )?;
            }
            let checkout = root.join("app");
            fs::create_dir_all(&checkout)?;
            git(&checkout, &["init", "--quiet"])?;
            git(
                &checkout,
                &["remote", "add", "origin", "git@github.com:acme/app.git"],
            )?;
            let store = root.join("store");
            HouseStore::initialize(&store, config.house, StoreOptions::default())?;
            let bin = root.join("bin");
            fs::create_dir_all(&bin)?;
            let log = root.join("gh.log");
            let gh = bin.join("gh");
            fs::write(
                &gh,
                format!(
                    "#!/bin/sh\necho \"$*\" >> '{log}'\nif [ \"$*\" = 'api --hostname github.com user' ] && [ \"$GH_TOKEN\" = '{TOKEN}' ]; then echo '{{\"login\":\"acme-bot\"}}'; exit 0; fi\nexit 1\n",
                    log = log.display()
                ),
            )?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&gh, fs::Permissions::from_mode(0o755))?;
            }
            Ok(Self {
                _temp: temp,
                path: format!("{}:/usr/bin:/bin", bin.display()),
                root,
                registry,
                checkout,
                store,
                log,
            })
        }

        fn place_token(&self) -> TestResult {
            let binding = kitchen::house::forge_binding(&self.registry, &HouseId::new("acme")?)?;
            let token = credential_path(&self.registry, &binding)?;
            fs::create_dir_all(token.parent().ok_or("no parent")?)?;
            fs::write(&token, TOKEN)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
            }
            Ok(())
        }

        fn file(&self, name: &str, contents: &str) -> TestResult<String> {
            let path = self.root.join(name);
            fs::write(&path, contents)?;
            Ok(path.display().to_string())
        }

        fn kitchen(&self, args: &[&str]) -> TestResult<Output> {
            Ok(Command::new(env!("CARGO_BIN_EXE_kitchen"))
                .current_dir(&self.checkout)
                .env("PATH", &self.path)
                .args(args)
                .output()?)
        }

        fn issue_apply(&self, draft: &str, digest: &str) -> TestResult<Output> {
            let registry = self.registry.root().display().to_string();
            let store = self.store.display().to_string();
            self.kitchen(&[
                "issue",
                "apply",
                "--draft",
                draft,
                "--approve",
                digest,
                "--registry",
                &registry,
                "--store",
                &store,
                "--holder",
                "person",
            ])
        }

        /// The digest `issue new` prints for the draft at `path`.
        fn digest(&self, path: &str) -> TestResult<String> {
            let draft: kitchen::workflows::interactive::IssueDraft =
                serde_json::from_slice(&fs::read(path)?)?;
            Ok(kitchen::workflows::interactive::draft_preview(&draft)?
                .digest
                .to_string())
        }

        fn gh_calls(&self) -> TestResult<Vec<String>> {
            if !self.log.exists() {
                return Ok(Vec::new());
            }
            Ok(fs::read_to_string(&self.log)?
                .lines()
                .map(str::to_owned)
                .collect())
        }

        fn tasks(&self) -> TestResult<usize> {
            let store =
                HouseStore::open(&self.store, HouseId::new("acme")?, StoreOptions::default())?;
            Ok(store.tasks()?.len())
        }
    }

    fn other_digest() -> String {
        format!("sha256:{}", "0".repeat(64))
    }

    #[test]
    fn issue_apply_refuses_a_house_without_a_forge_binding_by_name() -> TestResult {
        let desk = Desk::new(false)?;
        let draft = desk.file("draft.json", DRAFT)?;
        let digest = desk.digest(&draft)?;
        let output = desk.issue_apply(&draft, &digest)?;
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            text(&output.stderr),
            "error: house acme has no forge binding, so kitchen cannot write to its forge; bind one with `kitchen forge bind --house acme` or `kitchen house init`\n"
        );
        assert_eq!(text(&output.stdout), "");
        assert!(desk.gh_calls()?.is_empty());
        assert_eq!(desk.tasks()?, 0);
        Ok(())
    }

    #[test]
    fn issue_apply_needs_the_current_digest_and_a_ready_token_before_gh_runs() -> TestResult {
        let desk = Desk::new(true)?;
        let draft = desk.file("draft.json", DRAFT)?;
        let digest = desk.digest(&draft)?;

        // An approval of another preview.
        let stale = desk.issue_apply(&draft, &other_digest())?;
        assert_eq!(stale.status.code(), Some(1));
        assert!(
            text(&stale.stderr).contains(&format!(
                "the approval does not name the current preview (digest {digest})"
            )),
            "{}",
            text(&stale.stderr)
        );
        // Not a digest at all is invalid input.
        assert_eq!(
            desk.issue_apply(&draft, "sha256:nope")?.status.code(),
            Some(2)
        );

        // The right digest, but no token file yet.
        let missing = desk.issue_apply(&draft, &digest)?;
        assert_eq!(missing.status.code(), Some(1));
        assert!(
            text(&missing.stderr).contains("credential github of house acme is missing"),
            "{}",
            text(&missing.stderr)
        );
        assert!(desk.gh_calls()?.is_empty());
        assert_eq!(desk.tasks()?, 0);
        Ok(())
    }

    #[test]
    fn issue_apply_writes_through_gh_with_the_house_token_and_stops_on_refusal() -> TestResult {
        let desk = Desk::new(true)?;
        desk.place_token()?;
        let draft = desk.file("draft.json", DRAFT)?;
        let digest = desk.digest(&draft)?;

        // gh proves the token's account, then every forge read fails, so the
        // create is refused before anything is submitted.
        let output = desk.issue_apply(&draft, &digest)?;
        assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
        let stdout = text(&output.stdout);
        assert!(
            stdout.starts_with("The forge refused write create (rejected; check the token's account and access) (task draft-"),
            "{stdout}"
        );
        assert!(
            stdout.ends_with("Rerun to retry the remaining writes.\n"),
            "{stdout}"
        );
        let calls = desk.gh_calls()?;
        assert_eq!(
            calls.first().map(String::as_str),
            Some("api --hostname github.com user")
        );
        assert!(
            calls
                .iter()
                .any(|call| call.contains("--method GET repos/acme/app/issues")),
            "{calls:?}"
        );
        assert!(!calls.iter().any(|call| call.contains("POST")), "{calls:?}");
        for output in [&stdout, &text(&output.stderr)] {
            assert!(!output.contains(TOKEN));
        }
        // The approved draft is a durable task a rerun resumes.
        assert_eq!(desk.tasks()?, 2);

        // Without gh, nothing runs and the reason is named.
        let no_gh = Command::new(env!("CARGO_BIN_EXE_kitchen"))
            .current_dir(&desk.checkout)
            .env("PATH", "/usr/bin:/bin")
            .args(["issue", "apply", "--draft", &draft, "--approve", &digest])
            .arg("--registry")
            .arg(desk.registry.root())
            .arg("--store")
            .arg(&desk.store)
            .args(["--holder", "person"])
            .output()?;
        if !Path::new("/usr/bin/gh").exists() && !Path::new("/bin/gh").exists() {
            assert_eq!(no_gh.status.code(), Some(1));
            assert_eq!(
                text(&no_gh.stderr),
                "error: the GitHub CLI (`gh`) was not found on PATH; install it to write to GitHub\n"
            );
        }
        assert_eq!(
            git(&desk.checkout, &["status", "--porcelain", "--ignored"])?,
            ""
        );
        Ok(())
    }

    fn proposal(title: &str) -> String {
        format!(
            r#"{{"repository":"acme/app","issues":[{{"key":"core","title":"{title}","outcome":"It works.","ownedPaths":["crates/core"],"acceptance":["tested"]}}]}}"#
        )
    }

    fn decompose(desk: &Desk, args: &[&str]) -> TestResult<Output> {
        let registry = desk.registry.root().display().to_string();
        let store = desk.store.display().to_string();
        let mut all = vec!["decompose"];
        all.extend_from_slice(args);
        all.extend_from_slice(&[
            "--registry",
            &registry,
            "--house",
            "acme",
            "--store",
            &store,
        ]);
        all.extend_from_slice(&["--holder", "person"]);
        desk.kitchen(&all)
    }

    #[test]
    fn decompose_apply_refuses_a_changed_proposal_and_a_missing_binding() -> TestResult {
        let desk = Desk::new(true)?;
        let approved = desk.file("approved.json", &proposal("Build the core"))?;
        let preview = desk.kitchen(&["decompose", "preview", "--proposal", &approved, "--json"])?;
        assert_eq!(preview.status.code(), Some(0), "{}", text(&preview.stderr));
        let digest = serde_json::from_slice::<serde_json::Value>(&preview.stdout)?["digest"]
            .as_str()
            .ok_or("no digest")?
            .to_owned();

        let changed = desk.file("changed.json", &proposal("Build the core crate"))?;
        let output = decompose(
            &desk,
            &["apply", "--proposal", &changed, "--approve", &digest],
        )?;
        assert_eq!(output.status.code(), Some(1));
        assert!(
            text(&output.stderr).contains("the approval does not name the current preview"),
            "{}",
            text(&output.stderr)
        );

        let unbound = Desk::new(false)?;
        let approved = unbound.file("approved.json", &proposal("Build the core"))?;
        let output = decompose(
            &unbound,
            &["apply", "--proposal", &approved, "--approve", &digest],
        )?;
        assert_eq!(output.status.code(), Some(1));
        assert!(
            text(&output.stderr).starts_with("error: house acme has no forge binding"),
            "{}",
            text(&output.stderr)
        );
        assert!(desk.gh_calls()?.is_empty() && unbound.gh_calls()?.is_empty());
        assert_eq!(desk.tasks()? + unbound.tasks()?, 0);
        Ok(())
    }

    #[test]
    fn acknowledge_rereads_through_the_binding_unless_told_not_to() -> TestResult {
        let desk = Desk::new(true)?;
        let ack = |extra: &[&str]| {
            let mut args = vec![
                "acknowledge",
                "--task",
                "decomposition-missing",
                "--reason",
                "Checked.",
            ];
            args.extend_from_slice(extra);
            decompose(&desk, &args)
        };
        // A bound house whose token is not ready cannot be re-read: refused
        // before the store is touched.
        let output = ack(&[])?;
        assert_eq!(output.status.code(), Some(1));
        assert!(
            text(&output.stderr).contains("credential github of house acme is missing"),
            "{}",
            text(&output.stderr)
        );
        // --without-forge skips the re-read, so the unknown task is what fails.
        let output = ack(&["--without-forge"])?;
        assert!(
            !text(&output.stderr).contains("credential github"),
            "{}",
            text(&output.stderr)
        );
        assert_ne!(output.status.code(), Some(0));

        // `issue acknowledge` reads the same binding.
        let registry = desk.registry.root().display().to_string();
        let store = desk.store.display().to_string();
        let output = desk.kitchen(&[
            "issue",
            "acknowledge",
            "draft-0123456789abcdef-012345678901234567890123",
            "--reason",
            "Checked.",
            "--registry",
            &registry,
            "--store",
            &store,
            "--holder",
            "person",
        ])?;
        assert_eq!(output.status.code(), Some(1));
        assert!(
            text(&output.stderr).contains("credential github of house acme is missing"),
            "{}",
            text(&output.stderr)
        );
        assert!(desk.gh_calls()?.is_empty());
        Ok(())
    }
}
