//! Append-only, tamper-evident audit ledger.
//!
//! This is the Rust counterpart of the Go file `internal/audit/ledger.go`.
//!
//! The project plan requires that accounting, quota deduction and audit trails
//! are never skipped, by any subject, under any trust level, and that audit
//! evidence lives in a store separate from the business database. This module
//! provides that store.
//!
//! # Tamper evidence
//!
//! Every record is written as one JSON object per line with three chain fields:
//!
//! ```text
//! seq         monotonic sequence number starting at 1
//! prev_hash   entry_hash of the previous record ("genesis" for seq 1)
//! entry_hash  SHA-256 over the canonical JSON of the record with entry_hash
//!             removed
//! ```
//!
//! Because `entry_hash` covers `prev_hash`, altering, reordering, inserting or
//! deleting any record breaks verification from that point onward. An attacker
//! who rewrites the whole chain can still be caught by the periodic anchors
//! written to a separate file (and, in production, shipped off-host).
//!
//! # Concurrency
//!
//! Go serialises writes with a mutex held by `*Writer`. Rust cannot hand out
//! `&mut self` to an axum handler, so the mutable state lives behind an internal
//! `Mutex` and every method takes `&self`: `Arc<Writer>` is `Send + Sync` and
//! can be shared freely between request handlers.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::zero_time;

/// `GenesisHash` is the `prev_hash` of the first record.
pub const GENESIS_HASH: &str = "genesis";

// ---------------------------------------------------------------------------
// Record
// ---------------------------------------------------------------------------

/// Summarises what the gateway did with an exchange.
///
/// Go models this as `type Decision string` with constants and accepts any
/// string when reading a ledger, so it stays a `String` here: an old record must
/// remain parseable by a newer binary.
pub type Decision = String;

/// The exchange was forwarded unchanged.
pub const DECISION_ALLOW: &str = "allow";
/// It was forwarded but findings were recorded.
pub const DECISION_REPORT: &str = "report";
/// The request was rejected before reaching the upstream.
pub const DECISION_BLOCK_USER: &str = "block_user";
/// The upstream response was withheld from the user.
pub const DECISION_BLOCK_UPSTREAM: &str = "block_upstream";
/// The upstream failed.
pub const DECISION_UPSTREAM_ERROR: &str = "upstream_error";

/// The monetary accounting for one exchange.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cost {
    /// Currency of the amounts below.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub currency: String,
    /// The priced components.
    #[serde(skip_serializing_if = "crate::json::is_zero_f64")]
    pub input_cost: f64,
    #[serde(skip_serializing_if = "crate::json::is_zero_f64")]
    pub output_cost: f64,
    /// `input_cost + output_cost` (already discounted).
    #[serde(skip_serializing_if = "crate::json::is_zero_f64")]
    pub total_cost: f64,
    /// The undiscounted total, kept so the discount is auditable.
    #[serde(skip_serializing_if = "crate::json::is_zero_f64")]
    pub list_cost: f64,
    /// The effective discount percentage.
    #[serde(skip_serializing_if = "crate::json::is_zero_f64")]
    pub discount_pct: f64,
    /// How the price was obtained: "catalogue", "unknown".
    #[serde(skip_serializing_if = "String::is_empty")]
    pub basis: String,
    /// Identifies the price row used.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub price_model: String,
}

/// The metering verdict for one exchange.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageSummary {
    /// The gateway's own recount.
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    /// The upstream's claims.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub upstream_prompt_tokens: i64,
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub upstream_completion_tokens: i64,
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub upstream_total_tokens: i64,
    /// False when the upstream omitted usage.
    pub upstream_reported: bool,
    /// The estimator used for the recount.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tokenizer_family: String,
    /// "upstream", "stream_chunks" or "local_only".
    #[serde(skip_serializing_if = "String::is_empty")]
    pub method: String,
    /// Relative deviations.
    #[serde(skip_serializing_if = "crate::json::is_zero_f64")]
    pub prompt_delta: f64,
    #[serde(skip_serializing_if = "crate::json::is_zero_f64")]
    pub completion_delta: f64,
    /// "consistent", "over_billed", "under_billed", "not_reported" or
    /// "not_verifiable".
    pub verdict: String,
    /// Tolerance used for the verdict.
    #[serde(skip_serializing_if = "crate::json::is_zero_f64")]
    pub tolerance: f64,
    /// The absolute floor applied with the tolerance.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub min_absolute_delta: i64,
    /// The deviation in tokens, which is what the floor compares.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub worst_abs: i64,
}

/// The result of a behavioural model-identity check.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fingerprint {
    /// False when the request was not sampled for a probe.
    pub checked: bool,
    /// The divergence from the channel's baseline (JS divergence, 0 ==
    /// identical distributions).
    #[serde(skip_serializing_if = "crate::json::is_zero_f64")]
    pub score: f64,
    /// Threshold applied when deciding `verdict`.
    #[serde(skip_serializing_if = "crate::json::is_zero_f64")]
    pub threshold: f64,
    /// "match", "mismatch", "no_baseline" or "skipped".
    #[serde(skip_serializing_if = "String::is_empty")]
    pub verdict: String,
    /// Where the reference distribution came from.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub baseline_source: String,
    /// A digest of the probe sample, for reproducibility.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sample_hash: String,
}

/// Breaks down where the time went.
///
/// Every field is milliseconds. An earlier revision stored the two audit stages
/// in nanoseconds while the others were milliseconds, which silently produced
/// values a thousand times too large for any consumer that treated the object as
/// one unit; the names now carry the unit so the mistake cannot be repeated.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Latency {
    /// The whole request, from authentication to the last byte.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub total_ms: i64,
    /// The request-side policy evaluation.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub outbound_audit_ms: i64,
    /// The time spent waiting on the provider.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub upstream_ms: i64,
    /// The response-side policy evaluation.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub inbound_audit_ms: i64,
    /// The time to the first byte of the reply.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub ttfb_ms: i64,
}

/// A compact, non-sensitive reference to a policy finding.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FindingRef {
    pub rule: String,
    pub severity: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub field: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub excerpt_hash: String,
    #[serde(skip_serializing_if = "crate::json::is_false")]
    pub trust_exempt: bool,
}

/// Redacted excerpts for operator inspection.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sample {
    /// The redacted request payload excerpt.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub request_text: String,
    /// The redacted response payload excerpt.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub response_text: String,
    /// Explains why this exchange was sampled.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// One audited exchange.
///
/// Chain fields (`seq`, `prev_hash`, `entry_hash`) are managed by the [`Writer`]
/// and must not be set by callers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    pub seq: i64,
    pub prev_hash: String,
    /// The digest of this record.
    ///
    /// It is omitted while empty so that the bytes hashed at write time are
    /// exactly the bytes a verifier recovers by stripping the field: the writer
    /// hashes the record before the hash exists, and the verifier hashes the
    /// stored line with the hash removed. Emitting the field while empty made
    /// those two disagree, so every record failed its own check.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub entry_hash: String,

    /// Identifies the request end to end.
    pub id: String,
    /// The request start timestamp (RFC3339, UTC).
    pub time: DateTime<Utc>,
    /// When the response finished.
    ///
    /// Like Go, this field is always emitted: `omitempty` has no effect on a
    /// struct, so an unset end time appears as Go's zero time.
    pub end_time: DateTime<Utc>,

    /// The user name, and the upstream access point.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub actor: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub channel: String,
    /// Identify the credential that served the request. A provider with several
    /// keys expands to several channels, so without these an operator cannot
    /// tell which key is being rate-limited or is costing more.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// The canonical model name, and what was sent upstream.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub upstream_model: String,
    /// The protocol the client spoke: "openai", "responses" or "anthropic".
    #[serde(skip_serializing_if = "String::is_empty")]
    pub protocol: String,
    /// The protocol the channel actually speaks. It differs from `protocol`
    /// when the exchange was translated.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub upstream_protocol: String,
    /// Reports that the request and response were converted between protocols.
    /// A converted exchange has different fidelity from a forwarded one, so an
    /// operator debugging odd output needs to know which it was.
    #[serde(skip_serializing_if = "crate::json::is_false")]
    pub translated: bool,
    /// Reports that the request relied on provider-side conversation state, so
    /// the gateway could neither audit the whole prompt nor confirm the full
    /// token count.
    #[serde(skip_serializing_if = "crate::json::is_false")]
    pub server_state: bool,
    /// Whether the response was streamed.
    pub stream: bool,
    /// The requested path.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub endpoint: String,
    /// The remote address.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub client_ip: String,

    /// The overall outcome.
    pub decision: Decision,
    /// The HTTP status returned to the client.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub status_code: i64,
    /// A short, non-sensitive failure description.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,

    /// Summarise policy hits.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outbound_findings: Vec<FindingRef>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inbound_findings: Vec<FindingRef>,

    /// The metering verdict.
    pub usage: UsageSummary,
    /// The monetary accounting.
    pub cost: Cost,
    /// The identity check result.
    pub fingerprint: Fingerprint,

    /// Breaks down where the time went.
    pub latency_ms: Latency,

    /// A digest of the (redacted) request body.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub request_hash: String,
    /// A digest of the (redacted) response body.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub response_hash: String,
    /// Explains why this channel was chosen.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub route_reason: String,
    /// Counts upstream calls made, including failed failovers.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub attempts: i64,
    /// Records a channel switch caused by an upstream failure.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub failover_note: String,
    /// The actor's remaining allowance after this request.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub quota_after: i64,
    /// An optional redacted request/response excerpt, present only when sampling
    /// is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample: Option<Sample>,
}

