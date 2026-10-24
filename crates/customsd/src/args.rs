//! Command-line argument handling.
//!
//! Go's `flag` package accepts one or two dashes for every flag, so operators
//! type `-config` in rc.d scripts and cron entries. clap reserves a single dash
//! for short options, so [`normalize_argv`] rewrites `-known-long-option` into
//! `--known-long-option` before parsing. The rewrite is deliberately narrow: it
//! never touches `-c`, `-1`, `-h` or anything that is not a known long option,
//! so short options, negative numbers and dash-prefixed filenames keep working.

/// Long option names the Go daemon accepts.
///
/// Single-letter flags are deliberately absent: rewriting `-c` to `--c` would
/// break the short form.
pub const LONG_OPTIONS: &[&str] = &[
    "config",
    "env-file",
    "listen",
    "admin-listen",
    "log-level",
    "version",
    "check",
    "anchor-interval",
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

/// `filepath.Dir`: the directory of the configuration file, so that relative
/// paths inside it resolve the way the Go daemon resolves them.
pub fn dir_of(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        None => ".".to_string(),
        Some(0) => "/".to_string(),
        Some(i) => clean(&trimmed[..i]),
    }
}

/// Lexical cleaning with `path.Clean`'s rules.
fn clean(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => match parts.last() {
                Some(last) if *last != ".." => {
                    parts.pop();
                }
                _ => {
                    if !absolute {
                        parts.push("..");
                    }
                }
            },
            other => parts.push(other),
        }
    }
    let body = parts.join("/");
    if absolute {
        format!("/{body}")
    } else if body.is_empty() {
        ".".to_string()
    } else {
        body
    }
}

/// Builds the directory a relative configuration path lives in.
pub fn config_dir(config_path: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(dir_of(config_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(args: &[&str]) -> Vec<String> {
        let mut argv = vec!["customsd"];
        argv.extend_from_slice(args);
        normalize_argv(argv)
    }

    #[test]
    fn known_long_options_gain_a_dash() {
        assert_eq!(
            norm(&["-config", "/etc/customs.json", "-check"]),
            vec!["customsd", "--config", "/etc/customs.json", "--check"]
        );
        assert_eq!(
            norm(&["-env-file=/etc/customs.env"]),
            vec!["customsd", "--env-file=/etc/customs.env"]
        );
        assert_eq!(
            norm(&["-anchor-interval", "30s"]),
            vec!["customsd", "--anchor-interval", "30s"]
        );
    }

    #[test]
    fn everything_else_is_untouched() {
        assert_eq!(norm(&["-c", "x.json"]), vec!["customsd", "-c", "x.json"]);
        assert_eq!(norm(&["-1"]), vec!["customsd", "-1"]);
        assert_eq!(norm(&["-h"]), vec!["customsd", "-h"]);
        assert_eq!(norm(&["-unknown"]), vec!["customsd", "-unknown"]);
        assert_eq!(norm(&["--", "-config"]), vec!["customsd", "--", "-config"]);
    }

    #[test]
    fn go_directory_helpers_match_filepath() {
        assert_eq!(dir_of("customs.json"), ".");
        assert_eq!(dir_of("/etc/customapi/customs.json"), "/etc/customapi");
        assert_eq!(dir_of("/customs.json"), "/");
        assert_eq!(dir_of(""), ".");
    }

    #[test]
    fn program_name_is_never_rewritten() {
        assert_eq!(normalize_argv(vec!["-config"]), vec!["-config"]);
    }
}
