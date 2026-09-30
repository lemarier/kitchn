//! Kitchen command-line entry point.

use std::{
    io::{self, Write},
    process::ExitCode,
};

mod commands;

use clap::{CommandFactory, Parser, Subcommand, error::ErrorKind};
use kitchen::{ErrorClass, HouseId, TaskId};

#[derive(Parser)]
#[command(
    name = "kitchn",
    version,
    about = "Portable agent workflows",
    after_help = "Docs: https://getkitchn.com/docs/"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Bootstrap diagnostic: validate a house identifier without configuration or credentials.
    ValidateHouse { id: String },
    /// Bootstrap diagnostic: validate a task identifier without contacting a backend.
    ValidateTask { id: String },
    /// Configure a house and adopt repositories without activating workflows.
    House(commands::house::HouseArgs),
    /// Preview a new repository from a selected house template.
    Init(commands::scaffold::ScaffoldArgs),
    /// Preview house template additions or revisions in an existing repository.
    Adopt(commands::scaffold::ScaffoldArgs),
    /// Preview dishwasher cleanup decisions and record a person's approval; releases nothing.
    Cleanup(commands::cleanup::CleanupArgs),
    /// Preview a project's decomposition into dependency-linked issues, or post an approved one.
    Decompose(commands::decompose::DecomposeArgs),
    /// Daily issue hygiene: the scheduled precheck. Reads only.
    Gardener(commands::gardener::GardenerArgs),
    /// Schedule budget tick: pause exhausted schedules and report them.
    Budget(Box<commands::budget::BudgetArgs>),
    /// Offline issue pickup diagnostics.
    Pickup(commands::pickup::PickupArgs),
    /// Bind a house to the forge account it writes as, or show its binding.
    Forge(commands::forge::ForgeArgs),
    /// Record an independent review for the scheduled merge gate.
    Gate(commands::gate::GateArgs),
    /// Report how full the house store is and preview or apply its retention.
    Store(commands::store::StoreArgs),
    /// Report how full the trust ledger is and preview or apply its archival.
    Trust(commands::trust::TrustArgs),
    /// Preview the brigade audit and its draft proposals. Reads only.
    Audit(commands::audit::AuditArgs),
    /// Worker questions, reports, and escalations kept in the house store, and their answers.
    Mailbox(commands::mailbox::MailboxArgs),
    /// Run the house's due workflow passes once and record them in the run ledger.
    Tick(Box<commands::tick::TickArgs>),
    /// One scheduled pass of pickup, coordination, repair, or the gate, for a trigger.
    Run(commands::run::RunArgs),
    #[command(flatten)]
    Interactive(commands::interactive::InteractiveCommand),
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                return output_status(write!(io::stdout().lock(), "{}", error.render()));
            }
            if write!(io::stderr().lock(), "{}", error.render()).is_err() {
                return ExitCode::FAILURE;
            }
            return ExitCode::from(2);
        }
    };
    let result = match cli.command {
        Some(Command::ValidateHouse { id }) => HouseId::new(&id)
            .map(|id| (id.to_string(), true))
            .map_err(kitchen::Error::from),
        Some(Command::ValidateTask { id }) => TaskId::new(&id)
            .map(|id| (id.to_string(), true))
            .map_err(kitchen::Error::from),
        Some(Command::House(args)) => commands::house::run(args),
        Some(Command::Init(args)) => commands::scaffold::run(args, false),
        Some(Command::Adopt(args)) => commands::scaffold::run(args, true),
        Some(Command::Cleanup(args)) => commands::cleanup::run(args),
        Some(Command::Decompose(args)) => commands::decompose::run(args),
        // The precheck reports through its exit status, not the codes below.
        Some(Command::Gardener(args)) => return commands::gardener::run(args),
        Some(Command::Budget(args)) => match commands::budget::run(*args) {
            // The precheck reports through its exit status, not the codes below.
            commands::budget::Outcome::Exit(code) => return code,
            commands::budget::Outcome::Output(result) => result,
        },
        Some(Command::Pickup(args)) => commands::pickup::run(args).map(|output| (output, true)),
        Some(Command::Forge(args)) => commands::forge::run(args),
        Some(Command::Gate(args)) => commands::gate::run(args),
        Some(Command::Store(args)) => commands::store::run(args),
        Some(Command::Trust(args)) => commands::trust::run(args),
        Some(Command::Audit(args)) => commands::audit::run(args),
        Some(Command::Mailbox(args)) => commands::mailbox::run(args),
        Some(Command::Tick(args)) => commands::tick::run(*args),
        // A pass reports through its own exit status, not the codes below.
        Some(Command::Run(args)) => return commands::run::run(args),
        Some(Command::Interactive(command)) => commands::interactive::run(command),
        None => return output_status(Cli::command().print_help()),
    };
    match result {
        Ok((output, healthy)) => {
            let status = output_status(writeln!(io::stdout().lock(), "{output}"));
            if status == ExitCode::SUCCESS && !healthy {
                ExitCode::FAILURE
            } else {
                status
            }
        }
        Err(error) => {
            let code = match error.class() {
                ErrorClass::InvalidInput => 2,
                ErrorClass::Refused | ErrorClass::Conflict | ErrorClass::Execution => 1,
            };
            if writeln!(io::stderr().lock(), "error: {error}").is_err() {
                return ExitCode::FAILURE;
            }
            ExitCode::from(code)
        }
    }
}

fn output_status(result: io::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The diagnostic channel can also be closed; exit failure either way.
            let _ = writeln!(
                io::stderr().lock(),
                "error: failed to write command output ({:?})",
                error.kind()
            );
            ExitCode::FAILURE
        }
    }
}
