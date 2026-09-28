//! Real CLI, disposable local houses; no network or runtime activation.
use kitchen::{adoption::HouseRegistry, house::HouseConfig};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
type Result = std::result::Result<(), Box<dyn std::error::Error>>;
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
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
        fs::create_dir_all(root.join("template/files"))?;
        let fixture = Self { _temp: temp, root };
        fixture.revision(1)?;
        Ok(fixture)
    }
    fn revision(&self, revision: u32) -> Result {
        fs::write(
            self.root.join("template/template.toml"),
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
            ),
        )?;
        for name in ["AGENTS.md", "README.md"] {
            fs::write(
                self.root.join("template/files").join(name),
                format!("content revision {revision}\n"),
            )?;
        }
        Ok(())
    }
    fn command(&self, verb: &str, target: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kitchen"));
        command
            .arg(verb)
            .arg(target)
            .arg("--registry")
            .arg(self.root.join("registry"))
            .arg("--template")
            .arg(self.root.join("template"));
        command
    }
    fn selected(&self, verb: &str) -> Command {
        let mut command = self.command(verb, &self.root.join("consumer"));
        command.args([
            "--house",
            "crabnebula",
            "--repository",
            "crabnebula/tauri-fixture",
        ]);
        command
    }
}
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}
#[test]
fn preview_apply_and_bound_rerun() -> Result {
    let f = Fixture::new()?;
    let output = f.selected("init").output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("3 to add"));
    assert!(!f.root.join("consumer").exists());
    let output = f.selected("init").arg("--yes").output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(
        fs::read_to_string(f.root.join("consumer/AGENTS.md"))?.ends_with("content revision 1\n")
    );
    let output = f
        .command("adopt", &f.root.join("consumer"))
        .arg("--yes")
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("0 to add, 3 unchanged, 0 conflicts"));
    assert!(!f.root.join("consumer/.git").exists());
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
        assert_eq!(f.root.join("consumer/.kitchen.json").exists(), applied);
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
    assert!(stdout(&output).contains("with local edits"));
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
    fs::write(f.root.join("template/template.toml"), "invalid")?;
    assert_eq!(
        f.selected("init").arg("--yes").output()?.status.code(),
        Some(2)
    );
    assert!(!f.root.join("consumer").exists());
    f.revision(1)?;
    let manifest = fs::read_to_string(f.root.join("template/template.toml"))?.replace(
        "source = \"README.md\"",
        "source = \"README.md\"\npath = \".kitchen.json\"",
    );
    fs::write(f.root.join("template/template.toml"), manifest)?;
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
        .args(["--house", "other", "--yes"])
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(fs::read_to_string(f.root.join("consumer/.kitchen.json"))?.contains("crabnebula"));
    Ok(())
}
#[cfg(unix)]
#[test]
fn symlinked_root_is_resolved_but_links_below_root_are_refused() -> Result {
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
    assert!(!f.root.join("consumer/.kitchen.json").exists());
    Ok(())
}
