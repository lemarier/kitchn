//! Kitchen command-line entry point.

use std::{
    io::{self, Write},
    process::ExitCode,
};

use clap::{CommandFactory, Parser, Subcommand};
use kitchen::{HouseId, TaskId};

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
    /// Validate a house identifier without accessing configuration or credentials.
    ValidateHouse { id: String },
    /// Validate a Kitchen task identifier without contacting a backend.
    ValidateTask { id: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::ValidateHouse { id }) => HouseId::new(&id).map(|id| id.to_string()),
        Some(Command::ValidateTask { id }) => TaskId::new(&id).map(|id| id.to_string()),
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

fn output_status(result: io::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => {
            // The diagnostic channel can also be closed; exit failure either way.
            let _ = writeln!(io::stderr().lock(), "error: failed to write command output");
            ExitCode::FAILURE
        }
    }
}
