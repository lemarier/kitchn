//! Real CLI, disposable local houses; no network or runtime activation.
use kitchen::{
    HouseId,
    adoption::{
        HouseRegistry, InstructionAsset, InstructionBundle, RelativePath, role_cards_digest,
    },
    contracts::CommitId,
    house::{HouseConfig, HouseError},
};
use std::{
    cell::Cell,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
type Result = std::result::Result<(), Box<dyn std::error::Error>>;
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    registry: HouseRegistry,
    /// Fill character of the next guidance revision to publish.
    next_guidance: Cell<u8>,
}
fn manifest(revision: u32) -> String {
    format!(
        r#"schema = 1
name = "test"
house = "crabnebula"
revision = {revision}
description = "test"
[[files]]
source = "AGENTS.md"
provenance = "html-comment"
[[files]]
source = "README.md"
provenance = "html-comment"
"#
    )
}
fn asset(
    path: &str,
    contents: &str,
) -> std::result::Result<InstructionAsset, Box<dyn std::error::Error>> {
    Ok(InstructionAsset {
        path: RelativePath::new(path)?,
        contents: contents.to_owned(),
    })
}
impl Fixture {
    fn new() -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let registry = HouseRegistry::new(root.join("registry"))?;
        let house: HouseConfig = serde_json::from_str(include_str!(
            "../../kitchen/tests/fixtures/house/crabnebula.json"
        ))?;
        registry.initialize(&house)?;
        let fixture = Self {
            _temp: temp,
            root,
            registry,
            next_guidance: Cell::new(b'b'),
        };
        fixture.revision(1)?;
        Ok(fixture)
    }
    /// Publish template revision `revision` under a new pinned guidance revision.
    fn revision(&self, revision: u32) -> Result {
        let content = format!("content revision {revision}\n");
        self.publish(&manifest(revision), &content)
    }
    /// Import guidance containing the `test` template and select its revision.
    fn publish(&self, manifest: &str, content: &str) -> Result {
        let fill = self.next_guidance.get();
        self.next_guidance.set(fill + 1);
        let house = HouseId::new("crabnebula")?;
        let bundle = InstructionBundle {
            schema: 1,
            house: house.clone(),
            kitchen: CommitId::new(&"a".repeat(40))?,
            role_cards_digest: role_cards_digest(),
            guidance: CommitId::new(&char::from(fill).to_string().repeat(40))?,
            entrypoint: RelativePath::new("SKILL.md")?,
            notices: [RelativePath::new("NOTICE.md")?].into(),
            assets: vec![
                asset("SKILL.md", "Apply the house rules.")?,
                asset("NOTICE.md", "Synthetic fixture notice.")?,
                asset("templates/test/template.toml", manifest)?,
                asset("templates/test/files/AGENTS.md", content)?,
                asset("templates/test/files/README.md", content)?,
            ],
        };
        self.install(&bundle)
    }
    /// Select `bundle` as the house's guidance. Other tests spawn the CLI
    /// concurrently, and a child forked while this process holds the registry
    /// lock keeps it until exec, so `Busy` is retried within a bound.
    fn install(&self, bundle: &InstructionBundle) -> Result {
        for _ in 0..100 {
            let current = self.registry.load(&bundle.house)?;
            match self.registry.update(&current, bundle) {
                Err(HouseError::Busy) => std::thread::sleep(std::time::Duration::from_millis(20)),
                result => {
                    result?;
                    return Ok(());
                }
            }
        }
        Err("registry stayed busy".into())
    }
    fn command(&self, verb: &str, target: &Path) -> Command {
        self.command_for(verb, target, "test")
    }
    fn command_for(&self, verb: &str, target: &Path, template: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kitchen"));
        command
            .arg(verb)
            .arg(target)
            .arg("--registry")
            .arg(self.root.join("registry"))
            .args(["--template", template]);
        command
    }
    fn selected(&self, verb: &str) -> Command {
        self.selected_for(verb, "test")
    }
    fn selected_for(&self, verb: &str, template: &str) -> Command {
        let mut command = self.command_for(verb, &self.root.join("consumer"), template);
        command.args([
            "--house",
            "crabnebula",
            "--repository",
            "crabnebula/tauri-fixture",
        ]);
        command
    }
}
/// The registry binding for the fixture repository.
fn binding(f: &Fixture) -> PathBuf {
    f.root
        .join("registry/repositories/crabnebula/tauri-fixture.json")
}
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}
#[test]
fn preview_apply_and_bound_rerun() -> Result {
    let f = Fixture::new()?;
    let output = f.selected("init").output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("2 to add"));
    assert!(stdout(&output).contains(
        "Registry binding crabnebula/tauri-fixture -> house crabnebula: add (stored outside the working tree)"
    ));
    assert!(stdout(&output).contains(
        "No additions match known automation paths. Kitchen activates nothing. Matching is best-effort: other files may still be run by tools."
    ));
    assert!(!f.root.join("consumer").exists());
    let output = f.selected("init").arg("--yes").output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(
        fs::read_to_string(f.root.join("consumer/AGENTS.md"))?.ends_with("content revision 1\n")
    );
    assert!(binding(&f).is_file());
    // The stored binding supplies the house on a rerun.
    let output = f
        .command("adopt", &f.root.join("consumer"))
        .args(["--repository", "crabnebula/tauri-fixture", "--yes"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("0 to add, 2 unchanged, 0 withheld, 0 conflicts"));
    assert!(stdout(&output).contains("house crabnebula: unchanged"));
    assert!(!f.root.join("consumer/.git").exists());
    assert!(!f.root.join("consumer/.kitchen.json").exists());
    Ok(())
}
#[test]
fn confirmation_requires_complete_explicit_yes() -> Result {
    let f = Fixture::new()?;
    for (answer, applied) in [
        ("no\n", false),
        ("", false),
        ("yes", false),
        ("yes\n", true),
    ] {
        let mut child = f
            .selected("init")
            .arg("--confirm")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or("stdin")?
            .write_all(answer.as_bytes())?;
        let output = child.wait_with_output()?;
        assert!(output.status.success(), "{output:?}");
        assert_eq!(binding(&f).exists(), applied);
        assert!(!f.root.join("consumer/.kitchen.json").exists());
    }
    Ok(())
}
#[test]
fn changed_revision_distinguishes_and_preserves_local_edits() -> Result {
    let f = Fixture::new()?;
    assert!(f.selected("init").arg("--yes").output()?.status.success());
    let agents = fs::read_to_string(f.root.join("consumer/AGENTS.md"))? + "local instruction\n";
    fs::write(f.root.join("consumer/AGENTS.md"), &agents)?;
    let readme = fs::read(f.root.join("consumer/README.md"))?;
    f.revision(2)?;
    let output = f.selected("adopt").arg("--yes").output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).contains("content differs from its marker (local edits or damage)"));
    assert!(stdout(&output).contains("upstream content or provenance differs"));
    assert!(stdout(&output).contains("2 conflicts"));
    assert_eq!(
        fs::read_to_string(f.root.join("consumer/AGENTS.md"))?,
        agents
    );
    assert_eq!(fs::read(f.root.join("consumer/README.md"))?, readme);
    Ok(())
}
#[test]
fn missing_house_invalid_template_and_binding_override_write_nothing() -> Result {
    let f = Fixture::new()?;
    let output = f
        .command("init", &f.root.join("consumer"))
        .arg("--yes")
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(!f.root.join("consumer").exists());
    f.publish("invalid", "content\n")?;
    assert_eq!(
        f.selected("init").arg("--yes").output()?.status.code(),
        Some(2)
    );
    assert!(!f.root.join("consumer").exists());
    let manifest = manifest(1).replace(
        "source = \"README.md\"",
        "source = \"README.md\"\npath = \".kitchen.json\"",
    );
    f.publish(&manifest, "content\n")?;
    assert_eq!(
        f.selected("init").arg("--yes").output()?.status.code(),
        Some(2)
    );
    assert!(!f.root.join("consumer").exists());
    Ok(())
}
#[test]
fn mismatched_existing_house_is_refused() -> Result {
    let f = Fixture::new()?;
    assert!(f.selected("init").arg("--yes").output()?.status.success());
    let output = f
        .command("adopt", &f.root.join("consumer"))
        .args([
            "--house",
            "other",
            "--repository",
            "crabnebula/tauri-fixture",
            "--yes",
        ])
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(fs::read_to_string(binding(&f))?.contains("crabnebula"));
    Ok(())
}
#[cfg(unix)]
#[test]
fn symlinked_ancestor_is_resolved_but_links_below_root_are_refused() -> Result {
    use std::os::unix::fs::symlink;
    let f = Fixture::new()?;
    symlink(&f.root, f.root.join("alias"))?;
    let output = f
        .command("init", &f.root.join("alias/consumer"))
        .args([
            "--house",
            "crabnebula",
            "--repository",
            "crabnebula/tauri-fixture",
            "--yes",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    fs::remove_file(f.root.join("consumer/AGENTS.md"))?;
    fs::write(f.root.join("outside"), "preserved")?;
    symlink(f.root.join("outside"), f.root.join("consumer/AGENTS.md"))?;
    let output = f.selected("adopt").arg("--yes").output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(fs::read_to_string(f.root.join("outside"))?, "preserved");
    Ok(())
}

#[test]
fn destination_created_after_preview_blocks_confirmed_apply() -> Result {
    use std::io::{BufRead, BufReader};
    let f = Fixture::new()?;
    let mut child = f
        .selected("init")
        .arg("--confirm")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut output = BufReader::new(child.stdout.take().ok_or("stdout")?);
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(
            output.read_line(&mut line)?,
            0,
            "preview must precede confirmation"
        );
        if line.contains("Nothing is written until") {
            break;
        }
    }
    fs::create_dir(f.root.join("consumer"))?;
    fs::write(f.root.join("consumer/README.md"), "concurrent local file")?;
    child.stdin.take().ok_or("stdin")?.write_all(b"yes\n")?;
    let mut remaining = String::new();
    std::io::Read::read_to_string(&mut output, &mut remaining)?;
    let result = child.wait_with_output()?;
    assert_eq!(result.status.code(), Some(1));
    assert!(remaining.contains("Apply blocked:"));
    assert!(remaining.contains("README.md"));
    assert!(remaining.contains("No files added"));
    assert_eq!(
        fs::read_to_string(f.root.join("consumer/README.md"))?,
        "concurrent local file"
    );
    assert!(!f.root.join("consumer/AGENTS.md").exists());
    assert!(!binding(&f).exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn redirected_and_invalid_target_roots_are_refused() -> Result {
    use std::os::unix::fs::symlink;
    let f = Fixture::new()?;
    fs::create_dir(f.root.join("destination"))?;
    symlink(f.root.join("destination"), f.root.join("link"))?;
    symlink(f.root.join("absent"), f.root.join("dangling"))?;
    fs::write(f.root.join("file"), "local")?;
    for (verb, target, code) in [
        ("init", "link", 1),
        ("init", "link/.", 1),
        ("init", "dangling", 1),
        ("init", "destination/../consumer", 2),
        ("init", "file", 1),
        ("init", "file/child", 1),
        ("adopt", "absent", 2),
    ] {
        let output = f
            .command(verb, &f.root.join(target))
            .args([
                "--house",
                "crabnebula",
                "--repository",
                "crabnebula/tauri-fixture",
                "--yes",
            ])
            .output()?;
        assert_eq!(output.status.code(), Some(code), "{target}: {output:?}");
    }
    assert_eq!(fs::read_dir(f.root.join("destination"))?.count(), 0);
    assert_eq!(fs::read_to_string(f.root.join("file"))?, "local");
    assert!(!f.root.join("absent").exists());
    Ok(())
}

#[test]
fn template_names_resolve_only_from_the_pinned_guidance() -> Result {
    let f = Fixture::new()?;
    let output = f.selected_for("init", "absent").arg("--yes").output()?;
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(stderr.contains("no template named absent"), "{stderr}");
    let output = f.selected_for("init", "../test").output()?;
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(!f.root.join("consumer").exists());
    let output = f.selected("init").arg("--yes").output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(
        fs::read_to_string(f.root.join("consumer/README.md"))?
            .contains(&format!("guidance-revision={}", "b".repeat(40)))
    );
    Ok(())
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn assignments_report_specific_errors_without_echoing_values() -> Result {
    let f = Fixture::new()?;
    f.publish(
        &format!(
            "{}[variables.project]\ndescription = \"Project name\"\n[variables.owner]\ndescription = \"Owning team\"\n",
            manifest(1)
        ),
        "content\n",
    )?;
    for (set, expected) in [
        (&["project"][..], "name=value"),
        (&["project=a", "project=b"][..], "assigned more than once"),
        (&["Project=secret-value"][..], "variable names are"),
        (&["project=a"][..], "owner (\"Owning team\")"),
    ] {
        let mut command = f.selected("init");
        for assignment in set {
            command.args(["--set", assignment]);
        }
        let output = command.arg("--yes").output()?;
        assert_eq!(output.status.code(), Some(2), "{set:?}: {output:?}");
        assert!(
            stderr(&output).contains(expected),
            "{set:?}: {}",
            stderr(&output)
        );
        assert!(!stderr(&output).contains("secret-value"));
        assert!(!stderr(&output).contains("house configuration"));
        assert!(!f.root.join("consumer").exists());
    }
    let output = f.selected("init").arg("--yes").output()?;
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("owner (\"Owning team\"), project (\"Project name\")"),
        "{}",
        stderr(&output)
    );
    let output = f
        .selected("init")
        .args(["--set", "project=p", "--set", "owner=o=x", "--yes"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    Ok(())
}

#[test]
fn init_refuses_a_non_empty_root_and_adopt_accepts_it() -> Result {
    let f = Fixture::new()?;
    fs::create_dir(f.root.join("consumer"))?;
    let output = f.selected("init").arg("--yes").output()?;
    assert!(output.status.success(), "empty root: {output:?}");
    assert!(binding(&f).exists());
    assert!(!f.root.join("consumer/.kitchen.json").exists());

    fs::remove_dir_all(f.root.join("consumer"))?;
    fs::create_dir(f.root.join("consumer"))?;
    fs::write(f.root.join("consumer/local.txt"), "local")?;
    let output = f.selected("init").arg("--yes").output()?;
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        stderr(&output).contains("kitchen adopt"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fs::read_dir(f.root.join("consumer"))?.count(), 1);

    let output = f.selected("adopt").arg("--yes").output()?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        fs::read_to_string(f.root.join("consumer/local.txt"))?,
        "local"
    );
    assert!(!f.root.join("consumer/.kitchen.json").exists());
    Ok(())
}

#[test]
fn a_workflow_needing_a_conflicting_justfile_is_withheld_and_nothing_activates() -> Result {
    let f = Fixture::new()?;
    let bundle_manifest = format!(
        "{}[[files]]\nsource = \"justfile\"\n[[files]]\nsource = \"ci.yml\"\npath = \".github/workflows/ci.yml\"\nrequires = [\"justfile\"]\n",
        manifest(1)
    );
    let house = HouseId::new("crabnebula")?;
    let mut bundle = InstructionBundle {
        schema: 1,
        house,
        kitchen: CommitId::new(&"a".repeat(40))?,
        role_cards_digest: role_cards_digest(),
        guidance: CommitId::new(&"f".repeat(40))?,
        entrypoint: RelativePath::new("SKILL.md")?,
        notices: [RelativePath::new("NOTICE.md")?].into(),
        assets: vec![
            asset("SKILL.md", "Apply the house rules.")?,
            asset("NOTICE.md", "Synthetic fixture notice.")?,
            asset("templates/test/template.toml", &bundle_manifest)?,
        ],
    };
    for (path, contents) in [
        ("AGENTS.md", "agents\n"),
        ("README.md", "readme\n"),
        ("justfile", "check:\n    cargo test\n"),
        ("ci.yml", "on: push\njobs: {}\n"),
    ] {
        bundle
            .assets
            .push(asset(&format!("templates/test/files/{path}"), contents)?);
    }
    f.install(&bundle)?;
    fs::create_dir(f.root.join("consumer"))?;
    fs::write(f.root.join("consumer/justfile"), "check:\n    echo local\n")?;

    let output = f.selected("adopt").output()?;
    let preview = stdout(&output);
    assert!(
        preview.contains("  withheld   .github/workflows/ci.yml: requires justfile"),
        "{preview}"
    );
    let output = f.selected("adopt").arg("--yes").output()?;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(!f.root.join("consumer/.github").exists());
    assert!(!f.root.join("consumer/.git").exists());
    assert_eq!(
        fs::read_to_string(f.root.join("consumer/justfile"))?,
        "check:\n    echo local\n"
    );
    assert!(f.root.join("consumer/README.md").exists());
    Ok(())
}

#[test]
fn adopt_retains_existing_workflows_checks_and_reviewers() -> Result {
    let f = Fixture::new()?;
    assert!(f.selected("init").arg("--yes").output()?.status.success());
    let path = binding(&f);
    let mut stored: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    stored["workflows"] = serde_json::json!(["gate", "pickup"]);
    stored["additionalChecks"] = serde_json::json!(["local-check"]);
    stored["additionalReviewers"] = serde_json::json!(["local-reviewer"]);
    let stricter = serde_json::to_string_pretty(&stored)?;
    fs::write(&path, &stricter)?;
    fs::remove_file(f.root.join("consumer/README.md"))?;
    let output = f
        .command("adopt", &f.root.join("consumer"))
        .args(["--repository", "crabnebula/tauri-fixture", "--yes"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("house crabnebula: unchanged"));
    assert_eq!(fs::read_to_string(&path)?, stricter);
    assert!(f.root.join("consumer/README.md").exists());
    Ok(())
}

#[test]
fn selection_mismatches_and_partial_selection_write_nothing() -> Result {
    let f = Fixture::new()?;
    // An unbound repository needs both house and repository.
    let output = f
        .command("init", &f.root.join("consumer"))
        .args(["--house", "crabnebula", "--yes"])
        .output()?;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(!f.root.join("consumer").exists());
    let output = f
        .command("init", &f.root.join("consumer"))
        .args(["--repository", "crabnebula/tauri-fixture", "--yes"])
        .output()?;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(!f.root.join("consumer").exists());
    // A repository outside the house allowlist is refused.
    let output = f
        .command("init", &f.root.join("consumer"))
        .args([
            "--house",
            "crabnebula",
            "--repository",
            "crabnebula/other",
            "--yes",
        ])
        .output()?;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(!f.root.join("consumer").exists());
    // An unbound repository selection still needs a house after another is bound.
    assert!(f.selected("init").arg("--yes").output()?.status.success());
    let stored = fs::read(binding(&f))?;
    fs::remove_file(f.root.join("consumer/README.md"))?;
    let output = f
        .command("adopt", &f.root.join("consumer"))
        .args(["--repository", "crabnebula/other", "--yes"])
        .output()?;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(fs::read(binding(&f))?, stored);
    assert!(
        !f.root
            .join("registry/repositories/crabnebula/other.json")
            .exists()
    );
    assert!(!f.root.join("consumer/README.md").exists());
    Ok(())
}

#[test]
fn a_template_declaring_another_house_is_refused() -> Result {
    let f = Fixture::new()?;
    f.publish(
        &manifest(1).replace("house = \"crabnebula\"", "house = \"origin89\""),
        "content\n",
    )?;
    let output = f.selected("init").arg("--yes").output()?;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        stderr(&output).contains("template belongs to house origin89"),
        "{}",
        stderr(&output)
    );
    assert!(!f.root.join("consumer").exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn printed_doctor_command_survives_quotes_and_spaces_in_paths() -> Result {
    let f = Fixture::new()?;
    let target = f.root.join("it's a \"consumer\" $HOME");
    let output = f
        .command_for("init", &target, "test")
        .args([
            "--house",
            "crabnebula",
            "--repository",
            "crabnebula/tauri-fixture",
            "--yes",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    let command = text
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("kitchen house doctor "))
        .ok_or_else(|| format!("no doctor command in {text}"))?;
    let echoed = Command::new("sh")
        .arg("-c")
        .arg(format!("printf '%s\\n' {command}"))
        .env_remove("HOME")
        .output()?;
    assert!(echoed.status.success(), "{echoed:?}");
    let registry = f.root.join("registry");
    assert_eq!(
        String::from_utf8(echoed.stdout)?,
        format!(
            "--registry\n{}\n--repository-path\n{}\n",
            registry.display(),
            target.display()
        )
    );
    Ok(())
}

#[test]
fn adopt_in_a_checkout_reads_its_remote_and_adds_only_template_files() -> Result {
    let f = Fixture::new()?;
    let consumer = f.root.join("consumer");
    fs::create_dir(&consumer)?;
    for args in [
        vec!["init", "--quiet"],
        vec![
            "remote",
            "add",
            "origin",
            "git@github.com:crabnebula/tauri-fixture.git",
        ],
    ] {
        let status = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .arg("-C")
            .arg(&consumer)
            .args(&args)
            .status()?;
        assert!(status.success());
    }
    let output = f
        .command("adopt", &consumer)
        .args(["--house", "crabnebula", "--yes"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(binding(&f).is_file());
    let mut names: Vec<_> = fs::read_dir(&consumer)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::result::Result<_, _>>()?;
    names.sort();
    assert_eq!(names, [".git", "AGENTS.md", "README.md"]);
    Ok(())
}
