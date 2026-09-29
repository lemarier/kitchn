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
        merge_readiness: Default::default(),
        disk_pressure: None,
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
        "_lead",
        "trail_",
        "two__under",
        "mixed-_run",
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
    for login in [
        "a",
        "octo-cat",
        "user_acme",
        "kitchen-app[bot]",
        &"a".repeat(39),
    ] {
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
        "house acme has no forge binding, so kitchn cannot write to its forge; bind one with `kitchn forge bind --house acme` or `kitchn house init`"
    );
    Ok(())
}

#[test]
fn the_token_file_is_inspected_without_reading_or_following_it() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = registry(&root, &house_config(BTreeSet::new())?)?;
    let bound = binding("acme-bot")?;
    let token = credential_path(&registry, &bound)?;
    assert_eq!(
        credential_status(&registry, &bound)?,
        CredentialStatus::Missing
    );
    place_token(&token, 0o600)?;
    assert_eq!(
        credential_status(&registry, &bound)?,
        CredentialStatus::Ready
    );
    #[cfg(unix)]
    {
        place_token(&token, 0o640)?;
        assert_eq!(
            credential_status(&registry, &bound)?,
            CredentialStatus::Exposed
        );
        let elsewhere = root.join("elsewhere");
        place_token(&elsewhere, 0o600)?;
        fs::remove_file(&token)?;
        std::os::unix::fs::symlink(&elsewhere, &token)?;
        assert_eq!(
            credential_status(&registry, &bound)?,
            CredentialStatus::NotRegularFile
        );
        fs::remove_file(&token)?;
    }
    fs::create_dir_all(&token)?;
    assert_eq!(
        credential_status(&registry, &bound)?,
        CredentialStatus::NotRegularFile
    );
    Ok(())
}

