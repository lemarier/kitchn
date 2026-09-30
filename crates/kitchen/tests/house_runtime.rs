//! A house's private runtime configuration: storing, reading, validating,
//! and refusing what is damaged, shared, or for another house.
#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

use kitchen::{
    HouseId,
    adoption::HouseRegistry,
    contracts::{ExternalRef, Repository},
    house::{
        HouseConfig, OrcaHost, RUNTIME_SCHEMA, RuntimeConfig, RuntimeError, RuntimeOutcome,
        runtime_config, store_runtime,
    },
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Fixture {
    _dir: tempfile::TempDir,
    registry: HouseRegistry,
    house: HouseId,
    repository: Repository,
}

impl Fixture {
    fn new() -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let registry = HouseRegistry::new(dir.path().canonicalize()?.join("registry"))?;
        let config: HouseConfig =
            serde_json::from_str(include_str!("fixtures/house/origin89.json"))?;
        registry.initialize(&config)?;
        let repository = config
            .repositories
            .iter()
            .next()
            .cloned()
            .ok_or("the fixture house has a repository")?;
        Ok(Self {
            _dir: dir,
            registry,
            house: config.house,
            repository,
        })
    }

    fn runtime(&self) -> TestResult<RuntimeConfig> {
        Ok(RuntimeConfig {
            schema: RUNTIME_SCHEMA,
            house: self.house.clone(),
            orca: Some(OrcaHost {
                executable: PathBuf::from("/opt/orca/bin/orca"),
                runtime_dir: PathBuf::from("/var/run/kitchen"),
                run: ExternalRef::new("run-1")?,
                coordinator: ExternalRef::new("term-1")?,
                repo: ExternalRef::new("id:app")?,
            }),
            curl: Some(PathBuf::from("/usr/bin/curl")),
            repository: Some(self.repository.clone()),
        })
    }

    fn file(&self) -> PathBuf {
        self.registry
            .root()
            .join("private")
            .join(self.house.as_str())
            .join("runtime.json")
    }
}

#[test]
fn nothing_is_stored_until_a_configuration_is() -> TestResult {
    let fixture = Fixture::new()?;
    assert_eq!(runtime_config(&fixture.registry, &fixture.house)?, None);
    Ok(())
}

#[test]
fn a_stored_configuration_reads_back_owner_only() -> TestResult {
    let fixture = Fixture::new()?;
    let runtime = fixture.runtime()?;
    assert_eq!(
        store_runtime(&fixture.registry, &runtime)?,
        RuntimeOutcome::Created
    );
    assert_eq!(
        runtime_config(&fixture.registry, &fixture.house)?,
        Some(runtime.clone())
    );
    assert_eq!(
        fs::metadata(fixture.file())?.permissions().mode() & 0o077,
        0
    );
    // Only host facts are stored.
    let text = fs::read_to_string(fixture.file())?;
    assert!(!text.to_lowercase().contains("token"), "{text}");
    assert_eq!(
        store_runtime(&fixture.registry, &runtime)?,
        RuntimeOutcome::Unchanged
    );
    Ok(())
}

#[test]
fn a_different_valid_configuration_replaces_the_stored_one() -> TestResult {
    let fixture = Fixture::new()?;
    store_runtime(&fixture.registry, &fixture.runtime()?)?;
    let mut changed = fixture.runtime()?;
    changed.curl = None;
    changed.repository = None;
    assert_eq!(
        store_runtime(&fixture.registry, &changed)?,
        RuntimeOutcome::Replaced
    );
    assert_eq!(
        runtime_config(&fixture.registry, &fixture.house)?,
        Some(changed)
    );
    assert_eq!(
        fs::metadata(fixture.file())?.permissions().mode() & 0o077,
        0
    );
    assert!(!fixture.file().with_extension("json.tmp").exists());
    Ok(())
}

#[test]
fn a_configuration_that_fails_validation_is_never_stored() -> TestResult {
    let fixture = Fixture::new()?;
    let mut relative = fixture.runtime()?;
    if let Some(orca) = relative.orca.as_mut() {
        orca.executable = PathBuf::from("orca");
    }
    let mut curl = fixture.runtime()?;
    curl.curl = Some(PathBuf::from("curl"));
    let mut foreign = fixture.runtime()?;
    foreign.repository = Some(Repository::new("someone/else")?);
    let mut schema = fixture.runtime()?;
    schema.schema = RUNTIME_SCHEMA + 1;
    let mut house = fixture.runtime()?;
    house.house = HouseId::new("other")?;
    for (what, runtime) in [
        ("relative executable", relative),
        ("relative curl", curl),
        ("repository outside the house", foreign),
        ("another schema", schema),
    ] {
        assert!(
            matches!(
                store_runtime(&fixture.registry, &runtime),
                Err(RuntimeError::Invalid)
            ),
            "{what}"
        );
    }
    assert!(store_runtime(&fixture.registry, &house).is_err());
    assert!(!fixture.file().exists());
    Ok(())
}

#[test]
fn a_damaged_file_is_refused_and_kept() -> TestResult {
    let fixture = Fixture::new()?;
    store_runtime(&fixture.registry, &fixture.runtime()?)?;
    let stored = fs::read_to_string(fixture.file())?;
    for damaged in [
        "{".to_owned(),
        stored.replacen('{', "{\"token\": \"x\",", 1),
        stored.replace("/opt/orca/bin/orca", "orca"),
    ] {
        fs::write(fixture.file(), &damaged)?;
        assert!(matches!(
            runtime_config(&fixture.registry, &fixture.house),
            Err(RuntimeError::Invalid)
        ));
        // Storing never overwrites a damaged file.
        assert!(matches!(
            store_runtime(&fixture.registry, &fixture.runtime()?),
            Err(RuntimeError::Invalid)
        ));
        assert_eq!(fs::read_to_string(fixture.file())?, damaged);
    }
    Ok(())
}

#[test]
fn a_file_others_can_access_or_a_link_is_refused() -> TestResult {
    let fixture = Fixture::new()?;
    store_runtime(&fixture.registry, &fixture.runtime()?)?;
    fs::set_permissions(fixture.file(), fs::Permissions::from_mode(0o640))?;
    assert!(matches!(
        runtime_config(&fixture.registry, &fixture.house),
        Err(RuntimeError::NotPrivate)
    ));
    fs::set_permissions(fixture.file(), fs::Permissions::from_mode(0o600))?;
    let target = fixture.file().with_file_name("elsewhere.json");
    fs::rename(fixture.file(), &target)?;
    std::os::unix::fs::symlink(&target, fixture.file())?;
    assert!(matches!(
        runtime_config(&fixture.registry, &fixture.house),
        Err(RuntimeError::NotPrivate)
    ));
    Ok(())
}
