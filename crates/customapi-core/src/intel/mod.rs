//! Package intel is the customs gateway's L3 intelligence layer: it collects the
//! price, capability and model-identity evidence that the router uses to pick a
//! channel, and that the user-facing comparison page displays.
//!
//! The plan splits the work by cost and cadence, and the split is preserved here:
//!
//! | layer | evidence | cadence |
//! |---|---|---|
//! | L1 `fingerprint` | one cheap probe per channel | high frequency |
//! | L2 `bench` | a layered question suite | medium frequency |
//! | L3 `codebench` | execution-verified engineering tasks | low frequency |
//! | `price` | published price collection | high frequency |
//!
//! This module is a direct port of `internal/intel` in the Go implementation.
//! The Go package lives in a single file (`intel.go`) with section banners; the
//! file split below follows those banners one-for-one so that both versions can
//! be read side by side:
//!
//! | Go section | Rust file |
//! |---|---|
//! | Baseline store | `baseline` |
//! | Distribution comparison | `divergence` |
//! | Probes / Runner | `probe` |
//! | Capability scoring / Bench runner | `bench` |
//! | Price collection | `price` |
//! | Catalogue | `catalogue` |
//! | Grading | `grade` |
//! | Score assembly | `scores` |
//!
//! JSON field names, the `omitempty` behaviour, the map ordering (Go sorts map
//! keys when marshalling, so the maps here are `BTreeMap`s) and the error texts
//! are identical to the Go implementation: `baselines.json`, `catalogue.json`,
//! `benchmarks.json` and `fingerprints.json` are files an operator diffs and a
//! binary built from the other implementation has to read.

mod baseline;
mod bench;
mod catalogue;
mod divergence;
mod grade;
mod price;
mod probe;
mod scores;

pub use baseline::{
    load_baselines, Baseline, CompareResult, FingerprintBaselines, VERDICT_MATCH, VERDICT_MISMATCH,
    VERDICT_NO_BASELINE, VERDICT_SKIPPED,
};
pub use bench::{default_bench_suite, BenchCase, BenchOutcome, BenchReport, BenchRunner};
pub use catalogue::Catalogue;
pub use divergence::jensen_shannon;
pub use grade::grade_case;
pub use price::{collect_prices, PriceSource};
pub use probe::{default_probes, ChannelResult, Probe, ProbeResult, Runner};
pub use scores::assemble_scores;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Errors raised by the intelligence layer.
///
/// The message texts follow the Go implementation because operators grep for
/// them and because the Go tests compare phrasing. Go returns bare `os` / `json`
/// errors from some entry points; those are mirrored with the transparent
/// `Io` / `Json` variants, whose `Display` is the underlying error text.
#[derive(Debug, thiserror::Error)]
pub enum IntelError {
    /// Reading or writing an evidence file failed.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// JSON that Go's decoder reports verbatim (the catalogue loader adds no
    /// context, so neither does this).
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    /// A baseline file could not be parsed.
    #[error("intel: parse baselines {path}: {source}")]
    ParseBaselines {
        /// File that failed to parse.
        path: String,
        /// The decoder error.
        source: serde_json::Error,
    },
    /// The price source kind is not implemented.
    #[error("intel: unknown price source kind {0:?}")]
    UnknownPriceSourceKind(String),
    /// The pricing endpoint could not be reached.
    #[error("intel: fetch {url}: {source}")]
    Fetch {
        /// Endpoint that failed.
        url: String,
        /// The transport error.
        source: reqwest::Error,
    },
    /// The pricing endpoint answered with a non-200 status.
    #[error("intel: fetch {url}: HTTP {status}")]
    FetchStatus {
        /// Endpoint that failed.
        url: String,
        /// The status code it answered with.
        status: u16,
    },
    /// Reading the response body failed (Go returns the raw read error here, so
    /// this variant carries no context of its own).
    #[error("{0}")]
    Body(reqwest::Error),
    /// The pricing payload is not a price table.
    #[error("intel: parse {url}: {source}")]
    ParsePricing {
        /// Endpoint that returned the payload.
        url: String,
        /// The decoder error.
        source: serde_json::Error,
    },
}

