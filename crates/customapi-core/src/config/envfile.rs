//! KEY=VALUE secrets file loading.
//!
//! This exists so that a FreeBSD rc.d script can keep upstream credentials out
//! of the world-readable configuration file without relying on shell sourcing,
//! which silently fails across the daemon(8) boundary and cannot express
//! variable names that are not valid shell identifiers.
//!
//! The accepted syntax is deliberately small and strict:
//!
//! ```text
//! # comment
//! CUSTOMS_CHANNEL_UPSTREAM_A_APIKEY=sk-...
//! QUOTED="value with spaces"
//! EMPTY=
//! ```
//!
//! A line that is not a comment, blank, or a KEY=VALUE assignment is an error
//! rather than being ignored, so a malformed secrets file cannot silently leave
//! a channel without credentials.

use super::ConfigError;
use std::path::Path;

/// Reads a KEY=VALUE file and returns the pairs in file order.
pub fn load_env_file(path: &Path) -> Result<Vec<(String, String)>, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|err| ConfigError::EnvFileOpen {
        path: path.display().to_string(),
        source: err,
    })?;
    let mut out: Vec<(String, String)> = Vec::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(ConfigError::EnvFile(format!(
                "{}:{}: expected KEY=VALUE, got {:?}",
                path.display(),
                line_no,
                line
            )));
        };
        let key = key.trim();
        // Some secrets contain an '=' themselves, so only the first one
        // separates key from value.
        let mut value = value.trim();
        if key.is_empty() {
            return Err(ConfigError::EnvFile(format!(
                "{}:{}: empty key",
                path.display(),
                line_no
            )));
        }
        for (i, ch) in key.char_indices() {
            let valid = ch == '_'
                || ch.is_ascii_uppercase()
                || ch.is_ascii_lowercase()
                || (i > 0 && ch.is_ascii_digit());
            if !valid {
                return Err(ConfigError::EnvFile(format!(
                    "{}:{}: invalid key {:?}",
                    path.display(),
                    line_no,
                    key
                )));
            }
        }
        let bytes = value.as_bytes();
        if bytes.len() >= 2 {
            let first = bytes[0];
            let last = bytes[bytes.len() - 1];
            if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
                value = &value[1..value.len() - 1];
            }
        }
        out.push((key.to_string(), value.to_string()));
    }
    Ok(out)
}

/// Overlays KEY=VALUE pairs onto the configuration, using the same naming
/// convention as [`super::Config::apply_env`].
pub fn apply_env_pairs(config: &mut super::Config, pairs: &[(String, String)]) {
    config.apply_env(pairs);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "customs-envfile-{}-{}",
            name,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secrets.env");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn parses_comments_quotes_and_empty_values() {
        let path = write_temp(
            "basic",
            "# comment\nCUSTOMS_CHANNEL_A_APIKEY=sk-1\nQUOTED=\"with spaces\"\nEMPTY=\n",
        );
        let pairs = load_env_file(&path).unwrap();
        assert_eq!(pairs[0], ("CUSTOMS_CHANNEL_A_APIKEY".into(), "sk-1".into()));
        assert_eq!(pairs[1], ("QUOTED".into(), "with spaces".into()));
        assert_eq!(pairs[2], ("EMPTY".into(), "".into()));
    }

    #[test]
    fn rejects_malformed_lines() {
        let path = write_temp("bad", "NOT_AN_ASSIGNMENT\n");
        let err = load_env_file(&path).unwrap_err().to_string();
        assert!(err.contains("expected KEY=VALUE"), "{err}");
    }

    #[test]
    fn rejects_invalid_keys() {
        let path = write_temp("badkey", "1BAD=x\n");
        let err = load_env_file(&path).unwrap_err().to_string();
        assert!(err.contains("invalid key"), "{err}");
    }
}
