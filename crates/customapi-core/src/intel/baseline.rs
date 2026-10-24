//! Baseline store: the behavioural reference for each canonical model, plus the
//! comparison that turns a fresh observation into a verdict.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use super::divergence::jensen_shannon;
use super::{null_map, write_file_atomic, IntelError};
use crate::config::zero_time;

// ---------------------------------------------------------------------------
// Baseline store
// ---------------------------------------------------------------------------

/// The behavioural reference for one canonical model.
///
/// The reference is the assistant-reply *length* distribution of a
/// known-genuine endpoint under a fixed probe. Length is a coarse feature, but
/// it is available from every upstream without special entitlements (unlike
/// logprobs), it is deterministic enough to compare, and a substituted model
/// almost always shows a materially different length profile - the substitute
/// authors optimise for cost, not for matching another model's verbosity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    /// The canonical model name.
    pub model: String,
    /// Names the trusted endpoint the baseline came from.
    pub source: String,
    /// The observed reply lengths.
    ///
    /// Go declares `[]int` with no `omitempty`, so a nil slice marshals as
    /// `null`; here an empty vector marshals as `[]`. Both decode to "no
    /// samples", and a baseline that reached a file always has samples.
    #[serde(default)]
    pub samples: Vec<i32>,
    /// Counts unique replies, which detects a cached or canned upstream (a real
    /// model does not answer identically every time).
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i32")]
    pub distinct: i32,
    /// When the baseline was collected.
    #[serde(default = "zero_time", serialize_with = "crate::json::go_time")]
    pub created_at: DateTime<Utc>,
    /// A digest over the collected replies, so an operator can tell whether a
    /// baseline was regenerated.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fingerprint: String,
}

impl Default for Baseline {
    fn default() -> Self {
        Baseline {
            model: String::new(),
            source: String::new(),
            samples: Vec::new(),
            distinct: 0,
            created_at: zero_time(),
            fingerprint: String::new(),
        }
    }
}

/// A collection of baselines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FingerprintBaselines {
    /// Version of the baseline file format.
    #[serde(default)]
    pub version: i32,
    /// When the file was written.
    #[serde(default = "zero_time", serialize_with = "crate::json::go_time")]
    pub updated_at: DateTime<Utc>,
    /// Maps a canonical model name to its baseline.
    #[serde(default, deserialize_with = "null_map")]
    pub models: BTreeMap<String, Baseline>,
}

impl Default for FingerprintBaselines {
    fn default() -> Self {
        // Go's zero value: version 0, zero timestamp, empty map.
        FingerprintBaselines {
            version: 0,
            updated_at: zero_time(),
            models: BTreeMap::new(),
        }
    }
}

impl FingerprintBaselines {
    /// Creates an empty collection.
    pub fn new() -> FingerprintBaselines {
        FingerprintBaselines {
            version: 1,
            ..FingerprintBaselines::default()
        }
    }

    /// Writes the baseline file atomically.
    ///
    /// Go's method has a pointer receiver because it stamps `updated_at`; the
    /// same mutation needs `&mut self` here.
    pub fn save(&mut self, path: &str) -> Result<(), IntelError> {
        self.updated_at = Utc::now();
        let bytes = serde_json::to_vec_pretty(self)?;
        write_file_atomic(path, &bytes)
    }

    /// Scores observed replies against a baseline.
    pub fn compare(&self, model: &str, observed: &[i32], threshold: f64) -> CompareResult {
        let threshold = if threshold <= 0.0 { 0.35 } else { threshold };
        let baseline = match self.models.get(model) {
            Some(bl) if !bl.samples.is_empty() => bl,
            _ => {
                return CompareResult {
                    model: model.to_string(),
                    threshold,
                    verdict: VERDICT_NO_BASELINE.to_string(),
                    observed_samples: observed.len() as i32,
                    ..CompareResult::default()
                }
            }
        };
        let score = jensen_shannon(&baseline.samples, observed);
        let verdict = if score > threshold {
            VERDICT_MISMATCH
        } else {
            VERDICT_MATCH
        };
        CompareResult {
            model: model.to_string(),
            score,
            threshold,
            verdict: verdict.to_string(),
            baseline_source: baseline.source.clone(),
            baseline_samples: baseline.samples.len() as i32,
            observed_samples: observed.len() as i32,
            sample_hash: digest_ints(observed),
        }
    }
}

/// Reads a baseline file.
///
/// A missing file is not an error: it means "no reference collected yet", which
/// the intelligence layer represents as an empty collection so that the first
/// run can create one.
pub fn load_baselines(path: &str) -> Result<FingerprintBaselines, IntelError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FingerprintBaselines::new())
        }
        Err(err) => return Err(IntelError::Io(err)),
    };
    // Go decodes into a value that already exists, so a file holding `null`
    // leaves it at the zero value and reports no error. Decoding into an
    // `Option` reproduces that: `null` becomes `None`, which is exactly the Go
    // zero `FingerprintBaselines`.
    let parsed: Option<FingerprintBaselines> =
        serde_json::from_slice(&bytes).map_err(|source| IntelError::ParseBaselines {
            path: path.to_string(),
            source,
        })?;
    let parsed = parsed.unwrap_or_default();
    // Go re-creates a nil map after decoding; `null_map` already guarantees an
    // initialized map here.
    Ok(parsed)
}

// ---------------------------------------------------------------------------
// Comparison result
// ---------------------------------------------------------------------------

/// Verdict strings, matching the Go `FingerprintVerdict` constants.
///
/// The Go type is `type FingerprintVerdict string` and the value travels through
/// JSON files, so this stays a plain string: a closed enum would refuse to read
/// a verdict written by a newer version.
pub const VERDICT_MATCH: &str = "match";
/// The channel deviates beyond the threshold.
pub const VERDICT_MISMATCH: &str = "mismatch";
/// No reference existed for the model.
pub const VERDICT_NO_BASELINE: &str = "no_baseline";
/// The check did not run.
pub const VERDICT_SKIPPED: &str = "skipped";

/// The outcome of a fingerprint comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompareResult {
    /// The canonical model the comparison applies to.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Measured divergence, in bits.
    #[serde(default, serialize_with = "crate::json::go_f64")]
    pub score: f64,
    /// Threshold the score was compared against.
    #[serde(default, serialize_with = "crate::json::go_f64")]
    pub threshold: f64,
    /// One of the [`VERDICT_MATCH`] family.
    #[serde(default)]
    pub verdict: String,
    /// The trusted endpoint the baseline came from.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub baseline_source: String,
    /// How many reference samples the baseline held.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i32")]
    pub baseline_samples: i32,
    /// How many samples this run collected.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i32")]
    pub observed_samples: i32,
    /// Digest of the observed samples, so two runs can be compared without
    /// storing the samples twice.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sample_hash: String,
}

impl Default for CompareResult {
    fn default() -> Self {
        CompareResult {
            model: String::new(),
            score: 0.0,
            threshold: 0.0,
            verdict: String::new(),
            baseline_source: String::new(),
            baseline_samples: 0,
            observed_samples: 0,
            sample_hash: String::new(),
        }
    }
}

/// Digests the observed lengths: `sha256("1,2,3,")` truncated to 16 hex chars.
///
/// Go's version exists so that a comparison can be re-checked against the exact
/// sample set that produced it; the truncation is display convenience, not
/// security.
fn digest_ints(values: &[i32]) -> String {
    let mut hasher = Sha256::new();
    for n in values {
        hasher.update(format!("{n},").as_bytes());
    }
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    hex[..16].to_string()
}
