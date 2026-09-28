//! Kitchen command-line entry point.

use std::{
    io::{self, Write},
    process::ExitCode,
};

use clap::{CommandFactory, Parser, Subcommand, error::ErrorKind};
use kitchen::{HouseId, TaskId};

mod update;

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
    /// Replace this executable with the latest GitHub release when it is newer.
    Update,
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
        Some(Command::ValidateHouse { id }) => HouseId::new(&id).map(|id| id.to_string()),
        Some(Command::ValidateTask { id }) => TaskId::new(&id).map(|id| id.to_string()),
        Some(Command::Update) => return run_update(),
        None => return output_status(Cli::command().print_help()),
    };
    match result {
        Ok(id) => output_status(writeln!(io::stdout().lock(), "{id}")),
        Err(error) => {
            let code = match error {
                kitchen::Error::IdentifierLength { .. } | kitchen::Error::IdentifierCharacters => 2,
            };
            if writeln!(io::stderr().lock(), "error: {error}").is_err() {
                return ExitCode::FAILURE;
            }
            ExitCode::from(code)
        }
    }
}

fn run_update() -> ExitCode {
    match update::run() {
        Ok(outcome) => output_status(writeln!(io::stdout().lock(), "{outcome}")),
        Err(error) => {
            // Exit 1 whether or not the diagnostic can be written.
            let _ = writeln!(io::stderr().lock(), "error: {error}");
            ExitCode::FAILURE
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
