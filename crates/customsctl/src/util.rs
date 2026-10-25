//! Helpers shared by the commands: Go-compatible path handling, JSON that
//! looks the way `encoding/json` renders it, and ledger loading.

use std::path::Path;

use chrono::DateTime;
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

/// Renders the wall-clock time of a stored RFC3339 timestamp.
///
/// The offset in the stored text is authoritative. The Go gateway wrote local
/// time (`+08:00`) and its CLI prints records through
/// `time.Time.Format("15:04:05")`, which keeps whatever offset the record
/// carried. Normalising to UTC here would shift every record in a ledger written
/// by the Go build, so the offset is preserved.
pub fn clock_of(stamp: &str) -> String {
    match DateTime::parse_from_rfc3339(stamp) {
        Ok(instant) => instant.format("%H:%M:%S").to_string(),
        // Go leaves a zero time at 00:00:00 when the field is absent or
        // unparseable in a hand-edited record.
        Err(_) => "00:00:00".to_string(),
    }
}

/// Loads the ledger the configuration points at, keeping each record's original
/// text alongside its parsed form.
///
/// `audit tail` presents stored evidence, so it must not re-encode it: a record
/// written by the Go gateway carries a local UTC offset and the numbers its
/// encoder produced (`0.000009`, not `9e-6`), and both would change if the line
/// were parsed and serialised again.
pub fn load_ledger_entries(
    config_path: &str,
) -> Result<Vec<(Value, String)>, Box<dyn std::error::Error>> {
    if config_path.is_empty() {
        return Err("--config is required".into());
    }
    let cfg = config::load(Path::new(config_path))?;
    let ledger = resolve(&dir_of(config_path), &cfg.server.audit_path);
    let text =
        std::fs::read_to_string(&ledger).map_err(|err| format!("audit: open {ledger}: {err}"))?;
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_str(line).map_err(|err| format!("audit: parse record: {err}"))?;
        entries.push((value, line.to_string()));
    }
    Ok(entries)
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

/// Pretty-prints JSON text with the layout `json.MarshalIndent(v, "", "  ")`
/// produces, copying every token verbatim.
///
/// Re-serialising a parsed value would rewrite numbers (serde renders
/// `0.000009` as `9e-6`) and timestamps (parsed into UTC), which is exactly the
/// evidence `audit tail --json` exists to show unchanged.
pub fn pretty_preserving_tokens(raw: &str) -> String {
    let chars: Vec<char> = raw.trim().chars().collect();
    let mut out = String::with_capacity(raw.len() + raw.len() / 2);
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                let closing = if c == '{' { '}' } else { ']' };
                // An empty container stays on one line, like Go's encoder.
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                out.push(c);
                if j < chars.len() && chars[j] == closing {
                    out.push(closing);
                    i = j + 1;
                    continue;
                }
                depth += 1;
                out.push('\n');
                push_indent(&mut out, depth);
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                out.push('\n');
                push_indent(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(',');
                out.push('\n');
                push_indent(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            _ => out.push(c),
        }
        i += 1;
    }
    out
}

/// Writes two spaces per nesting level.
fn push_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn clock_keeps_the_stored_offset() {
        // The Go gateway writes local time; a record must display the hour it
        // was written at, not that instant in UTC.
        assert_eq!(clock_of("2026-10-25T20:46:45.789569071+08:00"), "20:46:45");
        assert_eq!(clock_of("2026-10-25T12:46:47.819585487Z"), "12:46:47");
        // A malformed or missing stamp renders like Go's zero time.
        assert_eq!(clock_of(""), "00:00:00");
        assert_eq!(clock_of("not a timestamp"), "00:00:00");
    }

    #[test]
    fn pretty_printing_preserves_tokens() {
        let raw = r#"{"seq":1,"time":"2026-10-25T20:46:45.789569071+08:00","cost":{"input_cost":0.000009},"empty":{},"list":[],"note":"a \"quoted\" {brace}"}"#;
        let pretty = pretty_preserving_tokens(raw);
        let expected = [
            "{",
            r#"  "seq": 1,"#,
            r#"  "time": "2026-10-25T20:46:45.789569071+08:00","#,
            r#"  "cost": {"#,
            r#"    "input_cost": 0.000009"#,
            "  },",
            r#"  "empty": {},"#,
            r#"  "list": [],"#,
            r#"  "note": "a \"quoted\" {brace}""#,
            "}",
        ]
        .join("\n");
        assert_eq!(pretty, expected);
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
