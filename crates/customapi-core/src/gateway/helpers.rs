//! Small pieces the proxy pipeline keeps needing: the token-bucket rate
//! limiter, sampling, digests, header hygiene and redaction.
//!
//! Everything here is a direct port of `helpers.go` plus the header utilities
//! that Go keeps in `proxy.go`; the split exists only because Rust needs the
//! helpers in several modules.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::config::User;
use crate::policy::Redactor;
use crate::audit;

use super::proxy::ProxyContext;

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

/// A token-bucket rate limiter for one user.
///
/// The same object also carries the concurrency counter, because Go keeps both
/// in one per-user `limiter` and the two must agree about who the caller is.
#[derive(Debug, Default)]
pub struct Limiter {
    tokens: f64,
    last: Option<Instant>,
    rate: f64,
    burst: f64,
    /// Counts in-flight requests for the concurrency cap.
    pub(crate) inflight: i32,
}

/// A reserved concurrency slot, released when dropped.
///
/// Go returns a closure with a `sync.Once`; a guard is the same idea with the
/// compiler enforcing the release on every exit path, including a panic.
pub struct LimiterGuard {
    limiter: Arc<Mutex<Limiter>>,
}

impl Drop for LimiterGuard {
    fn drop(&mut self) {
        let mut l = self.limiter.lock().unwrap_or_else(|e| e.into_inner());
        l.inflight -= 1;
    }
}

impl super::Server {
    /// Returns (creating if needed) the limiter for a user name.
    fn limiter_for(&self, name: &str) -> Arc<Mutex<Limiter>> {
        let mut map = self.limiters.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(name.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(Limiter::default())))
            .clone()
    }

    /// Consumes one token from the user's bucket.
    pub(crate) fn allow(&self, user: &User) -> bool {
        let limiter = self.limiter_for(&user.name);
        let mut l = limiter.lock().unwrap_or_else(|e| e.into_inner());

        let rate = user.rate_limit;
        let mut burst = user.burst as f64;
        if burst <= 0.0 {
            burst = rate * 2.0;
            if burst < 1.0 {
                burst = 1.0;
            }
        }
        let now = Instant::now();
        if l.rate != rate || l.burst != burst {
            // Configuration changed; adopt it and refill.
            l.rate = rate;
            l.burst = burst;
            l.tokens = burst;
            l.last = Some(now);
        }
        let elapsed = match l.last {
            Some(last) => now.saturating_duration_since(last).as_secs_f64(),
            None => {
                // First request from this user: start with a full bucket.
                l.tokens = burst;
                0.0
            }
        };
        l.last = Some(now);
        consume(&mut l, elapsed)
    }

    /// Reserves a concurrency slot, returning a guard that releases it.
    pub(crate) fn acquire(&self, user: &str, max: i32) -> Option<LimiterGuard> {
        let limiter = self.limiter_for(user);
        let mut l = limiter.lock().unwrap_or_else(|e| e.into_inner());
        if l.inflight >= max {
            return None;
        }
        l.inflight += 1;
        drop(l);
        Some(LimiterGuard { limiter })
    }
}

/// Applies one token-bucket step.
fn consume(l: &mut Limiter, elapsed: f64) -> bool {
    l.tokens += elapsed * l.rate;
    if l.tokens > l.burst {
        l.tokens = l.burst;
    }
    if l.tokens < 1.0 {
        return false;
    }
    l.tokens -= 1.0;
    true
}

// ---------------------------------------------------------------------------
// Digests and identifiers
// ---------------------------------------------------------------------------

/// Returns the SHA-256 digest of `bytes`, hex encoded.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

/// Hex-encodes a byte slice the way Go's `hex.EncodeToString` does.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

/// Makes a sortable, unique audit identifier: `req_` followed by 24 hex digits.
///
/// The first eight bytes are the UTC nanosecond clock, so records sort by
/// creation time even across processes; the last four are random, so two
/// requests in the same nanosecond still differ.
pub(crate) fn new_request_id() -> String {
    let mut b = [0u8; 12];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    for (i, slot) in b.iter_mut().take(8).enumerate() {
        *slot = (now >> (8 * (7 - i))) as u8;
    }
    rand::rng().fill_bytes(&mut b[8..]);
    format!("req_{}", hex(&b))
}

/// Redacts secrets from a body before it is hashed or persisted.
pub(crate) fn redact_bytes(redactor: Option<&Redactor>, body: &[u8]) -> Vec<u8> {
    match redactor {
        Some(r) if !body.is_empty() => r.redact(&String::from_utf8_lossy(body)).into_bytes(),
        _ => body.to_vec(),
    }
}

