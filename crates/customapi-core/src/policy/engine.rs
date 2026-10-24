//! The rule engine: compilation, decisions, and excerpt hashing.
//!
//! Design notes (from the Go package doc):
//!
//! - Rules are pure functions over extracted text. Nothing here performs I/O,
//!   which makes every rule trivially unit-testable and keeps the gateway's hot
//!   path free of hidden latency.
//! - Findings carry a SHA-256 prefix of the matched excerpt instead of the
//!   excerpt itself. Audit records therefore prove that *something* matched
//!   without persisting potentially illegal or secret content.
//! - Enforcement mode is per-direction ("enforce" or "report") plus two trust
//!   levels, so a trusted channel or user can be inspected without being
//!   blocked. Metering and audit persistence are never relaxed by trust.

use std::collections::HashSet;

use regex::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::rules::{default_inbound_rules, default_outbound_rules};
use crate::config;

/// Identifies which side of the gateway a rule protects.
pub type Direction = String;

/// Outbound rules run on the user's request before it reaches an upstream.
pub const OUTBOUND: &str = "outbound";
/// Inbound rules run on the upstream's response before it reaches a user.
pub const INBOUND: &str = "inbound";

/// Ranks a finding.
pub type Severity = String;

/// Informational.
pub const SEV_LOW: &str = "low";
/// Warrants recording and possibly throttling.
pub const SEV_MEDIUM: &str = "medium";
/// Should be blocked under normal policy.
pub const SEV_HIGH: &str = "high";
/// An unambiguous attack or credential leak.
pub const SEV_CRITICAL: &str = "critical";

/// The decision the engine reached.
pub type Action = String;

/// Forwards the exchange unchanged.
pub const ACTION_ALLOW: &str = "allow";
/// Forwards the exchange but records findings.
pub const ACTION_REPORT: &str = "report";
/// Rejects the exchange.
pub const ACTION_BLOCK: &str = "block";

/// The built-in salt used when none is configured.
pub const DEFAULT_HASH_SALT: &str = "customapi-policy";

/// The number of findings a decision keeps.
///
/// Go stores this on the Engine as `maxFindings` but `dedupeFindings` hardcodes
/// 64; the port keeps the effective value in one place instead of carrying a
/// field that can disagree with the code.
pub const MAX_FINDINGS: usize = 64;

/// One rule violation.
///
/// A finding never contains the offending text. It records which rule fired,
/// where, how bad it is, and a hash that lets an operator confirm a specific
/// input later without the audit log itself holding the payload.
///
/// The field set matches `audit::FindingRef` apart from `direction`, `match_len`
/// and `detail`, which the gateway drops when it converts a finding for the
/// ledger.
// A struct-level `default` keeps decoding as tolerant as Go's: audit records are
// read back by operators' tooling, and a missing field must not fail the load.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Finding {
    pub rule: String,
    pub direction: Direction,
    pub severity: Severity,
    /// Field names where the match occurred, e.g. "messages[2].content".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub field: String,
    /// The first 16 hex chars of SHA-256 over the salted matched text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub excerpt_hash: String,
    /// The byte length of the match.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub match_len: i64,
    /// A human-readable, non-sensitive explanation.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// Records that the finding would have blocked, but the subject is on the
    /// trusted allowlist.
    #[serde(default, skip_serializing_if = "crate::json::is_false")]
    pub trust_exempt: bool,
}

/// The outcome of evaluating a payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Decision {
    pub action: Action,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    /// Exposes engine cost for tuning.
    pub rules_evaluated: i64,
    /// Exposes engine cost for tuning.
    pub elapsed_ns: i64,
    /// Reports that the engine itself failed and the configured
    /// fail-open/fail-closed policy was applied.
    #[serde(default, skip_serializing_if = "crate::json::is_false")]
    pub degraded: bool,
    /// Explains a degraded decision.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub degraded_reason: String,
}

impl Default for Decision {
    fn default() -> Self {
        Decision {
            action: ACTION_ALLOW.to_string(),
            findings: Vec::new(),
            rules_evaluated: 0,
            elapsed_ns: 0,
            degraded: false,
            degraded_reason: String::new(),
        }
    }
}

