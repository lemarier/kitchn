//! `kitchn task cancel`: preview and owner settlement of a stuck task.

use std::{
    io::{self, BufRead, IsTerminal, Read, Write},
    path::PathBuf,
};

use clap::{Args, Subcommand};
use kitchen::{
    HolderId, HouseId, TaskId,
    adoption::origin_repository,
    contracts::{Capability, SystemClock, Text},
    workflows::{
        push::launched_writer_worktree,
        task_cancel::{self, CancelError},
    },
};

use super::run::{BackendArgs, Opened};

#[derive(Args)]
pub struct TaskArgs {
    #[command(subcommand)]
    command: TaskCommand,
}

#[derive(Subcommand)]
enum TaskCommand {
    /// Preview a stuck task; repeat with --confirm to settle it.
    Cancel {
        task: TaskId,
        /// Why this task should be cancelled; recorded with the decision.
        #[arg(long)]
        reason: String,
        /// Person making the decision (default: gh's configured GitHub user).
        #[arg(long)]
        holder: Option<HolderId>,
        /// House registry (default: KITCHN_HOME or ~/.kitchn).
        #[arg(long)]
        registry: PathBuf,
        /// House (default: this checkout's stored repository binding).
        #[arg(long)]
        house: HouseId,
        /// Settle after the checks printed in preview have been reviewed.
        #[arg(long)]
        confirm: bool,
    },
}

pub fn run(args: TaskArgs) -> Result<(String, bool), kitchen::Error> {
    let TaskCommand::Cancel {
        task,
        reason,
        holder,
        registry,
        house,
        confirm,
    } = args.command;
    let reason = Text::new(reason.trim())?;
    let cwd = std::env::current_dir().map_err(kitchen::house::HouseError::from)?;
    if std::env::var_os("ORCA_TERMINAL_HANDLE").is_some()
        || std::env::var_os("ORCA_DISPATCH_ID").is_some()
        || launched_writer_worktree(&cwd)
    {
        return Err(CancelError::Checkout.into());
    }
    let opened = Opened::open_parts(registry, &house, None, None)?;
    let record = opened.store.task(&task)?;
    if record.spec().repository.as_ref() != Some(&opened.repository)
        || origin_repository(&cwd).ok().as_ref() != Some(&opened.repository)
    {
        return Err(CancelError::Checkout.into());
    }
    let backend = opened.backend(
        &BackendArgs::default(),
        "task-cancel",
        None,
        &[Capability::WorkerStatusAndOutcome],
    )?;
    let preview = task_cancel::preview(&opened.store, backend.as_ref(), &task)?;
    let output = format!(
        "task {task}: {} effects reconciled; {} workers settled or stopped",
        preview.effects(),
        preview.workers()
    );
    if !confirm {
        return Ok((
            format!("{output}\nReview this preview, then repeat with --confirm to cancel."),
            true,
        ));
    }
    if !io::stdin().is_terminal() {
        return Err(CancelError::Confirmation.into());
    }
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{output}").map_err(|_| CancelError::Confirmation)?;
    write!(stdout, "Type {task} to confirm cancellation: ")
        .map_err(|_| CancelError::Confirmation)?;
    stdout.flush().map_err(|_| CancelError::Confirmation)?;
    let mut answer = String::new();
    io::BufReader::new(io::stdin().lock().take(256))
        .read_line(&mut answer)
        .map_err(|_| CancelError::Confirmation)?;
    if answer.trim_end_matches(['\r', '\n']) != task.as_str() {
        return Err(CancelError::Confirmation.into());
    }
    let holder = match holder {
        Some(holder) => holder,
        None => {
            let login = super::forge::gh_login(std::env::var_os("PATH"))
                .ok_or(kitchen::house::HouseError::MissingFlag { flag: "--holder" })?;
            HolderId::new(login.as_str())?
        }
    };
    task_cancel::cancel(
        &opened.store,
        backend.as_ref(),
        preview,
        holder.clone(),
        reason.clone(),
        &SystemClock,
    )?;
    Ok((
        format!(
            "{output}\ntask {task} cancelled by {holder}: {}",
            reason.as_str()
        ),
        true,
    ))
}
