//! A house's worker backend binding (#191): the one resolver builds the bound
//! backend and checks it against the workflow's required capabilities; a
//! house without a binding, or bound to a backend this Kitchen does not know,
//! is refused by name before anything is contacted; a house registered
//! before bindings loads unchanged and gets its binding by rerunning guided
//! init; and no command connects to Orca outside the resolver.
//!
//! Orca is the simulated runtime (`orca_sim`) and registries are disposable;
//! none of this is live runtime evidence.

mod orca_sim;

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use kitchen::{
    BackendId, CredentialId, ErrorClass, HouseId,
    adapters::{BackendError, OrcaSession, orca::OrcaError, resolve_backend},
    adoption::HouseRegistry,
    contracts::{Capability, ContractError, EffectExecutor, ExternalRef},
    house::{
        AgentInventory, BackendBinding, BackendKind, HouseConfig, HouseError, HouseInitError,
        InitAnswers, InitDecision, InitFacts, InitQuestion, NoGitHubAccess, Prompter,
        plan_house_init, register_house,
    },
    scheduling::AgentFamily,
    workflows::budget,
};
use orca_sim::SimOrca;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const KITCHEN: &str = "4f2a9c1e0b7d3a5f6c8e9d0a1b2c3d4e5f6a7b8c";

/// A house registered before bindings existed.
fn legacy_house() -> TestResult<HouseConfig> {
    Ok(serde_json::from_str(include_str!(
        "fixtures/house/origin89.json"
    ))?)
}

fn bound_house() -> TestResult<HouseConfig> {
    Ok(HouseConfig {
        backend: Some(BackendBinding {
            kind: BackendKind::Orca.into(),
            backend: BackendId::new("orca-local")?,
            credential: CredentialId::new("orca-host-session")?,
        }),
        ..legacy_house()?
    })
}

fn session(sim: &SimOrca) -> TestResult<OrcaSession> {
    Ok(OrcaSession {
        run: ExternalRef::new("run_sim")?,
        coordinator: ExternalRef::new("term_coordinator")?,
        repo: ExternalRef::new("id:repo-1")?,
        base_branch: None,
        branch_prefix: None,
        agent: AgentFamily::Claude,
        call_timeout: Duration::from_secs(5),
        launch_timeout: Duration::from_secs(60),
        runtime_dir: sim.runtime_dir()?,
        reservation_timeout: Duration::from_secs(10),
    })
}

#[test]
fn the_resolver_builds_the_bound_backend_under_its_namespace_and_credential() -> TestResult {
    let sim = SimOrca::default();
    let house = bound_house()?;
    let backend = resolve_backend(&house, session(&sim)?, &sim, &[Capability::ScheduleManage])?;
    let config = backend.config();
    assert_eq!(config.backend.as_str(), "orca-local");
    assert_eq!(config.credential.as_str(), "orca-host-session");
    assert_eq!(config.house, house.house);
    let descriptor = EffectExecutor::descriptor(&backend);
    assert_eq!(descriptor.backend.as_str(), "orca-local");
    assert_eq!(descriptor.house, house.house);
    assert_eq!(sim.calls_to(&["status"]).len(), 1, "one runtime probe");
    Ok(())
}

#[test]
fn a_house_without_a_binding_is_refused_before_orca_is_contacted() -> TestResult {
    let sim = SimOrca::default();
    let house = legacy_house()?;
    let error = resolve_backend(&house, session(&sim)?, &sim, &[])
        .err()
        .ok_or("an unbound house resolved")?;
    assert!(
        matches!(&error, BackendError::Unbound { house } if house.as_str() == "origin89"),
        "{error:?}"
    );
    assert!(error.to_string().contains("house origin89"), "{error}");
    assert_eq!(kitchen::Error::from(error).class(), ErrorClass::Refused);
    assert!(sim.calls_to(&[]).is_empty(), "nothing contacted");
    Ok(())
}

#[test]
fn an_unknown_backend_is_refused_by_name_before_anything_is_contacted() -> TestResult {
    let sim = SimOrca::default();
    let mut house = bound_house()?;
    if let Some(binding) = &mut house.backend {
        binding.kind = "sandbox".parse()?;
    }
    let error = resolve_backend(&house, session(&sim)?, &sim, &[])
        .err()
        .ok_or("an unknown backend resolved")?;
    assert!(
        matches!(&error, BackendError::Unknown { name, .. } if name.as_str() == "sandbox"),
        "{error:?}"
    );
    assert!(error.to_string().contains("`sandbox`"), "{error}");
    assert!(sim.calls_to(&[]).is_empty(), "nothing contacted");
    Ok(())
}

