//! The guided `house init` wizard driven through a scripted prompter, and
//! registration in disposable registries. Simulated: no terminal, GitHub, or
//! Orca is involved.
use kitchen::{
    ErrorClass, HouseId,
    adoption::{HouseRegistry, InstructionBundle, resolve_instructions},
    contracts::{CommitId, Repository, Role},
    house::{
        HouseConfig, HouseError, HouseInitError, InitAnswers, InitDecision, InitFacts,
        InitQuestion, InstalledAgents, NoGitHubAccess, ObservedChecks, Prompter,
        RequiredCheckSource, Station, default_guidance, plan_house_init, register_house,
    },
    scheduling::AgentFamily,
    selection::SelectionRequest,
};
use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
const KITCHEN: &str = "4f2a9c1e0b7d3a5f6c8e9d0a1b2c3d4e5f6a7b8c";

/// Answers in order; records everything shown and asked.
struct Script {
    answers: VecDeque<&'static str>,
    transcript: Vec<String>,
}
impl Script {
    fn new(answers: &[&'static str]) -> Self {
        Self {
            answers: answers.iter().copied().collect(),
            transcript: Vec::new(),
        }
    }
    fn said(&self, text: &str) -> bool {
        self.transcript.iter().any(|line| line.contains(text))
    }
}
impl Prompter for Script {
    fn show(&mut self, text: &str) -> Result<(), HouseInitError> {
        self.transcript.push(text.to_owned());
        Ok(())
    }
    fn ask(&mut self, prompt: &str) -> Result<String, HouseInitError> {
        self.transcript.push(prompt.to_owned());
        self.answers
            .pop_front()
            .map(str::to_owned)
            .ok_or(HouseInitError::Io(std::io::ErrorKind::UnexpectedEof))
    }
}

/// Required checks read per repository; repositories not listed are unreadable.
struct Protection(Vec<(&'static str, &'static [&'static str])>);
impl RequiredCheckSource for Protection {
    fn required_checks(&self, _: &HouseId, repository: &Repository) -> ObservedChecks {
        self.0
            .iter()
            .find(|(name, _)| *name == repository.as_str())
            .map_or(ObservedChecks::Unavailable, |(_, checks)| {
                ObservedChecks::Observed {
                    branch: "main".to_owned(),
                    checks: checks.iter().map(|check| (*check).to_owned()).collect(),
                }
            })
    }
}

fn facts(home: &Path) -> TestResult<InitFacts> {
    Ok(InitFacts {
        home: Some(home.to_path_buf()),
        checkout: Ok("acme/app".parse()?),
        kitchen: Some(CommitId::new(KITCHEN)?),
        agents: InstalledAgents::Observed {
            source: "PATH",
            found: vec![AgentFamily::Claude, AgentFamily::Codex],
        },
    })
}

/// Every flag answered, as a script would pass them.
fn flags(registry: &Path) -> InitAnswers {
    InitAnswers {
        registry: Some(registry.to_path_buf()),
        house: Some("acme".to_owned()),
        repositories: Some("acme/app".to_owned()),
        posting_destinations: Some("acme/app".to_owned()),
        sous_chef: Some("claude".to_owned()),
        station_cook: Some("codex".to_owned()),
        expediter: Some("claude".to_owned()),
        required_checks: Some("test,lint".to_owned()),
        required_reviewers: Some("expediter".to_owned()),
        kitchen: Some(KITCHEN.to_owned()),
        bundle: None,
        yes: true,
    }
}

fn confirmed(decision: InitDecision) -> TestResult<kitchen::house::HouseInitPlan> {
    match decision {
        InitDecision::Confirmed(plan) => Ok(plan),
        InitDecision::Declined(_) => Err("declined".into()),
    }
}

/// The house `--config` would register for the defaults of this checkout.
fn expected_config() -> TestResult<HouseConfig> {
    Ok(serde_json::from_str(&format!(
        r#"{{
            "schema": 1,
            "house": "acme",
            "kitchen": "{KITCHEN}",
            "guidance": "{KITCHEN}",
            "repositories": ["acme/app"],
            "postingDestinations": ["acme/app"],
            "requiredReviewers": ["expediter"],
            "requiredChecks": ["lint", "test"],
            "policyLimits": [],
            "grants": [],
            "agents": {{
                "default": {{"agent": "codex"}},
                "rules": [
                    {{"when": {{"role": "sous-chef"}}, "use": {{"agent": "claude"}}}},
                    {{"when": {{"role": "expediter"}}, "use": {{"agent": "claude"}}}}
                ]
            }}
        }}"#
    ))?)
}

#[test]
fn blank_answers_take_the_checkout_remote_and_station_defaults() -> TestResult {
    let temp = tempfile::tempdir()?;
    let home = temp.path().canonicalize()?;
    // Registry, house, repositories, destinations, three stations, checks,
    // reviewers, confirmation. The commit is this build's, so it is not asked.
    let mut script = Script::new(&["", "acme", "", "", "", "", "", "test, lint", "", "y"]);
    let plan = confirmed(plan_house_init(
        &InitAnswers::default(),
        &facts(&home)?,
        &NoGitHubAccess,
        Some(&mut script),
    )?)?;
    assert_eq!(plan.config, expected_config()?);
    assert_eq!(plan.registry, home.join(".kitchn"));
    assert_eq!(
        plan.bundle,
        default_guidance(&"acme".parse()?, &CommitId::new(KITCHEN)?)?
    );
    assert!(script.said("Repositories kitchen may work in [acme/app, from this checkout]: "));
    assert!(script.said("Found Claude Code and Codex on PATH."));
    assert!(script.said("No house-scoped GitHub access to read required checks"));
    // The printed configuration is what gets registered.
    assert!(script.said(r#""house": "acme""#));
    assert!(script.answers.is_empty());
    let policy = plan.config.agents.ok_or("no agents policy")?;
    for (role, agent) in [
        (Role::SousChef, AgentFamily::Claude),
        (Role::StationCook, AgentFamily::Codex),
        (Role::Expediter, AgentFamily::Claude),
        (Role::Commis, AgentFamily::Codex),
    ] {
        assert_eq!(
            policy.resolve(&SelectionRequest::new(role)).selection.agent,
            agent,
            "{role:?}"
        );
    }
    Ok(())
}

#[test]
fn a_declined_confirmation_writes_nothing() -> TestResult {
    let temp = tempfile::tempdir()?;
    let home = temp.path().canonicalize()?;
    let answers = InitAnswers {
        yes: false,
        ..flags(&home.join("registry"))
    };
    let mut script = Script::new(&["n"]);
    let decision = plan_house_init(&answers, &facts(&home)?, &NoGitHubAccess, Some(&mut script))?;
    assert!(matches!(decision, InitDecision::Declined(_)));
    assert!(script.said("Register house acme in "));
    assert!(!home.join("registry").exists());
    // End of input is not consent either.
    let mut silent = Script::new(&[]);
    assert!(matches!(
        plan_house_init(&answers, &facts(&home)?, &NoGitHubAccess, Some(&mut silent)),
        Err(HouseInitError::Io(std::io::ErrorKind::UnexpectedEof))
    ));
    assert!(!home.join("registry").exists());
    Ok(())
}

#[test]
fn a_config_output_failure_aborts_before_confirmation() -> TestResult {
    struct Broken;
    impl Prompter for Broken {
        fn show(&mut self, _: &str) -> Result<(), HouseInitError> {
            Err(HouseInitError::Io(std::io::ErrorKind::BrokenPipe))
        }
        fn ask(&mut self, _: &str) -> Result<String, HouseInitError> {
            Ok("y".to_owned())
        }
    }
    let temp = tempfile::tempdir()?;
    let home = temp.path().canonicalize()?;
    let answers = InitAnswers {
        yes: false,
        ..flags(&home.join("registry"))
    };
    let result = plan_house_init(&answers, &facts(&home)?, &NoGitHubAccess, Some(&mut Broken));
    assert!(matches!(
        result,
        Err(HouseInitError::Io(std::io::ErrorKind::BrokenPipe))
    ));
    assert!(!home.join("registry").exists());

    // The text shown is exactly what registration saves.
    let plan = confirmed(plan_house_init(
        &flags(&home.join("registry")),
        &facts(&home)?,
        &NoGitHubAccess,
        None,
    )?)?;
    let report = register_house(&plan)?;
    assert_eq!(
        plan.config_text()?.as_bytes(),
        fs::read(report.config_path)?
    );
    Ok(())
}

#[test]
fn unknown_agents_are_asked_without_claiming_any_available() -> TestResult {
    let temp = tempfile::tempdir()?;
    let home = temp.path().canonicalize()?;
    let answers = InitAnswers {
        sous_chef: None,
        station_cook: None,
        expediter: None,
        ..flags(&home.join("registry"))
    };
    let unknown = InitFacts {
        agents: InstalledAgents::Unknown,
        ..facts(&home)?
    };
    let mut script = Script::new(&["", "claude", "codex"]);
    let plan = confirmed(plan_house_init(
        &answers,
        &unknown,
        &NoGitHubAccess,
        Some(&mut script),
    )?)?;
    assert!(script.said("Kitchen cannot tell which agents are installed."));
    assert!(!script.said("Found"));
    assert!(!script.said("not found"));
    let policy = plan.config.agents.ok_or("no agents policy")?;
    assert_eq!(
        policy
            .resolve(&SelectionRequest::new(Role::StationCook))
            .selection
            .agent,
        AgentFamily::Claude
    );

    // Observed, but the chosen agent is missing: say so, keep the choice.
    let only_claude = InitFacts {
        agents: InstalledAgents::Observed {
            source: "PATH",
            found: vec![AgentFamily::Claude],
        },
        ..facts(&home)?
    };
    let mut script = Script::new(&["", "", ""]);
    confirmed(plan_house_init(
        &answers,
        &only_claude,
        &NoGitHubAccess,
        Some(&mut script),
    )?)?;
    assert!(script.said("Found Claude Code on PATH."));
    assert!(script.said("Codex was not found on PATH"));
    Ok(())
}

#[test]
fn non_interactive_input_names_every_missing_answer() -> TestResult {
    let temp = tempfile::tempdir()?;
    let home = temp.path().canonicalize()?;
    let error = plan_house_init(
        &InitAnswers::default(),
        &facts(&home)?,
        &NoGitHubAccess,
        None,
    )
    .err()
    .ok_or("planned without answers")?;
    assert!(matches!(
        &error,
        HouseInitError::MissingAnswers(missing) if *missing == [
            InitQuestion::House,
            InitQuestion::RequiredChecks,
            InitQuestion::Confirm,
        ]
    ));
    assert_eq!(error.class(), ErrorClass::InvalidInput);
    assert_eq!(
        error.to_string(),
        "standard input is not a terminal, so kitchen cannot ask; pass --house, --required-checks, --yes (or register a reviewed file with --config)"
    );
    assert!(!home.join(".kitchn").exists());

    // Only --yes missing still fails rather than registering.
    let answers = InitAnswers {
        yes: false,
        ..flags(&home.join("registry"))
    };
    assert!(matches!(
        plan_house_init(&answers, &facts(&home)?, &NoGitHubAccess, None),
        Err(HouseInitError::MissingAnswers(missing)) if missing == [InitQuestion::Confirm]
    ));

    // With every answer, defaults apply without a terminal.
    let answers = InitAnswers {
        house: Some("acme".to_owned()),
        required_checks: Some("test,lint".to_owned()),
        yes: true,
        ..InitAnswers::default()
    };
    let plan = confirmed(plan_house_init(
        &answers,
        &facts(&home)?,
        &NoGitHubAccess,
        None,
    )?)?;
    assert_eq!(plan.config, expected_config()?);
    Ok(())
}

#[test]
fn invalid_answers_are_asked_again_then_refused() -> TestResult {
    let temp = tempfile::tempdir()?;
    let home = temp.path().canonicalize()?;
    let answers = InitAnswers {
        posting_destinations: None,
        ..flags(&home.join("registry"))
    };
    // Outside the repositories, then a valid subset.
    let mut script = Script::new(&["other/repo", "acme/app"]);
    confirmed(plan_house_init(
        &answers,
        &facts(&home)?,
        &NoGitHubAccess,
        Some(&mut script),
    )?)?;
    assert!(script.said("Expected comma-separated owner/name repositories from the house's"));
    // No destinations is a valid read-only house.
    let mut script = Script::new(&["none"]);
    let plan = confirmed(plan_house_init(
        &answers,
        &facts(&home)?,
        &NoGitHubAccess,
        Some(&mut script),
    )?)?;
    assert!(plan.config.posting_destinations.is_empty());

    let mut script = Script::new(&["not a repo", "", "x/y/z"]);
    let answers = InitAnswers {
        repositories: None,
        ..flags(&home.join("registry"))
    };
    let no_checkout = InitFacts {
        checkout: Err(HouseError::RepositoryUnidentified),
        ..facts(&home)?
    };
    assert!(matches!(
        plan_house_init(&answers, &no_checkout, &NoGitHubAccess, Some(&mut script)),
        Err(HouseInitError::InvalidAnswer(InitQuestion::Repositories))
    ));
    assert!(script.said("No repository default from this directory"));
    assert!(script.said("An answer is required"));

    // An invalid flag is refused without a prompt.
    let answers = InitAnswers {
        station_cook: Some("gemini".to_owned()),
        ..flags(&home.join("registry"))
    };
    let mut script = Script::new(&[]);
    assert!(matches!(
        plan_house_init(&answers, &facts(&home)?, &NoGitHubAccess, Some(&mut script)),
        Err(HouseInitError::InvalidAnswer(InitQuestion::Agent(
            Station::StationCook
        )))
    ));
    assert!(script.answers.is_empty() && !script.said("Station cook"));

    let relative = InitAnswers {
        registry: Some(PathBuf::from("relative")),
        ..flags(&home)
    };
    assert!(matches!(
        plan_house_init(&relative, &facts(&home)?, &NoGitHubAccess, None),
        Err(HouseInitError::InvalidAnswer(InitQuestion::Registry))
    ));
    Ok(())
}

#[test]
fn required_checks_are_offered_only_when_every_repository_was_read() -> TestResult {
    let temp = tempfile::tempdir()?;
    let home = temp.path().canonicalize()?;
    let answers = InitAnswers {
        repositories: Some("acme/app,acme/api".to_owned()),
        posting_destinations: None,
        required_checks: None,
        ..flags(&home.join("registry"))
    };
    let both = Protection(vec![
        ("acme/app", &["test", "lint", "e2e"]),
        ("acme/api", &["test", "lint"]),
    ]);
    // Non-interactive: the common checks are the default.
    let plan = confirmed(plan_house_init(&answers, &facts(&home)?, &both, None)?)?;
    assert_eq!(
        plan.config.required_checks,
        BTreeSet::from(["lint".to_owned(), "test".to_owned()])
    );
    let mut script = Script::new(&["", ""]);
    confirmed(plan_house_init(
        &answers,
        &facts(&home)?,
        &both,
        Some(&mut script),
    )?)?;
    assert!(script.said("Required checks on acme/app main: e2e, lint, test"));
    assert!(script.said("Required checks [lint, test, from branch protection]: "));

    // One repository unreadable: no offer, so a script must answer.
    let one = Protection(vec![("acme/app", &["test"])]);
    assert!(matches!(
        plan_house_init(&answers, &facts(&home)?, &one, None),
        Err(HouseInitError::MissingAnswers(missing)) if missing == [InitQuestion::RequiredChecks]
    ));
    // Branch protection without checks offers none.
    let empty = Protection(vec![("acme/app", &[]), ("acme/api", &[])]);
    let plan = confirmed(plan_house_init(&answers, &facts(&home)?, &empty, None)?)?;
    assert!(plan.config.required_checks.is_empty());
    Ok(())
}

#[test]
fn registration_matches_the_config_path_and_pins_guidance() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let plan = confirmed(plan_house_init(
        &flags(&root.join("guided")),
        &facts(&root)?,
        &NoGitHubAccess,
        None,
    )?)?;
    let report = register_house(&plan)?;
    assert_eq!(report.config_path, root.join("guided/houses/acme.json"));
    let manual = HouseRegistry::new(root.join("manual"))?;
    manual.initialize(&expected_config()?)?;
    assert_eq!(
        fs::read(&report.config_path)?,
        fs::read(root.join("manual/houses/acme.json"))?
    );
    let guided = HouseRegistry::new(root.join("guided"))?;
    let house = guided.load(&"acme".parse()?)?;
    assert!(house.grants.is_empty() && house.policy_limits.is_empty());
    // The snapshot verifies without a separate house sync.
    assert_eq!(
        resolve_instructions(guided.root(), &house, None)?,
        report.instructions
    );

    // Rerunning with the same answers keeps everything; different answers
    // for the same house are a conflict and change nothing.
    register_house(&plan)?;
    let before = fs::read(&report.config_path)?;
    let mut changed = plan.clone();
    changed.config.required_checks.insert("e2e".to_owned());
    assert!(matches!(
        register_house(&changed),
        Err(HouseInitError::House(HouseError::Conflicts(_)))
    ));
    assert_eq!(fs::read(&report.config_path)?, before);
    Ok(())
}

#[test]
fn a_supplied_bundle_supplies_the_pins_and_must_match() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let bundle: InstructionBundle =
        serde_json::from_str(include_str!("fixtures/house/crabnebula-bundle.json"))?;
    let answers = InitAnswers {
        house: Some("crabnebula".to_owned()),
        repositories: Some("crabnebula/tauri-fixture".to_owned()),
        posting_destinations: None,
        kitchen: None,
        bundle: Some(bundle.clone()),
        ..flags(&root.join("registry"))
    };
    let plan = confirmed(plan_house_init(
        &answers,
        &facts(&root)?,
        &NoGitHubAccess,
        None,
    )?)?;
    assert_eq!(plan.config.kitchen, bundle.kitchen);
    assert_eq!(plan.config.guidance, bundle.guidance);
    register_house(&plan)?;

    let other_kitchen = InitAnswers {
        kitchen: Some(KITCHEN.to_owned()),
        ..answers.clone()
    };
    assert!(matches!(
        plan_house_init(&other_kitchen, &facts(&root)?, &NoGitHubAccess, None),
        Err(HouseInitError::House(HouseError::PinMismatch))
    ));
    let other_house = InitAnswers {
        house: Some("acme".to_owned()),
        ..answers
    };
    assert!(matches!(
        plan_house_init(&other_house, &facts(&root)?, &NoGitHubAccess, None),
        Err(HouseInitError::House(HouseError::PinMismatch))
    ));
    Ok(())
}

#[test]
fn embedded_guidance_needs_this_builds_commit() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = root.join("registry");

    // Matching --kitchen, and no --kitchen at all, pin the build's commit.
    for kitchen in [Some(KITCHEN.to_owned()), None] {
        let answers = InitAnswers {
            kitchen,
            ..flags(&registry)
        };
        let plan = confirmed(plan_house_init(
            &answers,
            &facts(&root)?,
            &NoGitHubAccess,
            None,
        )?)?;
        assert_eq!(plan.config.kitchen, CommitId::new(KITCHEN)?);
        assert_eq!(plan.bundle.guidance, CommitId::new(KITCHEN)?);
    }

    // A different commit would label bytes it never supplied.
    let other = InitAnswers {
        kitchen: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
        ..flags(&registry)
    };
    let error = plan_house_init(&other, &facts(&root)?, &NoGitHubAccess, None)
        .err()
        .ok_or("mismatched commit planned")?;
    assert!(matches!(error, HouseInitError::KitchenNotThisBuild));
    assert_eq!(error.class(), ErrorClass::InvalidInput);
    assert!(error.to_string().contains("--bundle"));

    // An unrecorded build commit cannot label the embedded guidance, even
    // when --kitchen is given; a supplied bundle still works.
    let unknown = InitFacts {
        kitchen: None,
        ..facts(&root)?
    };
    let error = plan_house_init(&flags(&registry), &unknown, &NoGitHubAccess, None)
        .err()
        .ok_or("planned without a build commit")?;
    assert!(matches!(error, HouseInitError::BuildCommitUnknown));
    assert!(error.to_string().contains("--bundle"));
    let mut script = Script::new(&["", "acme", "", "", "", "", "", "test", ""]);
    let prompted = plan_house_init(
        &InitAnswers::default(),
        &unknown,
        &NoGitHubAccess,
        Some(&mut script),
    );
    assert!(matches!(prompted, Err(HouseInitError::BuildCommitUnknown)));
    let bundle: InstructionBundle =
        serde_json::from_str(include_str!("fixtures/house/crabnebula-bundle.json"))?;
    let answers = InitAnswers {
        house: Some("crabnebula".to_owned()),
        repositories: Some("crabnebula/tauri-fixture".to_owned()),
        posting_destinations: None,
        kitchen: None,
        bundle: Some(bundle),
        ..flags(&registry)
    };
    confirmed(plan_house_init(&answers, &unknown, &NoGitHubAccess, None)?)?;
    assert!(!registry.exists());
    Ok(())
}

#[test]
fn a_registry_inside_a_repository_is_refused_before_writing() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    fs::create_dir(root.join(".git"))?;
    let plan = confirmed(plan_house_init(
        &flags(&root.join("registry")),
        &facts(&root)?,
        &NoGitHubAccess,
        None,
    )?)?;
    assert!(matches!(
        register_house(&plan),
        Err(HouseInitError::House(HouseError::InsideRepository))
    ));
    assert!(!root.join("registry").exists());
    Ok(())
}

#[test]
fn a_failed_pin_is_reported_and_a_rerun_resumes() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let plan = confirmed(plan_house_init(
        &flags(&root.join("registry")),
        &facts(&root)?,
        &NoGitHubAccess,
        None,
    )?)?;
    // Foreign content where the snapshot goes blocks the pin.
    let snapshot = root.join(format!("registry/snapshots/acme/{KITCHEN}-{KITCHEN}"));
    fs::create_dir_all(&snapshot)?;
    fs::write(snapshot.join("manifest.json"), "foreign")?;
    let error = register_house(&plan)
        .err()
        .ok_or("pinned over foreign content")?;
    assert!(matches!(error, HouseInitError::GuidanceNotPinned { .. }));
    assert_eq!(error.class(), ErrorClass::Conflict);
    assert!(root.join("registry/houses/acme.json").exists());
    assert_eq!(fs::read(snapshot.join("manifest.json"))?, b"foreign");

    fs::remove_file(snapshot.join("manifest.json"))?;
    register_house(&plan)?;
    let registry = HouseRegistry::new(root.join("registry"))?;
    resolve_instructions(registry.root(), &registry.load(&"acme".parse()?)?, None)?;
    Ok(())
}