/// Redacts a string, or returns it unchanged when no redactor is configured.
pub(crate) fn redact_string(redactor: Option<&Redactor>, text: &str) -> String {
    match redactor {
        Some(r) => r.redact(text),
        None => text.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Sampling
// ---------------------------------------------------------------------------

/// Bounds a redacted excerpt.
pub(crate) const MAX_SAMPLE_BYTES: usize = 2048;

impl super::Server {
    /// Reports whether this exchange should carry a redacted excerpt.
    ///
    /// Sampling exists so an operator can debug a policy rule without turning
    /// the audit ledger into a transcript store. It is limited to interesting
    /// exchanges: anything that produced a finding, a metering anomaly, or an
    /// error. A clean exchange is never sampled, which keeps the ledger small and
    /// preserves user privacy by default.
    pub(crate) fn sample_wanted(&self, pc: &ProxyContext) -> bool {
        if pc.decision == audit::DECISION_ALLOW && !pc.deviation.over_threshold {
            return false;
        }
        !pc.outbound_decision.findings.is_empty()
            || !pc.inbound_decision.findings.is_empty()
            || pc.deviation.over_threshold
            || pc.decision != audit::DECISION_ALLOW
    }

    /// Renders a redacted request/response excerpt.
    pub(crate) fn build_sample(&self, pc: &ProxyContext) -> audit::Sample {
        let mut note = "sampled for operator review".to_string();
        if let Some(f) = pc.inbound_decision.highest() {
            note = format!("inbound rule {}", f.rule);
        } else if let Some(f) = pc.outbound_decision.highest() {
            note = format!("outbound rule {}", f.rule);
        } else if pc.deviation.over_threshold {
            note = "usage deviation flagged".to_string();
        } else if pc.decision != audit::DECISION_ALLOW {
            note = pc.decision.clone();
        }
        audit::Sample {
            request_text: sample_excerpt(&pc.body, Some(self.redactor.as_ref())),
            // Filled by the streaming/buffered path when available.
            response_text: String::new(),
            note,
        }
    }
}

/// Truncates and redacts a body for storage.
pub(crate) fn sample_excerpt(body: &[u8], redactor: Option<&Redactor>) -> String {
    if body.is_empty() {
        return String::new();
    }
    let clipped = if body.len() > MAX_SAMPLE_BYTES {
        &body[..MAX_SAMPLE_BYTES]
    } else {
        body
    };
    redact_string(redactor, &String::from_utf8_lossy(clipped))
}

// ---------------------------------------------------------------------------
// Header hygiene
// ---------------------------------------------------------------------------

/// Hop-by-hop headers must not be forwarded by a proxy.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Reports whether a header is hop-by-hop.
pub(crate) fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP.contains(&name.as_str())
}

/// Reports whether a header carries a caller credential.
///
/// These are stripped from the client request because the gateway substitutes
/// the channel's own credential.
pub(crate) fn is_credential_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization" | "x-api-key" | "api-key" | "x-goog-api-key"
    )
}

/// Copies the upstream headers that matter to clients.
///
/// `content-length` is dropped: the gateway may rewrite the body (model rename,
/// translation, withheld response), and hyper must not inherit a length that
/// describes different bytes. The buffered path sets its own length afterwards.
pub(crate) fn copy_response_headers(dst: &mut HeaderMap, src: &HeaderMap) {
    for (name, value) in src.iter() {
        if is_hop_by_hop(name) || name.as_str() == "content-length" {
            continue;
        }
        let text = match value.to_str() {
            Ok(text) => super::sanitize_header_value(text),
            Err(_) => continue,
        };
        if let Ok(value) = HeaderValue::from_str(&text) {
            dst.append(name.clone(), value);
        }
    }
}

/// Reads a header as a UTF-8 string, treating a binary value as absent.
pub(crate) fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Returns the channel-pin header, trimmed.
pub(crate) fn pin_header_of(headers: &HeaderMap) -> String {
    header_str(headers, super::PIN_HEADER)
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

/// Renders a transport error without leaking query details.
///
/// Go unwraps `*url.Error` so the rendered text is `"Post failed: ..."`;
/// reqwest's error already carries the method and URL, so the URL's query string
/// is dropped to keep credentials out of the audit record.
pub(crate) fn upstream_err_text(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        return "upstream request timed out".to_string();
    }
    if err.is_connect() {
        return format!("upstream connect failed: {err}");
    }
    let mut text = err.to_string();
    if let Some(pos) = text.find('?') {
        text.truncate(pos);
    }
    text
}

/// Renders a failure for an upstream status code.
pub(crate) fn upstream_status_text(code: u16) -> String {
    format!("upstream returned HTTP {code}")
}

/// Compares two secrets without leaking their contents through timing.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Returns the short version stamped on upstream requests.
pub(crate) fn version_short() -> &'static str {
    "0.1"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_unique_and_ordered_by_time() {
        let first = new_request_id();
        let second = new_request_id();
        assert_ne!(first, second);
        assert_eq!(first.len(), 28);
        // The timestamp prefix is monotonic, so the ids sort by creation.
        assert!(first[4..20] <= second[4..20]);
    }

    #[test]
    fn sha256_matches_known_digest() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hop_by_hop_and_credentials_are_recognised() {
        for name in ["Connection", "TE", "Transfer-Encoding", "Upgrade"] {
            assert!(is_hop_by_hop(&HeaderName::from_bytes(name.as_bytes()).unwrap()));
        }
        for name in ["Authorization", "x-api-key", "API-Key", "x-goog-api-key"] {
            assert!(is_credential_header(
                &HeaderName::from_bytes(name.as_bytes()).unwrap()
            ));
        }
        assert!(!is_hop_by_hop(&HeaderName::from_static("content-type")));
    }

    #[test]
    fn constant_time_compare_requires_equal_bytes() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret2"));
        assert!(!constant_time_eq(b"", b"x"));
    }
}
