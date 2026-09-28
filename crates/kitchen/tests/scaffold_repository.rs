//! Repository plans that resolve templates from a house's verified instruction
//! snapshot, using a disposable external registry and consumer directory.

use std::{collections::BTreeMap, fs, path::PathBuf};

use kitchen::{
    Error, HouseId,
    adoption::{
        HouseRegistry, InstructionAsset, InstructionBundle, RelativePath, role_cards_digest,
    },
    contracts::{CommitId, Repository},
    house::{HouseConfig, HouseError},
    scaffold::{
        FilePlan, MAX_MANIFEST_BYTES, MAX_TEMPLATE_DEPTH, ManagedState, PlanAction, ScaffoldError,
        ScaffoldLimit, Template, TemplateName, TemplateProblem, VariableName, inspect_managed,
        plan_repository,
    },
};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const HOUSE: &str = "crabnebula";
const REPOSITORY: &str = "crabnebula/tauri-fixture";

fn commit(fill: char) -> TestResult<CommitId> {
    Ok(CommitId::new(&fill.to_string().repeat(40))?)
}

fn asset(path: &str, contents: &str) -> TestResult<InstructionAsset> {
    Ok(InstructionAsset {
        path: RelativePath::new(path)?,
        contents: contents.to_owned(),
    })
}

/// A minimal `app` template whose README records `marker`.
fn template_assets(name: &str, marker: &str) -> TestResult<Vec<InstructionAsset>> {
    Ok(vec![
        asset(
            &format!("templates/{name}/template.toml"),
            &format!(
                "schema = 1\nname = \"{name}\"\nhouse = \"{HOUSE}\"\nrevision = 1\n\
                 description = \"test\"\n[[files]]\nsource = \"README.md\"\n\
                 provenance = \"html-comment\"\n"
            ),
        )?,
        asset(
            &format!("templates/{name}/files/README.md"),
            &format!("{marker}\n"),
        )?,
    ])
}

fn bundle(guidance: char, assets: Vec<InstructionAsset>) -> TestResult<InstructionBundle> {
    let mut all = vec![
        asset("SKILL.md", "Apply the house rules.")?,
        asset("NOTICE.md", "Synthetic fixture notice.")?,
    ];
    all.extend(assets);
    Ok(InstructionBundle {
        schema: 1,
        house: HouseId::new(HOUSE)?,
        kitchen: commit('a')?,
        role_cards_digest: role_cards_digest(),
        guidance: commit(guidance)?,
        entrypoint: RelativePath::new("SKILL.md")?,
        notices: [RelativePath::new("NOTICE.md")?].into(),
        assets: all,
    })
}

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    registry: HouseRegistry,
}

impl Fixture {
    /// A registry with the house configured but no snapshot installed.
    fn unsynced() -> TestResult<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let registry = HouseRegistry::new(root.join("registry"))?;
        let house: HouseConfig =
            serde_json::from_str(include_str!("fixtures/house/crabnebula.json"))?;
        registry.initialize(&house)?;
        Ok(Self {
            _temp: temp,
            root,
            registry,
        })
    }

    /// A registry synced to guidance `b` holding `assets`.
    fn with(assets: Vec<InstructionAsset>) -> TestResult<Self> {
        let fixture = Self::unsynced()?;
        fixture
            .registry
            .sync(&HouseId::new(HOUSE)?, &bundle('b', assets)?)?;
        Ok(fixture)
    }

    fn house(&self) -> TestResult<HouseConfig> {
        Ok(self.registry.load(&HouseId::new(HOUSE)?)?)
    }

    fn plan(&self, template: &str) -> Result<FilePlan, Error> {
        plan_repository(
            &self.registry,
            &self.root.join("consumer"),
            HouseId::new(HOUSE).ok(),
            REPOSITORY.parse::<Repository>().ok(),
            &template.parse::<TemplateName>().map_err(Error::from)?,
            &BTreeMap::<VariableName, String>::new(),
        )
    }

    fn snapshot(&self, guidance: char) -> TestResult<PathBuf> {
        Ok(self
            .root
            .join("registry/snapshots")
            .join(HOUSE)
            .join(format!("{}-{}", commit('a')?, commit(guidance)?)))
    }
}

fn readme(plan: &FilePlan) -> Option<&str> {
    plan.files()
        .iter()
        .find(|planned| planned.file.path.as_str() == "README.md")
        .map(|planned| planned.file.contents.as_str())
}

fn marked_guidance(plan: &FilePlan) -> TestResult<CommitId> {
    match inspect_managed(readme(plan).ok_or("README.md is planned")?) {
        ManagedState::Pristine(marker) => Ok(marker.provenance.guidance),
        other => Err(format!("unexpected marker state {other:?}").into()),
    }
}

#[test]
fn template_resolves_from_the_pinned_snapshot_and_records_its_revision() -> TestResult {
    let f = Fixture::with(template_assets("app", "pinned content")?)?;
    let plan = f.plan("app")?;
    assert_eq!(plan.provenance().guidance, commit('b')?);
    assert_eq!(marked_guidance(&plan)?, commit('b')?);
    assert!(readme(&plan).is_some_and(|text| text.ends_with("\npinned content\n")));
    assert!(
        plan.files()
            .iter()
            .all(|planned| planned.action == PlanAction::Add)
    );
    plan.apply()?;
    assert_eq!(f.plan("app")?.additions().count(), 0);
    Ok(())
}

