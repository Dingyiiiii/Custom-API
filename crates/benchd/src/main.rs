//! Command benchd is the customs intelligence daemon.
//!
//! It performs the work of layer L3 on a schedule:
//!
//! - `prices` collect and normalise upstream prices;
//! - `fingerprint` L1 behavioural probes against every channel;
//! - `bench` L2 layered capability suite;
//! - `score` fold the evidence into the routing scorecard.
//!
//! It can run as a long-lived service (`run`) or execute a single task and exit,
//! which is what the FreeBSD rc.d script and cron entries use.

mod cli;
mod io;
mod tasks;

use std::path::Path;

use clap::{error::ErrorKind, CommandFactory, Parser};
use customapi_core::{config, version};

use crate::cli::Cli;
use crate::tasks::{Options, TaskError};

#[tokio::main]
async fn main() {
    let argv = cli::normalize_argv(std::env::args());
    if let Err(err) = run(argv).await {
        eprintln!("benchd: {err}");
        std::process::exit(1);
    }
}

async fn run(argv: Vec<String>) -> Result<(), TaskError> {
    let cli = match Cli::try_parse_from(&argv) {
        Ok(cli) => cli,
        Err(err) => return handle_parse_error(err),
    };
    if cli.version {
        println!("{}", version::identity());
        return Ok(());
    }
    let Some(config_path) = cli.config.clone().filter(|p| !p.is_empty()) else {
        // Go prints the usage before reporting the missing flag.
        eprint!("{}", Cli::command().render_help());
        return Err("--config is required".into());
    };

    let env_file = cli.env_file.clone().filter(|p| !p.is_empty());
    let cfg =
        config::load_with_env_file(Path::new(&config_path), env_file.as_deref().map(Path::new))?;

    // The output directory defaults to the configuration file's directory, so a
    // deployment keeps its evidence next to the file that describes it.
    let out_dir = if cli.out.is_empty() {
        io::go_dir(&config_path)
    } else {
        cli.out.clone()
    };
    // A probe identity from the configuration is the intended setup; --token
    // exists for one-off runs against a gateway whose config is not at hand.
    let token = if cli.token.is_empty() {
        tasks::probe_token(&cfg)
    } else {
        cli.token.clone()
    };
    if token.is_empty() {
        return Err("no probe credential: set --token, or configure a user named \"probe\" (or \"*-probe\") with unlimited quota".into());
    }

    let opts = Options {
        config_path: config_path.clone(),
        base_url: cli.gateway.clone(),
        token,
        model: cli.model.clone(),
        channel: cli.channel.clone(),
        out_dir,
        log_level: cli.log_level.clone(),
        prices_url: cli.prices_url.clone(),
        group: cli.group.clone(),
        include_outcomes: cli.outcomes,
    };

    let filter = match opts.log_level.to_ascii_lowercase().as_str() {
        "debug" => log::LevelFilter::Debug,
        "warn" | "warning" => log::LevelFilter::Warn,
        "error" => log::LevelFilter::Error,
        _ => log::LevelFilter::Info,
    };
    env_logger::Builder::new().filter_level(filter).init();

    let task = cli.task.clone().unwrap_or_else(|| "run".to_string());
    match task.as_str() {
        "prices" => tasks::prices(&cfg, &opts).await,
        "fingerprint" => tasks::fingerprint(&cfg, &opts).await,
        "bench" => tasks::bench(&cfg, &opts).await,
        "score" => tasks::score(&cfg, &opts),
        "run" => tasks::scheduler(cfg.clone(), opts.clone()).await,
        other => {
            eprint!("{}", Cli::command().render_help());
            Err(format!("unknown task {other:?}").into())
        }
    }
}

/// Prints help quietly and turns every other clap failure into one error line.
fn handle_parse_error(err: clap::Error) -> Result<(), TaskError> {
    if matches!(
        err.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        err.print()?;
        return Ok(());
    }
    eprint!("{}", Cli::command().render_help());
    let text = err.to_string();
    let first = text.lines().next().unwrap_or("invalid arguments").trim();
    let message = first.strip_prefix("error: ").unwrap_or(first).to_string();
    Err(message.into())
}
