//! Flag parsing and the small pieces of startup behaviour that do not need a
//! running gateway: the summary line, the log level and the anchor interval.

use std::time::Duration;

use clap::Parser;
use customapi_core::json::parse_duration;

/// The daemon's command line.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "customsd",
    about = "Custom-API customs gateway",
    disable_version_flag = true,
    after_help = "The process is designed to run under an rc.d script with no privileges\nbeyond the ability to read its configuration and write its ledger."
)]
pub struct Cli {
    /// path to the configuration file (required)
    #[arg(long, short = 'c', value_name = "FILE")]
    pub config: Option<String>,
    /// optional KEY=VALUE file supplying secrets (upstream API keys, user tokens)
    #[arg(long = "env-file", value_name = "FILE")]
    pub env_file: Option<String>,
    /// override server.listen
    #[arg(long, value_name = "ADDR")]
    pub listen: Option<String>,
    /// override server.admin_listen
    #[arg(long = "admin-listen", value_name = "ADDR")]
    pub admin_listen: Option<String>,
    /// override server.log_level (debug|info|warn|error)
    #[arg(long = "log-level", value_name = "LEVEL")]
    pub log_level: Option<String>,
    /// print the build identity and exit
    #[arg(long)]
    pub version: bool,
    /// validate the configuration and exit
    #[arg(long)]
    pub check: bool,
    /// how often to pin the audit chain head to the anchor file (0 disables)
    #[arg(
        long = "anchor-interval",
        value_name = "DURATION",
        default_value = "5m",
        value_parser = parse_duration_arg
    )]
    pub anchor_interval: Duration,
}

/// Parses a Go duration literal such as `5m`, `300s` or `0`.
///
/// Go's `flag.Duration` accepts exactly these spellings, and the rc.d script may
/// pass either a unit-suffixed value or a bare `0` to disable anchoring.
fn parse_duration_arg(text: &str) -> Result<Duration, String> {
    parse_duration(text).ok_or_else(|| format!("time: invalid duration {text:?}"))
}

/// Renders the `--check` summary line.
pub fn check_summary(channels: usize, users: usize, prices: usize) -> String {
    format!("configuration OK: {channels} channel(s), {users} user(s), {prices} price row(s)")
}

/// Maps a configured level onto a log filter the way the Go `newLogger` does.
pub fn log_filter(level: &str) -> log::LevelFilter {
    match level.to_ascii_lowercase().as_str() {
        "debug" => log::LevelFilter::Debug,
        "warn" | "warning" => log::LevelFilter::Warn,
        "error" => log::LevelFilter::Error,
        _ => log::LevelFilter::Info,
    }
}

/// Installs the logger.
///
/// The level comes from the configuration, not from `RUST_LOG`: the gateway is
/// started by rc.d with a fixed environment, and a stray variable must not
/// change how verbose the service is.
pub fn init_logging(level: &str) {
    env_logger::Builder::new()
        .filter_level(log_filter(level))
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::normalize_argv;

    fn parse(args: &[&str]) -> Cli {
        let mut argv = vec!["customsd"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(normalize_argv(argv)).expect("parses")
    }

    #[test]
    fn long_and_short_forms_are_equivalent() {
        let long = parse(&["--config", "/etc/customs.json", "--env-file", "/etc/x.env"]);
        let short = parse(&["-c", "/etc/customs.json", "-env-file", "/etc/x.env"]);
        assert_eq!(long.config.as_deref(), Some("/etc/customs.json"));
        assert_eq!(short.config, long.config);
        assert_eq!(short.env_file, long.env_file);
    }

    #[test]
    fn anchor_interval_defaults_to_five_minutes_and_accepts_go_literals() {
        assert_eq!(
            parse(&["--config", "c.json"]).anchor_interval,
            Duration::from_secs(300)
        );
        assert_eq!(
            parse(&["--config", "c.json", "--anchor-interval", "90s"]).anchor_interval,
            Duration::from_secs(90)
        );
        assert_eq!(
            parse(&["--config", "c.json", "--anchor-interval=0"]).anchor_interval,
            Duration::ZERO
        );
        let err = Cli::try_parse_from(["customsd", "--anchor-interval", "soon"])
            .expect_err("rejects a non-duration");
        assert!(
            err.to_string().contains(r#"time: invalid duration "soon""#),
            "{err}"
        );
    }

    #[test]
    fn check_and_version_flags_are_flags_not_values() {
        let cli = parse(&["--config", "c.json", "--check", "--version"]);
        assert!(cli.check);
        assert!(cli.version);
    }

    #[test]
    fn the_summary_line_is_verbatim() {
        assert_eq!(
            check_summary(2, 3, 4),
            "configuration OK: 2 channel(s), 3 user(s), 4 price row(s)"
        );
    }

    #[test]
    fn log_levels_map_like_the_go_daemon() {
        assert_eq!(log_filter("debug"), log::LevelFilter::Debug);
        assert_eq!(log_filter("WARN"), log::LevelFilter::Warn);
        assert_eq!(log_filter("warning"), log::LevelFilter::Warn);
        assert_eq!(log_filter("error"), log::LevelFilter::Error);
        assert_eq!(log_filter("info"), log::LevelFilter::Info);
        assert_eq!(log_filter("verbose"), log::LevelFilter::Info);
    }
}