impl Default for Record {
    /// The Go zero value, including Go's zero *time* (year 1) rather than the
    /// Unix epoch that `chrono` would pick.
    fn default() -> Self {
        Record {
            seq: 0,
            prev_hash: String::new(),
            entry_hash: String::new(),
            id: String::new(),
            time: zero_time(),
            end_time: zero_time(),
            actor: String::new(),
            channel: String::new(),
            provider: String::new(),
            key: String::new(),
            model: String::new(),
            upstream_model: String::new(),
            protocol: String::new(),
            upstream_protocol: String::new(),
            translated: false,
            server_state: false,
            stream: false,
            endpoint: String::new(),
            client_ip: String::new(),
            decision: String::new(),
            status_code: 0,
            error: String::new(),
            outbound_findings: Vec::new(),
            inbound_findings: Vec::new(),
            usage: UsageSummary::default(),
            cost: Cost::default(),
            fingerprint: Fingerprint::default(),
            latency_ms: Latency::default(),
            request_hash: String::new(),
            response_hash: String::new(),
            route_reason: String::new(),
            attempts: 0,
            failover_note: String::new(),
            quota_after: 0,
            sample: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failures of the audit store.
///
/// The Go package builds its errors with `errors.New` / `fmt.Errorf`, so the
/// text is the contract (operators and tests match on it). The variants below
/// keep the messages verbatim while still letting callers tell an I/O failure
/// from a broken chain.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// A failure whose message is already fully formatted, exactly as Go would
    /// have produced it.
    #[error("{0}")]
    Message(String),
    /// An I/O failure, wrapped with the operation that failed the way Go's
    /// `fmt.Errorf("audit: ...: %w", err)` does.
    #[error("audit: {context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    /// A JSON failure, wrapped with the operation that failed.
    #[error("audit: {context}: {source}")]
    Json {
        context: String,
        #[source]
        source: serde_json::Error,
    },
    /// The ledger already contains a broken chain, so appending would launder
    /// the tampering.
    #[error(
        "audit: refusing to append to a ledger whose hash chain is broken \
         ({kind} at line {line}, seq {seq}: {detail}); the ledger is {records} \
         record(s) long - restore it from backup or archive it and start a new one"
    )]
    BrokenChain {
        kind: String,
        line: i64,
        seq: i64,
        detail: String,
        records: i64,
    },
}

impl AuditError {
    /// Builds the exact text Go would have produced.
    pub(crate) fn msg(message: impl Into<String>) -> AuditError {
        AuditError::Message(message.into())
    }

    /// Wraps an I/O failure as `audit: <context>: <source>`.
    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> AuditError {
        AuditError::Io {
            context: context.into(),
            source,
        }
    }

    /// Wraps a JSON failure as `audit: <context>: <source>`.
    pub(crate) fn json(context: impl Into<String>, source: serde_json::Error) -> AuditError {
        AuditError::Json {
            context: context.into(),
            source,
        }
    }
}

// ---------------------------------------------------------------------------
// Canonical hashing
// ---------------------------------------------------------------------------

/// Returns the bytes the entry hash is computed over: the record's own JSON with
/// the `entry_hash` field removed.
///
/// # Why the raw bytes rather than a re-marshalled struct
///
/// Hashing a re-marshalled [`Record`] means the chain is only verifiable by a
/// binary whose struct definitions produce byte-identical JSON. Any later change
/// to a field name, a JSON tag or a field's presence silently invalidates every
/// historical record, and the ledger reports a wholesale tampering that never
/// happened - which is exactly what occurred when the latency fields were
/// renamed: all 96 records became "unverifiable" overnight.
///
/// Hashing the stored bytes makes the evidence self-describing: verification
/// depends only on the file, so a ledger written by any version stays verifiable
/// by any later version.
pub(crate) fn hash_payload(raw: &[u8]) -> Result<Vec<u8>, AuditError> {
    let (stripped, found) = remove_top_level_field(raw, "entry_hash")?;
    if !found {
        return Err(AuditError::msg("audit: record has no entry_hash field"));
    }
    Ok(stripped)
}

/// Deletes one top-level member from a JSON object and reports whether it was
/// present.
///
/// The remaining members keep their original bytes, order and spacing, so the
/// result is a function of the stored text alone.
///
/// Note that this is a text-level edit, not a parse/serialise round trip: it
/// never re-encodes a number, a string escape or a timestamp, which is what
/// makes the hash stable across schema changes.
pub(crate) fn remove_top_level_field(
    raw: &[u8],
    field: &str,
) -> Result<(Vec<u8>, bool), AuditError> {
    let trimmed = trim_ascii_space(raw);
    if trimmed.is_empty() || trimmed[0] != b'{' {
        return Err(AuditError::msg("audit: record is not a JSON object"));
    }
    // Find each top-level member by walking the object with depth tracking, so a
    // nested "entry_hash" (which no record has, but a hand-edited one might)
    // cannot be mistaken for the real field.
    let mut i = 1usize;
    while i < trimmed.len() {
        i = skip_json_space(trimmed, i);
        if i >= trimmed.len() {
            break;
        }
        if trimmed[i] == b'}' {
            break;
        }
        if trimmed[i] == b',' {
            i += 1;
            continue;
        }
        if trimmed[i] != b'"' {
            return Err(AuditError::msg(format!(
                "audit: unexpected byte {} at offset {}",
                quote_byte(trimmed[i]),
                i
            )));
        }
        let key_start = i;
        let key_end = match scan_json_string(trimmed, key_start) {
            Some(end) => end,
            None => {
                return Err(AuditError::msg(format!(
                    "audit: unterminated key at offset {key_start}"
                )))
            }
        };
        let name: String = serde_json::from_slice(&trimmed[key_start..key_end])
            .map_err(|e| AuditError::Json {
                context: "unmarshal key".to_string(),
                source: e,
            })?;
        i = skip_json_space(trimmed, key_end);
        if i >= trimmed.len() || trimmed[i] != b':' {
            return Err(AuditError::msg(format!(
                "audit: expected ':' after key {name:?}"
            )));
        }
        i = skip_json_space(trimmed, i + 1);
        let val_start = i;
        let val_end = match scan_json_value(trimmed, val_start) {
            Some(end) => end,
            None => {
                return Err(AuditError::msg(format!(
                    "audit: unterminated value for key {name:?}"
                )))
            }
        };
        if name != field {
            i = val_end;
            continue;
        }
        // Remove the member plus one adjacent comma, preferring the following
        // one so a trailing member leaves valid JSON behind.
        let mut start = key_start;
        let mut end = val_end;
        if end < trimmed.len() {
            let j = skip_json_space(trimmed, end);
            if j < trimmed.len() && trimmed[j] == b',' {
                end = j + 1;
            } else if start > 0 {
                // Last member: swallow the preceding comma instead.
                let mut k = start - 1;
                while k > 0 && matches!(trimmed[k], b' ' | b'\n' | b'\t' | b'\r') {
                    k -= 1;
                }
                if trimmed[k] == b',' {
                    start = k;
                }
            }
        }
        let mut out = Vec::with_capacity(trimmed.len() - (end - start));
        out.extend_from_slice(&trimmed[..start]);
        out.extend_from_slice(&trimmed[end..]);
        return Ok((out, true));
    }
    Ok((Vec::new(), false))
}

fn trim_ascii_space(b: &[u8]) -> &[u8] {
    let is_space = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let mut start = 0;
    while start < b.len() && is_space(b[start]) {
        start += 1;
    }
    let mut end = b.len();
    while end > start && is_space(b[end - 1]) {
        end -= 1;
    }
    &b[start..end]
}

fn skip_json_space(b: &[u8], mut i: usize) -> usize {
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            _ => return i,
        }
    }
    i
}

