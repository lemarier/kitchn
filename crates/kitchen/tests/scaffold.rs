//! House templates: loading, rendering, provenance, file plans, and applying
//! them through the create-only installer, all against isolated temporary
//! targets. Targets are canonicalized because the installer refuses paths
//! reached through a symbolic link, such as `/var` on macOS.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use kitchen::{
    Error, ErrorClass, HouseId,
    adoption::{FileMode, FileStatus, NewFile, RelativePath, install_new_files},
    contracts::CommitId,
    house::HouseError,
    scaffold::{
        Conflict, FilePlan, MAX_RENDERED_BYTES, ManagedState, Manifest, PlanAction, PlanKind,
        RenderedTemplate, ScaffoldError, ScaffoldLimit, Template, TemplateProblem, VariableName,
        inspect_managed,
    },
};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repository_root() -> PathBuf {
    manifest_dir().join("../..")
}

fn origin89_template() -> TestResult<Template> {
    Ok(Template::load(
        &manifest_dir().join("tests/fixtures/houses/origin89/rust-workspace"),
    )?)
}

fn crabnebula_template() -> TestResult<Template> {
    Ok(Template::load(
        &manifest_dir().join("tests/fixtures/houses/crabnebula/tauri-app"),
    )?)
}

fn example_template() -> TestResult<Template> {
    Ok(Template::load(
        &repository_root().join("templates/example"),
    )?)
}

fn guidance(digit: char) -> TestResult<CommitId> {
    Ok(CommitId::new(&digit.to_string().repeat(40))?)
}

fn vars(pairs: &[(&str, &str)]) -> TestResult<BTreeMap<VariableName, String>> {
    pairs
        .iter()
        .map(|(name, value)| Ok((name.parse()?, (*value).to_owned())))
        .collect()
}

/// Values that reproduce this repository from the Origin89 fixture.
fn kitchen_vars() -> TestResult<BTreeMap<VariableName, String>> {
    vars(&[
        ("project_name", "Kitchen"),
        ("summary", "Portable agent workflows."),
        ("crate_name", "kitchen"),
        ("copyright_holder", "lemarier"),
        ("copyright_year", "2026"),
        ("security_contact", "david@lemarier.ca"),
        ("repository_url", "https://github.com/lemarier/kitchen"),
    ])
}

fn render_origin89(digit: char) -> TestResult<RenderedTemplate> {
    Ok(origin89_template()?.render(
        &HouseId::new("origin89")?,
        &guidance(digit)?,
        &kitchen_vars()?,
    )?)
}

fn render_example(digit: char) -> TestResult<RenderedTemplate> {
    Ok(example_template()?.render(
        &HouseId::new("example")?,
        &guidance(digit)?,
        &vars(&[("project_name", "Demo")])?,
    )?)
}

fn real(dir: &TempDir) -> TestResult<PathBuf> {
    Ok(dir.path().canonicalize()?)
}

/// Install only the first `count` additions, as an interrupted apply would.
fn partial_apply(plan: &FilePlan, count: usize) -> TestResult {
    let files: Vec<NewFile<'_>> = plan
        .additions()
        .take(count)
        .map(|file| NewFile {
            path: &file.path,
            contents: file.contents.as_bytes(),
            mode: file.mode,
        })
        .collect();
    install_new_files(plan.target(), &files)?;
    Ok(())
}

fn action<'a>(plan: &'a FilePlan, path: &str) -> Option<&'a PlanAction> {
    plan.files()
        .iter()
        .find(|planned| planned.file.path.as_str() == path)
        .map(|planned| &planned.action)
}

fn contents<'a>(rendered: &'a RenderedTemplate, path: &str) -> Option<&'a str> {
    rendered
        .files
        .iter()
        .find(|file| file.path.as_str() == path)
        .map(|file| file.contents.as_str())
}

/// A template directory built from a manifest and `(source, text)` pairs.
fn template_dir(manifest: &str, sources: &[(&str, &str)]) -> TestResult<TempDir> {
    let dir = TempDir::new()?;
    fs::write(dir.path().join("template.toml"), manifest)?;
    fs::create_dir(dir.path().join("files"))?;
    for (source, text) in sources {
        let path = dir.path().join("files").join(source);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, text)?;
    }
    Ok(dir)
}

