//! Worker delivery through the checked push and durable PR effect.

use std::{
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::Args;
use kitchen::{
    HouseId, TaskId,
    adapters::{
        OrcaSession,
        orca::{
            DEFAULT_CALL_TIMEOUT, DEFAULT_LAUNCH_TIMEOUT, DEFAULT_RESERVATION_TIMEOUT, Invocation,
            OrcaRunner, SystemRunner,
        },
        resolve_backend,
    },
    adoption::HouseRegistry,
    contracts::{
        Capability, Clock, ExternalRef, GrantScope, HouseGrants, Permission, Repository,
        SystemClock, TaskAuthority, Text,
    },
    house::{CredentialKind, checked_forge_credential, runtime_config},
    integrations::github::{
        GitHubClient, GitHubExecutor, HouseScope, IntegrationError, PushPreflight, ReadLimits,
    },
    scheduling::AgentFamily,
    state::{AttemptState, HouseStore, StoreOptions, TaskState},
    workflows::{
        coordination::{Standing, current_worker, task_branch},
        pickup::{IssueRef, issue_task_id},
        push::{
            GitHubPullRequests, GitHubRemoteBranches, GitRemote, IsolatedGitConfig, OpenOutcome,
            OpenRequest, PullRequests, PushBoundary, PushIntent, PushOutcome, delivery_worker_live,
            launch_worktree, open_task_pull_request,
        },
        repair::Observed,
        stack::{LayerText, PullRequestText},
    },
};

use super::forge::connect_gh;

const DEADLINE: Duration = Duration::from_secs(30);

#[derive(Args)]
pub struct PushArgs {
    /// The house store named in the worker's launch brief.
    #[arg(long)]
    store: PathBuf,
    /// The house named in the brief.
    #[arg(long)]
    house: HouseId,
    /// The task named in the brief.
    #[arg(long)]
    task: TaskId,
    /// State that the evidence report covers all acceptance items.
    #[arg(long)]
    acceptance_done: bool,
}

struct Selected {
    registry: HouseRegistry,
    store: HouseStore,
    house: kitchen::house::HouseConfig,
    record: kitchen::state::TaskRecord,
    fence: kitchen::contracts::Fence,
    repository: kitchen::contracts::Repository,
}

impl PushArgs {
    fn select(&self) -> Result<Selected, kitchen::Error> {
        // This command is emitted only for the initialized store under the
        // registry. A copied brief cannot redirect it to another house.
        let store_path = self
            .store
            .canonicalize()
            .map_err(|_| kitchen::house::HouseError::HouseSelection)?;
        let root = store_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .ok_or(kitchen::house::HouseError::HouseSelection)?;
        let registry = HouseRegistry::new(root)?;
        if registry
            .store_path(&self.house)?
            .canonicalize()
            .ok()
            .as_deref()
            != Some(store_path.as_path())
        {
            return Err(kitchen::house::HouseError::HouseSelection.into());
        }
        let house = registry.load(&self.house)?;
        let store = HouseStore::open(&store_path, self.house.clone(), StoreOptions::default())?;
        let record = store.task(&self.task)?;
        let TaskState::Claimed { lease } = record.state() else {
            return Err(IntegrationError::PushPreflight(PushPreflight::Claim).into());
        };
        if !lease.is_live(SystemClock.now()) || record.cancel_request().is_some() {
            return Err(IntegrationError::PushPreflight(PushPreflight::Claim).into());
        }
        let worker = current_worker(&record)
            .ok_or(IntegrationError::PushPreflight(PushPreflight::Worker))?;
        let attempt = record
            .attempts()
            .last()
            .ok_or(IntegrationError::AttemptNotRunning)?;
        if attempt.number() != worker.attempt || attempt.state() != AttemptState::Running {
            return Err(IntegrationError::AttemptNotRunning.into());
        }
        let repository = record
            .spec()
            .repository
            .clone()
            .ok_or(kitchen::integrations::github::IntegrationError::InvalidInput)?;
        let grants = house.authority()?;
        let binding = super::forge::writer_binding(&registry, &self.house)?;
        require_app_binding(binding.credential_kind)?;
        let scope = binding.scope(&house)?;
        authorize_delivery(
            &record.spec().authority,
            &grants,
            &scope,
            &repository,
            &binding.backend,
            &binding.credential,
        )?;
        let fence = lease.fence();
        Ok(Selected {
            registry,
            store,
            house,
            record,
            fence,
            repository,
        })
    }
}

fn authorize_delivery(
    authority: &TaskAuthority,
    grants: &HouseGrants,
    scope: &HouseScope,
    repository: &Repository,
    backend: &kitchen::BackendId,
    credential: &kitchen::CredentialId,
) -> Result<(), kitchen::Error> {
    for permission in [Permission::PushBranch, Permission::OpenPullRequest] {
        let selected = authority.authorize(
            grants,
            permission,
            &GrantScope::Repository(repository.clone()),
            backend,
        )?;
        if &selected != credential {
            return Err(IntegrationError::CredentialMismatch(permission).into());
        }
        // Check the executor's policy before the branch update: it may be
        // narrower than the task's grant after a forge binding change.
        scope.authorize_effect(grants.house(), repository, permission, 0)?;
    }
    Ok(())
}

fn require_app_binding(kind: CredentialKind) -> Result<(), IntegrationError> {
    match kind {
        CredentialKind::GitHubApp(_) => Ok(()),
        CredentialKind::Token => Err(IntegrationError::AppRequiredForPush),
    }
}

pub fn run(args: PushArgs) -> Result<(String, bool), kitchen::Error> {
    let selected = args.select()?;
    let cwd = std::env::current_dir().map_err(|_| kitchen::house::HouseError::HouseSelection)?;
    let runtime = runtime_config(&selected.registry, &args.house)?
        .and_then(|runtime| runtime.orca)
        .ok_or(kitchen::house::HouseError::HouseSelection)?;
    let backend = resolve_backend(
        &selected.house,
        OrcaSession {
            run: runtime.run.clone(),
            coordinator: runtime.coordinator.clone(),
            repo: runtime.repo.clone(),
            base_branch: None,
            branch_prefix: None,
            agent: AgentFamily::Claude,
            call_timeout: DEFAULT_CALL_TIMEOUT,
            launch_timeout: DEFAULT_LAUNCH_TIMEOUT,
            runtime_dir: runtime.runtime_dir.clone(),
            reservation_timeout: DEFAULT_RESERVATION_TIMEOUT,
        },
        SystemRunner::new(runtime.executable.clone()),
        &[Capability::WorkerStatusAndOutcome],
    )?;
    delivery_worker_live(&selected.record, &backend)?;
    let current = SystemRunner::new(runtime.executable).run(&Invocation::new(
        vec!["worktree".into(), "current".into(), "--json".into()],
        DEADLINE,
    ))?;
    if current.exit_code != Some(0) {
        return Err(kitchen::integrations::github::IntegrationError::Unknown.into());
    }
    let current: serde_json::Value = serde_json::from_slice(&current.stdout)
        .map_err(|_| kitchen::integrations::github::IntegrationError::Unknown)?;
    if current["ok"] != true {
        return Err(IntegrationError::PushPreflight(PushPreflight::WorktreeContext).into());
    }
    let worktree = &current["result"]["worktree"];
    let worktree_id = worktree["id"]
        .as_str()
        .ok_or(kitchen::integrations::github::IntegrationError::Unknown)?;
    let worktree_path = worktree["path"]
        .as_str()
        .ok_or(kitchen::integrations::github::IntegrationError::Unknown)?;
    if !cwd
        .canonicalize()
        .map_err(|_| kitchen::house::HouseError::HouseSelection)?
        .starts_with(
            Path::new(worktree_path)
                .canonicalize()
                .map_err(|_| kitchen::house::HouseError::HouseSelection)?,
        )
    {
        return Err(IntegrationError::PushPreflight(PushPreflight::WorktreePath).into());
    }
    let worktree_id = ExternalRef::new(worktree_id)?;
    if !launch_worktree(&selected.record, &worktree_id) {
        return Err(IntegrationError::PushPreflight(PushPreflight::WorktreeOwnership).into());
    }
    if worktree["projectId"].as_str() != Some(&format!("github:{}", selected.repository)) {
        return Err(IntegrationError::PushPreflight(PushPreflight::WorktreeRepository).into());
    }
    if args.acceptance_done {
        acceptance_reported(&selected, Path::new(worktree_path))?;
    }
    let branch = task_branch(&selected.record)
        .ok_or(IntegrationError::PushPreflight(PushPreflight::Branch))?;
    let git = executable("git").ok_or(kitchen::house::HouseError::InvalidInput)?;
    let config = IsolatedGitConfig::create(
        &git,
        selected
            .registry
            .private_path(&args.house)?
            .join(format!("push-{}.gitconfig", args.task)),
        &[],
        DEADLINE,
    )?;
    let remote = GitRemote::new(
        git,
        PathBuf::from(worktree_path),
        "origin",
        config,
        DEADLINE,
    )?
    .with_github_https_transport(&selected.repository);
    let (actual_branch, head) = remote
        .checkout()
        .ok_or(kitchen::integrations::github::IntegrationError::Unknown)?;
    if actual_branch != branch
        || worktree["branch"].as_str() != Some(&format!("refs/heads/{branch}"))
        || worktree["head"].as_str() != Some(head.as_str())
    {
        return Err(IntegrationError::PushPreflight(PushPreflight::Checkout).into());
    }
    let worker = current_worker(&selected.record)
        .ok_or(IntegrationError::PushPreflight(PushPreflight::Worker))?;
    let launch = selected
        .record
        .effects()
        .iter()
        .rev()
        .find(|effect| {
            effect.request().attempt() == worker.attempt
                && matches!(
                    effect.request().effect(),
                    kitchen::contracts::Effect::Worker(
                        kitchen::contracts::Operation::LaunchWorker { .. }
                    )
                )
                && matches!(effect.state(), kitchen::state::EffectState::Applied { .. })
        })
        .ok_or(kitchen::workflows::push::PushWriterError::UnknownHistory)?;
    let writer_base =
        kitchen::adapters::orca::read_writer_base(&runtime.runtime_dir, launch.request().key())
            .map_err(|_| kitchen::workflows::push::PushWriterError::UnknownHistory)?;
    if writer_base.house != args.house
        || writer_base.worktree.as_str() != worktree_id.as_str()
        || writer_base.branch != branch
    {
        return Err(kitchen::workflows::push::PushWriterError::UnknownHistory.into());
    }
    let binding = super::forge::writer_binding(&selected.registry, &args.house)?;
    let (writer_name, writer_email) = binding
        .writer_identity()
        .ok_or(kitchen::workflows::push::PushWriterError::MissingIdentity)?;
    let gh = connect_gh(checked_forge_credential(&selected.registry, &binding)?)?;
    let remote = remote.with_push_credential(gh.clone(), binding.credential_ref());
    let client = GitHubClient::new(binding.scope(&selected.house)?, gh, ReadLimits::default());
    let reads = GitHubPullRequests {
        client: &client,
        house: &args.house,
        repository: &selected.repository,
    };
    let Observed::Known(default_branch) = reads.default_branch() else {
        return Err(kitchen::workflows::push::PushWriterError::UnknownHistory.into());
    };
    let kitchen::integrations::github::Observation::Known(default_tip) =
        client.branch_tip(&args.house, &selected.repository, &default_branch)
    else {
        return Err(kitchen::workflows::push::PushWriterError::UnknownHistory.into());
    };
    remote.verify_writer(&head, &default_tip, &writer_name, &writer_email)?;
    let remote_reads = GitHubRemoteBranches {
        git: &remote,
        client: &client,
        house: &args.house,
        repository: &selected.repository,
    };
    let grants = selected.house.authority()?;
    let outcome = PushBoundary {
        store: &selected.store,
        grants: &grants,
        destination: &binding.backend,
        stack_tool: selected.house.stack_tool,
        clock: &SystemClock,
        pull_requests: &reads,
        remote: &remote_reads,
        updater: &remote,
    }
    .push(
        &args.task,
        selected.fence,
        &PushIntent {
            pull_request: selected.record.pull_request(),
            expected_remote: kitchen::workflows::push::last_pushed_head(
                &selected.store,
                &args.task,
                &branch,
            )?,
        },
        &head,
    )?;
    match outcome {
        PushOutcome::Pushed { .. } | PushOutcome::AlreadyCurrent => {}
        PushOutcome::Refused(reason) => return Ok((format!("push refused: {reason:?}"), false)),
        PushOutcome::Stale => return Ok(("push refused: remote branch moved".into(), false)),
        PushOutcome::Uncertain => {
            return Ok((
                "push outcome uncertain; inspect remote before retry".into(),
                false,
            ));
        }
        _ => return Ok(("push outcome unsupported".into(), false)),
    }
    if let Some(number) = selected.record.pull_request() {
        let live = reads.pull_request(number);
        if !matches!(
            live,
            Observed::Known(Some(ref view))
                if view.state == kitchen::workflows::repair::PullRequestState::Open
                    && view.number == number
                    && view.head_branch == branch.as_str()
                    && view.head == head
        ) {
            return Ok((
                "pull request state changed; delivery unconfirmed".into(),
                false,
            ));
        }
        return Ok((
            format!(
                "branch {branch} at {head}; pull request #{} already linked",
                number.get()
            ),
            true,
        ));
    }
    let issue_number = args
        .task
        .as_str()
        .rsplit_once('-')
        .and_then(|(_, number)| number.parse::<u64>().ok())
        .and_then(|number| kitchen::contracts::IssueNumber::new(number).ok())
        .ok_or(kitchen::integrations::github::IntegrationError::InvalidInput)?;
    if issue_task_id(&IssueRef {
        repository: selected.repository.clone(),
        number: issue_number,
    })? != args.task
    {
        return Err(kitchen::integrations::github::IntegrationError::InvalidInput.into());
    }
    let Observed::Known(base) = reads.default_branch() else {
        return Err(kitchen::integrations::github::IntegrationError::Unknown.into());
    };
    let Observed::Known(PullRequestText { title, .. }) = remote.pull_request_text(&branch) else {
        return Err(kitchen::integrations::github::IntegrationError::Unknown.into());
    };
    let relation = if args.acceptance_done {
        "Closes"
    } else {
        "Part of"
    };
    let body = Text::new(&format!("{relation} #{}", issue_number.get()))?;
    let forge = GitHubExecutor::new(
        binding.backend.clone(),
        binding.scope(&selected.house)?,
        connect_gh(checked_forge_credential(&selected.registry, &binding)?)?,
        ReadLimits::default(),
    );
    let opened = open_task_pull_request(
        &selected.store,
        &grants,
        &binding.backend,
        &SystemClock,
        &forge,
        &Standing,
        OpenRequest {
            task: args.task.clone(),
            fence: selected.fence,
            head,
            base,
            title,
            body,
            reads: &reads,
        },
    )?;
    Ok(match opened {
        OpenOutcome::Opened(number) => (
            format!("opened pull request #{} for {branch}", number.get()),
            true,
        ),
        OpenOutcome::NotApplied => ("pull request was not opened".into(), false),
        OpenOutcome::Uncertain => (
            "pull request outcome uncertain; reconcile before retry".into(),
            false,
        ),
    })
}

fn acceptance_reported(selected: &Selected, worktree: &Path) -> Result<(), kitchen::Error> {
    let worker = current_worker(&selected.record)
        .ok_or(kitchen::integrations::github::IntegrationError::InvalidInput)?;
    let brief = selected
        .record
        .effects()
        .iter()
        .rev()
        .find_map(|effect| {
            if effect.request().attempt() != worker.attempt
                || !matches!(effect.state(), kitchen::state::EffectState::Applied { .. })
            {
                return None;
            }
            match effect.request().effect() {
                kitchen::contracts::Effect::Worker(
                    kitchen::contracts::Operation::LaunchWorker { brief, .. },
                ) => Some(brief.as_str()),
                _ => None,
            }
        })
        .ok_or(kitchen::integrations::github::IntegrationError::InvalidInput)?;
    let path = brief
        .lines()
        .find_map(|line| {
            line.strip_prefix("Evidence: write the report to ")
                .and_then(|line| {
                    line.split_once(", including commands run and their results.")
                        .map(|(path, _)| path)
                })
        })
        .ok_or(kitchen::integrations::github::IntegrationError::InvalidInput)?;
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(kitchen::integrations::github::IntegrationError::InvalidInput.into());
    }
    let canonical_worktree = worktree
        .canonicalize()
        .map_err(|_| kitchen::integrations::github::IntegrationError::InvalidInput)?;
    let canonical_file = worktree
        .join(path)
        .canonicalize()
        .map_err(|_| kitchen::integrations::github::IntegrationError::InvalidInput)?;
    if !canonical_file.starts_with(canonical_worktree) {
        return Err(IntegrationError::PushPreflight(PushPreflight::AcceptanceReport).into());
    }
    let file = std::fs::File::open(canonical_file)
        .map_err(|_| kitchen::integrations::github::IntegrationError::InvalidInput)?;
    let mut text = String::new();
    file.take(65537)
        .read_to_string(&mut text)
        .map_err(|_| kitchen::integrations::github::IntegrationError::InvalidInput)?;
    if text.len() > 65536 || !text.lines().any(|line| line == "Acceptance: done") {
        return Err(kitchen::integrations::github::IntegrationError::InvalidInput.into());
    }
    Ok(())
}

