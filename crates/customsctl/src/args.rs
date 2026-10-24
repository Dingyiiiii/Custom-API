//! Command-line argument handling shared by the three binaries.
//!
//! Two things live here:
//!
//! - the *table of long option names* the Go implementation understands, and
//! - [`normalize_argv`], which lets an operator keep typing `-config` (Go's
//!   `flag` package accepts one or two dashes for every flag) while the Rust
//!   side uses clap, where a single dash always means a short option.
//!
//! The rewrite is deliberately conservative: it only ever adds one dash, and
//! only in front of a name that is a known long option. `-n`, `-1`, `-h` and
//! any unknown token are passed through untouched, so short options, negative
//! numbers and filenames starting with a dash behave as before.

/// Long option names used by the Go implementation's three commands.
///
/// A name only belongs here if the Go program accepts it as a long flag. Short
/// single-letter flags (`-n`) are deliberately absent: mapping them to `--n`
/// would break them.
pub const LONG_OPTIONS: &[&str] = &[
    // customsd
    "config",
    "env-file",
    "listen",
    "admin-listen",
    "log-level",
    "version",
    "check",
    "anchor-interval",
    // benchd
    "gateway",
    "token",
    "model",
    "channel",
    "out",
    "prices-url",
    "group",
    "outcomes",
    // customsctl: audit
    "ledger",
    "anchors",
    "actor",
    "verdict",
    "json",
    "yes",
    // customsctl: users
    "admin",
    "user",
    "quota",
    "period",
    // customsctl: prices
    "at",
    "input",
    "output",
    "scheme",
    "base",
    "model-ratio",
    "group-ratio",
    "completion-ratio",
    "input-per-m",
    "per-request",
    "typical-input",
    "typical-output",
    "usd-cny",
];

/// Reports whether `name` (without dashes) is a known long option.
pub fn is_long_option(name: &str) -> bool {
    LONG_OPTIONS.contains(&name)
}

/// Rewrites `-flag` to `--flag` for known long options, leaving everything else
/// alone.
///
/// `argv` includes the program name, exactly as `std::env::args` yields it.
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
            // Program name: never rewritten.
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
        // Only single-dash tokens are candidates; `--x` and `-x` stay as they
        // are unless the name is a known long option.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(args: &[&str]) -> Vec<String> {
        let mut argv = vec!["customsctl"];
        argv.extend_from_slice(args);
        normalize_argv(argv)
    }

    #[test]
    fn known_long_options_get_a_second_dash() {
        assert_eq!(
            norm(&["audit", "tail", "-config", "c.json", "-n", "3"]),
            vec![
                "customsctl",
                "audit",
                "tail",
                "--config",
                "c.json",
                "-n",
                "3"
            ]
        );
        assert_eq!(
            norm(&["-anchor-interval", "0"]),
            vec!["customsctl", "--anchor-interval", "0"]
        );
        assert_eq!(
            norm(&["-env-file=/tmp/x.env"]),
            vec!["customsctl", "--env-file=/tmp/x.env"]
        );
    }

    #[test]
    fn short_options_and_numbers_are_not_touched() {
        assert_eq!(norm(&["-n", "2"]), vec!["customsctl", "-n", "2"]);
        assert_eq!(norm(&["-1"]), vec!["customsctl", "-1"]);
        assert_eq!(
            norm(&["audit", "tail", "--input", "-1"]),
            vec!["customsctl", "audit", "tail", "--input", "-1"]
        );
        // `-c` is the short form of --config, not a long option name.
        assert_eq!(norm(&["-c", "c.json"]), vec!["customsctl", "-c", "c.json"]);
        // An unknown long-ish name is left alone so the parser reports it.
        assert_eq!(norm(&["-nope", "x"]), vec!["customsctl", "-nope", "x"]);
        assert_eq!(norm(&["-h"]), vec!["customsctl", "-h"]);
    }

    #[test]
    fn double_dash_and_terminator_are_respected() {
        assert_eq!(
            norm(&["--config", "c.json"]),
            vec!["customsctl", "--config", "c.json"]
        );
        // After "--" nothing is rewritten: the words are positional arguments.
        assert_eq!(
            norm(&["--", "-config"]),
            vec!["customsctl", "--", "-config"]
        );
    }

    #[test]
    fn program_name_is_never_rewritten() {
        assert_eq!(
            normalize_argv(vec!["-config"]),
            vec!["-config"],
            "argv[0] is the program path"
        );
    }

    #[test]
    fn every_long_option_is_recognised() {
        for name in LONG_OPTIONS {
            assert!(
                is_long_option(name),
                "{name} should be registered as a known long option"
            );
            assert_eq!(
                norm(&[&format!("-{name}")]),
                vec!["customsctl".to_string(), format!("--{name}")],
                "-{name} should gain a dash"
            );
        }
    }
}