const MINIMAL: &str = r#"
schema = 1
name = "minimal"
house = "home"
revision = 1
description = "test"
"#;

fn minimal_with(files: &str) -> String {
    format!("{MINIMAL}{files}")
}

fn load_error(manifest: &str, sources: &[(&str, &str)]) -> TestResult<ScaffoldError> {
    let dir = template_dir(manifest, sources)?;
    match Template::load(dir.path()) {
        Ok(_) => Err("template unexpectedly loaded".into()),
        Err(error) => Ok(error),
    }
}

fn render_minimal(
    manifest: &str,
    sources: &[(&str, &str)],
    variables: &[(&str, &str)],
) -> TestResult<Result<RenderedTemplate, ScaffoldError>> {
    let dir = template_dir(manifest, sources)?;
    let template = Template::load(dir.path())?;
    Ok(template.render(&HouseId::new("home")?, &guidance('a')?, &vars(variables)?))
}

#[test]
fn new_repository_plan_adds_every_file_and_previews_it() -> TestResult {
    let workspace = TempDir::new()?;
    let target = real(&workspace)?.join("new-repo");
    let plan = FilePlan::new(render_origin89('a')?, &target)?;

    assert_eq!(plan.kind(), PlanKind::NewRepository);
    assert!(
        plan.files()
            .iter()
            .all(|planned| planned.action == PlanAction::Add)
    );
    assert_eq!(plan.additions().count(), plan.files().len());
    assert!(
        plan.files()
            .iter()
            .any(|planned| planned.file.path.as_str() == "crates/kitchen/src/lib.rs")
    );
    let preview = plan.to_string();
    assert!(preview.starts_with("Template origin89/rust-workspace revision 1, guidance aaaa"));
    assert!(preview.contains("(new repository)"));
    assert!(preview.contains("  add        .github/workflows/check.yml\n"));
    assert!(preview.contains(&format!(
        "{} to add, 0 unchanged, 0 conflicts.",
        plan.files().len()
    )));
    assert!(!target.exists(), "planning must not create the target");

    let report = plan.apply()?;
    assert!(
        report
            .files
            .iter()
            .all(|file| file.status == FileStatus::Created)
    );
    let agents = fs::read_to_string(target.join("AGENTS.md"))?;
    assert_eq!(
        Some(agents.as_str()),
        plan.files()
            .iter()
            .find(|p| p.file.path.as_str() == "AGENTS.md")
            .map(|p| p.file.contents.as_str())
    );
    Ok(())
}

#[test]
fn adoption_reports_conflicts_and_never_touches_existing_files() -> TestResult {
    let workspace = TempDir::new()?;
    let target = real(&workspace)?;
    let rendered = render_example('a')?;
    let agents = contents(&rendered, "AGENTS.md").ok_or("AGENTS.md rendered")?;
    fs::write(target.join("README.md"), "# Local readme\n")?;
    fs::write(target.join("AGENTS.md"), agents)?;
    fs::create_dir(target.join("CLAUDE.md"))?;

    let plan = FilePlan::new(rendered, &target)?;

    assert_eq!(plan.kind(), PlanKind::Adoption);
    assert_eq!(
        action(&plan, "README.md"),
        Some(&PlanAction::Conflict(Conflict::Unmanaged))
    );
    assert_eq!(action(&plan, "AGENTS.md"), Some(&PlanAction::Unchanged));
    assert_eq!(
        action(&plan, "CLAUDE.md"),
        Some(&PlanAction::Conflict(Conflict::NotRegularFile))
    );
    assert_eq!(action(&plan, ".gitignore"), Some(&PlanAction::Add));
    let added: Vec<&str> = plan.additions().map(|file| file.path.as_str()).collect();
    assert_eq!(added, [".gitignore"]);
    assert_eq!(plan.conflicts().count(), 2);
    let preview = plan.to_string();
    assert!(preview.contains("  conflict   README.md: existing file differs; left untouched\n"));
    assert!(preview.contains("  unchanged  AGENTS.md\n"));
    assert!(preview.contains("1 to add, 1 unchanged, 2 conflicts."));
    assert!(!target.join(".gitignore").exists());

    // Applying creates the addition and leaves every conflict untouched.
    let report = plan.apply()?;
    assert_eq!(report.files.len(), 1);
    assert_eq!(
        report.files.first().map(|file| file.status),
        Some(FileStatus::Created)
    );
    assert!(target.join(".gitignore").is_file());
    assert_eq!(
        fs::read_to_string(target.join("README.md"))?,
        "# Local readme\n"
    );
    assert!(target.join("CLAUDE.md").is_dir());
    Ok(())
}

