//! The gateway's own accounting for an exchange, and the anti-inflation check.

use serde::{Deserialize, Serialize};

use super::tokenizer::{Estimator, TokenizerFamily};
use crate::protocol::responses::{ResponsesRequest, ResponsesResponse};
use crate::protocol::types::{
    AnthropicRequest, AnthropicResponse, ChatRequest, ChatResponse, Usage as WireUsage,
};

/// The gateway's own accounting for one exchange.
///
/// Field order follows the Go struct so that a serialised record keeps the same
/// byte layout.
// A struct-level `default` keeps decoding as tolerant as Go's: a record written
// by an older build that lacks a field still loads, with the field at zero.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// The gateway's recount of the request side.
    pub prompt_tokens: i64,
    /// The gateway's recount of the response side.
    pub completion_tokens: i64,
    /// `PromptTokens + CompletionTokens`.
    pub total_tokens: i64,
    /// What the upstream claimed (0 when it reported nothing, e.g. some
    /// streaming responses).
    pub upstream_prompt_tokens: i64,
    /// What the upstream claimed.
    pub upstream_completion_tokens: i64,
    /// The upstream's total, when reported.
    pub upstream_total_tokens: i64,
    /// False when the upstream omitted usage entirely.
    pub upstream_reported: bool,
    /// Records how the completion count was obtained: `"upstream"`
    /// (non-streaming upstream usage), `"stream_chunks"` (accumulated from SSE
    /// deltas) or `"local_only"`.
    pub method: String,
    /// The tokenizer family used for the recount.
    pub family: TokenizerFamily,
}

/// Describes how far the upstream's claim is from the recount.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Deviation {
    /// Relative deviation of the prompt side, in `[-1, +inf)`.
    #[serde(default)]
    pub prompt_delta: f64,
    /// Relative deviation of the completion side, in `[-1, +inf)`.
    #[serde(default)]
    pub completion_delta: f64,
    /// The larger absolute deviation of the two.
    #[serde(default)]
    pub worst: f64,
    /// The larger absolute (token-count) deviation of the two.
    #[serde(default)]
    pub worst_abs: i64,
    /// The absolute floor that was applied.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub min_absolute: i64,
    /// Reports whether the deviation is both relatively beyond the tolerance and
    /// absolutely beyond the floor.
    #[serde(default)]
    pub over_threshold: bool,
    /// False when the upstream reported nothing to compare against, in which
    /// case `over_threshold` is always false.
    #[serde(default)]
    pub verifiable: bool,
    /// `"over"` when the upstream charged for more tokens than the gateway
    /// counted (inflation), `"under"` when it charged fewer.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub direction: String,
}

/// A relative deviation above which the value would stop being representable.
pub const MAX_FINITE_DELTA: f64 = 1e6;

/// Replaces infinities and NaNs with representable values.
pub fn clamp_finite(v: f64) -> f64 {
    if v.is_nan() {
        return 0.0;
    }
    if v.is_infinite() {
        return if v > 0.0 {
            MAX_FINITE_DELTA
        } else {
            -MAX_FINITE_DELTA
        };
    }
    v
}

/// An integer absolute value.
fn abs_int(v: i64) -> i64 {
    // `i64::MIN.abs()` overflows; Go's negation wraps too, so mirror that rather
    // than panicking in debug builds.
    v.wrapping_abs()
}

/// Returns `(claimed-actual)/actual`, guarding `actual == 0`.
pub fn relative_delta(claimed: i64, actual: i64) -> f64 {
    if actual <= 0 {
        if claimed <= 0 {
            return 0.0;
        }
        return f64::INFINITY;
    }
    (claimed - actual) as f64 / actual as f64
}

/// The default absolute floor, in tokens, below which a deviation is treated as
/// estimator noise rather than as evidence.
///
/// Why a floor is necessary: the recount is an estimate, and for a short
/// response a single token of disagreement is already a large *relative*
/// deviation. A 5-token reply claimed as 8 tokens is +60%, which a purely
/// relative check would call fraud. Without a floor a legitimate upstream gets
/// flagged on exactly the shortest, cheapest requests - the ones most likely to
/// be health checks and benchmark probes - and the integrity score becomes
/// noise-driven.
pub const MIN_ABSOLUTE_DELTA: i64 = 8;

