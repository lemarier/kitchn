//! House forge bindings and the approved-write hook, in disposable registries
//! with an in-memory forge. Simulated: no GitHub, `gh`, or token is used.
use kitchen::{
    BackendId, CredentialId, Error, ErrorClass, HolderId, HouseId,
    adoption::HouseRegistry,
    contracts::{
        Claimant, CommitId, EffectFailure, ExternalRef, GitHubAction, GitHubEffect, GitHubMutation,
        Grant, IssueNumber, Permission, PostingBudget, Repository, Text, Trigger,
    },
    house::{
        ApprovedWrite, BindOutcome, CredentialStatus, FORGE_BINDING_SCHEMA, ForgeBinding,
        ForgeError, ForgeKind, HouseConfig, HouseError, HouseInitError, InitAnswers, InitDecision,
        InitFacts, InitQuestion, InstalledAgents, NoGitHubAccess, Prompter, apply_approved,
        bind_forge, credential_path, credential_status, forge_binding, plan_house_init,
        register_house,
    },
    integrations::github::{
        CredentialFile, CredentialRef, GitHubExecutor, GitHubMutationTransport,
        GitHubReadTransport, IntegrationError, MutationRequest, ReadRequest,
    },
    scheduling::AgentFamily,
};
use std::{
    cell::Cell,
    collections::{BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
const KITCHEN: &str = "4f2a9c1e0b7d3a5f6c8e9d0a1b2c3d4e5f6a7b8c";
const TOKEN: &str = "fixture-token-never-copied";

fn house_config(policy_limits: BTreeSet<Grant>) -> TestResult<HouseConfig> {
    let app = Repository::new("acme/app")?;
    Ok(HouseConfig {
        schema: 1,
        house: HouseId::new("acme")?,
        kitchen: CommitId::new(KITCHEN)?,
        guidance: CommitId::new(KITCHEN)?,
        repositories: [app.clone()].into(),
        posting_destinations: [app].into(),
        required_reviewers: BTreeSet::new(),
        required_checks: BTreeSet::new(),
        policy_limits,
        grants: BTreeSet::new(),
        agents: None,
        stack_tool: None,
        schedules: None,
    })
}

fn grant(permission: Permission, credential: &str) -> TestResult<Grant> {
    Ok(Grant::repository(
        permission,
        Repository::new("acme/app")?,
        BackendId::new("github")?,
        CredentialId::new(credential)?,
    ))
}

fn binding(requester: &str) -> TestResult<ForgeBinding> {
    Ok(ForgeBinding {
        schema: FORGE_BINDING_SCHEMA,
        house: HouseId::new("acme")?,
        forge: ForgeKind::GitHub,
        backend: BackendId::new("github")?,
        requester: ExternalRef::new(requester)?,
        credential: CredentialId::new("github")?,
        posting_budget: PostingBudget::new(5)?,
    })
}

/// A registry holding house `acme` with `config`.
fn registry(root: &Path, config: &HouseConfig) -> TestResult<HouseRegistry> {
    let registry = HouseRegistry::new(root.join("registry"))?;
    registry.initialize(config)?;
    Ok(registry)
}

fn place_token(path: &Path, mode: u32) -> TestResult {
    fs::create_dir_all(path.parent().ok_or("no parent")?)?;
    fs::write(path, TOKEN)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

fn person() -> TestResult<Claimant> {
    Ok(Claimant {
        holder: HolderId::new("session-1")?,
        trigger: Trigger::Interactive,
        consumer: None,
    })
}

#[test]
fn a_binding_is_stored_privately_once_and_a_rerun_keeps_it() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = registry(&root, &house_config(BTreeSet::new())?)?;
    let bound = binding("acme-bot")?;

    assert_eq!(bind_forge(&registry, &bound)?, BindOutcome::Created);
    assert_eq!(bind_forge(&registry, &bound)?, BindOutcome::Unchanged);
    assert_eq!(forge_binding(&registry, &bound.house)?, bound);

    let stored = root.join("registry/private/acme/forge.json");
    let text = fs::read_to_string(&stored)?;
    assert!(text.contains(r#""requester": "acme-bot""#), "{text}");
    assert!(text.contains(r#""forge": "github""#), "{text}");
    assert!(!text.contains("credentials"), "no credential path: {text}");
    assert_eq!(
        credential_path(&registry, &bound)?,
        root.join("registry/private/acme/credentials/github")
    );
    Ok(())
}

#[test]
fn a_different_or_damaged_binding_is_kept_and_refused() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = registry(&root, &house_config(BTreeSet::new())?)?;
    bind_forge(&registry, &binding("acme-bot")?)?;
    let stored = root.join("registry/private/acme/forge.json");
    let before = fs::read(&stored)?;

    let error = bind_forge(&registry, &binding("someone-else")?)
        .err()
        .ok_or("replaced a binding")?;
    assert!(matches!(error, ForgeError::BindingConflict { .. }));
    assert_eq!(error.class(), ErrorClass::Conflict);
    assert_eq!(fs::read(&stored)?, before);

    fs::write(&stored, b"{not json")?;
    assert!(matches!(
        bind_forge(&registry, &binding("acme-bot")?),
        Err(ForgeError::BindingConflict { .. })
    ));
    assert_eq!(fs::read(&stored)?, b"{not json");
    Ok(())
}

#[test]
fn a_binding_for_another_house_or_schema_is_refused_before_writing() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = registry(&root, &house_config(BTreeSet::new())?)?;

    let unknown = ForgeBinding {
        house: HouseId::new("other")?,
        ..binding("acme-bot")?
    };
    assert!(bind_forge(&registry, &unknown).is_err());
    let future = ForgeBinding {
        schema: 2,
        ..binding("acme-bot")?
    };
    assert!(matches!(
        bind_forge(&registry, &future),
        Err(ForgeError::House(HouseError::InvalidInput))
    ));
    // Only logins GitHub can issue; they are printed into shell hints.
    for login in [
        "x;rm",
        "-lead",
        "trail-",
        "two--hyphens",
        "$(id)",
        &"a".repeat(40),
    ] {
        assert!(
            matches!(
                bind_forge(&registry, &binding(login)?),
                Err(ForgeError::House(HouseError::InvalidInput))
            ),
            "{login}"
        );
    }
    assert!(!root.join("registry/private").exists());
    for login in ["a", "octo-cat", "kitchen-app[bot]", &"a".repeat(39)] {
        assert!(
            ForgeKind::GitHub.accepts_requester(&ExternalRef::new(login)?),
            "{login}"
        );
    }

    // A stored file naming another house is not this house's binding.
    let path = root.join("registry/private/acme/forge.json");
    fs::create_dir_all(path.parent().ok_or("no parent")?)?;
    fs::write(&path, serde_json::to_vec(&unknown)?)?;
    assert!(matches!(
        forge_binding(&registry, &HouseId::new("acme")?),
        Err(ForgeError::House(HouseError::HouseSelection))
    ));
    Ok(())
}

#[test]
fn a_missing_binding_is_refused_by_name() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = registry(&root, &house_config(BTreeSet::new())?)?;
    let error = forge_binding(&registry, &HouseId::new("acme")?)
        .err()
        .ok_or("found a binding")?;
    assert!(matches!(&error, ForgeError::MissingBinding { house } if house.as_str() == "acme"));
    assert_eq!(error.class(), ErrorClass::Refused);
    assert_eq!(
        error.to_string(),
        "house acme has no forge binding, so kitchen cannot write to its forge; bind one with `kitchen forge bind --house acme` or `kitchen house init`"
    );
    Ok(())
}

#[test]
fn the_token_file_is_inspected_without_reading_or_following_it() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let token = root.join("credentials/github");
    assert_eq!(credential_status(&token)?, CredentialStatus::Missing);
    place_token(&token, 0o600)?;
    assert_eq!(credential_status(&token)?, CredentialStatus::Ready);
    #[cfg(unix)]
    {
        place_token(&token, 0o640)?;
        assert_eq!(credential_status(&token)?, CredentialStatus::Exposed);
        let link = root.join("credentials/link");
        std::os::unix::fs::symlink(&token, &link)?;
        assert_eq!(credential_status(&link)?, CredentialStatus::NotRegularFile);
    }
    fs::create_dir_all(root.join("credentials/directory"))?;
    assert_eq!(
        credential_status(&root.join("credentials/directory"))?,
        CredentialStatus::NotRegularFile
    );
    Ok(())
}

/// A forge that refuses every call; the hook's own tests never reach it.
struct Offline;
impl GitHubReadTransport for Offline {
    fn read(
        &self,
        _: &CredentialRef,
        _: &ReadRequest,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, IntegrationError> {
        Err(IntegrationError::Unavailable)
    }
}
impl GitHubMutationTransport for Offline {
    fn submit(
        &self,
        _: &CredentialRef,
        _: &MutationRequest,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, EffectFailure> {
        Err(EffectFailure::NotApplied(
            kitchen::contracts::NotAppliedReason::Rejected,
        ))
    }
}

/// A preview whose apply builds one comment effect through the executor it
/// was handed and counts its calls.
struct Comment {
    digest: &'static str,
    applied: Cell<u32>,
}
impl ApprovedWrite for Comment {
    type Digest = String;
    type Report = GitHubEffect;
    fn digest(&self) -> kitchen::Result<String> {
        Ok(self.digest.to_owned())
    }
    fn apply<T: GitHubMutationTransport>(
        &self,
        forge: &GitHubExecutor<T>,
        approved: &String,
        claimant: &Claimant,
    ) -> kitchen::Result<GitHubEffect> {
        assert_eq!(approved, self.digest);
        assert_eq!(claimant.trigger, Trigger::Interactive);
        self.applied.set(self.applied.get() + 1);
        Ok(forge.effect(GitHubMutation {
            repository: Repository::new("acme/app")?,
            action: GitHubAction::PostComment {
                issue: IssueNumber::new(1)?,
                body: Text::new("approved")?,
            },
        })?)
    }
}

fn comment() -> Comment {
    Comment {
        digest: "sha256:aaaa",
        applied: Cell::new(0),
    }
}

/// Run the hook, recording whether `connect` was reached and with what.
fn apply(
    registry: &HouseRegistry,
    write: &Comment,
    approved: &str,
    claimant: &Claimant,
    connected: &Cell<Option<CredentialRef>>,
) -> kitchen::Result<GitHubEffect> {
    apply_approved(
        registry,
        &HouseId::new("acme")?,
        write,
        &approved.to_owned(),
        claimant,
        |file: CredentialFile| {
            connected.set(Some(file.reference().clone()));
            Ok(Offline)
        },
    )
}

fn forge_error(result: kitchen::Result<GitHubEffect>) -> TestResult<ForgeError> {
    match result {
        Err(Error::Forge(error)) => Ok(error),
        Err(other) => Err(format!("unexpected error: {other}").into()),
        Ok(_) => Err("applied".into()),
    }
}

#[test]
fn an_approved_write_uses_the_bound_requester_budget_and_credential() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let limits = [grant(Permission::PostComment, "github")?].into();
    let registry = registry(&root, &house_config(limits)?)?;
    let bound = binding("acme-bot")?;
    bind_forge(&registry, &bound)?;
    place_token(&credential_path(&registry, &bound)?, 0o600)?;

    let write = comment();
    let connected = Cell::new(None);
    let effect = apply(&registry, &write, "sha256:aaaa", &person()?, &connected)?;
    assert_eq!(write.applied.get(), 1);
    assert_eq!(effect.requester.as_str(), "acme-bot");
    assert_eq!(effect.posting_budget.limit(), 5);
    assert_eq!(connected.take(), Some(bound.credential_ref()));
    // The binding still names only the credential, never its value.
    let stored = fs::read_to_string(root.join("registry/private/acme/forge.json"))?;
    assert!(!stored.contains(TOKEN));
    Ok(())
}

#[test]
fn policy_limits_for_another_credential_permit_nothing() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let limits = [grant(Permission::PostComment, "other")?].into();
    let registry = registry(&root, &house_config(limits)?)?;
    let bound = binding("acme-bot")?;
    bind_forge(&registry, &bound)?;
    place_token(&credential_path(&registry, &bound)?, 0o600)?;

    let result = apply(
        &registry,
        &comment(),
        "sha256:aaaa",
        &person()?,
        &Cell::new(None),
    );
    assert!(matches!(
        result,
        Err(Error::Integration(IntegrationError::PermissionDenied))
    ));
    Ok(())
}

#[test]
fn refusals_come_before_any_credential_is_read() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let limits = [grant(Permission::PostComment, "github")?].into();
    let registry = registry(&root, &house_config(limits)?)?;
    let write = comment();
    let connected = Cell::new(None);

    // No person present.
    let scheduled = Claimant {
        trigger: Trigger::Scheduled,
        ..person()?
    };
    let error = forge_error(apply(
        &registry,
        &write,
        "sha256:aaaa",
        &scheduled,
        &connected,
    ))?;
    assert!(matches!(error, ForgeError::NeedsPerson));
    assert_eq!(error.class(), ErrorClass::Refused);

    // No binding.
    let error = forge_error(apply(
        &registry,
        &write,
        "sha256:aaaa",
        &person()?,
        &connected,
    ))?;
    assert!(matches!(error, ForgeError::MissingBinding { .. }));

    let bound = binding("acme-bot")?;
    bind_forge(&registry, &bound)?;
    // A changed preview needs a new approval.
    let error = forge_error(apply(
        &registry,
        &write,
        "sha256:bbbb",
        &person()?,
        &connected,
    ))?;
    assert!(matches!(&error, ForgeError::StaleApproval { current } if current == "sha256:aaaa"));
    assert_eq!(error.class(), ErrorClass::Conflict);

    // No token file yet, then one other users can read.
    let error = forge_error(apply(
        &registry,
        &write,
        "sha256:aaaa",
        &person()?,
        &connected,
    ))?;
    assert!(matches!(
        error,
        ForgeError::CredentialUnavailable {
            status: CredentialStatus::Missing,
            ..
        }
    ));
    assert!(
        error
            .to_string()
            .contains("credential github of house acme is missing")
    );
    #[cfg(unix)]
    {
        place_token(&credential_path(&registry, &bound)?, 0o644)?;
        assert!(matches!(
            forge_error(apply(
                &registry,
                &write,
                "sha256:aaaa",
                &person()?,
                &connected
            ))?,
            ForgeError::CredentialUnavailable {
                status: CredentialStatus::Exposed,
                ..
            }
        ));
    }
    assert_eq!(write.applied.get(), 0);
    assert_eq!(connected.take(), None);
    Ok(())
}

#[test]
fn a_house_that_posts_nowhere_is_refused() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let config = HouseConfig {
        posting_destinations: BTreeSet::new(),
        ..house_config(BTreeSet::new())?
    };
    let registry = registry(&root, &config)?;
    let bound = binding("acme-bot")?;
    bind_forge(&registry, &bound)?;
    place_token(&credential_path(&registry, &bound)?, 0o600)?;
    let write = comment();
    let error = forge_error(apply(
        &registry,
        &write,
        "sha256:aaaa",
        &person()?,
        &Cell::new(None),
    ))?;
    assert!(matches!(error, ForgeError::NoPostingDestinations { .. }));
    assert_eq!(write.applied.get(), 0);
    Ok(())
}