#[test]
fn rerun_after_apply_is_a_noop_and_resumes_after_partial_apply() -> TestResult {
    let workspace = TempDir::new()?;
    let target = real(&workspace)?;
    let first = FilePlan::new(render_origin89('a')?, &target)?;
    let total = first.files().len();

    // An interrupted apply created only some files; the rerun adds the rest.
    partial_apply(&first, 3)?;
    let resumed = FilePlan::new(render_origin89('a')?, &target)?;
    assert_eq!(resumed.additions().count(), total - 3);
    assert_eq!(resumed.conflicts().count(), 0);
    resumed.apply()?;

    let rerun = FilePlan::new(render_origin89('a')?, &target)?;
    assert!(rerun.is_noop());
    assert!(
        rerun
            .files()
            .iter()
            .all(|planned| planned.action == PlanAction::Unchanged)
    );
    let report = rerun.apply()?;
    assert!(report.files.is_empty());
    Ok(())
}

#[test]
fn apply_writes_nothing_if_a_planned_path_appeared_since_planning() -> TestResult {
    let workspace = TempDir::new()?;
    let target = real(&workspace)?;
    let plan = FilePlan::new(render_example('a')?, &target)?;
    fs::write(target.join("README.md"), "written by someone else\n")?;

    let report = plan.apply()?;

    assert!(report.has_conflicts());
    assert_eq!(
        fs::read_to_string(target.join("README.md"))?,
        "written by someone else\n"
    );
    assert!(!target.join("AGENTS.md").exists());
    Ok(())
}

#[test]
fn provenance_separates_unedited_managed_files_from_local_edits() -> TestResult {
    let workspace = TempDir::new()?;
    let target = real(&workspace)?;
    FilePlan::new(render_example('a')?, &target)?.apply()?;
    let agents = target.join("AGENTS.md");
    let edited = format!("{}Local rule.\n", fs::read_to_string(&agents)?);
    fs::write(&agents, edited)?;

    // A new guidance revision changes every managed marker.
    let plan = FilePlan::new(render_example('b')?, &target)?;

    let Some(PlanAction::Conflict(Conflict::ManagedEdited(marker))) = action(&plan, "AGENTS.md")
    else {
        return Err(format!("AGENTS.md: {:?}", action(&plan, "AGENTS.md")).into());
    };
    assert_eq!(marker.provenance.guidance, guidance('a')?);
    assert_eq!(marker.provenance.house.as_str(), "example");
    assert_eq!(marker.provenance.template.as_str(), "example");
    assert_eq!(marker.provenance.revision.get(), 1);
    assert!(matches!(
        action(&plan, "CLAUDE.md"),
        Some(PlanAction::Conflict(Conflict::ManagedPristine(_)))
    ));
    // Unmarked files are unchanged because their content is revision-independent.
    assert_eq!(action(&plan, "README.md"), Some(&PlanAction::Unchanged));
    assert!(plan.is_noop());
    assert!(fs::read_to_string(&agents)?.ends_with("Local rule.\n"));
    Ok(())
}