fn executable(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|path| path.is_absolute())
        .map(|path| path.join(name))
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kitchen::{
        BackendId, CredentialId,
        contracts::{Grant, PostingBudget},
        integrations::github::{AppId, CredentialRef, GitHubApp, InstallationId},
    };

    #[test]
    fn delivery_checks_both_forge_permissions_before_push() -> Result<(), Box<dyn std::error::Error>>
    {
        let house = HouseId::new("house")?;
        let repository = Repository::new("owner/repo")?;
        let backend = BackendId::new("github")?;
        let credential = CredentialId::new("app")?;
        let grants = [Permission::PushBranch, Permission::OpenPullRequest].map(|permission| {
            Grant::repository(
                permission,
                repository.clone(),
                backend.clone(),
                credential.clone(),
            )
        });
        let current = HouseGrants::new(house.clone(), grants.clone());
        let authority = TaskAuthority::delegate(&current, grants)?;
        let requester = ExternalRef::new("bot[bot]")?;
        let budget = PostingBudget::new(2)?;
        let scope = |permitted: Vec<Permission>| {
            HouseScope::new(
                house.clone(),
                [repository.clone()],
                requester.clone(),
                CredentialRef::new(house.clone(), credential.clone(), requester.clone()),
                budget,
                permitted,
            )
        };
        authorize_delivery(
            &authority,
            &current,
            &scope(vec![Permission::PushBranch, Permission::OpenPullRequest])?,
            &repository,
            &backend,
            &credential,
        )?;
        assert!(matches!(
            authorize_delivery(
                &authority,
                &current,
                &scope(vec![Permission::PushBranch])?,
                &repository,
                &backend,
                &credential
            ),
            Err(kitchen::Error::Integration(
                IntegrationError::MissingPermission(Permission::OpenPullRequest)
            ))
        ));
        assert!(matches!(
            authorize_delivery(
                &authority,
                &current,
                &scope(vec![Permission::PushBranch, Permission::OpenPullRequest])?,
                &repository,
                &backend,
                &CredentialId::new("other")?
            ),
            Err(kitchen::Error::Integration(
                IntegrationError::CredentialMismatch(Permission::PushBranch)
            ))
        ));
        Ok(())
    }

    #[test]
    fn worker_push_requires_an_app_binding() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            require_app_binding(CredentialKind::Token),
            Err(IntegrationError::AppRequiredForPush)
        );
        assert_eq!(
            require_app_binding(CredentialKind::GitHubApp(GitHubApp {
                app_id: AppId::new(1)?,
                installation: InstallationId::new(2)?,
            })),
            Ok(())
        );
        Ok(())
    }
}