impl Decision {
    /// Reports whether the decision stops the exchange.
    pub fn blocked(&self) -> bool {
        self.action == ACTION_BLOCK
    }

    /// Returns the most severe finding, if any.
    pub fn highest(&self) -> Option<Finding> {
        let mut best = self.findings.first()?.clone();
        for f in &self.findings[1..] {
            if severity_rank(&f.severity) > severity_rank(&best.severity) {
                best = f.clone();
            }
        }
        Some(best)
    }
}

/// Ranks a severity for ordering; unknown severities rank lowest.
pub fn severity_rank(severity: &str) -> i32 {
    match severity {
        SEV_CRITICAL => 4,
        SEV_HIGH => 3,
        SEV_MEDIUM => 2,
        _ => 1,
    }
}

/// A compiled inspection rule.
///
/// A rule matches either by regexp, by substring keywords, or by exact
/// identifier (tool names). Exactly one of the three mechanisms is populated;
/// `exact` is `Some` only for identifier rules, mirroring Go's use of a non-nil
/// slice to mark them.
#[derive(Clone, Debug, Default)]
pub(super) struct Rule {
    pub(super) id: String,
    pub(super) direction: Direction,
    pub(super) severity: Severity,
    /// The payload surface the rule protects ("request", "response",
    /// "tool_call", "tool_arguments").
    ///
    /// Go keeps this on every rule but never reads it - findings take their
    /// `field` from the labelled text that matched - so it is documentation
    /// here too.
    #[allow(dead_code)]
    pub(super) field: String,
    pub(super) detail: String,
    pub(super) re: Option<Regex>,
    /// Lowercase substrings matched case-insensitively.
    pub(super) keywords: Vec<String>,
    /// Matches a whole identifier case-sensitively (tool names).
    pub(super) exact: Option<Vec<String>>,
}

impl Rule {
    /// Matches the text and returns the matched bytes.
    ///
    /// Bytes rather than `&str` because the gateway audits raw stream deltas: a
    /// chunk boundary can split a rune, and Go's regexp matches the resulting
    /// invalid sequence instead of refusing it.
    pub(super) fn match_bytes(&self, bytes: &[u8]) -> Option<Vec<u8>> {
        if let Some(re) = &self.re {
            let m = re.find(bytes)?;
            return Some(bytes[m.start()..m.end()].to_vec());
        }
        if !self.keywords.is_empty() {
            let text = String::from_utf8_lossy(bytes).into_owned();
            let lower = text.to_lowercase();
            for kw in &self.keywords {
                if let Some((start, end)) = keyword_range(&text, &lower, kw) {
                    return Some(text.as_bytes()[start..end].to_vec());
                }
            }
        }
        None
    }

    /// Convenience wrapper for callers that already hold a string.
    pub(super) fn match_text(&self, text: &str) -> Option<Vec<u8>> {
        self.match_bytes(text.as_bytes())
    }

    /// Reports whether an exact identifier rule fires.
    pub(super) fn matches_ident(&self, name: &str) -> bool {
        match &self.exact {
            Some(list) => list.iter().any(|e| e == name),
            None => false,
        }
    }
}

/// Returns the byte range of `keyword` inside `text`, using the pre-lowered
/// `lower` text for the search.
///
/// Go searches `strings.ToLower(text)` and then slices the *original* string with
/// the indices found there. That is exactly right while lowercasing preserves
/// byte lengths (the overwhelmingly common case, and the fast path here), and
/// produces an offset that may land inside a rune otherwise. Rust must not slice
/// off a character boundary, so the slow path maps the match through character
/// positions instead of trusting the byte offset; the excerpt can only differ for
/// text whose lowercased form changes byte lengths mid-string.
fn keyword_range(text: &str, lower: &str, keyword: &str) -> Option<(usize, usize)> {
    let start = lower.find(keyword)?;
    let end = start + keyword.len();
    if lower.len() == text.len() && text.is_char_boundary(start) && text.is_char_boundary(end) {
        return Some((start, end.min(text.len())));
    }
    let start_char = lower[..start].chars().count();
    let matched_chars = lower[start..end].chars().count();
    let start_byte = byte_index_of_char(text, start_char).unwrap_or(text.len());
    let end_byte = byte_index_of_char(text, start_char + matched_chars).unwrap_or(text.len());
    Some((start_byte, end_byte.max(start_byte)))
}