/// Returns the offset just past a JSON string's closing quote.
fn scan_json_string(b: &[u8], mut i: usize) -> Option<usize> {
    if i >= b.len() || b[i] != b'"' {
        return None;
    }
    i += 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Returns the offset just past a JSON value.
fn scan_json_value(b: &[u8], i: usize) -> Option<usize> {
    if i >= b.len() {
        return None;
    }
    match b[i] {
        b'"' => scan_json_string(b, i),
        open @ (b'{' | b'[') => {
            let close = if open == b'[' { b']' } else { b'}' };
            let mut i = i;
            let mut depth = 0i64;
            while i < b.len() {
                match b[i] {
                    b'"' => {
                        i = scan_json_string(b, i)?;
                        continue;
                    }
                    c if c == open => depth += 1,
                    c if c == close => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            None
        }
        _ => {
            let start = i;
            let mut i = i;
            while i < b.len() {
                match b[i] {
                    b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r' => {
                        if i == start {
                            return None;
                        }
                        return Some(i);
                    }
                    _ => i += 1,
                }
            }
            if i > start {
                Some(i)
            } else {
                None
            }
        }
    }
}

/// Renders one byte the way Go's `%q` renders a byte in a quoted character
/// literal: `'a'`, `'\n'`, `'\x01'`, `'é'`.
fn quote_byte(c: u8) -> String {
    match c {
        b'\'' => "'\\''".to_string(),
        b'\\' => "'\\\\'".to_string(),
        b'\n' => "'\\n'".to_string(),
        b'\r' => "'\\r'".to_string(),
        b'\t' => "'\\t'".to_string(),
        0x07 => "'\\a'".to_string(),
        0x08 => "'\\b'".to_string(),
        0x0b => "'\\v'".to_string(),
        0x0c => "'\\f'".to_string(),
        0x20..=0x7e => format!("'{}'", c as char),
        0x7f => "'\\x7f'".to_string(),
        _ => format!("'{}'", char::from(c)),
    }
}

/// Returns the entry hash for a record about to be written.
///
/// The caller guarantees `entry_hash` is empty, and because the field is omitted
/// while empty the marshalled bytes are exactly what a verifier recovers by
/// stripping `entry_hash` from the stored line. That equality is the whole
/// reason the chain can be verified by a different version of the software.
pub(crate) fn compute_hash(r: &Record) -> Result<String, AuditError> {
    if !r.entry_hash.is_empty() {
        return Err(AuditError::msg(
            "audit: entry hash must be computed before it is set",
        ));
    }
    let b = serde_json::to_vec(r).map_err(|e| AuditError::json("marshal record", e))?;
    Ok(sha256_hex(&b))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Shortens a hash for a human-readable message.
fn short(s: &str) -> String {
    if s.len() <= 12 {
        return s.to_string();
    }
    // Go slices bytes; keep that but do not panic on a non-ASCII boundary.
    format!("{}...", String::from_utf8_lossy(&s.as_bytes()[..12]))
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Appends records to an append-only JSONL ledger.
///
/// Writes are serialised by a mutex and flushed on every record, so a crash
/// loses at most the in-progress record and never corrupts the chain.
pub struct Writer {
    path: String,
    inner: Mutex<WriterInner>,
}

/// A callback invoked when the ledger file is replaced under the process.
pub type ReplacementHook = Arc<dyn Fn(&str) + Send + Sync>;

struct WriterInner {
    f: Option<File>,
    seq: i64,
    prev: String,
    /// `dev` and `ino` identify the open file. They are compared on every append
    /// so that a ledger replaced underneath the process (by a rotation script, a
    /// restore from backup, or an attacker) is noticed instead of silently
    /// receiving appends into an orphaned inode.
    dev: u64,
    ino: u64,
    /// Called once when a replacement is detected, before the ledger is
    /// reopened. Tests use it to observe the event.
    replacement_hook: Option<ReplacementHook>,
    /// Counts detected replacements.
    replaced: i64,
    /// Forces an fsync per record, trading throughput for durability. Enabled
    /// for audit logs where loss is unacceptable.
    fsync_on_write: bool,
}

/// Configures a [`Writer`].
#[derive(Debug, Clone, Default)]
pub struct WriterOptions {
    /// The ledger file. Parent directories are created.
    pub path: String,
    /// Forces an fsync after every record.
    pub sync: bool,
    /// Starts a fresh ledger instead of appending. Only for tests and for
    /// explicitly rotating a ledger.
    pub truncate: bool,
}

impl Writer {
    /// Opens (or creates) the ledger for appending, resuming the chain from the
    /// last valid record.
    ///
    /// The chain is verified first: see [`Writer::verify_existing`]. A gateway
    /// that silently appended to a tampered ledger would launder the tampering,
    /// so opening a broken ledger fails.
    pub fn open(opts: WriterOptions) -> Result<Writer, AuditError> {
        if opts.path.is_empty() {
            return Err(AuditError::msg("audit: path is required"));
        }
        if let Some(dir) = parent_dir(&opts.path) {
            mkdir_all(&dir).map_err(|e| AuditError::io("create dir", e))?;
        }
        let f = if opts.truncate {
            open_create(&opts.path, true, false)
        } else {
            open_create(&opts.path, false, true)
        }
        .map_err(|e| AuditError::io(format!("open {}", opts.path), e))?;

        let mut inner = WriterInner {
            f: Some(f),
            seq: 0,
            prev: GENESIS_HASH.to_string(),
            dev: 0,
            ino: 0,
            replacement_hook: None,
            replaced: 0,
            fsync_on_write: opts.sync,
        };
        inner
            .remember_identity(&opts.path)
            .map_err(|e| {
                AuditError::io(format!("stat {}", opts.path), e)
            })?;

        if !opts.truncate {
            let (seq, prev) = resume(&opts.path)?;
            inner.seq = seq;
            inner.prev = prev;
        }
        Ok(Writer {
            path: opts.path,
            inner: Mutex::new(inner),
        })
    }

    /// Installs a callback invoked when the ledger file is replaced under the
    /// process. It must not block.
    pub fn set_replacement_hook(&self, hook: Option<ReplacementHook>) {
        let mut inner = self.lock();
        inner.replacement_hook = hook;
    }

    /// Returns how many times the ledger was replaced under the process. A
    /// non-zero value means evidence written before the replacement is not in
    /// the file the writer is appending to.
    pub fn replacements(&self) -> i64 {
        self.lock().replaced
    }

    /// Writes one record and returns the sequence number and entry hash.
    ///
    /// The record is mutated in place: `seq`, `prev_hash` and `entry_hash` are
    /// filled in.
    pub fn append(&self, r: &mut Record) -> Result<(i64, String), AuditError> {
        let mut inner = self.lock();
        inner.check_replacement(&self.path)?;

        // Wrapping arithmetic, like Go: a ledger cannot realistically reach
        // i64::MAX, but a panic here would take the gateway down.
        let seq = inner.seq.wrapping_add(1);
        r.seq = seq;
        r.prev_hash = inner.prev.clone();
        let hash = compute_hash(r)
            .map_err(|e| AuditError::msg(format!("audit: hash record: {e}")))?;
        r.entry_hash = hash.clone();

        let mut line = serde_json::to_vec(r)
            .map_err(|e| AuditError::json("marshal record", e))?;
        line.push(b'\n');
        let fsync = inner.fsync_on_write;
        let f = inner
            .f
            .as_mut()
            .ok_or_else(|| AuditError::msg("audit: write: ledger is closed"))?;
        f.write_all(&line)
            .map_err(|e| AuditError::io("write", e))?;
        f.flush().map_err(|e| AuditError::io("flush", e))?;
        if fsync {
            f.sync_all().map_err(|e| AuditError::io("sync", e))?;
        }
        inner.seq = seq;
        inner.prev = hash.clone();

        Ok((seq, hash))
    }

    /// Returns the last written sequence number.
    pub fn seq(&self) -> i64 {
        self.lock().seq
    }

    /// Returns the current chain head hash.
    pub fn head(&self) -> String {
        self.lock().prev.clone()
    }

    /// Returns the ledger path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Flushes and closes the ledger.
    ///
    /// Records are written straight to the file (no user-space buffer), so this
    /// only drops the handle; like Go, closing does not add an fsync of its own.
    pub fn close(&self) -> Result<(), AuditError> {
        let mut inner = self.lock();
        let Some(mut f) = inner.f.take() else {
            return Ok(());
        };
        f.flush().map_err(|e| AuditError::io("flush", e))
    }

    fn lock(&self) -> MutexGuard<'_, WriterInner> {
        // Go has no poisoned-mutex notion; keep working after a panicked writer
        // thread instead of failing every later append.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl WriterInner {
    /// Records the device and inode of the open ledger file.
    fn remember_identity(&mut self, path: &str) -> io::Result<()> {
        let f = self.f.as_ref().expect("writer holds an open file");
        let meta = f.metadata()?;
        let _ = path;
        if let Some((dev, ino)) = file_identity(&meta) {
            self.dev = dev;
            self.ino = ino;
        }
        Ok(())
    }

    /// Reopens the ledger when the file behind the path has been swapped.
    fn check_replacement(&mut self, path: &str) -> Result<(), AuditError> {
        let meta = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // The ledger was removed. Recreating it is correct: a missing
                // file is unambiguous, unlike a swapped one.
                return self.reopen_after_replacement(path);
            }
            Err(e) => return Err(AuditError::io(format!("stat {path}"), e)),
        };
        let Some((dev, ino)) = file_identity(&meta) else {
            return Ok(());
        };
        if dev == self.dev && ino == self.ino {
            return Ok(());
        }
        let hook = self.replacement_hook.clone();
        self.replaced += 1;
        if let Some(hook) = hook {
            hook(path);
        }
        self.reopen_after_replacement(path)
    }

    /// Reopens the path and reconciles the chain.
    fn reopen_after_replacement(&mut self, path: &str) -> Result<(), AuditError> {
        if let Some(f) = self.f.as_mut() {
            f.flush().map_err(|e| AuditError::io("flush", e))?;
        }
        self.f = None;
        let f = open_create(path, false, true)
            .map_err(|e| AuditError::io(format!("reopen {path}"), e))?;
        self.f = Some(f);
        self.remember_identity(path)
            .map_err(|e| AuditError::io(format!("stat {path}"), e))?;
        // Re-read the file so the chain continues from what is actually there.
        let (seq, prev) = tail_of(path)?;
        self.seq = seq;
        self.prev = prev;
        Ok(())
    }
}

/// Reads the tail of an existing ledger to continue the chain.
///
/// It also verifies the chain. A gateway that silently appends to a tampered
/// ledger would launder the tampering: the resulting file would look internally
/// consistent from that point on, and the break would only be discoverable by
/// someone who happened to verify the whole file. Refusing to start is the
/// correct behaviour for an evidence store.
fn resume(path: &str) -> Result<(i64, String), AuditError> {
    let f = File::open(path).map_err(|e| AuditError::io(format!("reopen {path}"), e))?;
    let mut reader = BufReader::new(f);
    let mut buf = Vec::new();
    let mut last: Option<Record> = None;
    let mut line_no = 0i64;
    while read_next_line(&mut reader, &mut buf, RESUME_LINE_CAP)
        .map_err(|e| AuditError::io(format!("scan {path}"), e))?
    {
        if buf.is_empty() {
            continue;
        }
        line_no += 1;
        let rec: Record = serde_json::from_slice(&buf).map_err(|e| {
            AuditError::msg(format!("audit: corrupt record at line {line_no}: {e}"))
        })?;
        last = Some(rec);
    }
    verify_existing(path)?;
    match last {
        Some(rec) => Ok((rec.seq, rec.entry_hash)),
        None => Ok((0, GENESIS_HASH.to_string())),
    }
}

/// Walks the ledger and rejects a broken chain.
fn verify_existing(path: &str) -> Result<(), AuditError> {
    let res = verify(path, "")?;
    if res.ok {
        return Ok(());
    }
    let v = &res.violations[0];
    Err(AuditError::BrokenChain {
        kind: v.kind.clone(),
        line: v.line,
        seq: v.seq,
        detail: v.detail.clone(),
        records: res.records,
    })
}

/// Returns the final sequence number and hash of a ledger, or the genesis state
/// when the ledger is empty.
fn tail_of(path: &str) -> Result<(i64, String), AuditError> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok((0, GENESIS_HASH.to_string()))
        }
        Err(e) => return Err(AuditError::io(format!("open {path}"), e)),
    };
    let mut reader = BufReader::new(f);
    let mut buf = Vec::new();
    let mut seq = 0i64;
    let mut hash = GENESIS_HASH.to_string();
    while read_next_line(&mut reader, &mut buf, RESUME_LINE_CAP)
        .map_err(|e| AuditError::io(format!("scan {path}"), e))?
    {
        if buf.is_empty() {
            continue;
        }
        let rec: Record = serde_json::from_slice(&buf)
            .map_err(|e| AuditError::msg(format!("audit: parse {path}: {e}")))?;
        seq = rec.seq;
        hash = rec.entry_hash;
    }
    Ok((seq, hash))
}