#[test]
fn markers_record_house_template_and_both_revisions() -> TestResult {
    let rendered = render_origin89('c')?;
    let agents = contents(&rendered, "AGENTS.md").ok_or("AGENTS.md rendered")?;
    let first_line = agents.lines().next().unwrap_or_default();
    assert!(first_line.starts_with(&format!(
        "<!-- kitchen-managed: house=origin89 template=rust-workspace template-revision=1 guidance-revision={} content-sha256=",
        "c".repeat(40)
    )));
    let ManagedState::Pristine(marker) = inspect_managed(agents) else {
        return Err("rendered file should be pristine".into());
    };
    assert_eq!(marker.provenance, rendered.provenance);
    // Unmarked outputs stay byte-for-byte what the source renders.
    assert!(
        contents(&rendered, "justfile")
            .ok_or("justfile rendered")?
            .starts_with("default:\n")
    );
    Ok(())
}

#[test]
fn malformed_markers_read_as_unmanaged() {
    let digest = "0".repeat(64);
    let valid_fields = format!(
        "house=h template=t template-revision=1 guidance-revision={} content-sha256={digest}",
        "a".repeat(40)
    );
    for content in [
        String::new(),
        "no newline".to_owned(),
        "# plain comment\nbody".to_owned(),
        format!("<!-- kitchen-managed: {valid_fields}\nbody"),
        format!("<!-- kitchen-managed: {valid_fields} extra=1 -->\nbody"),
        format!(
            "# kitchen-managed: {}\nbody",
            valid_fields.replace("=1 ", "=0 ")
        ),
        format!(
            "# kitchen-managed: {}\nbody",
            valid_fields.replace(&digest, &"G".repeat(64))
        ),
        format!("# kitchen-managed: {valid_fields}\r\nbody"),
    ] {
        assert_eq!(
            inspect_managed(&content),
            ManagedState::Unmanaged,
            "{content:?}"
        );
    }
    assert!(matches!(
        inspect_managed(&format!("# kitchen-managed: {valid_fields}\nbody")),
        ManagedState::Edited(_)
    ));
}

#[test]
fn second_house_renders_without_origin89_rules_or_names() -> TestResult {
    let house = HouseId::new("crabnebula")?;
    let rendered = crabnebula_template()?.render(
        &house,
        &guidance('d')?,
        &vars(&[("app_name", "fleet"), ("product_name", "Fleet")])?,
    )?;

    let forbidden = ["origin89", "lemarier", "skills-sync", "kitchen/"];
    for file in &rendered.files {
        let path = file.path.as_str().to_ascii_lowercase();
        let text = file.contents.to_ascii_lowercase();
        for word in forbidden {
            assert!(!path.contains(word), "{word} in path {path}");
            assert!(!text.contains(word), "{word} in {path}");
        }
    }
    let agents = contents(&rendered, "AGENTS.md").ok_or("AGENTS.md rendered")?;
    assert!(agents.contains("#[specta::specta]"));
    assert!(agents.contains("Follow the crabnebula house rules"));
    assert!(
        contents(&rendered, "justfile")
            .ok_or("justfile rendered")?
            .starts_with(
                "# kitchen-managed: house=crabnebula template=tauri-app template-revision=3 "
            )
    );
    let script = rendered
        .files
        .iter()
        .find(|file| file.path.as_str() == "scripts/bindings.sh")
        .ok_or("bindings script rendered")?;
    assert_eq!(script.mode, FileMode::Executable);
    assert!(
        rendered
            .files
            .iter()
            .any(|file| file.path.as_str() == "apps/fleet/src-tauri/Cargo.toml")
    );
    Ok(())
}

#[test]
fn kitchen_example_template_carries_no_house_policy() -> TestResult {
    for file in render_example('a')?.files {
        let text = file.contents.to_ascii_lowercase();
        for word in ["origin89", "crabnebula", "lemarier", "skills-sync"] {
            assert!(!text.contains(word), "{word} in {}", file.path.as_str());
        }
    }
    Ok(())
}

#[test]
fn templates_render_only_for_their_own_house() -> TestResult {
    let error = origin89_template()?
        .render(
            &HouseId::new("crabnebula")?,
            &guidance('a')?,
            &kitchen_vars()?,
        )
        .err()
        .ok_or("cross-house render must fail")?;
    assert_eq!(
        error,
        ScaffoldError::CrossHouse {
            selected: HouseId::new("crabnebula")?,
            template: HouseId::new("origin89")?,
        }
    );
    assert_eq!(Error::from(error).class(), ErrorClass::Refused);
    Ok(())
}

