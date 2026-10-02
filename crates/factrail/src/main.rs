//! `factrail`: verbatim, fact-keeping context compaction for coding agents.
//!
//! `factrail hook <event>` serves the Claude Code plugin (docs/protocol.md);
//! the other subcommands compact, replay, evaluate and export at rest.

#![forbid(unsafe_code)]

mod cli;
mod hook;
mod judges;
mod offline;

use std::io::Read;
use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command, OutputsAction};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Hook { event } => {
            let mut input = String::new();
            let body = match std::io::stdin().read_to_string(&mut input) {
                Ok(_) => hook::run(&event, &input),
                Err(e) => {
                    serde_json::json!({ "protocol": hook::PROTOCOL, "ok": false, "error": format!("stdin: {e}") })
                }
            };
            println!("{body}");
            return ExitCode::SUCCESS;
        }
        Command::Compact(args) => offline::compact(&args),
        Command::Eval(args) => match offline::eval(&args) {
            Ok(true) => Ok(()),
            Ok(false) => return ExitCode::FAILURE,
            Err(e) => Err(e),
        },
        Command::Dataset(args) => offline::dataset(&args),
        Command::Outputs {
            action: OutputsAction::Expire { days },
        } => offline::expire(days),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("factrail: {e}");
            ExitCode::FAILURE
        }
    }
}