/// Reads one line (without its terminator) the way Go's `bufio.Scanner` does:
/// `\n` ends a line and one trailing `\r` is dropped, so a CRLF ledger hashes
/// the same bytes it would on any other platform.
fn read_next_line<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> io::Result<bool> {
    buf.clear();
    if reader.read_until(b'\n', buf)? == 0 {
        return Ok(false);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    // Go's ScanLines drops one trailing carriage return whether or not the line
    // ended in a newline, so a CRLF ledger hashes the same bytes anywhere.
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    if buf.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("line exceeds {max} bytes"),
        ));
    }
    Ok(true)
}

/// The Go scanner's token cap for `resume` and `tailOf`.
const RESUME_LINE_CAP: usize = 8 << 20;
/// The Go scanner's token cap for `Verify` and `ReadAll`.
const VERIFY_LINE_CAP: usize = 16 << 20;

// ---------------------------------------------------------------------------
// Anchoring and verification
// ---------------------------------------------------------------------------

/// Pins a chain head to an external location.
///
/// An attacker with write access to the ledger can rewrite the whole chain; an
/// anchor written elsewhere (another filesystem, a log shipper, a timestamping
/// service) makes that detectable. [`append_anchor`] is intentionally dumb: it
/// appends one line and never rewrites history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Anchor {
    pub seq: i64,
    pub hash: String,
    pub time: DateTime<Utc>,
    pub records: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub component: String,
}

impl Default for Anchor {
    fn default() -> Self {
        Anchor {
            seq: 0,
            hash: String::new(),
            time: zero_time(),
            records: 0,
            component: String::new(),
        }
    }
}

/// Appends an anchor line to the anchor file.
pub fn append_anchor(path: &str, a: &Anchor) -> Result<(), AuditError> {
    if path.is_empty() {
        return Err(AuditError::msg("audit: anchor path is required"));
    }
    if let Some(dir) = parent_dir(path) {
        mkdir_all(&dir).map_err(|e| AuditError::io("create anchor dir", e))?;
    }
    let mut f = open_create(path, false, true)
        .map_err(|e| AuditError::io(format!("open anchor {path}"), e))?;
    let mut b = serde_json::to_vec(a).map_err(|e| AuditError::json("marshal anchor", e))?;
    b.push(b'\n');
    f.write_all(&b)
        .map_err(|e| AuditError::io("write anchor", e))?;
    f.sync_all()
        .map_err(|e| AuditError::io("sync anchor", e))
}

/// Reads all anchors from a file, newest last.
///
/// A missing file is not an error: it means no anchor has been written yet.
pub fn read_anchors(path: &str) -> Result<Vec<Anchor>, AuditError> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(AuditError::io(format!("open {path}"), e)),
    };
    let mut reader = BufReader::new(f);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    while read_next_line(&mut reader, &mut buf, ANCHOR_LINE_CAP)
        .map_err(|e| AuditError::io(format!("scan {path}"), e))?
    {
        if buf.is_empty() {
            continue;
        }
        let a: Anchor = serde_json::from_slice(&buf)
            .map_err(|e| AuditError::msg(format!("audit: anchor parse: {e}")))?;
        out.push(a);
    }
    Ok(out)
}

/// The Go scanner's token cap for `ReadAnchors`.
const ANCHOR_LINE_CAP: usize = 1 << 20;

/// Classifies a chain integrity failure.
///
/// Kept as a `String` for the same reason as [`Decision`]: the JSON field of an
/// existing ledger must stay readable even if a newer version adds a kind.
pub type ViolationKind = String;

/// A sequence number was skipped.
pub const VIOLATION_SEQ_GAP: &str = "seq_gap";
/// `prev_hash` does not match the previous record's `entry_hash`.
pub const VIOLATION_PREV_MISMATCH: &str = "prev_hash_mismatch";
/// The record's own content no longer hashes to `entry_hash`.
pub const VIOLATION_HASH_MISMATCH: &str = "entry_hash_mismatch";
/// The line is not valid JSON.
pub const VIOLATION_PARSE: &str = "parse_error";
/// The ledger head disagrees with an anchor.
pub const VIOLATION_ANCHOR: &str = "anchor_mismatch";

/// One integrity failure.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Violation {
    pub kind: ViolationKind,
    pub line: i64,
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub seq: i64,
    pub detail: String,
}

/// Summarises a verification pass.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyResult {
    /// True when the chain is intact and all anchors agree.
    pub ok: bool,
    /// The number of records verified.
    pub records: i64,
    /// The verified chain head.
    pub head_seq: i64,
    pub head_hash: String,
    /// Every problem found.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<Violation>,
    /// Counts anchors that were compared.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub anchors_checked: i64,
    /// Counts anchors whose seq is beyond the ledger.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub anchors_ignored: i64,
}

/// Walks the ledger and checks the hash chain.
///
/// When `anchor_path` is non-empty, anchors are also checked: every anchor whose
/// seq is within the ledger must match the record at that sequence.
pub fn verify(path: &str, anchor_path: &str) -> Result<VerifyResult, AuditError> {
    let f = File::open(path).map_err(|e| AuditError::io(format!("open {path}"), e))?;
    let mut reader = BufReader::new(f);

    let mut res = VerifyResult {
        ok: true,
        ..Default::default()
    };
    let mut prev = GENESIS_HASH.to_string();
    let mut prev_seq = 0i64;
    let mut by_seq: HashMap<i64, String> = HashMap::new();
    let mut line_no = 0i64;
    let mut buf = Vec::new();

    while read_next_line(&mut reader, &mut buf, VERIFY_LINE_CAP)
        .map_err(|e| AuditError::io("scan", e))?
    {
        line_no += 1;
        let line = buf.as_slice();
        if line.is_empty() {
            continue;
        }
        let rec: Record = match serde_json::from_slice(line) {
            Ok(rec) => rec,
            Err(e) => {
                res.ok = false;
                res.violations.push(Violation {
                    kind: VIOLATION_PARSE.to_string(),
                    line: line_no,
                    seq: 0,
                    detail: format!("invalid JSON: {e}"),
                });
                continue;
            }
        };
        // Wrapping matches Go's integer arithmetic, so a ledger whose seq has
        // reached i64::MAX reports a gap instead of panicking on overflow.
        let expected_seq = prev_seq.wrapping_add(1);
        if rec.seq != expected_seq {
            res.ok = false;
            res.violations.push(Violation {
                kind: VIOLATION_SEQ_GAP.to_string(),
                line: line_no,
                seq: rec.seq,
                detail: format!("expected seq {expected_seq}, found {}", rec.seq),
            });
        }
        if rec.prev_hash != prev {
            res.ok = false;
            res.violations.push(Violation {
                kind: VIOLATION_PREV_MISMATCH.to_string(),
                line: line_no,
                seq: rec.seq,
                detail: format!(
                    "prev_hash {} does not match chain head {}",
                    short(&rec.prev_hash),
                    short(&prev)
                ),
            });
        }
        // Hash the record's own bytes rather than a re-marshalled struct, so a
        // ledger written by an older version stays verifiable by this one.
        match hash_payload(line) {
            Err(e) => {
                res.ok = false;
                res.violations.push(Violation {
                    kind: VIOLATION_PARSE.to_string(),
                    line: line_no,
                    seq: rec.seq,
                    detail: format!("cannot hash record: {e}"),
                });
                prev = rec.entry_hash;
                prev_seq = rec.seq;
                res.records += 1;
                continue;
            }
            Ok(payload) => {
                let want = sha256_hex(&payload);
                if want != rec.entry_hash {
                    res.ok = false;
                    res.violations.push(Violation {
                        kind: VIOLATION_HASH_MISMATCH.to_string(),
                        line: line_no,
                        seq: rec.seq,
                        detail: format!(
                            "content hashes to {} but entry_hash is {}",
                            short(&want),
                            short(&rec.entry_hash)
                        ),
                    });
                }
            }
        }
        by_seq.insert(rec.seq, rec.entry_hash.clone());
        prev = rec.entry_hash;
        prev_seq = rec.seq;
        res.records += 1;
    }

    res.head_seq = prev_seq;
    res.head_hash = prev;

    if !anchor_path.is_empty() {
        let anchors = read_anchors(anchor_path)?;
        for a in anchors {
            // An anchor at sequence 0 pins the empty ledger ("genesis"). The
            // gateway writes one every time it starts against a fresh ledger, so
            // it is a normal state and not evidence of removal.
            if a.seq == 0 {
                res.anchors_checked += 1;
                if res.records > 0 && a.hash != GENESIS_HASH {
                    res.ok = false;
                    res.violations.push(Violation {
                        kind: VIOLATION_ANCHOR.to_string(),
                        line: 0,
                        seq: 0,
                        detail: format!(
                            "genesis anchor has hash {}, expected {}",
                            short(&a.hash),
                            GENESIS_HASH
                        ),
                    });
                }
                continue;
            }
            if a.seq > prev_seq {
                // The anchor covers records that no longer exist. The ledger
                // shrank after the anchor was written, which means evidence was
                // removed - that is a violation, not a benign state: a torn
                // trailing write cannot extend a chain past an anchor that was
                // already recorded, because the anchor is written from the same
                // serialised writer.
                res.ok = false;
                res.anchors_ignored += 1;
                res.violations.push(Violation {
                    kind: VIOLATION_ANCHOR.to_string(),
                    line: 0,
                    seq: a.seq,
                    detail: format!(
                        "ledger ends at seq {prev_seq} but an anchor recorded seq {} (hash {}): records were removed",
                        a.seq,
                        short(&a.hash)
                    ),
                });
                continue;
            }
            res.anchors_checked += 1;
            match by_seq.get(&a.seq) {
                None => {
                    // The anchor points inside the ledger's range but the record
                    // is gone: evidence was removed.
                    res.ok = false;
                    res.violations.push(Violation {
                        kind: VIOLATION_ANCHOR.to_string(),
                        line: 0,
                        seq: a.seq,
                        detail: format!(
                            "anchor references seq {} which is missing from the ledger (expected hash {})",
                            a.seq,
                            short(&a.hash)
                        ),
                    });
                }
                Some(got) if *got != a.hash => {
                    res.ok = false;
                    res.violations.push(Violation {
                        kind: VIOLATION_ANCHOR.to_string(),
                        line: 0,
                        seq: a.seq,
                        detail: format!(
                            "anchor at seq {} expects {} but ledger has {}",
                            a.seq,
                            short(&a.hash),
                            short(got)
                        ),
                    });
                }
                Some(_) => {}
            }
        }
    }

    res.violations.sort_by_key(|v| v.line);
    Ok(res)
}