/// The per-message framing cost real tokenizers add (role marker, separators).
pub const MESSAGE_OVERHEAD: i64 = 4;

impl Usage {
    /// Measures the upstream's claim against the gateway's recount.
    ///
    /// A deviation is only reported when it is BOTH beyond the relative
    /// tolerance and beyond the absolute floor, which is how "close enough"
    /// comparisons are normally implemented for quantities with a known
    /// measurement error.
    pub fn compare(&self, tolerance: f64) -> Deviation {
        self.compare_with(tolerance, MIN_ABSOLUTE_DELTA)
    }

    /// [`Usage::compare`] with an explicit absolute floor.
    pub fn compare_with(&self, tolerance: f64, min_absolute: i64) -> Deviation {
        let mut d = Deviation::default();
        if !self.upstream_reported {
            return d;
        }
        d.verifiable = true;
        d.min_absolute = min_absolute;
        // An infinite relative deviation means the upstream billed for something
        // the gateway saw no content for. It is clamped to a large finite number
        // so that the value survives JSON round-trips and downstream arithmetic;
        // an infinity here would make the audit ledger unwritable.
        d.prompt_delta = clamp_finite(relative_delta(
            self.upstream_prompt_tokens,
            self.prompt_tokens,
        ));
        d.completion_delta = clamp_finite(relative_delta(
            self.upstream_completion_tokens,
            self.completion_tokens,
        ));

        let mut worst = d.prompt_delta.abs();
        let completion_worst = d.completion_delta.abs();
        if completion_worst > worst {
            worst = completion_worst;
        }
        let prompt_abs = abs_int(self.upstream_prompt_tokens - self.prompt_tokens);
        let completion_abs = abs_int(self.upstream_completion_tokens - self.completion_tokens);
        let mut worst_abs = prompt_abs;
        if completion_abs > worst_abs {
            worst_abs = completion_abs;
        }
        d.worst = worst;
        d.worst_abs = worst_abs;
        // Two conditions, not one: a deviation is evidence only when it is large
        // in relative terms AND large in absolute terms. See MIN_ABSOLUTE_DELTA.
        d.over_threshold = worst > tolerance && worst_abs >= min_absolute;
        if d.over_threshold {
            if d.completion_delta > 0.0 || d.prompt_delta > 0.0 {
                d.direction = "over".to_string();
            } else {
                d.direction = "under".to_string();
            }
        }
        d
    }
}

/// Accumulates a [`Usage`] across a request/response pair.
///
/// It is used both for non-streaming responses (where the whole body is
/// available at once) and for SSE streams (where deltas arrive incrementally).
#[derive(Clone, Debug)]
pub struct Counter {
    est: Estimator,
    prompt: i64,
    comp: i64,
    method: String,
    upstream: WireUsage,
    has_upstream: bool,
    overhead: i64,
}

impl Counter {
    /// Creates a counter for a model.
    pub fn new(model: &str) -> Self {
        Counter {
            est: Estimator::for_model(model),
            prompt: 0,
            comp: 0,
            method: "local_only".to_string(),
            upstream: WireUsage::default(),
            has_upstream: false,
            overhead: 0,
        }
    }

    /// Returns the tokenizer family in use.
    pub fn family(&self) -> &TokenizerFamily {
        self.est.family()
    }

    /// Adds one piece of request text.
    pub fn add_prompt_text(&mut self, s: &str) {
        self.prompt += self.est.estimate(s);
    }

    /// Adds an explicit token count (e.g. upstream prompt count when it is the
    /// only signal available).
    pub fn add_prompt_tokens(&mut self, n: i64) {
        self.prompt += n;
    }

    /// Adds one piece of response text.
    pub fn add_completion_text(&mut self, s: &str) {
        self.comp += self.est.estimate(s);
    }