/// Returns the byte offset of the `n`th character.
fn byte_index_of_char(text: &str, n: usize) -> Option<usize> {
    if n == 0 {
        return Some(0);
    }
    text.char_indices().nth(n).map(|(b, _)| b)
}

/// Compiles several expressions into one alternation.
///
/// Blank patterns are skipped; when every pattern is blank the result is `None`,
/// which is what Go's `compileAny` returns as a nil `*regexp.Regexp`.
pub(super) fn compile_any(patterns: &[String]) -> Result<Option<Regex>, regex::Error> {
    let mut parts: Vec<String> = Vec::new();
    for p in patterns {
        if p.trim().is_empty() {
            continue;
        }
        parts.push(format!("(?:{p})"));
    }
    if parts.is_empty() {
        return Ok(None);
    }
    compile_joined(&parts.join("|")).map(Some)
}

/// Compiles one pattern with Go-compatible class semantics.
///
/// Go's `regexp` treats `\b`, `\w`, `\d`, `\s` as ASCII classes while still
/// supporting Unicode property classes; the `regex` crate is the other way round
/// (Unicode classes by default). The built-in rules depend on Go's ASCII word
/// boundaries - "中文curl ... | sh" is caught by Go - so ASCII mode over bytes is
/// tried first, and Unicode mode is only used for patterns that need `\p{...}`.
fn compile_joined(pattern: &str) -> Result<Regex, regex::Error> {
    match RegexBuilder::new(pattern).unicode(false).build() {
        Ok(re) => Ok(re),
        Err(err) => {
            if pattern.contains("\\p") || pattern.contains("\\P") {
                RegexBuilder::new(pattern).build()
            } else {
                Err(err)
            }
        }
    }
}

/// Compiles a pattern that must be valid (the built-in rule table).
pub(super) fn must_compile(pattern: &str) -> Regex {
    match RegexBuilder::new(pattern).unicode(false).build() {
        Ok(re) => re,
        Err(err) => panic!("policy: built-in rule does not compile: {err}"),
    }
}

/// The errors engine construction can report.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// `policy: outbound.block_patterns: <cause>`
    #[error("policy: outbound.block_patterns: {0}")]
    OutboundBlockPatterns(String),
    /// `policy: outbound.deny_path_patterns: <cause>`
    #[error("policy: outbound.deny_path_patterns: {0}")]
    OutboundDenyPathPatterns(String),
    /// `policy: inbound.injection_patterns: <cause>`
    #[error("policy: inbound.injection_patterns: {0}")]
    InboundInjectionPatterns(String),
    /// `policy: inbound.secret_patterns: <cause>`
    #[error("policy: inbound.secret_patterns: {0}")]
    InboundSecretPatterns(String),
    /// `policy: redact patterns: <cause>`
    #[error("policy: redact patterns: {0}")]
    RedactPatterns(String),
}

/// Tunes engine construction.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The operator configuration.
    pub policy: config::Policy,
    /// "open" or "closed" when the engine itself fails.
    pub fail_mode: String,
    /// Mixed into excerpt hashes.
    pub hash_salt: String,
    /// Skips the built-in rule set, so only operator rules apply. Used by tests
    /// that need an exactly specified rule set.
    pub disable_defaults: bool,
}

/// Evaluates outbound and inbound rules.
///
/// An `Engine` is immutable after construction and safe for concurrent use; the
/// detection methods take `&self`.
#[derive(Clone, Debug)]
pub struct Engine {
    pub(super) outbound: Vec<Rule>,
    pub(super) inbound: Vec<Rule>,
    out_mode: String,
    in_mode: String,
    pub(super) tool_policy: String,
    fail_mode: String,
    /// When non-empty, switches outbound tool checking to allowlist mode.
    pub(super) allow_tools: Vec<String>,
    /// Prevents trivial recovery of hashed excerpts from a leaked audit log.
    excerpt_hash_salt: String,
}

