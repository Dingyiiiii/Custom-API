//! Reading and writing the evidence files.
//!
//! The files are shared with the Go implementation: an operator diffs them and a
//! binary built from the other implementation has to read them, so writes are
//! atomic (`write`, then `rename`) and rendered with Go's JSON conventions.

use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

/// Writes a JSON file the way the Go helpers do.
///
/// The parent directory is created with mode 0750, the file is written with mode
/// 0640 to a temporary name and renamed into place, so a reader never observes a
/// half-written evidence file.
pub fn write_json<T: Serialize>(path: &str, value: &T) -> Result<(), Box<dyn Error + Send + Sync>> {
    let parent = Path::new(path).parent();
    if let Some(dir) = parent {
        if !dir.as_os_str().is_empty() && dir != Path::new(".") {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o750)
                .create(dir)?;
        }
    }
    let mut json = serde_json::to_value(value)?;
    go_floats(&mut json);
    let bytes = serde_json::to_vec_pretty(&json)?;
    let tmp = format!("{path}.tmp");
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o640)
            .open(&tmp)?;
        file.write_all(&bytes)?;
        file.flush()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Reads a JSON file, treating a missing file as "nothing collected yet".
///
/// A literal `null` is treated the same way: Go's decoder leaves a nil slice or
/// map in place when the file says `null`, which is what the Go daemon writes for
/// a `bench` run that had nothing to probe.
pub fn read_json<T: DeserializeOwned + Default>(
    path: &str,
) -> Result<T, Box<dyn Error + Send + Sync>> {
    match fs::read(path) {
        Ok(bytes) => {
            let parsed: Option<T> = serde_json::from_slice(&bytes)?;
            Ok(parsed.unwrap_or_default())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(err) => Err(err.into()),
    }
}

/// `filepath.Join(parts...)`: joins and cleans.
///
/// Go cleans, so `(".", "var", "scores.json")` becomes `var/scores.json` rather
/// than `./var/scores.json`. The printed file paths are compared verbatim with
/// the Go daemon's output, so the cleaning matters.
pub fn join_path(parts: &[&str]) -> String {
    let non_empty: Vec<&str> = parts
        .iter()
        .copied()
        .filter(|part| !part.is_empty())
        .collect();
    if non_empty.is_empty() {
        return String::new();
    }
    clean(&non_empty.join("/"))
}

/// `filepath.Dir`: the directory part of a path, cleaned.
pub fn go_dir(path: &str) -> String {
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

/// Lexical cleaning with `path.Clean`'s rules for the shapes this daemon builds.
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

/// Converts every integral float in the value to an integer.
///
/// Go's `encoding/json` writes `float64(1)` as `1`; serde writes `1.0`. Scores,
/// integrity values and capability ratios are frequently exactly 0 or 1, so the
/// difference would be visible in every scorecard. The same helper exists in
/// `customsctl`; a shared home in `customapi-core` would be cleaner and is
/// flagged in the porting report.
pub fn go_floats(value: &mut Value) {
    match value {
        Value::Number(number) => {
            let Some(f) = number.as_f64() else {
                return;
            };
            if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
                *number = serde_json::Number::from(f as i64);
            }
        }
        Value::Array(items) => {
            for item in items {
                go_floats(item);
            }
        }
        Value::Object(map) => {
            for (_, item) in map.iter_mut() {
                go_floats(item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A throw-away directory that removes itself.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "benchd-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }

        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn scores_are_written_with_go_float_formatting() {
        let dir = TempDir::new("io-floats");
        let path = format!("{}/var/scores.json", dir.path());
        let value = json!({"alpha": {"mock-basic": {"capability": 1.0, "reliability": 0.5}}});
        write_json(&path, &value).expect("writes");
        let text = fs::read_to_string(&path).expect("reads");
        assert!(text.contains(r#""capability": 1"#), "{text}");
        assert!(text.contains(r#""reliability": 0.5"#), "{text}");
        // The temporary file must not be left behind.
        assert!(!Path::new(&format!("{path}.tmp")).exists());
    }

    #[test]
    fn a_null_file_reads_as_the_default() {
        // Go writes `null` for an empty slice, so a file from the Go daemon must
        // not be a parse error here.
        let dir = TempDir::new("io-null");
        let path = format!("{}/var/benchmarks.json", dir.path());
        fs::create_dir_all(format!("{}/var", dir.path())).expect("mkdir");
        fs::write(&path, "null").expect("writes null");
        let reports: Vec<serde_json::Value> = read_json(&path).expect("null is not an error");
        assert!(reports.is_empty());
    }

    #[test]
    fn a_missing_file_reads_as_the_default() {
        let dir = TempDir::new("io-missing");
        let path = format!("{}/var/benchmarks.json", dir.path());
        let reports: Vec<serde_json::Value> = read_json(&path).expect("missing is not an error");
        assert!(reports.is_empty());
    }

    #[test]
    fn paths_join_and_clean_like_filepath() {
        assert_eq!(join_path(&[".", "var", "scores.json"]), "var/scores.json");
        assert_eq!(
            join_path(&["/tmp/parity", "var", "scores.json"]),
            "/tmp/parity/var/scores.json"
        );
        assert_eq!(
            join_path(&["/tmp/parity/", "var", "scores.json"]),
            "/tmp/parity/var/scores.json"
        );
        assert_eq!(join_path(&[""]), "");
        assert_eq!(go_dir("c.json"), ".");
        assert_eq!(go_dir("etc/c.json"), "etc");
        assert_eq!(go_dir("/etc/customapi/c.json"), "/etc/customapi");
    }

    #[test]
    fn nested_directories_are_created() {
        let dir = TempDir::new("io-dirs");
        let path = format!("{}/var/deep/nested/x.json", dir.path());
        write_json(&path, &json!({"ok": true})).expect("writes");
        assert!(Path::new(&path).exists());
    }
}