/// Replace `link` with a symbolic link to `target`.
#[cfg(unix)]
fn redirect(link: &Path, target: &Path) -> TestResult {
    if link.is_dir() {
        fs::remove_dir_all(link)?;
    }
    fs::create_dir_all(link.parent().ok_or("no parent")?)?;
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_redirected_credentials_directory_is_refused() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let limits = [grant(Permission::PostComment, "github")?].into();
    let registry = registry(&root, &house_config(limits)?)?;
    let bound = binding("acme-bot")?;
    bind_forge(&registry, &bound)?;
    // Another house's private token, ready in every other respect.
    let other = root.join("registry/private/other/credentials");
    place_token(&other.join("github"), 0o600)?;
    let credentials = root.join("registry/private/acme/credentials");
    redirect(&credentials, &other)?;

    assert_eq!(
        credential_status(&registry, &bound)?,
        CredentialStatus::Redirected
    );
    let connected = Cell::new(None);
    let write = comment();
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
            status: CredentialStatus::Redirected,
            ..
        }
    ));
    assert!(!error.to_string().contains(TOKEN));
    assert_eq!(connected.take(), None);
    assert_eq!(write.applied.get(), 0);

    // An external directory is refused the same way.
    let external = root.join("external");
    place_token(&external.join("github"), 0o600)?;
    fs::remove_file(&credentials)?;
    redirect(&credentials, &external)?;
    assert_eq!(
        credential_status(&registry, &bound)?,
        CredentialStatus::Redirected
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_redirected_house_directory_is_refused() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = registry(&root, &house_config(BTreeSet::new())?)?;
    let bound = binding("acme-bot")?;
    // The house's private directory points at a copy holding a ready token.
    let copy = root.join("copy");
    place_token(&copy.join("credentials/github"), 0o600)?;
    redirect(&root.join("registry/private/acme"), &copy)?;
    assert!(matches!(
        credential_status(&registry, &bound),
        Err(ForgeError::House(HouseError::RedirectedPath))
    ));
    // Without the redirect, the same layout is ready.
    fs::remove_file(root.join("registry/private/acme"))?;
    place_token(&credential_path(&registry, &bound)?, 0o600)?;
    assert_eq!(
        credential_status(&registry, &bound)?,
        CredentialStatus::Ready
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
        script.transcript.iter().any(
            |line| line == "GitHub account kitchn writes as [octo-cat, logged-in gh account]: "
        )
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
    assert!(plan.forge_text().contains("kitchn forge bind"));
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

/// Approved drafts and decompositions written through `apply_approved`, and
/// acknowledgements re-read through `forge_reader`, against an in-memory
/// forge. Simulated: no GitHub, `gh`, or token is used.
mod approved_writes {
    use super::{TestResult, binding, grant, house_config, person, place_token, registry};
    use kitchen::{
        Error, HouseId,
        adoption::HouseRegistry,
        contracts::{
            Clock, EffectExecutor, EffectFailure, IssueNumber, LeaseTtl, NotAppliedReason,
            Permission, Provenance, Repository, Settlement, Timestamp, UncertainReason,
        },
        house::{
            ForgeError, HouseConfig, apply_approved, bind_forge, credential_path, forge_reader,
        },
        integrations::github::{
            CredentialRef, GitHubMutationTransport, GitHubReadTransport, IntegrationError,
            MutationRequest, ReadRequest,
        },
        state::{EffectOutcome, HouseStore, RiskAction, RiskDecision, StoreOptions},
        workflows::{
            decomposition::{
                ApplyOptions, ApplyOutcome, ApplyReport, ApprovedDecomposition, Blocker, IssueKey,
                OwnedPath, PreviewDigest, Proposal, ProposedIssue, preview,
            },
            interactive::{
                AcknowledgeOutcome, Acknowledgement, ApprovedDraft, DraftDigest, DraftOptions,
                DraftOutcome, DraftReport, DraftTarget, IssueDraft, ReadBack, acknowledge_draft,
                draft_preview, draft_task_id,
            },
        },
    };
    use serde_json::{Value, json};
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
        time::Duration,
    };

    const REPO: &str = "acme/app";
    const REQUESTER: &str = "acme-bot";

    /// A forge holding numbered issues and blocked-by links.
    #[derive(Default)]
    struct Forge {
        issues: Vec<Value>,
        relations: BTreeMap<String, Vec<Value>>,
        submissions: usize,
        reads: usize,
        /// Submission (1-based) whose response is lost after it applied.
        lose_after: Option<usize>,
    }

    impl Forge {
        fn seeded() -> Self {
            let mut forge = Self::default();
            forge.issues.push(json!({
                "id": 1007, "number": 7, "title": "Existing prerequisite", "body": "",
                "user": {"login": "maintainer"},
                "html_url": format!("https://github.com/{REPO}/issues/7"),
            }));
            forge
        }
        fn created_titled(&self, title: &str) -> usize {
            self.issues
                .iter()
                .filter(|issue| issue["user"]["login"] == REQUESTER && issue["title"] == title)
                .count()
        }
        fn blocked_by(&self, issue: u64) -> Vec<u64> {
            self.relations
                .get(&format!(
                    "repos/{REPO}/issues/{issue}/dependencies/blocked_by"
                ))
                .into_iter()
                .flatten()
                .filter_map(|entry| entry["number"].as_u64())
                .collect()
        }
        fn number_titled(&self, title: &str) -> Option<u64> {
            self.issues
                .iter()
                .find(|issue| issue["title"] == title)
                .and_then(|issue| issue["number"].as_u64())
        }
    }

    struct Transport<'a>(&'a RefCell<Forge>);

    impl GitHubReadTransport for Transport<'_> {
        fn read(
            &self,
            _: &CredentialRef,
            request: &ReadRequest,
            _: Duration,
            _: usize,
        ) -> Result<Vec<u8>, IntegrationError> {
            let mut forge = self.0.borrow_mut();
            forge.reads += 1;
            let endpoint = request.endpoint();
            let path = endpoint.split('?').next().unwrap_or(endpoint);
            let first_page = !endpoint.contains("page=") || endpoint.ends_with("&page=1");
            let root = format!("repos/{REPO}/issues");
            let value = if path == root {
                let mut listed: Vec<Value> = if first_page {
                    forge
                        .issues
                        .iter()
                        .filter(|issue| {
                            !endpoint.contains("creator=") || issue["user"]["login"] == REQUESTER
                        })
                        .cloned()
                        .collect()
                } else {
                    Vec::new()
                };
                listed.reverse();
                json!(listed)
            } else if path.ends_with("/dependencies/blocked_by") {
                let entries = if first_page {
                    forge.relations.get(path).cloned().unwrap_or_default()
                } else {
                    Vec::new()
                };
                json!(entries)
            } else if let Some(number) = path
                .strip_prefix(&format!("{root}/"))
                .and_then(|n| n.parse::<u64>().ok())
            {
                forge
                    .issues
                    .iter()
                    .find(|issue| issue["number"].as_u64() == Some(number))
                    .cloned()
                    .ok_or(IntegrationError::NotFound)?
            } else {
                return Err(IntegrationError::Unknown);
            };
            serde_json::to_vec(&value).map_err(|_| IntegrationError::Unknown)
        }
    }

    impl GitHubMutationTransport for Transport<'_> {
        fn submit(
            &self,
            _: &CredentialRef,
            request: &MutationRequest,
            _: Duration,
            _: usize,
        ) -> Result<Vec<u8>, EffectFailure> {
            let mut forge = self.0.borrow_mut();
            forge.submissions += 1;
            let path = request.endpoint().to_owned();
            let body = request.body().clone();
            if path == format!("repos/{REPO}/issues") {
                let number = forge
                    .issues
                    .iter()
                    .filter_map(|issue| issue["number"].as_u64())
                    .max()
                    .unwrap_or(0)
                    + 1;
                forge.issues.push(json!({
                    "id": 1000 + number, "number": number,
                    "title": body["title"], "body": body["body"],
                    "user": {"login": REQUESTER},
                    "html_url": format!("https://github.com/{REPO}/issues/{number}"),
                }));
            } else if path.ends_with("/dependencies/blocked_by") {
                let id = body["issue_id"]
                    .as_u64()
                    .ok_or(EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
                forge.relations.entry(path).or_default().push(json!({
                    "number": id - 1000,
                    "repository_url": format!("https://api.github.com/repos/{REPO}"),
                }));
            } else {
                return Err(EffectFailure::NotApplied(NotAppliedReason::Rejected));
            }
            if forge.lose_after == Some(forge.submissions) {
                return Err(EffectFailure::Uncertain(UncertainReason::ResponseLost));
            }
            Ok(b"{}".to_vec())
        }
    }

    /// A clock that moves one second each time it is read.
    struct Ticking(Cell<u64>);
    impl Clock for Ticking {
        fn now(&self) -> Timestamp {
            self.0.set(self.0.get() + 1000);
            Timestamp::from_unix_millis(self.0.get())
        }
    }

    /// House `acme` with its registry, store, and a ready token; bound to
    /// the forge unless `bound` is false.
    struct Bound {
        _temp: tempfile::TempDir,
        registry: HouseRegistry,
        config: HouseConfig,
        store: HouseStore,
        clock: Ticking,
        /// Whether `connect` was reached.
        connected: Cell<bool>,
    }

    impl Bound {
        fn new(bound: bool) -> TestResult<Self> {
            let temp = tempfile::tempdir()?;
            let root = temp.path().canonicalize()?;
            let limits = [
                grant(Permission::CreateIssue, "github")?,
                grant(Permission::EditIssueRelationships, "github")?,
            ]
            .into();
            let config = house_config(limits)?;
            let registry = registry(&root, &config)?;
            if bound {
                let binding = binding(REQUESTER)?;
                bind_forge(&registry, &binding)?;
                place_token(&credential_path(&registry, &binding)?, 0o600)?;
            }
            let store = HouseStore::initialize(
                root.join("store"),
                config.house.clone(),
                StoreOptions::default(),
            )?;
            Ok(Self {
                _temp: temp,
                registry,
                config,
                store,
                clock: Ticking(Cell::new(1_000_000)),
                connected: Cell::new(false),
            })
        }

        fn house(&self) -> &HouseId {
            &self.config.house
        }

        fn provenance(&self) -> Provenance {
            Provenance {
                kitchen: self.config.kitchen.clone(),
                house_guidance: self.config.guidance.clone(),
                repository_instructions: None,
            }
        }

        fn post_draft(
            &self,
            forge: &RefCell<Forge>,
            draft: &IssueDraft,
            approved: &DraftDigest,
        ) -> kitchen::Result<DraftReport> {
            let grants = self.config.authority()?;
            let options = DraftOptions {
                provenance: self.provenance(),
                lease: LeaseTtl::new(Duration::from_secs(600))?,
            };
            let write = ApprovedDraft {
                draft,
                store: &self.store,
                grants: &grants,
                clock: &self.clock,
                options: &options,
            };
            apply_approved(
                &self.registry,
                self.house(),
                &write,
                approved,
                &person().map_err(|_| ForgeError::NeedsPerson)?,
                |_| {
                    self.connected.set(true);
                    Ok(Transport(forge))
                },
            )
        }

        fn post_decomposition(
            &self,
            forge: &RefCell<Forge>,
            proposal: &Proposal,
            approved: &PreviewDigest,
        ) -> kitchen::Result<ApplyReport> {
            let grants = self.config.authority()?;
            let options = ApplyOptions {
                provenance: self.provenance(),
                lease: LeaseTtl::new(Duration::from_secs(600))?,
            };
            let write = ApprovedDecomposition {
                proposal,
                store: &self.store,
                grants: &grants,
                clock: &self.clock,
                options: &options,
            };
            apply_approved(
                &self.registry,
                self.house(),
                &write,
                approved,
                &person().map_err(|_| ForgeError::NeedsPerson)?,
                |_| {
                    self.connected.set(true);
                    Ok(Transport(forge))
                },
            )
        }
    }

    fn draft(body: &str) -> TestResult<IssueDraft> {
        Ok(IssueDraft {
            repository: Repository::new(REPO)?,
            target: DraftTarget::New {
                title: "Add flash retry".to_owned(),
                body: body.to_owned(),
            },
            add_labels: Vec::new(),
            remove_labels: Vec::new(),
            blocked_by: vec![IssueNumber::new(7)?],
            questions: Vec::new(),
        })
    }

    fn digest(draft: &IssueDraft) -> TestResult<DraftDigest> {
        Ok(draft_preview(draft)?.digest)
    }

    fn forge_error<T: std::fmt::Debug>(result: kitchen::Result<T>) -> TestResult<ForgeError> {
        match result {
            Err(Error::Forge(error)) => Ok(error),
            other => Err(format!("expected a forge refusal, got {other:?}").into()),
        }
    }

    #[test]
    fn an_approved_draft_posts_once_and_a_rerun_posts_nothing_more() -> TestResult {
        let house = Bound::new(true)?;
        let forge = RefCell::new(Forge::seeded());
        let draft = draft("## Outcome\nRetries a failed flash once.")?;
        let approved = digest(&draft)?;

        let report = house.post_draft(&forge, &draft, &approved)?;
        assert_eq!(report.outcome, DraftOutcome::Completed);
        assert_eq!(report.issue, Some(IssueNumber::new(8)?));
        assert_eq!(forge.borrow().created_titled("Add flash retry"), 1);
        assert_eq!(forge.borrow().blocked_by(8), vec![7]);
        assert_eq!(forge.borrow().submissions, 2);
        // The created issue carries the approved body.
        let body = forge
            .borrow()
            .issues
            .last()
            .map(|issue| issue["body"].clone());
        assert!(
            body.and_then(|body| body.as_str().map(str::to_owned))
                .is_some_and(|body| body.starts_with("## Outcome\nRetries a failed flash once."))
        );

        // The same approval again finds the settled task: nothing is posted.
        let again = house.post_draft(&forge, &draft, &approved)?;
        assert_eq!(again.outcome, DraftOutcome::Completed);
        assert_eq!(again.written.len(), 2);
        assert!(again.written.iter().all(|written| written.reused));
        assert_eq!(forge.borrow().submissions, 2);
        assert_eq!(forge.borrow().created_titled("Add flash retry"), 1);
        Ok(())
    }

    #[test]
    fn an_interrupted_draft_resumes_without_a_duplicate_issue() -> TestResult {
        let house = Bound::new(true)?;
        let forge = RefCell::new(Forge {
            lose_after: Some(1),
            ..Forge::seeded()
        });
        let draft = draft("## Outcome\nRetries a failed flash once.")?;
        let approved = digest(&draft)?;

        // The create applied, but its response was lost after the read-back
        // too, so the run stops before linking.
        let lost = house.post_draft(&forge, &draft, &approved)?;
        assert!(
            matches!(&lost.outcome, DraftOutcome::Uncertain { effect } if effect.as_str() == "create"),
            "{:?}",
            lost.outcome
        );
        assert_eq!(forge.borrow().submissions, 1);

        // The rerun reconciles the create from the forge and submits only the
        // link.
        let resumed = house.post_draft(&forge, &draft, &approved)?;
        assert_eq!(resumed.outcome, DraftOutcome::Completed);
        assert_eq!(forge.borrow().created_titled("Add flash retry"), 1);
        assert_eq!(forge.borrow().submissions, 2);
        assert_eq!(forge.borrow().blocked_by(8), vec![7]);
        let reused: Vec<_> = resumed
            .written
            .iter()
            .map(|written| (written.effect.as_str().to_owned(), written.reused))
            .collect();
        assert_eq!(
            reused,
            vec![
                ("create".to_owned(), true),
                ("blocked-by-7".to_owned(), false)
            ]
        );
        Ok(())
    }

    #[test]
    fn a_changed_draft_needs_a_new_approval_before_any_credential_is_read() -> TestResult {
        let house = Bound::new(true)?;
        let forge = RefCell::new(Forge::seeded());
        let original = draft("## Outcome\nRetries a failed flash once.")?;
        let approved = digest(&original)?;
        let changed = draft("## Outcome\nRetries a failed flash twice.")?;

        let error = forge_error(house.post_draft(&forge, &changed, &approved))?;
        let current = digest(&changed)?;
        assert!(
            matches!(&error, ForgeError::StaleApproval { current: shown } if *shown == current.to_string()),
            "{error}"
        );
        assert!(!house.connected.get());
        assert_eq!(forge.borrow().submissions, 0);
        assert_eq!(forge.borrow().reads, 0);
        assert!(house.store.tasks()?.is_empty());

        // Approving the changed preview's own digest posts it.
        let report = house.post_draft(&forge, &changed, &current)?;
        assert_eq!(report.outcome, DraftOutcome::Completed);
        assert_eq!(forge.borrow().created_titled("Add flash retry"), 1);
        Ok(())
    }

    #[test]
    fn a_draft_for_a_house_without_a_forge_binding_is_refused_by_name() -> TestResult {
        let house = Bound::new(false)?;
        let forge = RefCell::new(Forge::seeded());
        let draft = draft("## Outcome\nRetries a failed flash once.")?;
        let error = forge_error(house.post_draft(&forge, &draft, &digest(&draft)?))?;
        assert!(matches!(&error, ForgeError::MissingBinding { house } if house.as_str() == "acme"));
        assert!(
            error
                .to_string()
                .starts_with("house acme has no forge binding"),
            "{error}"
        );
        assert!(!house.connected.get());
        assert_eq!(forge.borrow().submissions, 0);
        assert!(house.store.tasks()?.is_empty());
        Ok(())
    }

    fn proposal(core_title: &str) -> TestResult<Proposal> {
        let issue = |key: &str, title: &str, path: &str, blocked_by| -> TestResult<ProposedIssue> {
            Ok(ProposedIssue {
                key: IssueKey::new(key)?,
                phase: None,
                title: title.to_owned(),
                outcome: format!("{title} works."),
                owned_paths: vec![OwnedPath::new(path)?],
                acceptance: vec![format!("{title} has tests")],
                blocked_by,
            })
        };
        Ok(Proposal {
            repository: Repository::new(REPO)?,
            parent: None,
            issues: vec![
                issue("core", core_title, "crates/core", Vec::new())?,
                issue(
                    "api",
                    "Build the api",
                    "crates/api",
                    vec![Blocker::Proposed(IssueKey::new("core")?)],
                )?,
            ],
        })
    }

    #[test]
    fn an_approved_decomposition_resumes_after_a_lost_response_without_duplicates() -> TestResult {
        let house = Bound::new(true)?;
        // The second create applies but its response is lost.
        let forge = RefCell::new(Forge {
            lose_after: Some(2),
            ..Forge::seeded()
        });
        let proposal = proposal("Build the core")?;
        let approved = preview(&proposal)?.digest;

        let lost = house.post_decomposition(&forge, &proposal, &approved)?;
        assert!(
            matches!(lost.outcome, ApplyOutcome::Uncertain { .. }),
            "{:?}",
            lost.outcome
        );
        assert_eq!(forge.borrow().submissions, 2);

        let resumed = house.post_decomposition(&forge, &proposal, &approved)?;
        assert_eq!(resumed.outcome, ApplyOutcome::Completed);
        let forge_now = forge.borrow();
        assert_eq!(forge_now.created_titled("Build the core"), 1);
        assert_eq!(forge_now.created_titled("Build the api"), 1);
        let core = forge_now.number_titled("Build the core").ok_or("no core")?;
        let api = forge_now.number_titled("Build the api").ok_or("no api")?;
        assert_eq!(forge_now.blocked_by(api), vec![core]);
        // Two creates and one link: the lost create was never resubmitted.
        assert_eq!(forge_now.submissions, 3);
        drop(forge_now);

        // Once settled, the same approval posts nothing.
        let again = house.post_decomposition(&forge, &proposal, &approved)?;
        assert_eq!(again.outcome, ApplyOutcome::Settled(Settlement::Succeeded));
        assert_eq!(forge.borrow().submissions, 3);
        Ok(())
    }

    #[test]
    fn a_changed_or_unbound_decomposition_writes_nothing() -> TestResult {
        let house = Bound::new(true)?;
        let forge = RefCell::new(Forge::seeded());
        let approved = preview(&proposal("Build the core")?)?.digest;
        let changed = proposal("Build the core crate")?;
        let error = forge_error(house.post_decomposition(&forge, &changed, &approved))?;
        assert!(matches!(error, ForgeError::StaleApproval { .. }), "{error}");

        let unbound = Bound::new(false)?;
        let error = forge_error(unbound.post_decomposition(
            &forge,
            &proposal("Build the core")?,
            &approved,
        ))?;
        assert!(
            matches!(error, ForgeError::MissingBinding { .. }),
            "{error}"
        );

        assert!(!house.connected.get() && !unbound.connected.get());
        assert_eq!(forge.borrow().submissions, 0);
        assert!(house.store.tasks()?.is_empty() && unbound.store.tasks()?.is_empty());
        Ok(())
    }

    /// Settle `draft` with its create's outcome unknown: the response was
    /// lost after the forge applied it, and the person cancelled the task.
    fn settle_with_unknown_create(
        house: &Bound,
        forge: &RefCell<Forge>,
        draft: &IssueDraft,
    ) -> TestResult<kitchen::TaskId> {
        let next = forge.borrow().submissions + 1;
        forge.borrow_mut().lose_after = Some(next);
        let lost = house.post_draft(forge, draft, &digest(draft)?)?;
        assert!(matches!(lost.outcome, DraftOutcome::Uncertain { .. }));
        let id = draft_task_id(&draft_preview(draft)?)?;
        let store = &house.store;
        let now = house.clock.now();
        let holder = kitchen::HolderId::new("session-1")?;
        let lease = store.claim(
            &id,
            &person()?,
            LeaseTtl::new(Duration::from_secs(600))?,
            now,
        )?;
        let record = store.task(&id)?;
        let effect = record.effects().first().ok_or("no effect")?;
        store.record_effect_outcome(
            &id,
            lease.fence(),
            effect.seq(),
            EffectOutcome::Unresolvable,
            now,
        )?;
        store.accept_risk(
            &id,
            lease.fence(),
            effect.seq(),
            RiskDecision {
                effect: effect.request().key().clone(),
                decided_by: holder.clone(),
                revision: record.evidence().revision(),
                action: RiskAction::SettleUnsuccessfully,
            },
            now,
        )?;
        store.request_cancel(&id, &holder, now)?;
        store.settle_cancelled(&id, lease.fence(), now)?;
        Ok(id)
    }

    #[test]
    fn an_acknowledgement_rereads_through_the_binding_and_submits_nothing() -> TestResult {
        let house = Bound::new(true)?;
        let forge = RefCell::new(Forge::seeded());
        let draft = draft("## Outcome\nRetries a failed flash once.")?;
        let stuck = settle_with_unknown_create(&house, &forge, &draft)?;
        let submitted = forge.borrow().submissions;

        let reader = forge_reader(&house.registry, house.house(), |_| Ok(Transport(&forge)))?;
        // The reader refuses every submission before it reaches the forge.
        let record = house.store.task(&stuck)?;
        let request = record.effects().first().ok_or("no effect")?.request();
        assert_eq!(
            reader.execute(request),
            Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
        );

        let report = acknowledge_draft(
            &house.store,
            Some(&reader),
            &stuck,
            &Acknowledgement {
                reason: "Issue #8 is the approved draft.".parse()?,
                accept_unknown: false,
            },
            &person()?,
            &house.clock,
        )?;
        // The re-read proved the lost create applied, so no acceptance of
        // an unknown write was needed.
        assert_eq!(
            report.writes.first().map(|write| &write.state),
            Some(&ReadBack::Applied {
                reference: kitchen::contracts::ExternalRef::new(&format!(
                    "https://github.com/{REPO}/issues/8"
                ))?,
            })
        );
        let AcknowledgeOutcome::Recorded(recorded) = report.outcome else {
            return Err(format!("not recorded: {:?}", report.outcome).into());
        };
        assert!(recorded.unresolved.is_empty());
        assert_eq!(forge.borrow().submissions, submitted);
        Ok(())
    }

    #[test]
    fn a_forge_reader_needs_a_binding_and_a_ready_token() -> TestResult {
        let unbound = Bound::new(false)?;
        let forge = RefCell::new(Forge::seeded());
        let connected = Cell::new(false);
        let reader = forge_reader(&unbound.registry, unbound.house(), |_| {
            connected.set(true);
            Ok(Transport(&forge))
        });
        assert!(matches!(reader, Err(ForgeError::MissingBinding { .. })));

        let house = Bound::new(true)?;
        let binding = binding(REQUESTER)?;
        std::fs::remove_file(credential_path(&house.registry, &binding)?)?;
        let reader = forge_reader(&house.registry, house.house(), |_| {
            connected.set(true);
            Ok(Transport(&forge))
        });
        assert!(matches!(
            reader,
            Err(ForgeError::CredentialUnavailable { .. })
        ));
        assert!(!connected.get());
        assert_eq!(forge.borrow().reads, 0);
        Ok(())
    }
}