    /// Accounts for protocol scaffolding (role markers, message separators) that
    /// real tokenizers charge for.
    pub fn add_overhead(&mut self, n: i64) {
        self.overhead += n;
    }

    /// Records the usage block reported by the upstream.
    pub fn set_upstream(&mut self, u: WireUsage, present: bool) {
        self.upstream = u;
        self.has_upstream = present;
        if present {
            self.method = "upstream".to_string();
        }
    }

    /// Switches the method to stream accounting.
    pub fn mark_stream_accumulated(&mut self) {
        if self.method != "upstream" {
            self.method = "stream_chunks".to_string();
        }
    }

    /// Finalises the accounting.
    pub fn usage(&self) -> Usage {
        let mut prompt = self.prompt + self.overhead;
        let mut comp = self.comp;
        if comp == 0 && self.has_upstream && self.upstream.completion_tokens > 0 {
            // The response body carried no countable text (for example a
            // tool-call-only reply that the parser did not extract) but the
            // upstream did report a count; keep the upstream figure to avoid
            // manufacturing a false inflation signal.
            comp = self.upstream.completion_tokens;
        }
        if prompt == 0 && self.has_upstream && self.upstream.prompt_tokens > 0 {
            prompt = self.upstream.prompt_tokens;
        }
        let mut u = Usage {
            prompt_tokens: prompt,
            completion_tokens: comp,
            total_tokens: prompt + comp,
            method: self.method.clone(),
            family: self.est.family().clone(),
            upstream_reported: self.has_upstream,
            ..Usage::default()
        };
        if self.has_upstream {
            u.upstream_prompt_tokens = self.upstream.prompt_tokens;
            u.upstream_completion_tokens = self.upstream.completion_tokens;
            if self.upstream.total_tokens > 0 {
                u.upstream_total_tokens = self.upstream.total_tokens;
            } else {
                u.upstream_total_tokens =
                    self.upstream.prompt_tokens + self.upstream.completion_tokens;
            }
        }
        u
    }
}

// ---------------------------------------------------------------------------
// Request / response accounting helpers
// ---------------------------------------------------------------------------

/// Estimates the prompt tokens of an OpenAI-style chat request.
pub fn count_request(model: &str, req: Option<&ChatRequest>) -> (i64, TokenizerFamily) {
    let mut c = Counter::new(model);
    let Some(req) = req else {
        return (0, c.family().clone());
    };
    for m in &req.messages {
        c.add_prompt_text(&m.role);
        for part in m.content.text_and_tool_input() {
            c.add_prompt_text(&part);
        }
        if !m.name.is_empty() {
            c.add_prompt_text(&m.name);
        }
        for tc in &m.tool_calls {
            c.add_prompt_text(&tc.function.name);
            c.add_prompt_text(&tc.function.arguments);
        }
        c.add_overhead(MESSAGE_OVERHEAD);
    }
    for t in &req.tools {
        // Tool schemas are part of the prompt and are usually the single largest
        // contributor after the messages themselves.
        if let Ok(raw) = serde_json::to_string(t) {
            let estimated = c.est.estimate(&raw);
            c.add_overhead(estimated);
        }
    }
    if let Some(rf) = &req.response_format {
        if let Some(schema) = &rf.schema {
            if !schema.is_empty() {
                let estimated = c.est.estimate(schema.as_str());
                c.add_overhead(estimated);
            }
        }
    }
    // Assistant priming.
    c.add_overhead(3);
    (c.prompt + c.overhead, c.family().clone())
}

/// Estimates the prompt tokens of a Messages API request.
pub fn count_request_anthropic(
    model: &str,
    req: Option<&AnthropicRequest>,
) -> (i64, TokenizerFamily) {
    let mut c = Counter::new(model);
    let Some(req) = req else {
        return (0, c.family().clone());
    };
    if let Some(system) = &req.system {
        for part in system.text_and_tool_input() {
            c.add_prompt_text(&part);
        }
    }
    for m in &req.messages {
        c.add_prompt_text(&m.role);
        for part in m.content.text_and_tool_input() {
            c.add_prompt_text(&part);
        }
        c.add_overhead(MESSAGE_OVERHEAD);
    }
    for t in &req.tools {
        if let Ok(raw) = serde_json::to_string(t) {
            let estimated = c.est.estimate(&raw);
            c.add_overhead(estimated);
        }
    }
    c.add_overhead(3);
    (c.prompt + c.overhead, c.family().clone())
}