/// Truncates a string to `n` bytes the way Go's `intel.truncate` does, with one
/// deliberate difference: the cut is pulled back to a character boundary so an
/// error message never contains a broken UTF-8 sequence. Go slices bytes and
/// relies on its JSON encoder to substitute invalid sequences later; the visible
/// text is identical for any ASCII payload and better for the rest.
fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// Serializes an `f64` the way Go's `encoding/json` does.
///
/// Go renders a float with `strconv.FormatFloat(v, 'f', -1, 64)` whenever
/// `1e-6 <= |v| < 1e21`, so a whole number is written as `0` or `12` and never
/// as `0.0`. Probe request bodies are audited by the gateway, so matching that
/// rendering keeps the two implementations' audit records textually identical.
/// (Outside the range Go switches to exponent notation with a two-digit
/// exponent, which serde writes more compactly; no value that reaches this
/// helper is anywhere near it.)
struct GoFloat(f64);

impl serde::Serialize for GoFloat {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = self.0;
        if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e21 {
            serializer.serialize_i64(value as i64)
        } else {
            serializer.serialize_f64(value)
        }
    }
}

/// One `messages` entry, with the key order Go's map marshalling produces.
fn chat_message(role: &str, content: &str) -> serde_json::Value {
    serde_json::json!({ "content": content, "role": role })
}

/// Builds the `/v1/chat/completions` request body shared by the L1 probe runner
/// and the L2 bench runner.
///
/// Go marshals a `map[string]any`, whose keys it sorts alphabetically. The
/// literal below is written in that same order (`serde_json` is built with
/// `preserve_order` here) so the bytes leaving the gateway match the Go
/// implementation's.
fn chat_request_body(
    model: &str,
    max_tokens: i32,
    temperature: f64,
    system: Option<&str>,
    prompt: &str,
) -> serde_json::Value {
    let mut messages = Vec::with_capacity(2);
    if let Some(system) = system {
        messages.push(chat_message("system", system));
    }
    messages.push(chat_message("user", prompt));
    serde_json::json!({
        "max_tokens": max_tokens,
        "messages": messages,
        "model": model,
        "temperature": GoFloat(temperature),
    })
}

/// Deserialises a map that may be absent or `null`.
///
/// Go decodes a JSON `null` into a map as a nil map, which the ported code
/// treats as "empty"; `serde_json` would reject it, so both spellings fold into
/// an empty `BTreeMap` here. A `BTreeMap` (rather than `HashMap`) is used
/// throughout because Go sorts map keys when marshalling and these files are
/// meant to stay byte-comparable.
fn null_map<'de, D, K, V>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: DeserializeOwned + Ord,
    V: DeserializeOwned,
{
    Ok(Option::<BTreeMap<K, V>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Writes `bytes` to `path` atomically: two-space indentation at the call site,
/// a `.tmp` sibling, 0640 on the file, 0750 on a directory created for it, then
/// a rename.
///
/// Both the baseline store and the catalogue use this, exactly as the two Go
/// `Save` methods do.
fn write_file_atomic(path: &str, bytes: &[u8]) -> Result<(), IntelError> {
    let target = Path::new(path);
    if let Some(dir) = target.parent() {
        if !dir.as_os_str().is_empty() && dir != Path::new(".") && !dir.exists() {
            fs::create_dir_all(dir)?;
            // MkdirAll(0o750): only a directory we created gets the mode; an
            // operator's existing directory keeps its own permissions.
            fs::set_permissions(dir, fs::Permissions::from_mode(0o750))?;
        }
    }
    let tmp = format!("{path}.tmp");
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o640)
            .open(&tmp)?;
        file.write_all(bytes)?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests;
