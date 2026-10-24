//! Helpers shared by the commands: Go-compatible path handling, JSON that
//! looks the way `encoding/json` renders it, and ledger loading.

use std::path::Path;

use chrono::{DateTime, Utc};
use customapi_core::{audit, config};
use serde::Serialize;
use serde_json::Value;

/// `dirOf` from the Go CLI: everything before the last slash.
///
/// It is *not* `filepath.Dir`: the Go command has its own helper, and it is the
/// one whose output ends up in the printed paths. They agree for every path an
/// operator uses, and copying this one keeps the text identical.
pub fn dir_of(p: &str) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    match p.rfind('/') {
        None => ".".to_string(),
        Some(0) => "/".to_string(),
        Some(i) => p[..i].to_string(),
    }
}

/// `config.Resolve`: joins a relative path onto the configuration directory.
///
/// Go's `filepath.Join` cleans the result, so `(".", "var/a.jsonl")` becomes
/// `var/a.jsonl` rather than `./var/a.jsonl`. The printed ledger paths come
/// from here and are compared verbatim, so the cleaning matters.
pub fn resolve(base: &str, path: &str) -> String {
    if path.is_empty() || path.starts_with('/') {
        return path.to_string();
    }
    join_clean(base, path)
}

/// `filepath.Join(base, path)`, i.e. join then lexically clean.
pub fn join_clean(base: &str, path: &str) -> String {
    let joined = if base.is_empty() {
        path.to_string()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), path)
    };
    clean(&joined)
}

/// Lexical cleaning with the same rules as `path.Clean` for the shapes this
/// tool builds: collapse slashes, drop `.`, resolve `..` where possible.
fn clean(p: &str) -> String {
    let absolute = p.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in p.split('/') {
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

/// Loads the audit ledger the configuration points at.
///
/// Mirrors the Go `loadRecords`: the path is resolved against the configuration
/// file's directory, and a missing `--config` is an error with the same text.
pub fn load_records(config_path: &str) -> Result<Vec<audit::Record>, Box<dyn std::error::Error>> {
    if config_path.is_empty() {
        return Err("--config is required".into());
    }
    let cfg = config::load(Path::new(config_path))?;
    let ledger = resolve(&dir_of(config_path), &cfg.server.audit_path);
    Ok(audit::read_all(&ledger)?)
}

/// Renders an instant the way Go's `time.Time` JSON marshaller does
/// (`RFC3339Nano`): fractional seconds, trailing zeros trimmed, omitted when
/// zero.
///
/// chrono's serialiser pads the fraction to 3, 6 or 9 digits, so a record with
/// exactly `…:21.5` would come out as `…:21.500Z`. `audit tail --json` output is
/// diffed against the Go build, so the trimming is reproduced here.
pub fn rfc3339_nano(t: &DateTime<Utc>) -> String {
    let nanos = t.timestamp_subsec_nanos();
    let base = t.format("%Y-%m-%dT%H:%M:%S");
    if nanos == 0 {
        return format!("{base}Z");
    }
    let digits = format!("{nanos:09}");
    let trimmed = digits.trim_end_matches('0');
    format!("{base}.{trimmed}Z")
}

/// Converts every integral float in the value to an integer.
///
/// Go's `encoding/json` writes `float64(1)` as `1`, while serde writes `1.0`.
/// Files and console output are compared byte for byte against the Go build, and
/// the difference shows up immediately for scores, completion ratios and
/// integrity values, which are frequently exactly 0 or 1.
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

/// Serialises a value with Go's JSON conventions applied.
pub fn to_go_json<T: Serialize>(value: &T) -> Value {
    let mut value = serde_json::to_value(value).unwrap_or(Value::Null);
    go_floats(&mut value);
    value
}

/// Pretty-prints a value the way `json.MarshalIndent(v, "", "  ")` does.
pub fn go_json_pretty<T: Serialize>(value: &T) -> String {
    // `to_string_pretty` cannot fail for a value we just built.
    serde_json::to_string_pretty(&to_go_json(value)).unwrap_or_else(|_| "null".to_string())
}

/// Sorts every object's keys, as Go does when marshalling a `map[string]any`.
pub fn sort_keys(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = std::mem::take(map).into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (key, mut item) in entries {
                sort_keys(&mut item);
                map.insert(key, item);
            }
        }
        Value::Array(items) => {
            for item in items {
                sort_keys(item);
            }
        }
        _ => {}
    }
}

/// Renders a record for `audit tail --json`, with Go's timestamp text.
pub fn record_json(record: &audit::Record) -> Value {
    let mut value = to_go_json(record);
    if let Value::Object(map) = &mut value {
        // `time` and `end_time` are always present (Go's omitempty does not
        // apply to a struct), so both are rewritten rather than conditionally.
        for (key, instant) in [("time", &record.time), ("end_time", &record.end_time)] {
            if map.get(key).map(|v| v.is_string()).unwrap_or(false) {
                map.insert(key.to_string(), Value::String(rfc3339_nano(instant)));
            }
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn dir_of_matches_the_go_helper() {
        assert_eq!(dir_of(""), ".");
        assert_eq!(dir_of("customs.json"), ".");
        assert_eq!(dir_of("var/audit.jsonl"), "var");
        assert_eq!(dir_of("/tmp/x/audit.jsonl"), "/tmp/x");
        assert_eq!(dir_of("/audit.jsonl"), "/");
        assert_eq!(dir_of("a/"), "a");
    }

    #[test]
    fn resolve_cleans_like_filepath_join() {
        assert_eq!(resolve(".", "var/audit.jsonl"), "var/audit.jsonl");
        assert_eq!(
            resolve("/tmp/x", "var/audit.jsonl"),
            "/tmp/x/var/audit.jsonl"
        );
        assert_eq!(resolve("/tmp/x", "/abs/audit.jsonl"), "/abs/audit.jsonl");
        assert_eq!(resolve("/tmp/x", ""), "");
        assert_eq!(join_clean("/tmp/x/", "a/../b"), "/tmp/x/b");
    }

    #[test]
    fn timestamps_are_trimmed_like_rfc3339_nano() {
        let base = Utc.with_ymd_and_hms(2026, 3, 4, 13, 5, 6).unwrap();
        assert_eq!(rfc3339_nano(&base), "2026-03-04T13:05:06Z");
        let half = base + chrono::Duration::milliseconds(500);
        assert_eq!(rfc3339_nano(&half), "2026-03-04T13:05:06.5Z");
        let nanos = base + chrono::Duration::nanoseconds(635_246_221);
        assert_eq!(rfc3339_nano(&nanos), "2026-03-04T13:05:06.635246221Z");
        let micros = base + chrono::Duration::microseconds(123_456);
        assert_eq!(rfc3339_nano(&micros), "2026-03-04T13:05:06.123456Z");
    }

    #[test]
    fn integral_floats_render_as_integers() {
        let value = serde_json::json!({"a": 1.0, "b": 0.5, "c": [3.0, 2.25], "d": "x"});
        let mut value = value;
        go_floats(&mut value);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"{"a":1,"b":0.5,"c":[3,2.25],"d":"x"}"#
        );
    }

    #[test]
    fn keys_are_sorted_recursively() {
        let mut value = serde_json::json!({"b": {"z": 1, "a": 2}, "a": [{"y": 1, "x": 2}]});
        sort_keys(&mut value);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"{"a":[{"x":2,"y":1}],"b":{"a":2,"z":1}}"#
        );
    }
}
