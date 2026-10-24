//! `customsctl config check`: validate a configuration file and summarise it.

use std::error::Error;
use std::path::Path;

use clap::{Args, Subcommand};
use customapi_core::config;

/// `customsctl config <command>`.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Validate a configuration file.
    Check(CheckArgs),
}

/// Flags accepted by `config check`.
#[derive(Args, Debug, Clone)]
pub struct CheckArgs {
    /// configuration file
    #[arg(long, short = 'c', value_name = "FILE")]
    pub config: Option<String>,
}

/// Runs the check and prints the summary.
pub fn check(args: &CheckArgs) -> Result<(), Box<dyn Error>> {
    let path = args
        .config
        .clone()
        .filter(|p| !p.is_empty())
        .ok_or("--config is required")?;
    let cfg = config::load(Path::new(&path))?;
    println!(
        "OK: {} channel(s), {} user(s), {} price row(s)",
        cfg.all_channels().len(),
        cfg.users.len(),
        cfg.prices.len()
    );
    println!(
        "    listen={} admin={} strategy={} fail_mode={}",
        cfg.server.listen, cfg.server.admin_listen, cfg.server.route_strategy, cfg.server.fail_mode
    );
    for ch in cfg.all_channels() {
        println!(
            "    channel {:<12} {:<28} protocol={:<9} trust={:<9} key={}",
            ch.name,
            ch.base_url,
            ch.protocol,
            ch.trust_level(),
            !ch.api_key.is_empty()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Captures what a command prints by rendering the same format strings.
    /// The process-level test in `tests/cli.rs` asserts the real stdout; this
    /// keeps the shape of the summary pinned without spawning a process.
    #[test]
    fn summary_lines_match_the_go_format() {
        let line = format!(
            "    channel {:<12} {:<28} protocol={:<9} trust={:<9} key={}",
            "alpha", "http://127.0.0.1:19001/v1", "openai", "trusted", true
        );
        assert_eq!(
            line,
            "    channel alpha        http://127.0.0.1:19001/v1    protocol=openai    trust=trusted   key=true"
        );
    }
}