/// Loads every record from a ledger.
pub fn read_all(path: &str) -> Result<Vec<Record>, AuditError> {
    let f = File::open(path).map_err(|e| AuditError::io(format!("open {path}"), e))?;
    let mut reader = BufReader::new(f);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    while read_next_line(&mut reader, &mut buf, VERIFY_LINE_CAP)
        .map_err(|e| AuditError::io("scan", e))?
    {
        if buf.is_empty() {
            continue;
        }
        let rec: Record = serde_json::from_slice(&buf)
            .map_err(|e| AuditError::msg(format!("audit: parse: {e}")))?;
        out.push(rec);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Platform helpers
// ---------------------------------------------------------------------------

/// Returns the parent directory of a path, or `None` for `"."` and empty
/// parents, mirroring Go's `filepath.Dir` guard in the original code.
fn parent_dir(path: &str) -> Option<PathBuf> {
    let dir = Path::new(path).parent()?;
    if dir.as_os_str().is_empty() || dir == Path::new(".") {
        return None;
    }
    Some(dir.to_path_buf())
}

/// Creates `dir` and its parents with the deployment's 0750 mode.
#[cfg(unix)]
fn mkdir_all(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().recursive(true).mode(0o750).create(dir)
}

/// Creates `dir` and its parents.
#[cfg(not(unix))]
fn mkdir_all(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

/// Opens a file for appending (or truncating) with the ledger's 0640 mode.
#[cfg(unix)]
fn open_create(path: &str, truncate: bool, append: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(truncate)
        .append(append)
        .mode(0o640)
        .open(path)
}

/// Opens a file for appending (or truncating).
#[cfg(not(unix))]
fn open_create(path: &str, truncate: bool, append: bool) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(truncate)
        .append(append)
        .open(path)
}

/// Returns the device and inode that identify an open file.
#[cfg(unix)]
fn file_identity(meta: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

/// Replacement detection is unavailable on platforms without a stable
/// (device, inode) identity, exactly as in Go when `Stat_t` is absent.
#[cfg(not(unix))]
fn file_identity(_meta: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

// ---------------------------------------------------------------------------
// Tests (translated from ledger_test.go)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::testutil::TempDir;
    use chrono::SubsecRound;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn temp_ledger(t: &TempDir) -> (String, String) {
        (
            t.join("audit.jsonl").to_string_lossy().into_owned(),
            t.join("anchor.jsonl").to_string_lossy().into_owned(),
        )
    }

    fn sample_record(id: &str) -> Record {
        Record {
            id: id.to_string(),
            time: Utc::now().trunc_subsecs(0),
            actor: "alice".to_string(),
            channel: "upstream-a".to_string(),
            model: "gpt-4o".to_string(),
            protocol: "openai".to_string(),
            decision: DECISION_ALLOW.to_string(),
            usage: UsageSummary {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                upstream_reported: true,
                verdict: "consistent".to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn append_and_verify() {
        let dir = TempDir::new();
        let (path, anchor_path) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        for i in 0..5 {
            let mut rec = sample_record(&format!("r{}", (b'a' + i) as char));
            w.append(&mut rec).expect("append");
        }
        assert_eq!(w.seq(), 5, "expected seq 5");
        w.close().expect("close");

        let res = verify(&path, "").expect("verify");
        assert!(res.ok, "expected OK, got {res:?}");
        assert_eq!(res.records, 5);
        assert_eq!(res.head_seq, 5);
        let _ = anchor_path;
    }

    /// The central tamper-evidence test: rewriting any field of any record must
    /// break verification.
    #[test]
    fn chain_detects_modification() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        for i in 0..4 {
            let mut rec = sample_record("r");
            rec.usage.prompt_tokens = 100 + i;
            w.append(&mut rec).expect("append");
        }
        w.close().expect("close");

        // Tamper with the third record's token count.
        let mut lines = read_lines(&path);
        let mut rec: Record = serde_json::from_str(&lines[2]).expect("unmarshal");
        rec.usage.prompt_tokens = 1; // pretend the user sent almost nothing
        rec.usage.total_tokens = 1;
        lines[2] = serde_json::to_string(&rec).expect("marshal");
        write_lines(&path, &lines);

        let res = verify(&path, "").expect("verify");
        assert!(!res.ok, "tampering was not detected");
        assert!(
            has_violation(&res, VIOLATION_HASH_MISMATCH),
            "expected an entry_hash_mismatch, got {:?}",
            res.violations
        );
    }

    /// Removing a record must break the chain.
    #[test]
    fn chain_detects_deletion() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        for _ in 0..5 {
            let mut rec = sample_record("r");
            w.append(&mut rec).expect("append");
        }
        w.close().expect("close");

        let mut lines = read_lines(&path);
        lines.remove(1); // drop record 2
        write_lines(&path, &lines);

        let res = verify(&path, "").expect("verify");
        assert!(!res.ok, "deletion was not detected");
        assert!(
            has_violation(&res, VIOLATION_SEQ_GAP)
                || has_violation(&res, VIOLATION_PREV_MISMATCH),
            "expected a seq gap or prev-hash mismatch, got {:?}",
            res.violations
        );
    }

    /// Swapping two records must be detected.
    #[test]
    fn chain_detects_reorder() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        for _ in 0..4 {
            let mut rec = sample_record("r");
            w.append(&mut rec).expect("append");
        }
        w.close().expect("close");

        let mut lines = read_lines(&path);
        lines.swap(1, 2);
        write_lines(&path, &lines);

        let res = verify(&path, "").expect("verify");
        assert!(!res.ok, "reordering was not detected");
    }

    /// Injecting a fabricated record must fail.
    #[test]
    fn chain_detects_insertion() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        for _ in 0..3 {
            let mut rec = sample_record("r");
            w.append(&mut rec).expect("append");
        }
        w.close().expect("close");

        let mut forged = sample_record("forged");
        forged.seq = 2;
        forged.prev_hash = GENESIS_HASH.to_string();
        forged.entry_hash = "f".repeat(64);
        let b = serde_json::to_string(&forged).expect("marshal");

        let mut lines = read_lines(&path);
        lines.insert(1, b);
        write_lines(&path, &lines);

        let res = verify(&path, "").expect("verify");
        assert!(!res.ok, "insertion was not detected");
    }

    /// Covers the scenario the hash chain alone cannot catch: an attacker with
    /// write access to the whole ledger who rewrites it consistently.
    #[test]
    fn anchor_detects_full_rewrite() {
        let dir = TempDir::new();
        let (path, anchor_path) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        for _ in 0..3 {
            let mut rec = sample_record("r");
            w.append(&mut rec).expect("append");
        }
        // Anchor the honest head.
        append_anchor(
            &anchor_path,
            &Anchor {
                seq: w.seq(),
                hash: w.head(),
                time: Utc::now(),
                records: w.seq(),
                ..Default::default()
            },
        )
        .expect("append anchor");
        w.close().expect("close");

        // Attacker truncates and rewrites the ledger with a consistent chain
        // claiming fewer tokens were used.
        let fresh = Writer::open(WriterOptions {
            path: path.clone(),
            truncate: true,
            ..Default::default()
        })
        .expect("open fresh");
        for _ in 0..2 {
            let mut rec = sample_record("r");
            rec.usage.total_tokens = 1;
            fresh.append(&mut rec).expect("append");
        }
        fresh.close().expect("close");

        // The internally consistent rewrite passes plain verification...
        let res = verify(&path, "").expect("verify");
        assert!(
            res.ok,
            "rewritten chain should be internally consistent, got {:?}",
            res.violations
        );
        // ...but the anchor exposes it.
        let res = verify(&path, &anchor_path).expect("verify with anchors");
        assert!(!res.ok, "anchor did not detect the rewrite");
        assert!(
            has_violation(&res, VIOLATION_ANCHOR),
            "expected an anchor_mismatch, got {:?}",
            res.violations
        );
    }

    /// Reopening a ledger continues the chain rather than restarting it, which
    /// would silently discard evidence.
    #[test]
    fn resume_continues_chain() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        for _ in 0..3 {
            let mut rec = sample_record("r");
            w.append(&mut rec).expect("append");
        }
        let head_before = w.head();
        w.close().expect("close");

        let w2 = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("reopen");
        assert_eq!(w2.seq(), 3, "expected to resume at seq 3");
        assert_eq!(w2.head(), head_before, "resumed head differs");
        let mut rec = sample_record("r4");
        let (seq, _) = w2.append(&mut rec).expect("append after resume");
        assert_eq!(seq, 4, "expected seq 4 after resume");
        w2.close().expect("close");

        let res = verify(&path, "").expect("verify");
        assert!(res.ok, "chain broken after resume: {:?}", res.violations);
    }

    #[test]
    fn corrupt_line_reported() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        let mut rec = sample_record("r");
        w.append(&mut rec).expect("append");
        w.close().expect("close");

        let mut lines = read_lines(&path);
        lines.push("{not valid json".to_string());
        write_lines(&path, &lines);

        let res = verify(&path, "").expect("verify");
        assert!(!res.ok, "corrupt line was not reported");
        assert!(
            has_violation(&res, VIOLATION_PARSE),
            "expected a parse_error, got {:?}",
            res.violations
        );
    }

    #[test]
    fn read_all_records() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        for _ in 0..3 {
            let mut rec = sample_record("r");
            w.append(&mut rec).expect("append");
        }
        w.close().expect("close");

        let recs = read_all(&path).expect("read all");
        assert_eq!(recs.len(), 3);
        for (i, r) in recs.iter().enumerate() {
            assert_eq!(r.seq, i as i64 + 1, "record {i} has wrong seq");
        }
    }

    #[test]
    fn verify_missing_file_is_an_error() {
        let dir = TempDir::new();
        assert!(verify(&dir.join("nope.jsonl").to_string_lossy(), "").is_err());
    }

    #[test]
    fn read_anchors_missing_file_is_empty() {
        let dir = TempDir::new();
        let a = read_anchors(&dir.join("nope.jsonl").to_string_lossy()).expect("read anchors");
        assert!(a.is_empty(), "expected no anchors, got {a:?}");
    }

    /// The writer must not append to a ledger whose chain is already broken.
    /// Appending would launder the tampering: the file would look
    /// self-consistent from that point on.
    #[test]
    fn open_refuses_tampered_ledger() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        for _ in 0..3 {
            let mut rec = sample_record("r");
            w.append(&mut rec).expect("append");
        }
        w.close().expect("close");

        // Alter a token count, which invalidates the entry hash.
        let mut lines = read_lines(&path);
        let mut rec: Record = serde_json::from_str(&lines[1]).expect("unmarshal");
        rec.usage.total_tokens = 1;
        lines[1] = serde_json::to_string(&rec).expect("marshal");
        write_lines(&path, &lines);

        let err = match Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        }) {
            Ok(_) => panic!("expected Open to refuse a tampered ledger"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("hash chain is broken"),
            "unexpected error: {err}"
        );
    }

    /// Swapping the file underneath a running writer is noticed, and the writer
    /// re-anchors onto the new file rather than appending into an orphaned
    /// inode.
    #[test]
    fn detects_ledger_replacement() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");

        let hook_calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hook_calls);
        w.set_replacement_hook(Some(Arc::new(move |_p: &str| {
            counter.fetch_add(1, Ordering::SeqCst);
        })));

        for _ in 0..2 {
            let mut rec = sample_record("r");
            w.append(&mut rec).expect("append");
        }

        // Replace the ledger with an independent, self-consistent one.
        let other = format!("{path}.other");
        let ow = Writer::open(WriterOptions {
            path: other.clone(),
            truncate: true,
            ..Default::default()
        })
        .expect("open other");
        for _ in 0..4 {
            let mut rec = sample_record("other");
            ow.append(&mut rec).expect("append other");
        }
        ow.close().expect("close other");
        fs::rename(&other, &path).expect("rename");

        let mut rec = sample_record("after-replacement");
        let (seq, _) = w.append(&mut rec).expect("append after replacement");
        assert_eq!(
            hook_calls.load(Ordering::SeqCst),
            1,
            "replacement hook called the wrong number of times"
        );
        assert_eq!(w.replacements(), 1, "Replacements() != 1");
        // The chain must continue from the replacement's tail, so the resulting
        // file stays verifiable.
        assert_eq!(seq, 5, "sequence after replacement != 5");

        let res = verify(&path, "").expect("verify");
        assert!(
            res.ok,
            "ledger not verifiable after replacement: {:?}",
            res.violations
        );
        assert_eq!(res.records, 5);
        w.close().expect("close");
    }

    /// The ledger under contention.
    #[test]
    fn concurrent_appends_stay_consistent() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        const N: i64 = 60;
        let w = &w;
        std::thread::scope(|scope| {
            for _ in 0..N {
                scope.spawn(move || {
                    let mut rec = sample_record("r");
                    w.append(&mut rec).expect("append");
                });
            }
        });
        w.close().expect("close");

        let res = verify(&path, "").expect("verify");
        assert!(
            res.ok,
            "concurrent appends broke the chain: {:?}",
            res.violations
        );
        assert_eq!(res.records, N);
    }

    /// Regression test for a real false alarm found on FreeBSD: a gateway that
    /// starts against an empty ledger writes an anchor at sequence 0, and
    /// verification then reported "anchor references seq 0 which is missing from
    /// the ledger". That trains an operator to ignore the integrity check, which
    /// is worse than not having one.
    #[test]
    fn genesis_anchor_is_not_a_violation() {
        let dir = TempDir::new();
        let (path, anchor_path) = temp_ledger(&dir);

        // Start a gateway against a fresh ledger: it anchors sequence 0.
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        append_anchor(
            &anchor_path,
            &Anchor {
                seq: w.seq(),
                hash: w.head(),
                time: Utc::now(),
                ..Default::default()
            },
        )
        .expect("append anchor");

        // The empty ledger plus a genesis anchor must verify cleanly.
        let res = verify(&path, &anchor_path).expect("verify");
        assert!(
            res.ok,
            "a genesis anchor was reported as a violation: {:?}",
            res.violations
        );
        assert_eq!(res.anchors_checked, 1, "genesis anchor not counted: {res:?}");

        // And it must still verify after records are appended.
        for _ in 0..3 {
            let mut rec = sample_record("r");
            w.append(&mut rec).expect("append");
        }
        w.close().expect("close");
        let res = verify(&path, &anchor_path).expect("verify");
        assert!(
            res.ok,
            "genesis anchor broke a non-empty ledger: {:?}",
            res.violations
        );
    }

    /// The other side: a genesis anchor that does not carry the genesis hash is
    /// malformed.
    #[test]
    fn genesis_anchor_with_wrong_hash_is_rejected() {
        let dir = TempDir::new();
        let (path, anchor_path) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        let mut rec = sample_record("r");
        w.append(&mut rec).expect("append");
        w.close().expect("close");

        append_anchor(
            &anchor_path,
            &Anchor {
                seq: 0,
                hash: "not-the-genesis-hash".to_string(),
                time: Utc::now(),
                ..Default::default()
            },
        )
        .expect("append anchor");
        let res = verify(&path, &anchor_path).expect("verify");
        assert!(!res.ok, "a malformed genesis anchor was accepted");
    }

    /// Regression test for a real and serious defect found on the FreeBSD
    /// deployment.
    ///
    /// Verification originally hashed a *re-marshalled* record, so it was only
    /// valid for a binary whose struct definitions produced byte-identical JSON.
    /// Renaming the latency fields (to make their unit explicit) made all 96
    /// historical records report `entry_hash_mismatch`, and the gateway then
    /// refused to start.
    ///
    /// The test writes a ledger in an *older* record shape (latency fields
    /// without their unit suffix, and no provider/key fields at all) and hashes
    /// it the way that older writer would have. The current verifier must accept
    /// it, because the bytes are untouched.
    #[test]
    fn verifies_a_ledger_written_by_an_older_schema() {
        let dir = TempDir::new();
        let path = dir.join("old.jsonl").to_string_lossy().into_owned();

        // Records as an older release would have written them, with the latency
        // fields that carried no unit and before provider/key existed. The map
        // is built as raw text so the bytes are exactly what that writer
        // produced; entry_hash is appended last, as a map insertion would.
        let shapes = [
            r#"{"seq":1,%PREV%"id":"req_old_1","time":"2026-01-01T00:00:00Z","actor":"alice","channel":"vendor","model":"gpt-4o","protocol":"openai","decision":"allow","status_code":200,"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"upstream_reported":true,"verdict":"consistent"},"latency_ms":{"total":12,"outbound_audit":3,"upstream":8,"ttfb":9}}"#,
            r#"{"seq":2,%PREV%"id":"req_old_2","time":"2026-01-01T00:00:01Z","actor":"bob","channel":"vendor","model":"gpt-4o-mini","protocol":"openai","decision":"allow","status_code":200,"usage":{"prompt_tokens":20,"completion_tokens":7,"total_tokens":27,"upstream_reported":true,"verdict":"consistent"},"latency_ms":{"total":15,"outbound_audit":2,"upstream":11,"ttfb":12}}"#,
        ];

        let mut lines = Vec::new();
        let mut prev = GENESIS_HASH.to_string();
        for shape in shapes {
            let payload = shape.replace("%PREV%", &format!("\"prev_hash\":\"{prev}\","));
            // The chain hash covers the record with entry_hash absent, computed
            // over that version's own serialisation.
            let hash = sha256_hex(payload.as_bytes());
            // Inserting the field the way a map insertion does: at the end.
            let line = format!(
                "{},\"entry_hash\":\"{}\"}}",
                &payload[..payload.len() - 1],
                hash
            );
            lines.push(line);
            prev = hash;
        }
        fs::write(&path, lines.join("\n") + "\n").expect("write ledger");

        let res = verify(&path, "").expect("verify");
        assert!(
            res.ok,
            "a ledger written by an older schema must stay verifiable: {:?}",
            res.violations
        );
        assert_eq!(res.records, 2, "records != 2");
    }

    /// The security property is unchanged by hashing stored bytes: altering a
    /// value must still break it.
    #[test]
    fn tampering_is_still_detected_after_the_hash_change() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        for _ in 0..3 {
            let mut rec = sample_record("r");
            w.append(&mut rec).expect("append");
        }
        w.close().expect("close");

        let mut lines = read_lines(&path);
        let mut raw: serde_json::Value =
            serde_json::from_str(&lines[1]).expect("unmarshal");
        raw["usage"]["total_tokens"] = serde_json::json!(1);
        lines[1] = serde_json::to_string(&raw).expect("marshal");
        write_lines(&path, &lines);

        let res = verify(&path, "").expect("verify");
        assert!(!res.ok, "tampering was not detected after the hash change");
    }

    /// Deleting the field the hash is computed over is a violation rather than
    /// an error that is skipped.
    #[test]
    fn removing_entry_hash_is_detected() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        let mut rec = sample_record("r");
        w.append(&mut rec).expect("append");
        w.close().expect("close");

        let lines = read_lines(&path);
        let mut raw: serde_json::Value =
            serde_json::from_str(&lines[0]).expect("unmarshal");
        raw.as_object_mut().expect("object").remove("entry_hash");
        let modified = serde_json::to_string(&raw).expect("marshal");
        write_lines(&path, &[modified]);

        let res = verify(&path, "").expect("verify");
        assert!(!res.ok, "removing entry_hash must be detected");
    }

    /// The operator-facing repair path: a ledger whose hashes were computed by
    /// an older format can be rebuilt, the original is preserved, and the result
    /// verifies.
    #[test]
    fn rehash_repairs_an_unverifiable_chain() {
        let dir = TempDir::new();
        let path = dir.join("old.jsonl").to_string_lossy().into_owned();

        // Two records whose hashes were computed over a payload that still
        // contained an empty entry_hash, which is what the old writer produced.
        let shapes = [
            r#"{"seq":1,%PREV%"id":"r1","actor":"a","channel":"c","model":"m","decision":"allow","usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2},"entry_hash":""}"#,
            r#"{"seq":2,%PREV%"id":"r2","actor":"a","channel":"c","model":"m","decision":"allow","usage":{"prompt_tokens":2,"completion_tokens":2,"total_tokens":4},"entry_hash":""}"#,
        ];
        let mut lines = Vec::new();
        let mut prev = GENESIS_HASH.to_string();
        for shape in shapes {
            let payload = shape.replace("%PREV%", &format!("\"prev_hash\":\"{prev}\","));
            let hash = sha256_hex(payload.as_bytes());
            let line = payload.replace(
                "\"entry_hash\":\"\"",
                &format!("\"entry_hash\":\"{hash}\""),
            );
            lines.push(line);
            prev = hash;
        }
        fs::write(&path, lines.join("\n") + "\n").expect("write ledger");

        // The current verifier must reject it, because stripping entry_hash no
        // longer reproduces the payload the old hash covered.
        let before = verify(&path, "").expect("verify");
        assert!(
            !before.ok,
            "the old-format ledger should not verify; nothing to repair"
        );

        let res = super::super::rehash::rehash(&path).expect("rehash");
        assert_eq!(res.records, 2);
        assert!(res.changed > 0, "expected the hashes to change");
        assert!(
            Path::new(&res.backup).exists(),
            "the original must be preserved"
        );

        // An anchor pinning the old head must be moved aside, or the gateway
        // would refuse to start against the rebuilt chain.
        let anchor_path = dir.join("anchor.jsonl").to_string_lossy().into_owned();
        append_anchor(
            &anchor_path,
            &Anchor {
                seq: 2,
                hash: prev,
                time: Utc::now(),
                ..Default::default()
            },
        )
        .expect("append anchor");

        let after = verify(&path, "").expect("verify after rehash");
        assert!(
            after.ok,
            "the rebuilt chain does not verify: {:?}",
            after.violations
        );
        assert_eq!(after.records, 2, "records after rehash != 2");
    }

    /// The tool is a no-op on a healthy ledger.
    #[test]
    fn rehash_reports_when_nothing_is_needed() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        for _ in 0..3 {
            let mut rec = sample_record("r");
            w.append(&mut rec).expect("append");
        }
        w.close().expect("close");

        let res = super::super::rehash::rehash(&path).expect("rehash");
        assert_eq!(
            res.changed, 0,
            "a healthy chain should need no changes, got {}",
            res.changed
        );
        let after = verify(&path, "").expect("verify");
        assert!(after.ok, "verification broke: {:?}", after.violations);
    }

    // -----------------------------------------------------------------------
    // Rust-side boundary tests (beyond the Go suite)
    // -----------------------------------------------------------------------

    // The expected strings below are the byte-for-byte output of the Go
    // `removeTopLevelField`, captured from the reference implementation, so this
    // test pins the text-level removal the chain hash depends on.
    #[test]
    fn remove_top_level_field_handles_every_position() {
        let cases: [(&str, &str, &str); 10] = [
            // (name, input, expected output)
            (
                "first member",
                r#"{"entry_hash":"x","seq":1}"#,
                r#"{"seq":1}"#,
            ),
            (
                "middle member",
                r#"{"seq":1,"entry_hash":"x","id":"a"}"#,
                r#"{"seq":1,"id":"a"}"#,
            ),
            (
                "last member",
                r#"{"seq":1,"entry_hash":"x"}"#,
                r#"{"seq":1}"#,
            ),
            ("only member", r#"{"entry_hash":"x"}"#, r#"{}"#),
            (
                "empty value is still removed",
                r#"{"entry_hash":"","seq":1}"#,
                r#"{"seq":1}"#,
            ),
            (
                "surrounding whitespace is kept as-is",
                "{ \"seq\" : 1 , \"entry_hash\" : \"x\" , \"id\" : \"a\" }",
                // The comma after the value is swallowed, leaving the whitespace
                // that surrounded it on both sides.
                "{ \"seq\" : 1 ,  \"id\" : \"a\" }",
            ),
            (
                "newlines inside the object",
                "{ \"seq\" : 1 ,\n  \"entry_hash\" : \"x\" ,\n  \"id\" : \"a\" }",
                "{ \"seq\" : 1 ,\n  \n  \"id\" : \"a\" }",
            ),
            (
                "leading and trailing space are trimmed",
                "  {\"seq\":1,\"entry_hash\":\"x\"}  ",
                r#"{"seq":1}"#,
            ),
            (
                "nested same-named field is left alone",
                r#"{"sample":{"entry_hash":"keep"},"entry_hash":"x"}"#,
                r#"{"sample":{"entry_hash":"keep"}}"#,
            ),
            (
                "escaped quotes inside values",
                r#"{"a":"x\"y","entry_hash":"h","b":"z"}"#,
                r#"{"a":"x\"y","b":"z"}"#,
            ),
        ];
        for (name, input, want) in cases {
            let (got, found) = remove_top_level_field(input.as_bytes(), "entry_hash")
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(found, "{name}: field not found");
            assert_eq!(String::from_utf8(got).unwrap(), want, "{name}: wrong removal");
        }
    }

    #[test]
    fn remove_top_level_field_reports_absent_and_malformed_input() {
        // Go reports "not found" (and no error) when the object stops early or
        // simply has no such member.
        for (name, input) in [
            ("no field", &br#"{"seq":1}"#[..]),
            ("truncated after a member", &br#"{"seq":1"#[..]),
        ] {
            let (out, found) = remove_top_level_field(input, "entry_hash")
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(!found, "{name}: unexpectedly found the field");
            assert!(out.is_empty(), "{name}: expected no output");
        }

        // The remaining failures have fixed messages; the strings are the Go
        // implementation's own (see the reference probe).
        let cases: [(&str, &[u8], &str); 5] = [
            (
                "empty input",
                b"",
                "audit: record is not a JSON object",
            ),
            (
                "not an object",
                b"[1,2,3]",
                "audit: record is not a JSON object",
            ),
            (
                "truncated key",
                br#"{"seq"#,
                "audit: unterminated key at offset 1",
            ),
            (
                "missing colon",
                br#"{"seq" 1}"#,
                "audit: expected ':' after key \"seq\"",
            ),
            (
                "truncated value",
                br#"{"seq":"#,
                "audit: unterminated value for key \"seq\"",
            ),
        ];
        for (name, input, want) in cases {
            let err = remove_top_level_field(input, "entry_hash")
                .err()
                .unwrap_or_else(|| panic!("{name}: expected an error"));
            assert_eq!(err.to_string(), want, "{name}: wrong message");
        }

        // A record without the field yields the specific Go error.
        let err = hash_payload(br#"{"seq":1}"#).unwrap_err();
        assert_eq!(err.to_string(), "audit: record has no entry_hash field");
    }

    #[test]
    fn scan_helpers_agree_with_go() {
        // Escaped quotes inside a string do not end it.
        let s = br#" "a\"b" rest"#;
        assert_eq!(scan_json_string(s, 1), Some(7));

        // Nested objects/arrays must be skipped whole.
        let v = br#"{"a":[1,{"b":"}"}]},"next":"#;
        assert_eq!(scan_json_value(v, 0), Some(19));

        // Numeric literals end at the delimiter.
        assert_eq!(scan_json_value(b"12.5}", 0), Some(4));
        assert_eq!(scan_json_value(b"true", 0), Some(4));
        assert_eq!(scan_json_value(b"", 0), None);
    }

    #[test]
    fn compute_hash_rejects_a_preset_entry_hash() {
        let mut rec = sample_record("r");
        rec.entry_hash = "deadbeef".to_string();
        let err = compute_hash(&rec).unwrap_err();
        assert_eq!(
            err.to_string(),
            "audit: entry hash must be computed before it is set"
        );
    }

    #[test]
    fn append_rejects_a_record_whose_hash_is_already_set() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path,
            ..Default::default()
        })
        .expect("open");
        let mut rec = sample_record("r");
        rec.entry_hash = "deadbeef".to_string();
        let err = w.append(&mut rec).unwrap_err();
        assert_eq!(
            err.to_string(),
            "audit: hash record: audit: entry hash must be computed before it is set"
        );
    }

    #[test]
    fn append_rejects_records_for_a_closed_writer() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path,
            ..Default::default()
        })
        .expect("open");
        w.close().expect("close");
        let mut rec = sample_record("r");
        assert!(w.append(&mut rec).is_err());
    }

    #[test]
    fn blank_lines_are_skipped() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        for _ in 0..2 {
            let mut rec = sample_record("r");
            w.append(&mut rec).expect("append");
        }
        w.close().expect("close");

        let text = fs::read_to_string(&path).expect("read");
        fs::write(&path, format!("\n{text}\n")).expect("write");
        let res = verify(&path, "").expect("verify");
        assert!(res.ok, "blank lines broke verification: {:?}", res.violations);
        assert_eq!(res.records, 2);
        assert_eq!(read_all(&path).expect("read all").len(), 2);
    }

    #[test]
    fn crlf_lines_hash_the_same_bytes() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        for _ in 0..2 {
            let mut rec = sample_record("r");
            w.append(&mut rec).expect("append");
        }
        w.close().expect("close");

        // Rewriting the ledger with CRLF endings must still verify, because the
        // scanner drops the carriage return before hashing (as Go's does).
        let text = fs::read_to_string(&path).expect("read");
        let crlf: String = text.replace('\n', "\r\n");
        fs::write(&path, crlf).expect("write");
        let res = verify(&path, "").expect("verify");
        assert!(res.ok, "CRLF endings broke verification: {:?}", res.violations);
    }

    /// A ledger whose last record has no trailing newline still verifies: Go's
    /// scanner yields the final token, and a torn write must not be mistaken for
    /// tampering.
    #[test]
    fn last_line_without_newline_still_verifies() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        for _ in 0..2 {
            let mut rec = sample_record("r");
            w.append(&mut rec).expect("append");
        }
        w.close().expect("close");

        let text = fs::read_to_string(&path).expect("read");
        fs::write(&path, text.trim_end_matches('\n')).expect("write");
        let res = verify(&path, "").expect("verify");
        assert!(res.ok, "a missing final newline broke verification: {:?}", res.violations);
        assert_eq!(res.records, 2);
    }

    #[test]
    fn verify_sorts_violations_by_line() {
        let dir = TempDir::new();
        let (path, _) = temp_ledger(&dir);
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        for _ in 0..3 {
            let mut rec = sample_record("r");
            w.append(&mut rec).expect("append");
        }
        w.close().expect("close");

        // Both an anchor violation (line 0) and hash violations are produced;
        // the result must be ordered by line number.
        let anchor_path = dir.join("anchor.jsonl").to_string_lossy().into_owned();
        append_anchor(
            &anchor_path,
            &Anchor {
                seq: 3,
                hash: "0".repeat(64),
                time: Utc::now(),
                ..Default::default()
            },
        )
        .expect("append anchor");
        let mut lines = read_lines(&path);
        let mut rec: Record = serde_json::from_str(&lines[2]).expect("unmarshal");
        rec.actor = "mallory".to_string();
        lines[2] = serde_json::to_string(&rec).expect("marshal");
        write_lines(&path, &lines);

        let res = verify(&path, &anchor_path).expect("verify");
        assert!(!res.ok);
        let lines_seen: Vec<i64> = res.violations.iter().map(|v| v.line).collect();
        let mut sorted = lines_seen.clone();
        sorted.sort_unstable();
        assert_eq!(lines_seen, sorted, "violations are not ordered by line");
    }

    #[test]
    fn anchor_round_trips_through_json() {
        let dir = TempDir::new();
        let path = dir.join("anchor.jsonl").to_string_lossy().into_owned();
        let a = Anchor {
            seq: 7,
            hash: "abc".to_string(),
            time: Utc::now().trunc_subsecs(0),
            records: 7,
            component: "customsd".to_string(),
        };
        append_anchor(&path, &a).expect("append");
        let read = read_anchors(&path).expect("read");
        assert_eq!(read, vec![a]);
    }

    /// Pins the wire shape of a fully populated record against the Go
    /// implementation: field names, field order and `omitempty` semantics must
    /// not drift, or a ledger written by one binary stops being readable by the
    /// other's tooling.
    ///
    /// The expected text is Go's `json.Marshal` output for the same record, with
    /// one deliberate difference: Go prints integral floats as `4` where
    /// `serde_json` prints `4.0`. Both are the same JSON number, and the chain
    /// hash covers each writer's own bytes, so the two ledgers remain mutually
    /// verifiable.
    #[test]
    fn record_json_matches_the_go_field_order() {
        let r = Record {
            seq: 7,
            prev_hash: "prev".to_string(),
            entry_hash: "hash".to_string(),
            id: "req_1".to_string(),
            time: chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 3, 1, 10, 0, 0).unwrap(),
            end_time: chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 3, 1, 10, 0, 1).unwrap(),
            actor: "alice".to_string(),
            channel: "upstream-a".to_string(),
            provider: "prov".to_string(),
            key: "k1".to_string(),
            model: "gpt-4o".to_string(),
            upstream_model: "gpt-4o-2024".to_string(),
            protocol: "openai".to_string(),
            upstream_protocol: "anthropic".to_string(),
            translated: true,
            server_state: true,
            stream: true,
            endpoint: "/v1/chat/completions".to_string(),
            client_ip: "10.0.0.1".to_string(),
            decision: DECISION_REPORT.to_string(),
            status_code: 200,
            error: "boom".to_string(),
            outbound_findings: vec![FindingRef {
                rule: "r1".to_string(),
                severity: "high".to_string(),
                field: "messages".to_string(),
                excerpt_hash: "eh".to_string(),
                trust_exempt: true,
            }],
            inbound_findings: vec![FindingRef {
                rule: "r2".to_string(),
                severity: "low".to_string(),
                ..Default::default()
            }],
            usage: UsageSummary {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                upstream_prompt_tokens: 101,
                upstream_completion_tokens: 51,
                upstream_total_tokens: 152,
                upstream_reported: true,
                tokenizer_family: "cl100k".to_string(),
                method: "upstream".to_string(),
                prompt_delta: 0.01,
                completion_delta: 0.02,
                verdict: "over_billed".to_string(),
                tolerance: 0.05,
                min_absolute_delta: 3,
                worst_abs: 4,
            },
            cost: Cost {
                currency: "CNY".to_string(),
                input_cost: 1.5,
                output_cost: 2.5,
                total_cost: 4.0,
                list_cost: 5.0,
                discount_pct: 20.0,
                basis: "catalogue".to_string(),
                price_model: "pm1".to_string(),
            },
            fingerprint: Fingerprint {
                checked: true,
                score: 0.125,
                threshold: 0.3,
                verdict: "match".to_string(),
                baseline_source: "sample".to_string(),
                sample_hash: "sh".to_string(),
            },
            latency_ms: Latency {
                total_ms: 120,
                outbound_audit_ms: 1,
                upstream_ms: 100,
                inbound_audit_ms: 2,
                ttfb_ms: 40,
            },
            request_hash: "rh".to_string(),
            response_hash: "sh2".to_string(),
            route_reason: "cheapest".to_string(),
            attempts: 2,
            failover_note: "switched".to_string(),
            quota_after: 42,
            sample: Some(Sample {
                request_text: "req".to_string(),
                response_text: "res".to_string(),
                note: "why".to_string(),
            }),
        };
        let want = concat!(
            "{\"seq\":7,\"prev_hash\":\"prev\",\"entry_hash\":\"hash\",\"id\":\"req_1\",\"time\":\"2026-03-01T10:00:00Z\",",
            "\"end_time\":\"2026-03-01T10:00:01Z\",\"actor\":\"alice\",\"channel\":\"upstream-a\",\"provider\":\"prov\",",
            "\"key\":\"k1\",\"model\":\"gpt-4o\",\"upstream_model\":\"gpt-4o-2024\",\"protocol\":\"openai\",",
            "\"upstream_protocol\":\"anthropic\",\"translated\":true,\"server_state\":true,\"stream\":true,",
            "\"endpoint\":\"/v1/chat/completions\",\"client_ip\":\"10.0.0.1\",\"decision\":\"report\",",
            "\"status_code\":200,\"error\":\"boom\",\"outbound_findings\":[{\"rule\":\"r1\",\"severity\":\"high\",",
            "\"field\":\"messages\",\"excerpt_hash\":\"eh\",\"trust_exempt\":true}],",
            "\"inbound_findings\":[{\"rule\":\"r2\",\"severity\":\"low\"}],\"usage\":{\"prompt_tokens\":100,",
            "\"completion_tokens\":50,\"total_tokens\":150,\"upstream_prompt_tokens\":101,",
            "\"upstream_completion_tokens\":51,\"upstream_total_tokens\":152,\"upstream_reported\":true,",
            "\"tokenizer_family\":\"cl100k\",\"method\":\"upstream\",\"prompt_delta\":0.01,\"completion_delta\":0.02,",
            "\"verdict\":\"over_billed\",\"tolerance\":0.05,\"min_absolute_delta\":3,\"worst_abs\":4},",
            "\"cost\":{\"currency\":\"CNY\",\"input_cost\":1.5,\"output_cost\":2.5,\"total_cost\":4.0,",
            "\"list_cost\":5.0,\"discount_pct\":20.0,\"basis\":\"catalogue\",\"price_model\":\"pm1\"},",
            "\"fingerprint\":{\"checked\":true,\"score\":0.125,\"threshold\":0.3,\"verdict\":\"match\",",
            "\"baseline_source\":\"sample\",\"sample_hash\":\"sh\"},\"latency_ms\":{\"total_ms\":120,",
            "\"outbound_audit_ms\":1,\"upstream_ms\":100,\"inbound_audit_ms\":2,\"ttfb_ms\":40},",
            "\"request_hash\":\"rh\",\"response_hash\":\"sh2\",\"route_reason\":\"cheapest\",\"attempts\":2,",
            "\"failover_note\":\"switched\",\"quota_after\":42,\"sample\":{\"request_text\":\"req\",",
            "\"response_text\":\"res\",\"note\":\"why\"}}"
        );
        assert_eq!(serde_json::to_string(&r).expect("marshal"), want);
    }
    /// A ledger whose sequence numbers sit at the integer extremes must be
    /// reported, not panic: Go's arithmetic wraps, and a crash on a hand-edited
    /// ledger would take the gateway down instead of rejecting it.
    #[test]
    fn extreme_sequence_numbers_do_not_overflow() {
        let dir = TempDir::new();
        let path = dir.join("max.jsonl").to_string_lossy().into_owned();
        let mut prev = GENESIS_HASH.to_string();
        let mut lines = Vec::new();
        for seq in [i64::MAX, i64::MIN] {
            let payload = format!(
                r#"{{"seq":{seq},"prev_hash":"{prev}","id":"x","time":"2026-01-01T00:00:00Z","decision":"allow","usage":{{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2,"upstream_reported":true,"verdict":"consistent"}}}}"#
            );
            let hash = sha256_hex(payload.as_bytes());
            lines.push(format!(
                "{},\"entry_hash\":\"{hash}\"}}",
                &payload[..payload.len() - 1]
            ));
            prev = hash;
        }
        fs::write(&path, lines.join("\n") + "\n").expect("write");

        let res = verify(&path, "").expect("verify must not panic on extreme seq");
        assert_eq!(res.records, 2);
        // The first record cannot be seq 1, so exactly one gap is reported; the
        // second one continues from the wrapped value and must not add another.
        let gaps = res
            .violations
            .iter()
            .filter(|v| v.kind == VIOLATION_SEQ_GAP)
            .count();
        assert_eq!(gaps, 1, "violations: {:?}", res.violations);
    }

    // --- helpers ---------------------------------------------------------

    fn read_lines(path: &str) -> Vec<String> {
        let text = fs::read_to_string(path).expect("read");
        let trimmed = text.trim_end_matches('\n');
        if trimmed.is_empty() {
            return Vec::new();
        }
        trimmed.split('\n').map(|s| s.to_string()).collect()
    }

    fn write_lines(path: &str, lines: &[String]) {
        fs::write(path, lines.join("\n") + "\n").expect("write");
    }

    fn has_violation(res: &VerifyResult, kind: &str) -> bool {
        res.violations.iter().any(|v| v.kind == kind)
    }
}