#[test]
fn variables_are_declared_required_and_single_line() -> TestResult {
    let manifest = minimal_with(
        r#"
[variables.name]
description = "required"
[variables.flavor]
description = "optional"
default = "plain"
[[files]]
source = "out.tera"
"#,
    );
    let sources = [("out.tera", "{{ vars.name }}-{{ vars.flavor }}")];
    let rendered = render_minimal(&manifest, &sources, &[("name", "x")])??;
    assert_eq!(contents(&rendered, "out"), Some("x-plain"));

    let missing = render_minimal(&manifest, &sources, &[])?;
    assert_eq!(
        missing,
        Err(ScaffoldError::MissingVariable {
            name: "name".parse()?
        })
    );
    let unknown = render_minimal(&manifest, &sources, &[("name", "x"), ("other", "y")])?;
    assert_eq!(
        unknown,
        Err(ScaffoldError::UnknownVariable {
            name: "other".parse()?
        })
    );
    let multiline = render_minimal(&manifest, &sources, &[("name", "x\ny")])?;
    assert_eq!(
        multiline,
        Err(ScaffoldError::InvalidVariableValue {
            name: "name".parse()?
        })
    );
    let longest = "v".repeat(kitchen::scaffold::MAX_VARIABLE_BYTES);
    assert!(render_minimal(&manifest, &sources, &[("name", &longest)])?.is_ok());
    let oversized = format!("{longest}v");
    assert_eq!(
        render_minimal(&manifest, &sources, &[("name", &oversized)])?,
        Err(ScaffoldError::Limit {
            limit: ScaffoldLimit::VariableBytes
        })
    );
    for invalid in ["", "Name", "1st", "with-dash", "x".repeat(65).as_str()] {
        assert_eq!(
            VariableName::new(invalid),
            Err(ScaffoldError::InvalidVariableName)
        );
    }
    Ok(())
}

#[test]
fn output_paths_cannot_escape_or_address_git_metadata() -> TestResult {
    for path in [
        "../escape",
        "/etc/passwd",
        ".git/hooks/pre-commit",
        "sub/.GIT/config",
        "a/../../b",
        "a//b",
        "C:/windows",
    ] {
        let manifest = minimal_with(&format!("[[files]]\nsource = \"out\"\npath = \"{path}\"\n"));
        let error = render_minimal(&manifest, &[("out", "x")], &[])?;
        assert_eq!(
            error,
            Err(ScaffoldError::InvalidOutputPath {
                source_path: RelativePath::new("out")?,
            }),
            "{path}"
        );
    }
    // A variable cannot smuggle a traversal into a rendered path either.
    let manifest = minimal_with(
        r#"
[variables.dir]
description = "directory"
[[files]]
source = "out"
path = "crates/{{ vars.dir }}/lib.rs"
"#,
    );
    let error = render_minimal(&manifest, &[("out", "x")], &[("dir", "../../../home")])?;
    assert_eq!(
        error,
        Err(ScaffoldError::InvalidOutputPath {
            source_path: RelativePath::new("out")?,
        })
    );
    // Source paths in the manifest are validated when it parses.
    let traversal = minimal_with("[[files]]\nsource = \"../outside\"\n");
    assert!(matches!(
        Manifest::parse(&traversal),
        Err(ScaffoldError::Manifest { .. })
    ));
    Ok(())
}