impl Engine {
    /// Compiles an engine from configuration plus the built-in rule set.
    pub fn new(opts: Options) -> Result<Engine, PolicyError> {
        let mut e = Engine {
            outbound: Vec::new(),
            inbound: Vec::new(),
            out_mode: or_default(&opts.policy.outbound.mode, "enforce"),
            in_mode: or_default(&opts.policy.inbound.mode, "enforce"),
            tool_policy: or_default(&opts.policy.inbound.tool_call_policy, "block"),
            fail_mode: or_default(&opts.fail_mode, "open"),
            allow_tools: Vec::new(),
            excerpt_hash_salt: if opts.hash_salt.is_empty() {
                DEFAULT_HASH_SALT.to_string()
            } else {
                opts.hash_salt.clone()
            },
        };
        if !opts.disable_defaults {
            e.outbound.extend(default_outbound_rules());
            e.inbound.extend(default_inbound_rules());
        }

        let ob = &opts.policy.outbound;
        if !ob.block_keywords.is_empty() {
            e.outbound.push(Rule {
                id: "user.block_keyword".to_string(),
                direction: OUTBOUND.to_string(),
                severity: SEV_HIGH.to_string(),
                field: "request".to_string(),
                detail: "request contains an operator-blocked keyword".to_string(),
                keywords: lower_all(&ob.block_keywords),
                ..Rule::default()
            });
        }
        if !ob.block_patterns.is_empty() {
            let re = compile_any(&ob.block_patterns)
                .map_err(|err| PolicyError::OutboundBlockPatterns(err.to_string()))?;
            e.outbound.push(Rule {
                id: "user.block_pattern".to_string(),
                direction: OUTBOUND.to_string(),
                severity: SEV_HIGH.to_string(),
                field: "request".to_string(),
                detail: "request matches an operator-blocked pattern".to_string(),
                re,
                ..Rule::default()
            });
        }
        if !ob.deny_tools.is_empty() {
            e.outbound.push(Rule {
                id: "user.deny_tool".to_string(),
                direction: OUTBOUND.to_string(),
                severity: SEV_HIGH.to_string(),
                field: "tool_call".to_string(),
                detail: "tool/function name is on the operator deny list".to_string(),
                exact: Some(ob.deny_tools.clone()),
                ..Rule::default()
            });
        }
        if !ob.allow_tools.is_empty() {
            e.allow_tools = ob.allow_tools.clone();
        }
        if !ob.deny_path_patterns.is_empty() {
            let re = compile_any(&ob.deny_path_patterns)
                .map_err(|err| PolicyError::OutboundDenyPathPatterns(err.to_string()))?;
            e.outbound.push(Rule {
                id: "user.deny_path".to_string(),
                direction: OUTBOUND.to_string(),
                severity: SEV_HIGH.to_string(),
                field: "tool_arguments".to_string(),
                detail: "tool arguments reference a protected path or dangerous command"
                    .to_string(),
                re,
                ..Rule::default()
            });
        }

        let ib = &opts.policy.inbound;
        if !ib.injection_patterns.is_empty() {
            let re = compile_any(&ib.injection_patterns)
                .map_err(|err| PolicyError::InboundInjectionPatterns(err.to_string()))?;
            e.inbound.push(Rule {
                id: "user.injection".to_string(),
                direction: INBOUND.to_string(),
                severity: SEV_HIGH.to_string(),
                field: "response".to_string(),
                detail: "response matches an operator-supplied injection pattern".to_string(),
                re,
                ..Rule::default()
            });
        }
        if !ib.secret_patterns.is_empty() {
            let re = compile_any(&ib.secret_patterns)
                .map_err(|err| PolicyError::InboundSecretPatterns(err.to_string()))?;
            e.inbound.push(Rule {
                id: "user.secret".to_string(),
                direction: INBOUND.to_string(),
                severity: SEV_CRITICAL.to_string(),
                field: "response".to_string(),
                detail: "response matches an operator-supplied secret pattern".to_string(),
                re,
                ..Rule::default()
            });
        }
        Ok(e)
    }

    /// Reports how many outbound rules are compiled.
    pub fn outbound_rule_count(&self) -> usize {
        self.outbound.len()
    }

