//! Pickup diagnostics that need no configuration, credentials, or backend.

use clap::{Args, Subcommand};
use kitchen::{
    contracts::{IssueNumber, Repository},
    workflows::pickup::{BranchName, IssueRef, issue_task_id},
};

/// Pickup, coordination, and repair diagnostics.
#[derive(Args)]
pub struct PickupArgs {
    #[command(subcommand)]
    command: PickupCommand,
}

#[derive(Subcommand)]
enum PickupCommand {
    /// Print the durable task id scheduled and interactive work share for an issue.
    TaskId {
        /// Repository as `owner/name`.
        repository: String,
        /// Issue number.
        issue: u64,
    },
    /// Validate an exact worker branch name without contacting Git.
    CheckBranch {
        /// The branch name, used verbatim.
        #[arg(allow_hyphen_values = true)]
        name: String,
    },
}

/// Run a pickup subcommand and return its output line.
pub fn run(args: PickupArgs) -> kitchen::Result<String> {
    match args.command {
        PickupCommand::TaskId { repository, issue } => {
            let issue = IssueRef {
                repository: Repository::new(&repository)?,
                number: IssueNumber::new(issue)?,
            };
            Ok(issue_task_id(&issue)?.to_string())
        }
        PickupCommand::CheckBranch { name } => Ok(BranchName::new(&name)?.to_string()),
    }
}