/// Answers in order; records everything shown and asked.
struct Script {
    answers: VecDeque<&'static str>,
    transcript: Vec<String>,
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

fn facts(home: &Path, login: Option<&str>) -> TestResult<InitFacts> {
    Ok(InitFacts {
        home: Some(home.to_path_buf()),
        checkout: Ok("acme/app".parse()?),
        kitchen: Some(CommitId::new(KITCHEN)?),
        agents: InstalledAgents::Observed {
            source: "PATH",
            found: vec![AgentFamily::Claude, AgentFamily::Codex],
        },
        forge_login: login.map(ExternalRef::new).transpose()?,
    })
}

fn flags(registry: PathBuf) -> InitAnswers {
    InitAnswers {
        registry: Some(registry),
        house: Some("acme".to_owned()),
        required_checks: Some("none".to_owned()),
        yes: true,
        ..InitAnswers::default()
    }
}

#[test]
fn guided_init_offers_the_logged_in_gh_account_and_binds_it() -> TestResult {
    let temp = tempfile::tempdir()?;
    let home = temp.path().canonicalize()?;
    // Registry, house, repositories, destinations, three stations, checks,
    // reviewers, forge requester, credential name, confirmation.
    let mut script = Script {
        answers: ["", "acme", "", "", "", "", "", "none", "", "", "", "y"].into(),
        transcript: Vec::new(),
    };
    let decision = plan_house_init(
        &InitAnswers::default(),
        &facts(&home, Some("octo-cat"))?,
        &NoGitHubAccess,
        Some(&mut script),
    )?;
    let InitDecision::Confirmed(plan) = decision else {
        return Err("declined".into());
    };
    assert!(script.answers.is_empty());
    assert!(
        script
            .transcript
            .iter()
            .any(|line| line
                == "GitHub account kitchen writes as [octo-cat, logged-in gh account]: ")
    );
    assert!(
        script
            .transcript
            .iter()
            .any(|line| line == "Credential name for its token [github]: ")
    );
    let registry = home.join(".kitchn");
    let expected = ForgeBinding {
        requester: ExternalRef::new("octo-cat")?,
        posting_budget: PostingBudget::new(20)?,
        ..binding("octo-cat")?
    };
    assert_eq!(plan.forge.as_ref(), Some(&expected));
    let token = registry.join("private/acme/credentials/github");
    assert!(script.transcript.iter().any(|line| line.contains(&format!(
        "Forge: GitHub as octo-cat, credential github, at most 20 writes per task. Kitchen reads its token from {} and never copies it.",
        token.display()
    ))));

    let report = register_house(&plan)?;
    let bound = report.forge.ok_or("no forge binding stored")?;
    assert_eq!(bound.binding, expected);
    assert_eq!(bound.outcome, BindOutcome::Created);
    assert_eq!(bound.credential_path, token);
    assert_eq!(bound.credential, CredentialStatus::Missing);
    // Nothing but the binding was written for it; no token appeared.
    assert!(!token.exists());
    // A rerun with the same answers resumes without a conflict.
    let rerun = register_house(&plan)?;
    assert_eq!(
        rerun.forge.map(|bound| bound.outcome),
        Some(BindOutcome::Unchanged)
    );
    Ok(())
}

#[test]
fn guided_init_without_a_login_binds_nothing_by_default() -> TestResult {
    let temp = tempfile::tempdir()?;
    let home = temp.path().canonicalize()?;
    let registry = home.join("registry");
    let InitDecision::Confirmed(plan) = plan_house_init(
        &flags(registry.clone()),
        &facts(&home, None)?,
        &NoGitHubAccess,
        None,
    )?
    else {
        return Err("declined".into());
    };
    assert_eq!(plan.forge, None);
    assert!(plan.forge_text().contains("kitchen forge bind"));
    let report = register_house(&plan)?;
    assert_eq!(report.forge, None);
    assert!(!registry.join("private").exists());
    let registry = HouseRegistry::new(registry)?;
    assert!(matches!(
        forge_binding(&registry, &HouseId::new("acme")?),
        Err(ForgeError::MissingBinding { .. })
    ));

    // An inferred login is the default without a terminal too, and flags
    // override both answers; "none" declines.
    let InitDecision::Confirmed(plan) = plan_house_init(
        &InitAnswers {
            forge_requester: Some("acme-bot".to_owned()),
            forge_credential: Some("acme-token".to_owned()),
            ..flags(home.join("other"))
        },
        &facts(&home, Some("octo-cat"))?,
        &NoGitHubAccess,
        None,
    )?
    else {
        return Err("declined".into());
    };
    let forge = plan.forge.ok_or("no binding")?;
    assert_eq!(forge.requester.as_str(), "acme-bot");
    assert_eq!(forge.credential.as_str(), "acme-token");
    let InitDecision::Confirmed(plan) = plan_house_init(
        &InitAnswers {
            forge_requester: Some("none".to_owned()),
            ..flags(home.join("other"))
        },
        &facts(&home, Some("octo-cat"))?,
        &NoGitHubAccess,
        None,
    )?
    else {
        return Err("declined".into());
    };
    assert_eq!(plan.forge, None);
    Ok(())
}

#[test]
fn invalid_forge_answers_are_refused_by_flag() -> TestResult {
    let temp = tempfile::tempdir()?;
    let home = temp.path().canonicalize()?;
    for (answers, question) in [
        (
            InitAnswers {
                forge_requester: Some("x;rm".to_owned()),
                ..flags(home.join("registry"))
            },
            InitQuestion::ForgeRequester,
        ),
        (
            InitAnswers {
                forge_requester: Some("acme-bot".to_owned()),
                forge_credential: Some("../escape".to_owned()),
                ..flags(home.join("registry"))
            },
            InitQuestion::ForgeCredential,
        ),
    ] {
        let error = plan_house_init(&answers, &facts(&home, None)?, &NoGitHubAccess, None)
            .err()
            .ok_or("planned an invalid answer")?;
        assert!(
            matches!(&error, HouseInitError::InvalidAnswer(asked) if *asked == question),
            "{error}"
        );
        assert_eq!(error.class(), ErrorClass::InvalidInput);
    }
    assert!(!home.join("registry").exists());
    Ok(())
}