#[test]
fn a_backend_lacking_required_capabilities_is_refused_naming_every_gap() -> TestResult {
    let sim = SimOrca::default();
    let error = resolve_backend(
        &bound_house()?,
        session(&sim)?,
        &sim,
        &budget::REQUIRED_CAPABILITIES,
    )
    .err()
    .ok_or("Orca met the budget tick's requirements")?;
    let BackendError::Unsupported {
        kind,
        source: ContractError::UnsupportedCapabilities { missing, partial },
        ..
    } = &error
    else {
        return Err(format!("unexpected {error:?}").into());
    };
    assert_eq!(*kind, BackendKind::Orca);
    assert_eq!(
        missing,
        &[
            Capability::ScheduleSingleConsumer,
            Capability::ScheduleRunTimeout
        ]
    );
    assert_eq!(partial, &[Capability::SchedulePrecheck]);
    let message = error.to_string();
    for capability in [
        "schedule.single_consumer",
        "schedule.run_timeout",
        "schedule.precheck",
    ] {
        assert!(message.contains(capability), "{capability}: {message}");
    }
    assert_eq!(kitchen::Error::from(error).class(), ErrorClass::Refused);
    Ok(())
}

#[test]
fn an_unreachable_backend_is_an_orca_error_not_a_refusal() -> TestResult {
    let sim = SimOrca::default();
    sim.state().ready = false;
    let error = resolve_backend(&bound_house()?, session(&sim)?, &sim, &[])
        .err()
        .ok_or("a runtime that is not ready resolved")?;
    assert!(
        matches!(error, BackendError::Orca(OrcaError::RuntimeNotReady)),
        "{error:?}"
    );
    Ok(())
}

#[test]
fn a_legacy_house_loads_unchanged_with_no_implied_backend() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = HouseRegistry::new(temp.path().canonicalize()?.join("registry"))?;
    let legacy = legacy_house()?;
    registry.initialize(&legacy)?;
    let loaded = registry.load(&HouseId::new("origin89")?)?;
    assert_eq!(loaded, legacy);
    assert_eq!(loaded.backend, None);
    // Saving it again writes no binding either.
    assert!(!serde_json::to_string(&loaded)?.contains("backend"));
    Ok(())
}

/// Answers with every question given as a flag, as a script would pass them.
fn flags(registry: &Path) -> InitAnswers {
    InitAnswers {
        registry: Some(registry.to_path_buf()),
        house: Some("acme".to_owned()),
        repositories: Some("acme/app".to_owned()),
        posting_destinations: Some("acme/app".to_owned()),
        required_checks: Some("test".to_owned()),
        forge_requester: Some("none".to_owned()),
        kitchen: Some(KITCHEN.to_owned()),
        yes: true,
        ..InitAnswers::default()
    }
}

fn facts(home: &Path) -> TestResult<InitFacts> {
    Ok(InitFacts {
        home: Some(home.to_path_buf()),
        checkout: Ok("acme/app".parse()?),
        kitchen: Some(kitchen::contracts::CommitId::new(KITCHEN)?),
        agents: AgentInventory::UNKNOWN,
        forge_login: None,
    })
}

fn plan(answers: &InitAnswers, home: &Path) -> TestResult<kitchen::house::HouseInitPlan> {
    match plan_house_init(answers, &facts(home)?, &NoGitHubAccess, None)? {
        InitDecision::Confirmed(plan) => Ok(plan),
        InitDecision::Declined(_) => Err("declined".into()),
    }
}

fn orca_default() -> TestResult<BackendBinding> {
    Ok(BackendBinding {
        kind: BackendKind::Orca.into(),
        backend: BackendId::new("orca")?,
        credential: CredentialId::new("orca")?,
    })
}

fn temp_home() -> TestResult<(tempfile::TempDir, PathBuf)> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().canonicalize()?;
    Ok((temp, home))
}

