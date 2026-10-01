//! Person-held confirmation of a settled writer's exact PR head.

use std::{
    io::{self, BufRead, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use clap::Args;
use kitchen::{
    HolderId, HouseId, TaskId,
    adapters::orca::{Invocation, OrcaRunner, SystemRunner},
    adoption::{HouseRegistry, RepositoryMatch},
    contracts::{
        Clock, CommitId, Effect, ExternalRef, IssueNumber, Operation, ResourceKind, SystemClock,
    },
    house::{checked_forge_credential, forge_binding, runtime_config},
    integrations::github::{
        GitHubClient, HeadLocation, IntegrationError, IssueState, Observation, ReadLimits,
    },
    state::{EffectState, HouseStore, StoreOptions, TaskState},
    workflows::{
        coordination::{current_worker, task_branch},
        push::{checkout_changes_except_report, launched_writer_worktree, observed_branch_head},
    },
};

use super::{forge::connect_gh, push::report_path};

#[derive(Args)]
pub struct PreserveArgs {
    /// Settled task that wrote the pull request branch.
    task: TaskId,
    /// The task's linked pull request.
    #[arg(long)]
    pull_request: u64,
    /// Exact head the person is confirming.
    #[arg(long)]
    head: CommitId,
    /// Person making the decision.
    #[arg(long)]
    holder: HolderId,
    /// House registry containing this repository's binding.
    #[arg(long)]
    registry: PathBuf,
    /// Launched worker worktree to inspect from a separate coordinator checkout.
    #[arg(long)]
    worktree: PathBuf,
    /// Record the decision after reviewing the printed observations.
    #[arg(long)]
    confirm_preserved: bool,
}

pub fn run(args: PreserveArgs) -> Result<(String, bool), kitchen::Error> {
    let path = std::env::current_dir()
        .map_err(|_| IntegrationError::InvalidInput)?
        .canonicalize()
        .map_err(|_| IntegrationError::InvalidInput)?;
    let target = args
        .worktree
        .canonicalize()
        .map_err(|_| IntegrationError::InvalidInput)?;
    if path.starts_with(&target)
        || worker_context(&path)
        || (args.confirm_preserved && !io::stdin().is_terminal())
    {
        return Err(IntegrationError::InvalidInput.into());
    }
    let registry = HouseRegistry::new(args.registry)?;
    let binding = match registry.resolve_repository(&path)? {
        RepositoryMatch::Bound(binding) => binding,
        RepositoryMatch::Unbound { .. } => return Err(IntegrationError::ScopeMismatch.into()),
    };
    let house_id: HouseId = binding.house;
    let repository = binding.repository;
    let house = registry.load(&house_id)?;
    let store = HouseStore::open(
        registry.store_path(&house_id)?,
        house_id.clone(),
        StoreOptions::default(),
    )?;
    let record = store.task(&args.task)?;
    let number = IssueNumber::new(args.pull_request)?;
    if !matches!(
        record.state(),
        TaskState::Settled {
            settlement: kitchen::contracts::Settlement::Succeeded,
            ..
        }
    ) || record.spec().repository.as_ref() != Some(&repository)
        || record.pull_request() != Some(number)
    {
        return Err(IntegrationError::ScopeMismatch.into());
    }
    let branch = task_branch(&record).ok_or(IntegrationError::InvalidInput)?;
    let (local_branch, local_head) =
        observed_branch_head(&target).map_err(|_| IntegrationError::Unknown)?;
    let runtime = runtime_config(&registry, &house_id)?
        .and_then(|config| config.orca)
        .ok_or(IntegrationError::Unknown)?;
    let worker = current_worker(&record).ok_or(IntegrationError::Unknown)?;
    let worktree_id = record
        .effects()
        .iter()
        .rev()
        .find_map(|effect| {
            if effect.request().attempt() != worker.attempt
                || !matches!(
                    effect.request().effect(),
                    Effect::Worker(Operation::LaunchWorker { .. })
                )
            {
                return None;
            }
            match effect.state() {
                EffectState::Applied { receipt, .. } => receipt
                    .created()
                    .iter()
                    .chain(receipt.touched())
                    .find(|resource| resource.kind == ResourceKind::Worktree)
                    .map(|resource| resource.handle.clone()),
                _ => None,
            }
        })
        .ok_or(IntegrationError::Unknown)?;
    let current = SystemRunner::new(runtime.executable).run(&Invocation::new(
        vec![
            "worktree".into(),
            "show".into(),
            "--worktree".into(),
            format!("id:{worktree_id}"),
            "--json".into(),
        ],
        Duration::from_secs(30),
    ))?;
    if current.exit_code != Some(0) {
        return Err(IntegrationError::Unknown.into());
    }
    let current: serde_json::Value =
        serde_json::from_slice(&current.stdout).map_err(|_| IntegrationError::Unknown)?;
    let worktree = &current["result"]["worktree"];
    let owned = current["ok"] == true
        && worktree["id"].as_str() == Some(worktree_id.as_str())
        && worktree["path"]
            .as_str()
            .and_then(|found| std::path::Path::new(found).canonicalize().ok())
            .as_deref()
            == Some(target.as_path())
        && worktree["projectId"].as_str() == Some(&format!("github:{repository}"))
        && worktree["branch"].as_str() == Some(&format!("refs/heads/{branch}"))
        && worktree["head"].as_str() == Some(local_head.as_str());
    if !owned {
        return Err(IntegrationError::PushPreflight(
            kitchen::integrations::github::PushPreflight::WorktreeOwnership,
        )
        .into());
    }
    let dirty = checkout_changes_except_report(&target, &report_path(&record)?)
        .map_err(|_| IntegrationError::Unknown)?;
    let forge = forge_binding(&registry, &house_id)?;
    let client = GitHubClient::new(
        forge.scope(&house)?,
        connect_gh(checked_forge_credential(&registry, &forge)?)?,
        ReadLimits::default(),
    );
    let live = match client.pull_request(&house_id, &repository, number) {
        Observation::Known(live) => live,
        _ => return Err(IntegrationError::Unknown.into()),
    };
    let live_head = &live.head.sha;
    let matched = live.state == IssueState::Open
        && live.head_location(&repository) == HeadLocation::SameRepository
        && live.head.name == branch.as_str()
        && live_head == &args.head
        && local_branch == branch
        && local_head == args.head;
    let unpushed = if &local_head == live_head {
        "none".to_owned()
    } else {
        format!("{live_head}..{local_head} (checkout differs from live PR head)")
    };
    let dirty_text = if dirty.is_empty() {
        "none".to_owned()
    } else {
        dirty
            .iter()
            .map(|path| path.escape_debug().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut output = format!(
        "pull request #{} live head {live_head}; checkout {local_branch} at {local_head}\ndirty files: {}\nunpushed range: {}",
        number.get(),
        dirty_text,
        unpushed,
    );
    if !matched || !dirty.is_empty() {
        output.push_str("\npreservation refused: checkout or live head contradicts the decision");
        return Ok((output, false));
    }
    if !args.confirm_preserved {
        output.push_str(
            "\nReview this evidence, then repeat with --confirm-preserved to record the decision.",
        );
        return Ok((output, true));
    }
    let prefix = &args.head.as_str()[..12];
    print!("Type {prefix} to confirm this exact PR head: ");
    io::stdout()
        .flush()
        .map_err(|_| IntegrationError::Unknown)?;
    confirm_head(
        prefix,
        io::BufReader::new(io::stdin().lock().take(128)),
        true,
    )?;
    let source = format!("owner-preserved-{}", args.holder);
    store.record_owner_preservation(
        &args.task,
        number,
        args.head,
        ExternalRef::new(&source)?,
        SystemClock.now(),
    )?;
    output.push_str("\npreservation recorded for this head");
    Ok((output, true))
}

fn worker_context(path: &Path) -> bool {
    if std::env::var_os("ORCA_TERMINAL_HANDLE").is_some()
        || std::env::var_os("ORCA_DISPATCH_ID").is_some()
    {
        return true;
    }
    launched_writer_worktree(path)
}

fn confirm_head(
    prefix: &str,
    mut input: impl BufRead,
    is_terminal: bool,
) -> Result<(), kitchen::Error> {
    if !is_terminal {
        return Err(IntegrationError::InvalidInput.into());
    }
    let mut typed = String::new();
    input
        .read_line(&mut typed)
        .map_err(|_| IntegrationError::Unknown)?;
    if typed.trim_end_matches(['\r', '\n']) != prefix {
        return Err(IntegrationError::InvalidInput.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command};

    #[test]
    fn owner_confirmation_needs_tty_and_exact_typed_head() {
        let prefix = "aaaaaaaaaaaa";
        assert!(confirm_head(prefix, "aaaaaaaaaaaa\n".as_bytes(), true).is_ok());
        assert!(confirm_head(prefix, "aaaaaaaaaaaa\n".as_bytes(), false).is_err());
        assert!(confirm_head(prefix, "aaaaaaaaaaab\n".as_bytes(), true).is_err());
    }

    #[test]
    fn launched_worktree_marker_refuses_owner_context() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["init", "-q"])
                .status()?
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["config", "--local", "extensions.worktreeConfig", "true"])
                .status()?
                .success()
        );
        assert!(!launched_writer_worktree(root));
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args([
                    "config",
                    "--worktree",
                    "kitchen.launchBase",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                ])
                .status()?
                .success()
        );
        assert!(launched_writer_worktree(root));
        fs::create_dir(root.join("nested"))?;
        assert!(launched_writer_worktree(&root.join("nested")));
        Ok(())
    }
}
