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
    adoption::{
        FileMode, FileStatus, MAX_INSTALL_BYTES, MAX_INSTALL_FILES, NewFile, RelativePath,
        install_new_files,
    },
    contracts::CommitId,
    house::HouseError,
    scaffold::{
        Conflict, FilePlan, MAX_RENDERED_BYTES, MAX_TEMPLATE_OUTPUT_BYTES, ManagedState, Manifest,
        MissingVariable, PlanAction, PlanKind, RenderedFile, RenderedTemplate, ScaffoldError,
        ScaffoldLimit, Template, TemplateProblem, VariableName, inspect_managed,
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
    assert!(preview.starts_with("Template origin89/rust-workspace revision 2, guidance aaaa"));
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

    let Err(Error::House(HouseError::Conflicts(report))) = plan.apply() else {
        return Err("changed destination must return a typed conflict".into());
    };
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
        "<!-- kitchen-managed: house=origin89 template=rust-workspace template-revision=2 guidance-revision={} content-sha256=",
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
        Err(ScaffoldError::MissingVariables {
            variables: vec![MissingVariable {
                name: "name".parse()?,
                description: "required".into(),
            }],
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
fn every_missing_variable_is_reported_with_its_description() -> TestResult {
    let manifest = minimal_with(
        r#"
[variables.project]
description = "Project name"
[variables.flavor]
description = "optional"
default = "plain"
[variables.summary]
description = "One-line \"summary\""
[[files]]
source = "out.tera"
"#,
    );
    let sources = [("out.tera", "{{ vars.project }} {{ vars.summary }}")];
    let error = render_minimal(&manifest, &sources, &[])?
        .err()
        .ok_or("rendered without required values")?;
    assert_eq!(
        error,
        ScaffoldError::MissingVariables {
            variables: vec![
                MissingVariable {
                    name: "project".parse()?,
                    description: "Project name".into(),
                },
                MissingVariable {
                    name: "summary".parse()?,
                    description: "One-line \"summary\"".into(),
                },
            ],
        }
    );
    assert_eq!(
        error.to_string(),
        "missing values for template variables: project (\"Project name\"), summary (\"One-line \\\"summary\\\"\")"
    );
    // Supplying one leaves only the other.
    assert_eq!(
        render_minimal(&manifest, &sources, &[("summary", "s")])?,
        Err(ScaffoldError::MissingVariables {
            variables: vec![MissingVariable {
                name: "project".parse()?,
                description: "Project name".into(),
            }],
        })
    );
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

/// An in-memory template of verbatim, unmarked files with the given contents.
fn verbatim_template(contents: &[String]) -> TestResult<Result<Template, ScaffoldError>> {
    let mut manifest = MINIMAL.to_owned();
    let mut sources = BTreeMap::new();
    for (index, text) in contents.iter().enumerate() {
        manifest.push_str(&format!(
            "[[files]]\nsource = \"f{index}\"\nrender = false\n"
        ));
        sources.insert(RelativePath::new(&format!("f{index}"))?, text.clone());
    }
    Ok(Template::from_parts(Manifest::parse(&manifest)?, sources))
}

/// A binding-sized file, as `plan_repository` appends to every plan.
fn binding(bytes: usize) -> TestResult<RenderedFile> {
    Ok(RenderedFile {
        path: RelativePath::new(".kitchen.json")?,
        contents: "x".repeat(bytes),
        mode: FileMode::Regular,
        managed: false,
    })
}

#[test]
fn template_file_count_leaves_an_installer_slot_for_the_binding() -> TestResult {
    let dir = TempDir::new()?;
    let target = real(&dir)?.join("absent");
    let largest = vec!["x".to_owned(); MAX_INSTALL_FILES - 1];
    let mut rendered = verbatim_template(&largest)??.render(
        &HouseId::new("home")?,
        &guidance('a')?,
        &vars(&[])?,
    )?;
    rendered.files.push(binding(2)?);
    let plan = FilePlan::new(rendered, &target)?;
    assert_eq!(plan.additions().count(), MAX_INSTALL_FILES);
    let one_more = vec!["x".to_owned(); MAX_INSTALL_FILES];
    assert_eq!(
        verbatim_template(&one_more)?.err(),
        Some(ScaffoldError::Limit {
            limit: ScaffoldLimit::TemplateFiles
        })
    );
    assert!(!target.exists());
    Ok(())
}

#[test]
fn total_rendered_bytes_leave_installer_room_for_the_binding() -> TestResult {
    let dir = TempDir::new()?;
    let target = real(&dir)?.join("absent");
    let mut contents =
        vec!["x".repeat(MAX_RENDERED_BYTES); MAX_TEMPLATE_OUTPUT_BYTES / MAX_RENDERED_BYTES];
    contents.push("x".repeat(MAX_TEMPLATE_OUTPUT_BYTES % MAX_RENDERED_BYTES));
    let mut rendered = verbatim_template(&contents)??.render(
        &HouseId::new("home")?,
        &guidance('a')?,
        &vars(&[])?,
    )?;
    let total: usize = rendered.files.iter().map(|file| file.contents.len()).sum();
    assert_eq!(total, MAX_TEMPLATE_OUTPUT_BYTES);
    rendered
        .files
        .push(binding(MAX_INSTALL_BYTES - MAX_TEMPLATE_OUTPUT_BYTES)?);
    assert!(
        FilePlan::new(rendered, &target)?
            .conflicts()
            .next()
            .is_none()
    );

    contents.push("x".to_owned());
    assert_eq!(
        verbatim_template(&contents)??
            .render(&HouseId::new("home")?, &guidance('a')?, &vars(&[])?)
            .err(),
        Some(ScaffoldError::Limit {
            limit: ScaffoldLimit::TotalRenderedBytes
        })
    );
    assert!(!target.exists());
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
/// file not listed here must match this repository byte for byte. The five
/// build-file exceptions are executed by generated_origin89_fixture_passes_offline_checks;
/// this list alone is not build evidence.
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
    (
        ".origin89/NOTICE.md",
        "Kitchen's notice also covers its vendored bootstrap tests",
    ),
];

#[test]
fn origin89_fixture_ships_its_bootstrap_and_notices() -> TestResult {
    let rendered = render_origin89('a')?;
    let justfile = contents(&rendered, "justfile").ok_or("justfile is rendered")?;
    let scripts: Vec<&str> = justfile
        .lines()
        .filter_map(|line| line.trim().strip_prefix("python3 "))
        .map(|command| command.split_whitespace().next().unwrap_or_default())
        .collect();
    assert_eq!(
        scripts,
        [
            ".origin89/sync-engineering.py",
            ".origin89/sync-engineering.py"
        ]
    );
    for path in scripts {
        assert!(contents(&rendered, path).is_some(), "{path} is not shipped");
    }
    let notice = contents(&rendered, ".origin89/NOTICE.md").ok_or("notice is shipped")?;
    assert!(notice.contains("Origin89 contributors"));
    for license in ["LICENSE-MIT", "LICENSE-APACHE"] {
        assert!(
            notice.contains(&format!("]({license})")),
            "{license} is linked"
        );
    }
    // Upstream bytes are preserved; Kitchen's vendored copies match upstream.
    for path in [
        ".origin89/sync-engineering.py",
        ".origin89/LICENSE-MIT",
        ".origin89/LICENSE-APACHE",
    ] {
        assert_eq!(
            contents(&rendered, path),
            Some(fs::read_to_string(repository_root().join(path))?.as_str()),
            "{path}"
        );
    }
    Ok(())
}

/// Run `python3` with `args` in `dir` and collect its output, killing it after
/// 30 seconds. `None`, with a message on stderr, when `python3` is not installed.
fn run_python(what: &str, args: &[&str], dir: &Path) -> TestResult<Option<std::process::Output>> {
    use std::{
        io::Write,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let mut child = match Command::new("python3")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            writeln!(std::io::stderr(), "SKIP {what}: python3 unavailable")?;
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            return Err(format!("{what} exceeded 30 seconds").into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(Some(child.wait_with_output()?))
}

#[test]
fn generated_origin89_bootstrap_reports_a_missing_cache_offline() -> TestResult {
    let temp = TempDir::new()?;
    let consumer = real(&temp)?.join("consumer");
    FilePlan::new(render_origin89('a')?, &consumer)?.apply()?;
    let Some(output) = run_python(
        "generated Origin89 offline bootstrap",
        &[".origin89/sync-engineering.py", "--offline"],
        &consumer,
    )?
    else {
        return Ok(());
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr.contains("No cached engineering skills available"),
        "{stderr}"
    );
    assert!(!consumer.join(".agents").exists());
    assert!(!consumer.join(".claude/skills").exists());
    Ok(())
}

const UPSTREAM_REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
const UPSTREAM_SKILLS: [&str; 4] = [
    "origin89-commits",
    "origin89-rust",
    "origin89-working",
    "origin89-writing",
];
const UPSTREAM_HEAD_URL: &str = "https://api.github.com/repos/origin89hq/engineering/commits/main";

/// Runs the rendered bootstrap's own `refresh` with its `fetch` seam pointed at
/// an in-memory upstream, so no request leaves the process. `mode` selects what
/// upstream serves: a valid archive, an archive with an escaping path, one
/// missing a required skill, or a network outage. Prints one JSON report.
const CONTROLLED_UPSTREAM: &str = r##"
import importlib.util, io, json, sys, tarfile, urllib.error
from pathlib import Path

consumer, mode, revision = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
spec = importlib.util.spec_from_file_location(
    "sync_engineering", consumer / ".origin89" / "sync-engineering.py")
sync = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sync)
requests = []

def skill(name):
    return (f"---\nname: {name}\ndescription: Controlled upstream skill {name}\n---\n"
            f"Body of {name}.\n").encode()

def archive():
    names = ["origin89-rust", "origin89-working", "origin89-writing"]
    if mode != "incomplete":
        names.append("origin89-commits")
    entries = {f"skills/{name}/SKILL.md": skill(name) for name in names}
    entries["README.md"] = b"not a skill\n"
    if mode == "unsafe":
        entries["skills/../escape"] = b"outside\n"
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as bundle:
        for path, data in entries.items():
            info = tarfile.TarInfo(f"engineering-{revision}/{path}")
            info.size = len(data)
            bundle.addfile(info, io.BytesIO(data))
    return buffer.getvalue()

def fetch(url):
    requests.append(url)
    if mode == "unreachable":
        raise urllib.error.URLError("controlled outage")
    if url == sync.HEAD_URL:
        return json.dumps({"sha": revision}).encode()
    if url == f"https://codeload.github.com/{sync.SOURCE}/tar.gz/{revision}":
        return archive()
    raise AssertionError(f"unexpected request: {url}")

try:
    report = {"ok": True, "result": sync.refresh(consumer, fetch=fetch)}
except ValueError as error:
    report = {"ok": False, "error": str(error)}
print(json.dumps(report | {"requests": requests}))
"##;

/// The controlled-upstream report, or `None` when `python3` is unavailable.
fn refresh_from_controlled_upstream(
    consumer: &Path,
    mode: &str,
) -> TestResult<Option<serde_json::Value>> {
    let consumer_arg = consumer.to_str().ok_or("consumer path is not UTF-8")?;
    let Some(output) = run_python(
        "generated Origin89 first-use bootstrap",
        &[
            "-c",
            CONTROLLED_UPSTREAM,
            consumer_arg,
            mode,
            UPSTREAM_REVISION,
        ],
        consumer,
    )?
    else {
        return Ok(None);
    };
    assert!(output.status.success(), "{output:?}");
    Ok(Some(serde_json::from_slice(&output.stdout)?))
}

fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

#[test]
fn generated_origin89_bootstrap_first_use_installs_guidance_from_a_controlled_upstream()
-> TestResult {
    use serde_json::{Value, json};
    let temp = TempDir::new()?;
    let consumer = real(&temp)?.join("consumer");
    FilePlan::new(render_origin89('a')?, &consumer)?.apply()?;
    let cache = consumer.join(".origin89/engineering");
    assert!(!cache.exists(), "a new consumer starts with an empty cache");

    let Some(report) = refresh_from_controlled_upstream(&consumer, "good")? else {
        return Ok(());
    };
    let archive_url =
        format!("https://codeload.github.com/origin89hq/engineering/tar.gz/{UPSTREAM_REVISION}");
    assert_eq!(report["ok"], true, "{report}");
    assert_eq!(
        report["requests"],
        json!([UPSTREAM_HEAD_URL, archive_url]),
        "the archive is fetched by the revision the head lookup returned"
    );
    assert_eq!(report["result"]["revision"], UPSTREAM_REVISION);
    assert_eq!(report["result"]["skills"], json!(UPSTREAM_SKILLS));
    assert_eq!(report["result"]["cached"], Value::Null);

    // The cache holds a verified snapshot of the skill folders only.
    let snapshot = cache.join("versions").join(UPSTREAM_REVISION);
    let state: Value = serde_json::from_str(&fs::read_to_string(snapshot.join("state.json"))?)?;
    assert_eq!(state["revision"], UPSTREAM_REVISION);
    assert_eq!(state["skills"], json!(UPSTREAM_SKILLS));
    assert_eq!(
        state["files"].as_object().map(serde_json::Map::len),
        Some(UPSTREAM_SKILLS.len())
    );
    assert!(!snapshot.join("README.md").exists());
    assert_eq!(
        fs::read_link(cache.join("current"))?,
        PathBuf::from(format!("versions/{UPSTREAM_REVISION}"))
    );
    assert!(fs::symlink_metadata(cache.join("sync.lock")).is_err());

    // Every skill is discoverable by both assistants and resolves to the cache.
    for assistant in [".agents", ".claude"] {
        for name in UPSTREAM_SKILLS {
            let link = consumer.join(assistant).join("skills").join(name);
            assert!(is_link(&link), "{}", link.display());
            let text = fs::read_to_string(link.join("SKILL.md"))?;
            assert!(text.contains(&format!("Body of {name}.")), "{name}");
        }
    }

    // An unchanged upstream revision reuses the verified snapshot.
    let again = refresh_from_controlled_upstream(&consumer, "good")?.ok_or("python3 vanished")?;
    assert_eq!(again["ok"], true, "{again}");
    assert_eq!(again["requests"], json!([UPSTREAM_HEAD_URL]));
    assert_eq!(again["result"]["cached"], Value::Null);

    // With no network at all, the shipped command line reuses that cache.
    let output = run_python(
        "generated Origin89 offline bootstrap",
        &[".origin89/sync-engineering.py", "--offline"],
        &consumer,
    )?
    .ok_or("python3 vanished")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(
        stdout.contains(&format!("\"revision\": \"{UPSTREAM_REVISION}\"")),
        "{stdout}"
    );
    assert!(
        stdout
            .contains("Using cached skills; this run did not confirm the latest upstream content."),
        "{stdout}"
    );
    Ok(())
}

#[test]
fn a_rejected_first_use_refresh_activates_nothing_and_a_later_one_recovers() -> TestResult {
    let temp = TempDir::new()?;
    let consumer = real(&temp)?.join("consumer");
    FilePlan::new(render_origin89('a')?, &consumer)?.apply()?;
    let cache = consumer.join(".origin89/engineering");
    for (mode, error) in [
        ("unsafe", "Unsafe path in skill archive"),
        (
            "incomplete",
            "Archive is missing the shared working, writing, or commit skill",
        ),
        (
            "unreachable",
            "No cached engineering skills available (refresh unavailable:",
        ),
    ] {
        let Some(report) = refresh_from_controlled_upstream(&consumer, mode)? else {
            return Ok(());
        };
        assert_eq!(report["ok"], false, "{mode}: {report}");
        assert!(
            report["error"]
                .as_str()
                .is_some_and(|message| message.contains(error)),
            "{mode}: {report}"
        );
        assert!(!cache.join("versions").exists(), "{mode}");
        assert!(
            fs::symlink_metadata(cache.join("current")).is_err(),
            "{mode}"
        );
        assert!(
            fs::symlink_metadata(cache.join("sync.lock")).is_err(),
            "{mode}"
        );
        assert!(!consumer.join(".agents").exists(), "{mode}");
        assert!(!consumer.join(".claude/skills").exists(), "{mode}");
    }

    let report = refresh_from_controlled_upstream(&consumer, "good")?.ok_or("python3 vanished")?;
    assert_eq!(report["ok"], true, "{report}");
    for assistant in [".agents", ".claude"] {
        for name in UPSTREAM_SKILLS {
            let link = consumer.join(assistant).join("skills").join(name);
            assert!(is_link(&link), "{}", link.display());
        }
    }
    Ok(())
}

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

#[test]
fn changed_or_removed_unchanged_files_refuse_additions() -> TestResult {
    for remove in [false, true] {
        let temp = tempfile::tempdir()?;
        let target = real(&temp)?.join("consumer");
        FilePlan::new(render_example('a')?, &target)?.apply()?;
        fs::remove_file(target.join("README.md"))?;
        let plan = FilePlan::new(render_example('a')?, &target)?;
        if remove {
            fs::remove_file(target.join("AGENTS.md"))?;
        } else {
            fs::write(target.join("AGENTS.md"), "new local instructions")?;
        }
        assert!(matches!(
            plan.apply(),
            Err(Error::House(HouseError::Conflict))
        ));
        assert!(!target.join("README.md").exists());
        if !remove {
            assert_eq!(
                fs::read_to_string(target.join("AGENTS.md"))?,
                "new local instructions"
            );
        }
    }
    Ok(())
}

#[test]
fn structured_fixture_variables_refuse_code_and_syntax_injection() -> TestResult {
    for (name, value) in [
        ("summary", "A \"quoted\" tool"),
        ("crate_name", "my-crate"),
        ("crate_name", "type"),
        ("rust_version", "1.98.1; touch /tmp/kitchen-injected"),
        ("repository_url", "https://example.com/\"injected"),
        ("copyright_year", "2026; command"),
    ] {
        let mut values = kitchen_vars()?;
        values.insert(name.parse()?, value.to_owned());
        assert!(
            matches!(
                origin89_template()?.render(&HouseId::new("origin89")?, &guidance('a')?, &values),
                Err(ScaffoldError::InvalidVariableValue { .. })
            ),
            "{name} must be rejected"
        );
    }
    Ok(())
}

#[test]
fn markers_refuse_first_line_sensitive_content() -> TestResult {
    for (body, mode) in [
        ("#!/bin/sh\necho hello\n", "regular"),
        ("---\nname: skill\n---\n", "regular"),
        ("+++\nname = \"skill\"\n+++\n", "regular"),
        ("echo hello\n", "executable"),
    ] {
        let manifest = Manifest::parse(&minimal_with(&format!(
            "[[files]]\nsource = \"file\"\nmode = \"{mode}\"\nprovenance = \"hash-comment\"\n"
        )))?;
        let template = Template::from_parts(
            manifest,
            BTreeMap::from([(RelativePath::new("file")?, body.to_owned())]),
        )?;
        assert!(
            matches!(
                template.render(&HouseId::new("home")?, &guidance('a')?, &BTreeMap::new()),
                Err(ScaffoldError::Template {
                    problem: TemplateProblem::MarkerPlacement(_)
                })
            ),
            "{body}"
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn real_apply_io_failure_rolls_back_and_retry_recovers() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new()?;
    let root = real(&temp)?;
    let target = root.join("consumer");
    fs::create_dir_all(target.join("crates"))?;
    fs::write(target.join("local.txt"), "preserve me")?;
    let template = Template::from_parts(
        Manifest::parse(&minimal_with(
            "[[files]]\nsource = \"first\"\n[[files]]\nsource = \"crates/second\"\n",
        ))?,
        BTreeMap::from([
            (RelativePath::new("first")?, "first".into()),
            (RelativePath::new("crates/second")?, "second".into()),
        ]),
    )?;
    let plan = FilePlan::new(
        template.render(&HouseId::new("home")?, &guidance('a')?, &BTreeMap::new())?,
        &target,
    )?;
    fs::set_permissions(target.join("crates"), fs::Permissions::from_mode(0o555))?;
    let result = plan.apply();
    fs::set_permissions(target.join("crates"), fs::Permissions::from_mode(0o755))?;
    assert!(
        matches!(
            result,
            Err(Error::House(HouseError::Io(
                std::io::ErrorKind::PermissionDenied
            )))
        ),
        "{result:?}; test requires an unprivileged user"
    );
    assert!(!target.join("first").exists());
    assert!(!target.join("crates/second").exists());
    assert_eq!(fs::read_to_string(target.join("local.txt"))?, "preserve me");
    let report = plan.apply()?;
    assert_eq!(report.files.len(), 2);
    assert_eq!(fs::read_to_string(target.join("first"))?, "first");
    assert_eq!(fs::read_to_string(target.join("crates/second"))?, "second");
    Ok(())
}

#[test]
fn text_variables_are_not_evaluated_as_templates() -> TestResult {
    let rendered = example_template()?.render(
        &HouseId::new("example")?,
        &guidance('a')?,
        &vars(&[("project_name", "{{ throw(message='must not run') }}")])?,
    )?;
    let readme = rendered
        .files
        .iter()
        .find(|file| file.path.as_str() == "README.md")
        .ok_or("README missing")?;
    assert!(
        readme
            .contents
            .contains("{{ throw(message='must not run') }}")
    );
    Ok(())
}

#[test]
fn generated_origin89_fixture_passes_offline_checks() -> TestResult {
    use std::{
        io::Write,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    for tool in ["just", "cargo"] {
        match Command::new(tool).arg("--version").output() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                writeln!(
                    std::io::stderr(),
                    "SKIP generated Origin89 offline just check: {tool} unavailable"
                )?;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
            Ok(output) if !output.status.success() => {
                return Err(format!("{tool} --version failed").into());
            }
            Ok(_) => {}
        }
    }
    let temp = TempDir::new()?;
    let root = real(&temp)?;
    let consumer = root.join("consumer");
    FilePlan::new(render_origin89('a')?, &consumer)?.apply()?;
    let log_path = root.join("check.log");
    let log = fs::File::create(&log_path)?;
    let mut child = Command::new("just")
        .arg("check")
        .current_dir(&consumer)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env("CARGO_NET_OFFLINE", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(180);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline || fs::metadata(&log_path)?.len() > 2 * 1024 * 1024 {
            child.kill()?;
            child.wait()?;
            return Err("generated fixture check exceeded 180 seconds or 2 MiB output".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let output = fs::read_to_string(log_path)?;
    assert!(
        status.success(),
        "generated Origin89 just check failed:\n{output}"
    );
    writeln!(
        std::io::stderr(),
        "PASS generated Origin89 offline just check: fmt, clippy, tests, build, docs, MSRV, actionlint (current checkout)"
    )?;
    Ok(())
}

#[test]
fn variable_constraints_validate_defaults_and_reject_unknown_kinds() -> TestResult {
    let manifest = minimal_with(
        "[variables.version]\ndescription = \"Version\"\nkind = \"version\"\ndefault = \"1.2.3; command\"\n[[files]]\nsource = \"file\"\n",
    );
    let template = Template::from_parts(
        Manifest::parse(&manifest)?,
        BTreeMap::from([(RelativePath::new("file")?, "{{ vars.version }}".into())]),
    )?;
    assert!(matches!(
        template.render(&HouseId::new("home")?, &guidance('a')?, &BTreeMap::new()),
        Err(ScaffoldError::InvalidVariableValue { .. })
    ));
    let rendered = template.render(
        &HouseId::new("home")?,
        &guidance('a')?,
        &vars(&[("version", "1.2.3")])?,
    )?;
    assert_eq!(
        rendered.files.first().ok_or("file missing")?.contents,
        "1.2.3"
    );
    assert!(
        Manifest::parse(&manifest.replace("kind = \"version\"", "kind = \"unknown\"")).is_err()
    );
    Ok(())
}
