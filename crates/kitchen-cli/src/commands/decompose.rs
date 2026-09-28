//! `kitchen decompose preview`: validate a proposed project decomposition
//! and show the exact issues, owned paths, acceptance criteria, blocked-by
//! edges, and ownership overlaps a person approves, with the digest that
//! binds the approval. Reads only the proposal file; writes nothing and
//! contacts no forge. Writing happens through the library's
//! `workflows::decomposition::apply` with a person's approval of that digest.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use kitchen::{
    adoption::{decode, encode},
    house::HouseError,
    workflows::decomposition::{Proposal, preview},
};

#[derive(Args)]
pub struct DecomposeArgs {
    #[command(subcommand)]
    command: DecomposeCommand,
}

#[derive(Subcommand)]
enum DecomposeCommand {
    /// Show the preview and its digest. Exits 0 when it can be approved, 1
    /// while ownership overlaps are unordered, and 2 for an invalid proposal
    /// such as a dependency cycle.
    Preview {
        /// The proposal (JSON).
        #[arg(long)]
        proposal: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(args: DecomposeArgs) -> Result<(String, bool), kitchen::Error> {
    match args.command {
        DecomposeCommand::Preview { proposal, json } => {
            let proposal: Proposal = decode(&proposal)?;
            let preview = preview(&proposal)?;
            let output = if json {
                String::from_utf8(encode(&preview)?).map_err(|_| HouseError::InvalidInput)?
            } else {
                preview.render()
            };
            Ok((output, preview.ready()))
        }
    }
}
