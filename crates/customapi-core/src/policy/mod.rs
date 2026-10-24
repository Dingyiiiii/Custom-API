//! The customs "audit" layer: request-side rules that protect upstream access
//! points from abusive users, and response-side rules that protect users from a
//! hostile or compromised upstream.
//!
//! This module is a direct port of `internal/policy` in the Go implementation.
//! Rule IDs, severities, matching order, the dedupe key, the excerpt-hash
//! construction and the JSON field names are identical, so audit records written
//! by either implementation stay comparable.
//!
//! # Design notes
//!
//! - Rules are pure functions over extracted text. Nothing here performs I/O,
//!   which makes every rule trivially unit-testable and keeps the gateway's hot
//!   path free of hidden latency.
//! - Findings carry a SHA-256 prefix of the matched excerpt instead of the
//!   excerpt itself. Audit records therefore prove that *something* matched
//!   without persisting potentially illegal or secret content.
//! - Enforcement mode is per-direction ("enforce" or "report") plus two trust
//!   levels, so a trusted channel or user can be inspected without being blocked.
//!   Metering and audit persistence are never relaxed by trust.

mod engine;
mod evaluate;
mod rules;

pub use engine::{
    severity_rank, Action, Decision, Direction, Engine, Finding, Options, PolicyError, Severity,
    ACTION_ALLOW, ACTION_BLOCK, ACTION_REPORT, DEFAULT_HASH_SALT, INBOUND, MAX_FINDINGS, OUTBOUND,
    SEV_CRITICAL, SEV_HIGH, SEV_LOW, SEV_MEDIUM,
};
pub use evaluate::{
    inspect_request_from_anthropic, inspect_request_from_openai, inspect_request_from_responses,
    inspect_response_from_anthropic, inspect_response_from_openai, inspect_response_from_responses,
    labelled, InspectRequest, InspectResponse, LabelledText, Redactor, StreamDetector,
    ToolCallRequest, MAX_SCAN_BYTES, REDACTED_MASK,
};

#[cfg(test)]
mod tests;
