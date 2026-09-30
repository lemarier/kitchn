//! A house's private runtime configuration: storing, reading, validating,
//! and refusing what is damaged, shared, or for another house.
#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

use kitchen::{
    HouseId,
    adoption::HouseRegistry,
    contracts::{ExternalRef, Repository},
    house::{
        HouseConfig, OrcaHost, PickupConfig, RUNTIME_SCHEMA, RuntimeConfig, RuntimeError,
        RuntimeOutcome, runtime_config, store_runtime,
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
            pickup: Some(PickupConfig {
                ready_label: "agent-ready".to_owned(),
                capacity: 2,
                ..PickupConfig::default()
            }),
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

#[test]
fn pickup_settings_round_trip_and_default_when_absent() -> TestResult {
    let fixture = Fixture::new()?;
    let runtime = fixture.runtime()?;
    store_runtime(&fixture.registry, &runtime)?;
    let stored = runtime_config(&fixture.registry, &fixture.house)?.ok_or("stored")?;
    assert_eq!(stored.pickup, runtime.pickup);
    let text = fs::read_to_string(fixture.file())?;
    assert!(text.contains("\"readyLabel\": \"agent-ready\""), "{text}");

    // A file without the field reads as none, which means the defaults.
    let bare = RuntimeConfig {
        pickup: None,
        ..runtime
    };
    store_runtime(&fixture.registry, &bare)?;
    let text = fs::read_to_string(fixture.file())?;
    assert!(!text.contains("pickup"), "{text}");
    let stored = runtime_config(&fixture.registry, &fixture.house)?.ok_or("stored")?;
    assert_eq!(stored.pickup, None);
    Ok(())
}

#[test]
fn invalid_pickup_settings_are_never_stored() -> TestResult {
    type Break = fn(&mut PickupConfig);
    let breaks: [(&str, Break); 8] = [
        ("empty label", |p| p.ready_label.clear()),
        ("control in label", |p| p.human_label = "a\nb".to_owned()),
        ("zero capacity", |p| p.capacity = 0),
        ("huge capacity", |p| p.capacity = 65),
        ("bad branch prefix", |p| p.branch_prefix = "a b".to_owned()),
        ("absolute report path", |p| {
            p.report_path = "/etc/report".to_owned()
        }),
        ("escaping report path", |p| {
            p.report_path = "../report.md".to_owned()
        }),
        ("empty report segment", |p| {
            p.report_path = "a//b.md".to_owned()
        }),
    ];
    for (what, damage) in breaks {
        let fixture = Fixture::new()?;
        let mut runtime = fixture.runtime()?;
        if let Some(pickup) = runtime.pickup.as_mut() {
            damage(pickup);
        }
        assert!(
            matches!(
                store_runtime(&fixture.registry, &runtime),
                Err(RuntimeError::Invalid)
            ),
            "{what}"
        );
        assert!(!fixture.file().exists(), "{what}");
    }
    // The boundaries themselves are accepted.
    let fixture = Fixture::new()?;
    let mut runtime = fixture.runtime()?;
    runtime.pickup = Some(PickupConfig {
        capacity: 64,
        report_path: "reports/out.md".to_owned(),
        ..PickupConfig::default()
    });
    store_runtime(&fixture.registry, &runtime)?;
    Ok(())
}