/// Estimates the completion tokens of a non-streaming response.
pub fn count_response(model: &str, resp: Option<&ChatResponse>) -> i64 {
    let mut c = Counter::new(model);
    let Some(resp) = resp else {
        return 0;
    };
    for ch in &resp.choices {
        c.add_completion_text(&ch.message.content.plain());
        c.add_completion_text(&ch.message.reasoning_content);
        for tc in &ch.message.tool_calls {
            c.add_completion_text(&tc.function.name);
            c.add_completion_text(&tc.function.arguments);
        }
    }
    c.comp
}

/// Estimates completion tokens for a Messages response.
pub fn count_response_anthropic(model: &str, resp: Option<&AnthropicResponse>) -> i64 {
    let mut c = Counter::new(model);
    let Some(resp) = resp else {
        return 0;
    };
    for blk in &resp.content {
        match blk.r#type.as_str() {
            "text" => c.add_completion_text(&blk.text),
            "thinking" => c.add_completion_text(&blk.thinking),
            "tool_use" => {
                c.add_completion_text(&blk.name);
                if let Ok(raw) = serde_json::to_string(&blk.input) {
                    c.add_completion_text(&raw);
                }
            }
            _ => {}
        }
    }
    c.comp
}

// ---------------------------------------------------------------------------
// Responses API accounting
// ---------------------------------------------------------------------------

/// Estimates the input tokens of a `/v1/responses` request.
///
/// The prompt is assembled from `instructions` plus every text-bearing part of
/// `input`, including function-call arguments and outputs, because all of those
/// are sent to the model and are billed.
///
/// Note that `instructions` is counted twice when it is non-empty:
/// `ResponsesRequest::input_text` already yields it as its first item, and the
/// Go implementation adds it separately as well. That is kept deliberately -
/// the recount must stay comparable with the Go one, and the duplicate is a
/// handful of tokens against a tolerance check.
pub fn count_request_responses(
    model: &str,
    req: Option<&ResponsesRequest>,
) -> (i64, TokenizerFamily) {
    let mut c = Counter::new(model);
    let Some(req) = req else {
        return (0, c.family().clone());
    };
    if !req.instructions.is_empty() {
        c.add_prompt_text(&req.instructions);
    }
    for item in req.input_text() {
        c.add_prompt_text(&item.text);
    }
    for t in &req.tools {
        if !t.description.is_empty() {
            c.add_prompt_text(&t.description);
        }
        if let Some(schema) = t.tool_schema() {
            if !schema.is_empty() {
                let estimated = c.est.estimate(schema.as_str());
                c.add_overhead(estimated);
            }
        }
    }
    // The Responses API bills a fixed framing cost per input item, similar to
    // the chat message overhead.
    c.add_overhead(3);
    (c.prompt + c.overhead, c.family().clone())
}

/// Estimates the output tokens of a `/v1/responses` reply.
///
/// Reasoning summaries and function-call arguments are generated tokens and are
/// billed, so they count toward the completion side.
pub fn count_response_responses(model: &str, resp: Option<&ResponsesResponse>) -> i64 {
    let mut c = Counter::new(model);
    let Some(resp) = resp else {
        return 0;
    };
    for item in &resp.output {
        match item.r#type.as_str() {
            "message" => {
                for part in &item.content {
                    c.add_completion_text(&part.text);
                }
            }
            "reasoning" => {
                for part in &item.summary {
                    c.add_completion_text(&part.text);
                }
            }
            "function_call" => {
                c.add_completion_text(&item.name);
                c.add_completion_text(&item.arguments);
            }
            _ => {}
        }
    }
    c.comp
}
