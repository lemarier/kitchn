//! Worker delivery through the checked push and durable PR effect.

use std::{
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::Args;
use kitchen::{
    HouseId, TaskId,
    adapters::orca::{Invocation, OrcaRunner, SystemRunner},
    adoption::HouseRegistry,
    contracts::{Clock, ExternalRef, GrantScope, Permission, SystemClock, Text},
    house::{checked_forge_credential, forge_binding, runtime_config},
    integrations::github::{GitHubClient, GitHubExecutor, ReadLimits},
    state::{AttemptState, HouseStore, StoreOptions, TaskState},
    workflows::{
        coordination::{Standing, current_worker, task_branch},
        pickup::{IssueRef, issue_task_id},
        push::{
            GitHubPullRequests, GitRemote, IsolatedGitConfig, OpenOutcome, OpenRequest,
            PullRequests, PushBoundary, PushIntent, PushOutcome, PushSetting,
            open_task_pull_request, owns_worktree, task_published,
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
    /// Internal Git credential-helper invocation.
    #[arg(long, hide = true)]
    credential_helper: bool,
    #[arg(hide = true)]
    helper_operation: Option<String>,
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
            return Err(kitchen::integrations::github::IntegrationError::PermissionDenied.into());
        };
        if !lease.is_live(SystemClock.now()) || record.cancel_request().is_some() {
            return Err(kitchen::integrations::github::IntegrationError::PermissionDenied.into());
        }
        let worker = current_worker(&record)
            .ok_or(kitchen::integrations::github::IntegrationError::PermissionDenied)?;
        let attempt = record
            .attempts()
            .last()
            .ok_or(kitchen::integrations::github::IntegrationError::PermissionDenied)?;
        if attempt.number() != worker.attempt
            || !matches!(
                attempt.state(),
                AttemptState::Running | AttemptState::Interrupted { .. }
            )
        {
            return Err(kitchen::integrations::github::IntegrationError::PermissionDenied.into());
        }
        let repository = record
            .spec()
            .repository
            .clone()
            .ok_or(kitchen::integrations::github::IntegrationError::InvalidInput)?;
        let grants = house.authority()?;
        let binding = forge_binding(&registry, &self.house)?;
        for permission in [Permission::PushBranch, Permission::OpenPullRequest] {
            record.spec().authority.authorize(
                &grants,
                permission,
                &GrantScope::Repository(repository.clone()),
                &binding.backend,
            )?;
        }
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

pub fn run(args: PushArgs) -> Result<(String, bool), kitchen::Error> {
    let selected = args.select()?;
    if args.credential_helper {
        return credential(&args, &selected);
    }
    let cwd = std::env::current_dir().map_err(|_| kitchen::house::HouseError::HouseSelection)?;
    let runtime = runtime_config(&selected.registry, &args.house)?
        .and_then(|runtime| runtime.orca)
        .ok_or(kitchen::house::HouseError::HouseSelection)?;
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
        return Err(kitchen::integrations::github::IntegrationError::PermissionDenied.into());
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
        return Err(kitchen::integrations::github::IntegrationError::PermissionDenied.into());
    }
    let worktree_id = ExternalRef::new(worktree_id)?;
    if !owns_worktree(&selected.record, &worktree_id)
        || worktree["projectId"].as_str() != Some(&format!("github:{}", selected.repository))
    {
        return Err(kitchen::integrations::github::IntegrationError::PermissionDenied.into());
    }
    if args.acceptance_done {
        acceptance_reported(&selected, Path::new(worktree_path))?;
    }
    let branch = task_branch(&selected.record)
        .ok_or(kitchen::integrations::github::IntegrationError::PermissionDenied)?;
    let git = executable("git").ok_or(kitchen::house::HouseError::InvalidInput)?;
    let self_exe = std::env::current_exe().map_err(|_| kitchen::house::HouseError::InvalidInput)?;
    let helper = format!(
        "!{} push --store {} --house {} --task {} --credential-helper",
        shell_quote(&self_exe.to_string_lossy()),
        shell_quote(&args.store.to_string_lossy()),
        shell_quote(&args.house.to_string()),
        shell_quote(&args.task.to_string())
    );
    let config = IsolatedGitConfig::create(
        &git,
        selected
            .registry
            .private_path(&args.house)?
            .join(format!("push-{}.gitconfig", args.task)),
        &[PushSetting::CredentialHelper { url: None, helper }],
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
        return Err(kitchen::integrations::github::IntegrationError::PermissionDenied.into());
    }
    let binding = forge_binding(&selected.registry, &args.house)?;
    let client = GitHubClient::new(
        binding.scope(&selected.house)?,
        connect_gh(checked_forge_credential(&selected.registry, &binding)?)?,
        ReadLimits::default(),
    );
    let reads = GitHubPullRequests {
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
        remote: &remote,
        updater: &remote,
    }
    .push(
        &args.task,
        selected.fence,
        &PushIntent {
            pull_request: selected.record.pull_request(),
            expected_remote: task_published(&selected.record)
                .then(|| remote.tracking_head(&branch))
                .flatten(),
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

fn credential(args: &PushArgs, selected: &Selected) -> Result<(String, bool), kitchen::Error> {
    if args.helper_operation.as_deref() != Some("get") {
        return Err(kitchen::integrations::github::IntegrationError::InvalidInput.into());
    }
    let mut input = String::new();
    std::io::stdin()
        .take(4097)
        .read_to_string(&mut input)
        .map_err(|_| kitchen::integrations::github::IntegrationError::InvalidInput)?;
    if input.len() > 4096
        || !input.lines().any(|line| line == "protocol=https")
        || !input.lines().any(|line| line == "host=github.com")
    {
        return Err(kitchen::integrations::github::IntegrationError::PermissionDenied.into());
    }
    let binding = forge_binding(&selected.registry, &args.house)?;
    let token = connect_gh(checked_forge_credential(&selected.registry, &binding)?)?
        .push_token(&binding.credential_ref(), &selected.repository)?;
    Ok((
        format!("protocol=https\nhost=github.com\nusername=x-access-token\npassword={token}"),
        true,
    ))
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
        return Err(kitchen::integrations::github::IntegrationError::PermissionDenied.into());
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
