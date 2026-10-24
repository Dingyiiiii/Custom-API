//! Command-line argument handling.
//!
//! Go's `flag` package accepts one or two dashes for every flag, so rc.d scripts
//! and cron entries pass `-config`. clap reserves a single dash for short
//! options, so [`normalize_argv`] rewrites `-known-long-option` into
//! `--known-long-option` before parsing, and leaves everything else alone.

use clap::Parser;

/// Long option names the Go daemon accepts.
///
/// Single-letter flags are deliberately absent: rewriting `-c` to `--c` would
/// break the short form.
pub const LONG_OPTIONS: &[&str] = &[
    "config",
    "env-file",
    "gateway",
    "token",
    "model",
    "channel",
    "out",
    "log-level",
    "prices-url",
    "group",
    "outcomes",
    "version",
];

/// Reports whether `name` (without dashes) is a known long option.
pub fn is_long_option(name: &str) -> bool {
    LONG_OPTIONS.contains(&name)
}

/// Rewrites `-flag` to `--flag` for known long options, leaving everything else
/// alone. `argv` includes the program name.
pub fn normalize_argv<I, S>(argv: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut out: Vec<String> = Vec::new();
    let mut terminated = false;
    for (i, raw) in argv.into_iter().enumerate() {
        let arg = raw.into();
        if i == 0 {
            out.push(arg);
            continue;
        }
        if terminated {
            out.push(arg);
            continue;
        }
        if arg == "--" {
            terminated = true;
            out.push(arg);
            continue;
        }
        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 1 {
            let body = &arg[1..];
            let name = body.split('=').next().unwrap_or(body);
            if is_long_option(name) {
                out.push(format!("-{arg}"));
                continue;
            }
        }
        out.push(arg);
    }
    out
}

/// The daemon's command line.
///
/// The task is a positional word, as in Go: `benchd --config c.json fingerprint`.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "benchd",
    about = "Custom-API intelligence daemon: prices, probes, benchmarks, scores",
    disable_version_flag = true
)]
pub struct Cli {
    /// gateway configuration file (required)
    #[arg(long, short = 'c', value_name = "FILE")]
    pub config: Option<String>,
    /// optional KEY=VALUE file supplying secrets
    #[arg(long = "env-file", value_name = "FILE")]
    pub env_file: Option<String>,
    /// gateway base URL to probe through
    #[arg(long, value_name = "URL", default_value = "http://127.0.0.1:8080")]
    pub gateway: String,
    /// gateway API key used for probes (defaults to the first probe user)
    #[arg(long, value_name = "KEY", default_value = "")]
    pub token: String,
    /// canonical model to probe/bench (default: all priced models)
    #[arg(long, value_name = "MODEL", default_value = "")]
    pub model: String,
    /// restrict to one channel
    #[arg(long, value_name = "NAME", default_value = "")]
    pub channel: String,
    /// output directory (defaults to the config directory)
    #[arg(long, value_name = "DIR", default_value = "")]
    pub out: String,
    /// debug|info|warn|error
    #[arg(long = "log-level", value_name = "LEVEL", default_value = "info")]
    pub log_level: String,
    /// new-api compatible /api/pricing URL to collect from
    #[arg(long = "prices-url", value_name = "URL", default_value = "")]
    pub prices_url: String,
    /// group name for multiplier pricing
    #[arg(long, value_name = "NAME", default_value = "default")]
    pub group: String,
    /// include per-case benchmark outcomes
    #[arg(long)]
    pub outcomes: bool,
    /// print build identity and exit
    #[arg(long)]
    pub version: bool,
    /// task to run: run, prices, fingerprint, bench or score (default run)
    #[arg(value_name = "TASK")]
    pub task: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(args: &[&str]) -> Vec<String> {
        let mut argv = vec!["benchd"];
        argv.extend_from_slice(args);
        normalize_argv(argv)
    }

    fn parse(args: &[&str]) -> Cli {
        let mut argv = vec!["benchd"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(normalize_argv(argv)).expect("parses")
    }

    #[test]
    fn known_long_options_gain_a_dash() {
        assert_eq!(
            norm(&["-config", "c.json", "-gateway", "http://x"]),
            vec!["benchd", "--config", "c.json", "--gateway", "http://x"]
        );
        assert_eq!(
            norm(&["-prices-url=http://x/api/pricing"]),
            vec!["benchd", "--prices-url=http://x/api/pricing"]
        );
    }

    #[test]
    fn everything_else_is_untouched() {
        assert_eq!(norm(&["-c", "x"]), vec!["benchd", "-c", "x"]);
        assert_eq!(norm(&["-1"]), vec!["benchd", "-1"]);
    }

    #[test]
    fn the_task_is_a_positional_word_after_the_flags() {
        let cli = parse(&["--config", "c.json", "--channel", "alpha", "fingerprint"]);
        assert_eq!(cli.task.as_deref(), Some("fingerprint"));
        assert_eq!(cli.channel, "alpha");
        let cli = parse(&[]);
        assert_eq!(cli.task, None);
        assert_eq!(cli.gateway, "http://127.0.0.1:8080");
        assert_eq!(cli.group, "default");
        assert_eq!(cli.log_level, "info");
    }

    #[test]
    fn overrides_and_flags_parse() {
        let cli = parse(&[
            "-config",
            "c.json",
            "-token",
            "probe-token",
            "-out",
            "/tmp/x",
            "-model",
            "mock-basic",
            "-outcomes",
            "-prices-url",
            "http://127.0.0.1:9/api/pricing",
            "-group",
            "vip",
            "-log-level",
            "debug",
            "bench",
        ]);
        assert_eq!(cli.config.as_deref(), Some("c.json"));
        assert_eq!(cli.token, "probe-token");
        assert_eq!(cli.out, "/tmp/x");
        assert_eq!(cli.model, "mock-basic");
        assert!(cli.outcomes);
        assert_eq!(cli.prices_url, "http://127.0.0.1:9/api/pricing");
        assert_eq!(cli.group, "vip");
        assert_eq!(cli.log_level, "debug");
        assert_eq!(cli.task.as_deref(), Some("bench"));
    }
}
