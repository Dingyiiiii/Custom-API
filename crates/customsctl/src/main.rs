//! Command customsctl is the operator CLI for the customs gateway.
//!
//! It performs the tasks an administrator needs without a browser: issuing and
//! adjusting quota, suspending users, browsing the audit ledger, verifying its
//! integrity, and maintaining the price catalogue. Every command talks to the
//! gateway's own admin API where one exists, and to files directly where the
//! task is inherently offline (for example verifying a ledger).
//!
//! The printed text is part of the interface: `deploy/scripts/smoke-test.sh` and
//! `deploy/scripts/verify-deployment.sh` parse it, so the formats here are
//! byte-for-byte the Go ones, including the tabwriter alignment.

mod args;
mod commands;
mod ui;
mod util;

use std::error::Error;

use clap::{error::ErrorKind, Parser, Subcommand};
use customapi_core::version;

const USAGE: &str = r#"customsctl - operator CLI for the Custom-API customs gateway

Usage: customsctl <command> [flags]

Commands:
  version                     print build identity
  config check  -config FILE  validate a configuration file
  audit verify  -config FILE  verify the audit hash chain
  audit rehash  -config FILE  rebuild the chain in the current format (preserves a backup)
  audit tail    -config FILE  print the most recent audit records
  audit stats   -config FILE  aggregate usage from the audit ledger
  users list    -admin URL    list users and their quota
  users grant   -admin URL -user NAME -quota N
  users add     -admin URL -user NAME -quota N
  users suspend -admin URL -user NAME
  users resume  -admin URL -user NAME
  prices compare -model M [-input N] [-output N] -config FILE
  prices normalize -scheme S ...   convert a raw upstream price
  fingerprint baseline -config FILE -file OUT   (see also: benchd)
"#;

#[derive(Parser, Debug)]
#[command(
    name = "customsctl",
    disable_version_flag = true,
    disable_help_subcommand = true,
    about = "operator CLI for the Custom-API customs gateway"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// print build identity
    Version,
    /// validate a configuration file
    Config {
        #[command(subcommand)]
        command: commands::config::Command,
    },
    /// verify, repair, read or aggregate the audit ledger
    Audit {
        #[command(subcommand)]
        command: commands::audit::Command,
    },
    /// list users and change their quota, period or state
    Users {
        #[command(subcommand)]
        command: commands::users::Command,
    },
    /// compare and normalise prices
    Prices {
        #[command(subcommand)]
        command: commands::prices::Command,
    },
    /// A word that is not a known command, reported the way Go reports it.
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[tokio::main]
async fn main() {
    let argv = args::normalize_argv(std::env::args());
    if let Err(err) = run(argv).await {
        eprintln!("customsctl: {err}");
        std::process::exit(1);
    }
}

async fn run(argv: Vec<String>) -> Result<(), Box<dyn Error>> {
    let cli = match Cli::try_parse_from(&argv) {
        Ok(cli) => cli,
        Err(err) => return handle_parse_error(err),
    };
    match cli.command {
        None => {
            print!("{USAGE}");
            Ok(())
        }
        Some(Command::Version) => {
            println!("{}", version::identity());
            Ok(())
        }
        Some(Command::Config { command }) => match command {
            commands::config::Command::Check(args) => commands::config::check(&args),
        },
        Some(Command::Audit { command }) => match command {
            commands::audit::Command::Verify(args) => commands::audit::verify(&args),
            commands::audit::Command::Rehash(args) => commands::audit::rehash(&args),
            commands::audit::Command::Tail(args) => commands::audit::tail(&args),
            commands::audit::Command::Stats(args) => commands::audit::stats(&args),
        },
        Some(Command::Users { command }) => match command {
            commands::users::Command::List(args) => commands::users::list(&args).await,
            commands::users::Command::Grant(args) => commands::users::action(&args, "grant").await,
            commands::users::Command::Add(args) => {
                commands::users::action(&args, "add_quota").await
            }
            commands::users::Command::Suspend(args) => {
                commands::users::action(&args, "suspend").await
            }
            commands::users::Command::Resume(args) => {
                commands::users::action(&args, "resume").await
            }
        },
        Some(Command::Prices { command }) => match command {
            commands::prices::Command::Compare(args) => commands::prices::compare(&args),
            commands::prices::Command::Normalize(args) => commands::prices::normalize(&args),
        },
        Some(Command::External(words)) => {
            // Go prints the usage and reports the words it could not match.
            print!("{USAGE}");
            Err(format!("unknown command \"{}\"", words.join(" ")).into())
        }
    }
}

/// Turns a clap failure into Go's behaviour: a help request exits quietly, and
/// anything else prints the usage and one error line.
fn handle_parse_error(err: clap::Error) -> Result<(), Box<dyn Error>> {
    if matches!(
        err.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        err.print()?;
        return Ok(());
    }
    print!("{USAGE}");
    let text = err.to_string();
    let first = text.lines().next().unwrap_or("invalid arguments").trim();
    let message = first.strip_prefix("error: ").unwrap_or(first).to_string();
    Err(message.into())
}
