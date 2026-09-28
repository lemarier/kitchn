//! Kitchen command-line entry point.

use clap::{CommandFactory, Parser};

#[derive(Parser)]
#[command(
    name = "kitchen",
    version,
    about = "Portable agent workflows",
    after_help = "Workspace bootstrap: automation commands are not implemented yet."
)]
struct Cli {}

fn main() -> std::io::Result<()> {
    let _cli = Cli::parse();
    Cli::command().print_help()
}