#[test]
fn guided_init_writes_orca_explicitly_when_nothing_is_chosen() -> TestResult {
    let (_temp, home) = temp_home()?;
    let registry = home.join("registry");
    let plan = plan(&flags(&registry), &home)?;
    assert_eq!(plan.config.backend, Some(orca_default()?));
    let report = register_house(&plan)?;
    let stored = fs::read_to_string(report.config_path)?;
    assert!(
        stored.contains(r#""kind": "orca""#),
        "the default is written, not implied: {stored}"
    );
    Ok(())
}

/// Answers the worker backend question, taking `answers` from the end, and
/// nothing else.
struct Backend {
    transcript: Vec<String>,
    answers: Vec<&'static str>,
}
impl Prompter for Backend {
    fn show(&mut self, text: &str) -> Result<(), HouseInitError> {
        self.transcript.push(text.to_owned());
        Ok(())
    }
    fn ask(&mut self, prompt: &str) -> Result<String, HouseInitError> {
        self.transcript.push(prompt.to_owned());
        // Every other question takes its offered default.
        if !prompt.starts_with("Worker backend") {
            return Ok(String::new());
        }
        self.answers
            .pop()
            .map(str::to_owned)
            .ok_or(HouseInitError::Io(std::io::ErrorKind::UnexpectedEof))
    }
}

#[test]
fn guided_init_offers_orca_and_asks_again_for_an_unknown_backend() -> TestResult {
    let (_temp, home) = temp_home()?;
    let answers = InitAnswers {
        yes: true,
        ..flags(&home.join("registry"))
    };
    // Every other answer is a flag or its offered default; the backend is
    // answered `sandbox`, then `orca`.
    let mut prompter = Backend {
        transcript: Vec::new(),
        answers: vec!["orca", "sandbox"],
    };
    let decision = plan_house_init(
        &answers,
        &facts(&home)?,
        &NoGitHubAccess,
        Some(&mut prompter),
    )?;
    let InitDecision::Confirmed(plan) = decision else {
        return Err("declined".into());
    };
    assert_eq!(plan.config.backend, Some(orca_default()?));
    assert!(
        prompter
            .transcript
            .iter()
            .any(|line| line == "Worker backend [orca, the only one this Kitchen supports]: "),
        "{:?}",
        prompter.transcript
    );
    assert!(
        prompter
            .transcript
            .iter()
            .any(|line| line == "Expected orca.")
    );
    assert!(prompter.answers.is_empty(), "asked twice");
    Ok(())
}

#[test]
fn guided_init_refuses_an_unknown_backend_flag() -> TestResult {
    let (_temp, home) = temp_home()?;
    for unknown in ["sandbox", "Orca", ""] {
        let answers = InitAnswers {
            worker_backend: Some(unknown.to_owned()),
            ..flags(&home.join("registry"))
        };
        assert!(
            matches!(
                plan_house_init(&answers, &facts(&home)?, &NoGitHubAccess, None),
                Err(HouseInitError::InvalidAnswer(InitQuestion::WorkerBackend))
            ),
            "{unknown}"
        );
    }
    Ok(())
}

#[test]
fn rerunning_init_writes_the_orca_binding_into_a_matching_legacy_house() -> TestResult {
    let (_temp, home) = temp_home()?;
    let registry_path = home.join("registry");
    let plan = plan(&flags(&registry_path), &home)?;
    // The same house as registered before bindings existed.
    let legacy = HouseConfig {
        backend: None,
        ..plan.config.clone()
    };
    let registry = HouseRegistry::new(registry_path)?;
    registry.initialize(&legacy)?;

    register_house(&plan)?;
    let house = HouseId::new("acme")?;
    assert_eq!(registry.load(&house)?, plan.config);
    // Rerunning again changes nothing.
    register_house(&plan)?;
    assert_eq!(registry.load(&house)?, plan.config);
    Ok(())
}

#[test]
fn rerunning_init_never_rewrites_a_legacy_house_that_differs() -> TestResult {
    let (_temp, home) = temp_home()?;
    let registry_path = home.join("registry");
    let plan = plan(&flags(&registry_path), &home)?;
    let other = HouseConfig {
        backend: None,
        required_checks: ["lint".to_owned()].into(),
        ..plan.config.clone()
    };
    let registry = HouseRegistry::new(registry_path)?;
    registry.initialize(&other)?;

    let error = register_house(&plan)
        .err()
        .ok_or("a differing house was replaced")?;
    assert!(
        matches!(error, HouseInitError::House(HouseError::Conflicts(_))),
        "{error:?}"
    );
    assert_eq!(registry.load(&HouseId::new("acme")?)?, other);
    // A house bound elsewhere is not rebound either.
    let bound = HouseConfig {
        backend: Some(BackendBinding {
            backend: BackendId::new("orca-other")?,
            ..orca_default()?
        }),
        ..plan.config.clone()
    };
    assert!(matches!(
        registry.bind_backend(&bound, &orca_default()?),
        Err(HouseError::Conflict)
    ));
    Ok(())
}

/// Commands build backends only through `resolve_backend`: no crate source
/// outside the resolver calls `OrcaBackend::connect`. Tests may connect to the
/// simulated runtime directly.
#[test]
fn only_the_resolver_connects_to_orca() -> TestResult {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("no crates directory")?;
    let resolver = crates.join("kitchen/src/adapters/resolve.rs");
    let mut callers = Vec::new();
    for entry in fs::read_dir(crates)? {
        let source = entry?.path().join("src");
        if !source.is_dir() {
            continue;
        }
        visit(&source, &mut |path, text| {
            if text.contains("OrcaBackend::connect(") {
                callers.push(path.to_path_buf());
            }
        })?;
    }
    assert_eq!(callers, vec![resolver]);
    Ok(())
}

fn visit(dir: &Path, found: &mut dyn FnMut(&Path, &str)) -> TestResult {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            visit(&path, found)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found(&path, &fs::read_to_string(&path)?);
        }
    }
    Ok(())
}