#[test]
fn inconsistent_templates_are_rejected() -> TestResult {
    let one_file = minimal_with("[[files]]\nsource = \"a\"\n");
    assert_eq!(
        load_error(&one_file, &[("a", "x"), ("b", "y")])?,
        ScaffoldError::Template {
            problem: TemplateProblem::UnlistedSource(RelativePath::new("b")?)
        }
    );
    assert_eq!(
        load_error(&one_file, &[])?,
        ScaffoldError::Template {
            problem: TemplateProblem::MissingSource(RelativePath::new("a")?)
        }
    );
    let twice = minimal_with("[[files]]\nsource = \"a\"\n[[files]]\nsource = \"a\"\n");
    assert_eq!(
        load_error(&twice, &[("a", "x")])?,
        ScaffoldError::Template {
            problem: TemplateProblem::DuplicateSource(RelativePath::new("a")?)
        }
    );
    let syntax = load_error(&one_file, &[("a", "{{ unclosed")])?;
    assert!(matches!(syntax, ScaffoldError::Syntax { ref path, .. } if path.as_str() == "a"));
    // Output paths are templates too, parsed at load time even for verbatim sources.
    let bad_path = minimal_with("[[files]]\nsource = \"a\"\npath = \"{{ oops\"\nrender = false\n");
    assert!(matches!(
        load_error(&bad_path, &[("a", "x")])?,
        ScaffoldError::Syntax { ref path, .. } if path.as_str() == "a"
    ));
    assert_eq!(Error::from(syntax).class(), ErrorClass::InvalidInput);

    let empty = format!("{MINIMAL}files = []\n");
    assert_eq!(
        load_error(&empty, &[])?,
        ScaffoldError::Template {
            problem: TemplateProblem::Empty
        }
    );
    let future = minimal_with("[[files]]\nsource = \"a\"\n").replace("schema = 1", "schema = 2");
    assert_eq!(
        load_error(&future, &[("a", "x")])?,
        ScaffoldError::Template {
            problem: TemplateProblem::UnsupportedSchema(2)
        }
    );
    let unknown_field = minimal_with("[[files]]\nsource = \"a\"\noverwrite = true\n");
    assert!(matches!(
        load_error(&unknown_field, &[("a", "x")])?,
        ScaffoldError::Manifest { .. }
    ));
    let bad_house = one_file.replace("house = \"home\"", "house = \"../home\"");
    assert!(matches!(
        load_error(&bad_house, &[("a", "x")])?,
        ScaffoldError::Manifest { .. }
    ));
    Ok(())
}

#[test]
fn colliding_outputs_are_rejected() -> TestResult {
    let duplicate = minimal_with(
        "[[files]]\nsource = \"a\"\npath = \"out\"\n[[files]]\nsource = \"b\"\npath = \"out\"\n",
    );
    assert_eq!(
        render_minimal(&duplicate, &[("a", "x"), ("b", "y")], &[])?,
        Err(ScaffoldError::Template {
            problem: TemplateProblem::DuplicateOutput(RelativePath::new("out")?)
        })
    );
    // `dir/f.txt` sorts between `dir/f` and `dir/f/x`; nesting is still found.
    let nested = minimal_with(
        "[[files]]\nsource = \"a\"\npath = \"dir/f\"\n[[files]]\nsource = \"b\"\npath = \"dir/f.txt\"\n[[files]]\nsource = \"c\"\npath = \"dir/f/x\"\n",
    );
    assert_eq!(
        render_minimal(&nested, &[("a", "1"), ("b", "2"), ("c", "3")], &[])?,
        Err(ScaffoldError::Template {
            problem: TemplateProblem::OutputNesting {
                parent: RelativePath::new("dir/f")?,
                child: RelativePath::new("dir/f/x")?,
            }
        })
    );
    Ok(())
}

#[test]
fn template_and_output_sizes_are_bounded() -> TestResult {
    let one_file = minimal_with("[[files]]\nsource = \"a\"\n");
    let oversized = "x".repeat(usize::try_from(kitchen::scaffold::MAX_SOURCE_BYTES)? + 1);
    assert_eq!(
        load_error(&one_file, &[("a", &oversized)])?,
        ScaffoldError::Limit {
            limit: ScaffoldLimit::SourceBytes
        }
    );
    let in_memory = Template::from_parts(
        Manifest::parse(&one_file)?,
        BTreeMap::from([(RelativePath::new("a")?, oversized)]),
    );
    assert_eq!(
        in_memory.err(),
        Some(ScaffoldError::Limit {
            limit: ScaffoldLimit::SourceBytes
        })
    );
    let runaway = "{% for i in range(end=100000) %}{{ i }}-padding-padding{% endfor %}";
    assert_eq!(
        render_minimal(&one_file, &[("a", runaway)], &[])?,
        Err(ScaffoldError::Limit {
            limit: ScaffoldLimit::RenderedBytes
        })
    );
    let long_path = minimal_with(
        "[[files]]\nsource = \"a\"\npath = \"{% for i in range(end=100000) %}d/{% endfor %}f\"\n",
    );
    assert_eq!(
        render_minimal(&long_path, &[("a", "x")], &[])?,
        Err(ScaffoldError::InvalidOutputPath {
            source_path: RelativePath::new("a")?,
        })
    );
    let exact = "x".repeat(MAX_RENDERED_BYTES);
    let rendered = render_minimal(&one_file, &[("a", &exact)], &[])??;
    assert_eq!(
        contents(&rendered, "a").map(str::len),
        Some(MAX_RENDERED_BYTES)
    );
    Ok(())
}