    /// Reports how many inbound rules are compiled.
    pub fn inbound_rule_count(&self) -> usize {
        self.inbound.len()
    }

    /// Returns the configured degradation behaviour.
    pub fn fail_mode(&self) -> &str {
        &self.fail_mode
    }

    /// Returns the configured outbound enforcement mode.
    pub fn outbound_mode(&self) -> &str {
        &self.out_mode
    }

    /// Returns the configured inbound enforcement mode.
    pub fn inbound_mode(&self) -> &str {
        &self.in_mode
    }

    /// Returns the configured tool-call policy.
    pub fn tool_call_policy(&self) -> &str {
        &self.tool_policy
    }

    /// Returns the operator allowlist (empty unless allowlist mode is on).
    pub fn allow_tools(&self) -> &[String] {
        &self.allow_tools
    }

    /// Returns the salt used for excerpt hashes.
    pub fn excerpt_hash_salt(&self) -> &str {
        &self.excerpt_hash_salt
    }

    /// Returns a short, salted digest of matched text.
    pub fn excerpt_hash(&self, s: &str) -> String {
        excerpt_hash_with(&self.excerpt_hash_salt, s.as_bytes())
    }

    /// Builds a finding for a matched rule from the matched bytes.
    pub(super) fn finding(&self, rule: &Rule, field: &str, matched: &[u8]) -> Finding {
        Finding {
            rule: rule.id.clone(),
            direction: rule.direction.clone(),
            severity: rule.severity.clone(),
            field: field.to_string(),
            detail: rule.detail.clone(),
            excerpt_hash: excerpt_hash_with(&self.excerpt_hash_salt, matched),
            match_len: matched.len() as i64,
            trust_exempt: false,
        }
    }
}

/// Returns a short, salted digest of matched text: the first 16 hex characters
/// of `SHA-256(salt || text)`.
pub(super) fn excerpt_hash_with(salt: &str, text: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(salt.as_bytes());
    h.update(text);
    let digest = h.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..16].to_string()
}

fn or_default(v: &str, def: &str) -> String {
    if v.is_empty() {
        def.to_string()
    } else {
        v.to_string()
    }
}

/// Lowercases operator keywords, dropping blank entries.
fn lower_all(input: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(input.len());
    for s in input {
        if !s.trim().is_empty() {
            out.push(s.to_lowercase());
        }
    }
    out
}

/// Turns findings into an action, honouring enforcement mode and the trust
/// exemption.
pub(super) fn build_decision(
    findings: Vec<Finding>,
    mode: &str,
    trusted: bool,
) -> Decision {
    let mut d = Decision {
        action: ACTION_ALLOW.to_string(),
        findings: dedupe_findings(findings),
        ..Decision::default()
    };
    if d.findings.is_empty() {
        return d;
    }
    // Go also computes the highest severity here and never reads it; the
    // ordering it would have informed already happens in `dedupe_findings`.
    if mode == "report" {
        d.action = ACTION_REPORT.to_string();
    } else if trusted {
        // Trusted subjects are still inspected and recorded, but a blocking
        // finding is downgraded to a report. Metering/audit never downgrade.
        d.action = ACTION_REPORT.to_string();
        for f in &mut d.findings {
            f.trust_exempt = true;
        }
    } else {
        d.action = ACTION_BLOCK.to_string();
    }
    d
}

/// Removes duplicates, sorts by severity and applies the cap.
pub(super) fn dedupe_findings(input: Vec<Finding>) -> Vec<Finding> {
    if input.is_empty() {
        return Vec::new();
    }
    let mut seen: HashSet<String> = HashSet::with_capacity(input.len());
    let mut out = Vec::with_capacity(input.len());
    for f in input {
        let key = format!("{}|{}|{}", f.rule, f.field, f.excerpt_hash);
        if !seen.insert(key) {
            continue;
        }
        out.push(f);
    }
    // `sort_by_key` is stable, matching Go's `sort.SliceStable`.
    out.sort_by_key(|f| std::cmp::Reverse(severity_rank(&f.severity)));
    out.truncate(MAX_FINDINGS);
    out
}