#[test]
fn a_template_absent_from_the_pinned_guidance_is_refused() -> TestResult {
    let f = Fixture::with(template_assets("app", "pinned")?)?;
    let error = f.plan("other").err().ok_or("absent template planned")?;
    assert!(
        matches!(&error, Error::Scaffold(ScaffoldError::TemplateNotFound { name }) if name.as_str() == "other"),
        "{error:?}"
    );
    assert!(!f.root.join("consumer").exists());
    Ok(())
}

#[test]
fn a_manifest_claiming_another_name_is_refused() -> TestResult {
    let mut assets = template_assets("app", "pinned")?;
    assets[0].contents = assets[0]
        .contents
        .replace("name = \"app\"", "name = \"web\"");
    let f = Fixture::with(assets)?;
    assert!(matches!(
        f.plan("app"),
        Err(Error::Scaffold(ScaffoldError::Template {
            problem: TemplateProblem::NameMismatch
        }))
    ));
    Ok(())
}

#[test]
fn a_missing_snapshot_is_refused_before_rendering() -> TestResult {
    let f = Fixture::unsynced()?;
    assert!(matches!(
        f.plan("app"),
        Err(Error::House(HouseError::UnverifiedSnapshot))
    ));
    assert!(!f.root.join("consumer").exists());
    Ok(())
}

#[test]
fn a_modified_template_file_in_the_snapshot_is_refused() -> TestResult {
    let f = Fixture::with(template_assets("app", "pinned")?)?;
    fs::write(
        f.snapshot('b')?.join("house/templates/app/files/README.md"),
        "substituted content\n",
    )?;
    assert!(matches!(
        f.plan("app"),
        Err(Error::House(HouseError::UnverifiedSnapshot))
    ));
    Ok(())
}

#[test]
fn a_snapshot_claiming_another_revision_is_refused() -> TestResult {
    let f = Fixture::with(template_assets("app", "pinned")?)?;
    let manifest = f.snapshot('b')?.join("manifest.json");
    let text = fs::read_to_string(&manifest)?.replace(&"b".repeat(40), &"c".repeat(40));
    fs::write(&manifest, text)?;
    assert!(matches!(
        f.plan("app"),
        Err(Error::House(HouseError::PinMismatch))
    ));
    Ok(())
}

#[test]
fn an_update_selects_the_new_snapshot_and_retains_the_old_one() -> TestResult {
    let f = Fixture::with(template_assets("app", "first revision")?)?;
    let old = f.house()?;
    let updated = bundle('c', template_assets("app", "second revision")?)?;
    f.registry.update(&old, &updated)?;
    let plan = f.plan("app")?;
    assert_eq!(marked_guidance(&plan)?, commit('c')?);
    assert!(readme(&plan).is_some_and(|text| text.ends_with("\nsecond revision\n")));
    // The earlier snapshot stays verified for tasks that retain it, and
    // damage to it cannot affect plans for the current pin.
    kitchen::adoption::resolve_instructions(f.registry.root(), &old, None)?;
    fs::write(
        f.snapshot('b')?.join("house/templates/app/files/README.md"),
        "damaged\n",
    )?;
    assert_eq!(marked_guidance(&f.plan("app")?)?, commit('c')?);
    Ok(())
}

#[test]
fn guidance_templates_keep_the_directory_bounds() -> TestResult {
    let name: TemplateName = "app".parse()?;
    let nested = |depth: usize| -> TestResult<Vec<InstructionAsset>> {
        let source = format!("{}README.md", "d/".repeat(depth));
        Ok(vec![
            asset(
                "templates/app/template.toml",
                &format!(
                    "schema = 1\nname = \"app\"\nhouse = \"{HOUSE}\"\nrevision = 1\n\
                     description = \"test\"\n[[files]]\nsource = \"{source}\"\n"
                ),
            )?,
            asset(&format!("templates/app/files/{source}"), "text\n")?,
        ])
    };
    assert!(Template::from_guidance(&nested(MAX_TEMPLATE_DEPTH)?, &name).is_ok());
    assert!(matches!(
        Template::from_guidance(&nested(MAX_TEMPLATE_DEPTH + 1)?, &name),
        Err(ScaffoldError::Limit {
            limit: ScaffoldLimit::TemplateDepth
        })
    ));
    let mut oversized = template_assets("app", "text")?;
    let padding = usize::try_from(MAX_MANIFEST_BYTES)?;
    oversized[0].contents.push_str(&"#".repeat(padding));
    assert!(matches!(
        Template::from_guidance(&oversized, &name),
        Err(ScaffoldError::Limit {
            limit: ScaffoldLimit::ManifestBytes
        })
    ));
    // Another template's files are not part of this one.
    let mut unrelated = template_assets("app", "text")?;
    unrelated.extend(template_assets("web", "other")?);
    let template = Template::from_guidance(&unrelated, &name)?;
    assert_eq!(template.manifest().files.len(), 1);
    Ok(())
}