#[test]
fn verbatim_sources_are_not_interpreted() -> TestResult {
    let manifest = minimal_with("[[files]]\nsource = \"ci.yml\"\nrender = false\n");
    let workflow = "run: echo ${{ github.ref }} {% raw %}\n";
    let rendered = render_minimal(&manifest, &[("ci.yml", workflow)], &[])??;
    assert_eq!(contents(&rendered, "ci.yml"), Some(workflow));
    Ok(())
}

#[test]
fn targets_must_be_absolute_real_directories() -> TestResult {
    let relative = FilePlan::new(render_example('a')?, Path::new("relative/repo"));
    assert!(matches!(
        relative,
        Err(Error::Scaffold(ScaffoldError::RelativeTarget))
    ));

    let workspace = TempDir::new()?;
    let file = real(&workspace)?.join("file");
    fs::write(&file, "x")?;
    let error = FilePlan::new(render_example('a')?, &file)
        .err()
        .ok_or("file target")?;
    assert!(matches!(
        error,
        Error::Scaffold(ScaffoldError::UntrustedTarget { .. })
    ));
    assert_eq!(error.class(), ErrorClass::Refused);
    Ok(())
}

#[cfg(unix)]
#[test]
fn symbolic_links_are_never_followed() -> TestResult {
    use std::os::unix::fs::symlink;

    let workspace = TempDir::new()?;
    let root = real(&workspace)?;
    let outside = root.join("outside");
    fs::create_dir(&outside)?;
    let refused = |target: &Path| -> TestResult<bool> {
        Ok(matches!(
            FilePlan::new(render_origin89('a')?, target),
            Err(Error::Scaffold(ScaffoldError::UntrustedTarget { .. })
                | Error::House(HouseError::RedirectedPath))
        ))
    };
    // A linked target, or one reached through a link, is refused.
    let linked = root.join("linked");
    symlink(&outside, &linked)?;
    assert!(refused(&linked)?);
    assert!(refused(&linked.join("repo"))?);

    // Inside the target, a linked directory or file refuses the whole plan.
    for (name, destination) in [
        (".github", outside.clone()),
        ("justfile", outside.join("f")),
    ] {
        let target = root.join(format!("repo-{}", name.trim_start_matches('.')));
        fs::create_dir(&target)?;
        symlink(&destination, target.join(name))?;
        assert!(refused(&target)?, "{name}");
    }
    assert!(fs::read_dir(&outside)?.next().is_none());

    // A file where a directory belongs cannot be inspected through; the plan fails closed.
    let target = root.join("repo-file-parent");
    fs::create_dir(&target)?;
    fs::write(target.join("crates"), "a file where a directory belongs")?;
    let error = FilePlan::new(render_origin89('a')?, &target)
        .err()
        .ok_or("file parent")?;
    assert!(matches!(error, Error::House(HouseError::Io(_))));

    // A linked entry inside a template is rejected when it loads.
    let dir = template_dir(&minimal_with("[[files]]\nsource = \"a\"\n"), &[("a", "x")])?;
    symlink(dir.path().join("files/a"), dir.path().join("files/b"))?;
    assert_eq!(
        Template::load(dir.path()).err(),
        Some(ScaffoldError::Template {
            problem: TemplateProblem::NotRegularFile(RelativePath::new("b")?)
        })
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_mode_mismatch_is_a_conflict() -> TestResult {
    let rendered = crabnebula_template()?.render(
        &HouseId::new("crabnebula")?,
        &guidance('a')?,
        &vars(&[("app_name", "fleet"), ("product_name", "Fleet")])?,
    )?;
    let script = contents(&rendered, "scripts/bindings.sh").ok_or("script rendered")?;
    let workspace = TempDir::new()?;
    let target = real(&workspace)?;
    fs::create_dir(target.join("scripts"))?;
    fs::write(target.join("scripts/bindings.sh"), script)?;

    let plan = FilePlan::new(rendered, &target)?;
    assert_eq!(
        action(&plan, "scripts/bindings.sh"),
        Some(&PlanAction::Conflict(Conflict::ModeDiffers {
            planned: FileMode::Executable
        }))
    );

    // Applied from scratch, the script is created executable and reruns clean.
    let fresh = TempDir::new()?;
    let fresh_target = real(&fresh)?;
    let rendered = crabnebula_template()?.render(
        &HouseId::new("crabnebula")?,
        &guidance('a')?,
        &vars(&[("app_name", "fleet"), ("product_name", "Fleet")])?,
    )?;
    FilePlan::new(rendered.clone(), &fresh_target)?.apply()?;
    let mode = fs::metadata(fresh_target.join("scripts/bindings.sh"))?.permissions();
    assert_ne!(std::os::unix::fs::PermissionsExt::mode(&mode) & 0o111, 0);
    assert!(FilePlan::new(rendered, &fresh_target)?.is_noop());
    Ok(())
}

#[test]
fn missing_template_is_an_execution_error() -> TestResult {
    let workspace = TempDir::new()?;
    let error = Template::load(&workspace.path().join("absent"))
        .err()
        .ok_or("absent template")?;
    assert!(matches!(error, ScaffoldError::Io { .. }));
    assert_eq!(Error::from(error).class(), ErrorClass::Execution);
    Ok(())
}

/// Kitchen's files that differ from the Origin89 template, and why. A rendered
/// file not listed here must match this repository byte for byte.
const DOCUMENTED_DIFFERENCES: &[(&str, &str)] = &[
    (
        "README.md",
        "Kitchen documents its own purpose and bootstrap commands",
    ),
    ("AGENTS.md", "Kitchen adds its module map and brigade rules"),
    ("CLAUDE.md", "Kitchen's copy predates provenance markers"),
    (
        "CONTRIBUTING.md",
        "Kitchen adds module ownership and shared-parent rules",
    ),
    (
        "Cargo.toml",
        "Kitchen has two crates and shared dependencies",
    ),
    ("Cargo.lock", "Kitchen locks real dependencies"),
    (
        "crates/kitchen/Cargo.toml",
        "Kitchen's library declares its dependencies",
    ),
    (
        "crates/kitchen/src/lib.rs",
        "Kitchen's library has real modules",
    ),
    (
        "justfile",
        "Kitchen keeps a temporary bootstrap-test recipe for its vendored .origin89 tests",
    ),
];

#[test]
fn kitchen_layout_matches_the_origin89_template() -> TestResult {
    let root = repository_root();
    let rendered = render_origin89('a')?;
    let mut undocumented = Vec::new();
    for file in &rendered.files {
        let path = file.path.as_str();
        let actual = fs::read_to_string(root.join(file.path.as_path())).ok();
        let documented = DOCUMENTED_DIFFERENCES
            .iter()
            .any(|(listed, _)| *listed == path);
        match (
            actual.as_deref() == Some(file.contents.as_str()),
            documented,
        ) {
            (true, true) => undocumented.push(format!("{path} now matches; remove its entry")),
            (false, false) => undocumented.push(format!("{path} drifted from the template")),
            (true, false) | (false, true) => {}
        }
    }
    for (listed, _) in DOCUMENTED_DIFFERENCES {
        if !rendered
            .files
            .iter()
            .any(|file| file.path.as_str() == *listed)
        {
            undocumented.push(format!("{listed} is no longer rendered"));
        }
    }
    assert!(undocumented.is_empty(), "{undocumented:#?}");
    Ok(())
}
