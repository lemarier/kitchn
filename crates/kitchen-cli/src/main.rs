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
    name = "kitchen",
    version,
    about = "Portable agent workflows",
    after_help = "Workspace bootstrap: automation commands are not implemented yet."
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
    /// Preview dishwasher cleanup decisions; releases nothing.
    Cleanup(commands::cleanup::CleanupArgs),
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
