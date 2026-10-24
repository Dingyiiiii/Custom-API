//! Payload extraction, rule evaluation, streaming detection and redaction.

use crate::protocol::responses::{ResponsesOutputItem, ResponsesRequest, ResponsesResponse};
use crate::protocol::types::{AnthropicRequest, AnthropicResponse, ChatRequest, ChatResponse};

use super::engine::{
    build_decision, dedupe_findings, excerpt_hash_with, Decision, Engine, Finding, PolicyError,
    Rule, ACTION_ALLOW, ACTION_BLOCK, INBOUND, OUTBOUND, SEV_HIGH, SEV_MEDIUM,
};

// ---------------------------------------------------------------------------
// Request / response payloads
// ---------------------------------------------------------------------------

/// A piece of text with its origin path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LabelledText {
    pub field: String,
    pub text: String,
}

/// A small helper for building [`LabelledText`].
pub fn labelled(field: impl Into<String>, text: impl Into<String>) -> LabelledText {
    LabelledText {
        field: field.into(),
        text: text.into(),
    }
}

/// The request-side payload handed to the engine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InspectRequest {
    /// "openai" or "anthropic".
    pub protocol: String,
    /// The canonical model name.
    pub model: String,
    /// Identifies the user for audit correlation.
    pub actor: String,
    /// Marks the user as allowlisted, which downgrades blocks to reports but
    /// never disables inspection.
    pub trusted: bool,
    /// Every piece of user-authored text, labelled by origin.
    pub text: Vec<LabelledText>,
    /// The tool names the client declared or already invoked.
    pub tools: Vec<String>,
    /// The arguments of declared/invoked tool calls, also labelled so a finding
    /// can point at the offending path.
    pub tool_args: Vec<LabelledText>,
}

/// The response-side payload handed to the engine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InspectResponse {
    /// "openai" or "anthropic".
    pub protocol: String,
    /// The upstream that produced the response.
    pub channel: String,
    /// Marks the channel as allowlisted.
    pub trusted: bool,
    /// The generated text, plus any tool-call payloads.
    pub text: Vec<LabelledText>,
    /// The tool calls the upstream asked the client to execute.
    pub tool_calls: Vec<ToolCallRequest>,
}

/// A tool call proposed by the upstream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolCallRequest {
    pub name: String,
    pub arguments: String,
    /// "function", "tool_use", ...
    pub kind: String,
}

/// Extracts the audit surface of a chat request.
pub fn inspect_request_from_openai(
    actor: &str,
    model: &str,
    trusted: bool,
    req: Option<&ChatRequest>,
) -> InspectRequest {
    let mut ir = InspectRequest {
        protocol: "openai".to_string(),
        model: model.to_string(),
        actor: actor.to_string(),
        trusted,
        ..InspectRequest::default()
    };
    let Some(req) = req else {
        return ir;
    };
    for (i, m) in req.messages.iter().enumerate() {
        let f = format!("messages[{i}]");
        for (j, p) in m.content.parts_or_parse().iter().enumerate() {
            match p.r#type.as_str() {
                "" | "text" | "input_text" | "output_text" => {
                    ir.text
                        .push(labelled(format!("{f}.content[{j}]"), p.text.clone()));
                }
                "tool_result" => {
                    if let Some(content) = &p.content {
                        ir.text
                            .push(labelled(format!("{f}.tool_result[{j}]"), content.text()));
                    }
                }
                _ => {}
            }
        }
        for (j, tc) in m.tool_calls.iter().enumerate() {
            if !tc.function.name.is_empty() {
                ir.tools.push(tc.function.name.clone());
            }
            if !tc.function.arguments.is_empty() {
                ir.tool_args.push(labelled(
                    format!("{f}.tool_calls[{j}].arguments"),
                    tc.function.arguments.clone(),
                ));
            }
        }
    }
    for t in &req.tools {
        if !t.function.name.is_empty() {
            ir.tools.push(t.function.name.clone());
        }
        if let Some(parameters) = &t.function.parameters {
            if !parameters.is_empty() {
                ir.tool_args
                    .push(labelled("tools.schema", parameters.as_str()));
            }
        }
    }
    ir
}

/// Extracts the audit surface of a Messages call.
pub fn inspect_request_from_anthropic(
    actor: &str,
    model: &str,
    trusted: bool,
    req: Option<&AnthropicRequest>,
) -> InspectRequest {
    let mut ir = InspectRequest {
        protocol: "anthropic".to_string(),
        model: model.to_string(),
        actor: actor.to_string(),
        trusted,
        ..InspectRequest::default()
    };
    let Some(req) = req else {
        return ir;
    };
    if let Some(system) = &req.system {
        for (j, p) in system.parts_or_parse().iter().enumerate() {
            if p.r#type.is_empty() || p.r#type == "text" {
                ir.text
                    .push(labelled(format!("system[{j}]"), p.text.clone()));
            }
        }
    }
    for (i, m) in req.messages.iter().enumerate() {
        let f = format!("messages[{i}]");
        for (j, p) in m.content.parts_or_parse().iter().enumerate() {
            match p.r#type.as_str() {
                "" | "text" => {
                    ir.text
                        .push(labelled(format!("{f}.content[{j}]"), p.text.clone()));
                }
                "tool_result" => {
                    if let Some(content) = &p.content {
                        ir.text
                            .push(labelled(format!("{f}.tool_result[{j}]"), content.text()));
                    }
                }
                "tool_use" => {
                    if !p.name.is_empty() {
                        ir.tools.push(p.name.clone());
                    }
                    if let Some(input) = &p.input {
                        if !input.is_empty() {
                            ir.tool_args
                                .push(labelled(format!("{f}.content[{j}].input"), input.as_str()));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    for t in &req.tools {
        if !t.name.is_empty() {
            ir.tools.push(t.name.clone());
        }
        if let Some(schema) = &t.input_schema {
            if !schema.is_empty() {
                ir.tool_args
                    .push(labelled("tools.input_schema", schema.as_str()));
            }
        }
    }
    ir
}

/// Extracts the audit surface of a chat response.
pub fn inspect_response_from_openai(
    channel: &str,
    trusted: bool,
    resp: Option<&ChatResponse>,
) -> InspectResponse {
    let mut ir = InspectResponse {
        protocol: "openai".to_string(),
        channel: channel.to_string(),
        trusted,
        ..InspectResponse::default()
    };
    let Some(resp) = resp else {
        return ir;
    };
    for (i, ch) in resp.choices.iter().enumerate() {
        let f = format!("choices[{i}].message");
        let text = ch.message.content.plain();
        if !text.is_empty() {
            ir.text.push(labelled(format!("{f}.content"), text));
        }
        if !ch.message.reasoning_content.is_empty() {
            ir.text.push(labelled(
                format!("{f}.reasoning_content"),
                ch.message.reasoning_content.clone(),
            ));
        }
        for (j, tc) in ch.message.tool_calls.iter().enumerate() {
            ir.tool_calls.push(ToolCallRequest {
                name: tc.function.name.clone(),
                arguments: tc.function.arguments.clone(),
                kind: if tc.r#type.is_empty() {
                    "function".to_string()
                } else {
                    tc.r#type.clone()
                },
            });
            if !tc.function.arguments.is_empty() {
                ir.text.push(labelled(
                    format!("{f}.tool_calls[{j}].arguments"),
                    tc.function.arguments.clone(),
                ));
            }
        }
    }
    ir
}

/// Extracts the audit surface of a Messages reply.
pub fn inspect_response_from_anthropic(
    channel: &str,
    trusted: bool,
    resp: Option<&AnthropicResponse>,
) -> InspectResponse {
    let mut ir = InspectResponse {
        protocol: "anthropic".to_string(),
        channel: channel.to_string(),
        trusted,
        ..InspectResponse::default()
    };
    let Some(resp) = resp else {
        return ir;
    };
    for (i, b) in resp.content.iter().enumerate() {
        let f = format!("content[{i}]");
        match b.r#type.as_str() {
            "text" => ir.text.push(labelled(format!("{f}.text"), b.text.clone())),
            "thinking" => ir
                .text
                .push(labelled(format!("{f}.thinking"), b.thinking.clone())),
            "tool_use" => {
                let args = b
                    .input
                    .as_ref()
                    .map(|r| r.as_str())
                    .unwrap_or("")
                    .to_string();
                ir.tool_calls.push(ToolCallRequest {
                    name: b.name.clone(),
                    arguments: args.clone(),
                    kind: "tool_use".to_string(),
                });
                if !args.is_empty() {
                    ir.text.push(labelled(format!("{f}.input"), args));
                }
            }
            _ => {}
        }
    }
    ir
}

/// Extracts the audit surface of a `/v1/responses` reply.
///
/// The Go gateway builds this payload itself (`gateway.protocols.inspectResponses`)
/// rather than exposing it from the policy package; the helper lives here so that
/// the Responses dialect is audited the same way as the other two.
pub fn inspect_response_from_responses(
    channel: &str,
    trusted: bool,
    resp: Option<&ResponsesResponse>,
) -> InspectResponse {
    let mut ir = InspectResponse {
        protocol: "responses".to_string(),
        channel: channel.to_string(),
        trusted,
        ..InspectResponse::default()
    };
    let Some(resp) = resp else {
        return ir;
    };
    for item in &resp.output {
        collect_responses_item(&mut ir, item);
    }
    ir
}

fn collect_responses_item(ir: &mut InspectResponse, item: &ResponsesOutputItem) {
    let id = if item.id.is_empty() {
        "output".to_string()
    } else {
        item.id.clone()
    };
    match item.r#type.as_str() {
        "message" => {
            for (j, part) in item.content.iter().enumerate() {
                if !part.text.is_empty() {
                    ir.text.push(labelled(
                        format!("output[{id}].content[{j}].text"),
                        part.text.clone(),
                    ));
                }
            }
        }
        "reasoning" => {
            for (j, part) in item.summary.iter().enumerate() {
                if !part.text.is_empty() {
                    ir.text.push(labelled(
                        format!("output[{id}].summary[{j}].text"),
                        part.text.clone(),
                    ));
                }
            }
        }
        "function_call" => {
            ir.tool_calls.push(ToolCallRequest {
                name: item.name.clone(),
                arguments: item.arguments.clone(),
                kind: "function_call".to_string(),
            });
            if !item.arguments.is_empty() {
                ir.text.push(labelled(
                    format!("output[{id}].arguments"),
                    item.arguments.clone(),
                ));
            }
        }
        _ => {}
    }
}

/// Extracts the audit surface of a `/v1/responses` request.
pub fn inspect_request_from_responses(
    actor: &str,
    model: &str,
    trusted: bool,
    req: Option<&ResponsesRequest>,
) -> InspectRequest {
    let mut ir = InspectRequest {
        protocol: "responses".to_string(),
        model: model.to_string(),
        actor: actor.to_string(),
        trusted,
        ..InspectRequest::default()
    };
    let Some(req) = req else {
        return ir;
    };
    if !req.instructions.is_empty() {
        ir.text
            .push(labelled("instructions", req.instructions.clone()));
    }
    for item in req.input_text() {
        ir.text.push(labelled(item.field, item.text));
    }
    for (i, t) in req.tools.iter().enumerate() {
        let name = t.tool_name();
        if !name.is_empty() {
            ir.tools.push(name);
        }
        if let Some(schema) = t.tool_schema() {
            if !schema.is_empty() {
                ir.tool_args
                    .push(labelled(format!("tools[{i}].parameters"), schema.as_str()));
            }
        }
    }
    ir
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

impl Engine {
    /// Evaluates request-side rules.
    pub fn check_outbound(&self, req: Option<&InspectRequest>) -> Decision {
        let start = std::time::Instant::now();
        let Some(req) = req else {
            return Decision {
                action: ACTION_ALLOW.to_string(),
                ..Decision::default()
            };
        };
        let mut findings: Vec<Finding> = Vec::new();
        let mut evaluated = 0i64;

        for r in &self.outbound {
            if r.exact.is_some() {
                continue;
            }
            evaluated += 1;
            for lt in &req.text {
                if lt.text.is_empty() {
                    continue;
                }
                if let Some(matched) = r.match_text(&lt.text) {
                    findings.push(self.finding(r, &lt.field, &matched));
                }
            }
        }
        // Exact-identifier tool rules.
        for r in &self.outbound {
            if r.exact.is_none() {
                continue;
            }
            evaluated += 1;
            for name in &req.tools {
                if r.matches_ident(name) {
                    findings.push(Finding {
                        rule: r.id.clone(),
                        direction: r.direction.clone(),
                        severity: r.severity.clone(),
                        field: "tool_call".to_string(),
                        detail: format!("{}: {}", r.detail, name),
                        excerpt_hash: self.excerpt_hash(name),
                        match_len: name.len() as i64,
                        trust_exempt: false,
                    });
                }
            }
        }
        // Allowlist mode: any tool not on the list is a violation.
        if !self.allow_tools.is_empty() {
            evaluated += 1;
            for name in &req.tools {
                if !self.allow_tools.iter().any(|a| a == name) {
                    findings.push(Finding {
                        rule: "allowlist.tool_not_allowed".to_string(),
                        direction: OUTBOUND.to_string(),
                        severity: SEV_HIGH.to_string(),
                        field: "tool_call".to_string(),
                        detail: format!("tool is not on the operator allowlist: {name}"),
                        excerpt_hash: self.excerpt_hash(name),
                        match_len: name.len() as i64,
                        trust_exempt: false,
                    });
                }
            }
        }
        // Dangerous-path rules also inspect tool arguments.
        for r in &self.outbound {
            if r.id != "default.tool_dangerous_path" && r.id != "user.deny_path" {
                continue;
            }
            for lt in &req.tool_args {
                if lt.text.is_empty() {
                    continue;
                }
                if let Some(matched) = r.match_text(&lt.text) {
                    findings.push(self.finding(r, &lt.field, &matched));
                }
            }
        }

        let mut d = build_decision(findings, self.outbound_mode(), req.trusted);
        d.rules_evaluated = evaluated;
        d.elapsed_ns = start.elapsed().as_nanos() as i64;
        d
    }

    /// Evaluates response-side rules, including the tool-call policy.
    pub fn check_inbound(&self, resp: Option<&InspectResponse>) -> Decision {
        let start = std::time::Instant::now();
        let Some(resp) = resp else {
            return Decision {
                action: ACTION_ALLOW.to_string(),
                ..Decision::default()
            };
        };
        let mut findings: Vec<Finding> = Vec::new();
        let mut evaluated = 0i64;

        for r in &self.inbound {
            evaluated += 1;
            for lt in &resp.text {
                if lt.text.is_empty() {
                    continue;
                }
                if let Some(matched) = r.match_text(&lt.text) {
                    findings.push(self.finding(r, &lt.field, &matched));
                }
            }
        }

        // Upstream-proposed tool calls are a distinct risk channel: they are the
        // mechanism by which a hostile upstream would try to run code locally.
        if !resp.tool_calls.is_empty() && self.tool_policy != "off" {
            evaluated += 1;
            for tc in &resp.tool_calls {
                let severity = if self.tool_policy == "report" {
                    SEV_MEDIUM
                } else {
                    SEV_HIGH
                };
                findings.push(Finding {
                    rule: "default.upstream_tool_call".to_string(),
                    direction: INBOUND.to_string(),
                    severity: severity.to_string(),
                    field: "tool_call".to_string(),
                    detail: format!(
                        "upstream proposed tool call {:?} (policy={}, {} bytes of arguments)",
                        tc.name,
                        self.tool_policy,
                        tc.arguments.len()
                    ),
                    excerpt_hash: self.excerpt_hash(&format!("{}\u{0}{}", tc.name, tc.arguments)),
                    match_len: tc.arguments.len() as i64,
                    trust_exempt: false,
                });
            }
        }

        // Go recomputes `trusted` on both branches of an if/else that produces
        // the same value; the effective rule is simply "trust downgrades content
        // findings, and the tool-call override below removes the exemption".
        let trusted = resp.trusted;
        let mut d = build_decision(findings, self.inbound_mode(), trusted);
        if self.inbound_mode() != "report"
            && self.tool_policy == "block"
            && !resp.tool_calls.is_empty()
        {
            d.action = ACTION_BLOCK.to_string();
            for f in &mut d.findings {
                f.trust_exempt = false;
            }
        }
        d.rules_evaluated = evaluated;
        d.elapsed_ns = start.elapsed().as_nanos() as i64;
        d
    }
}

// ---------------------------------------------------------------------------
// Streaming inspection
// ---------------------------------------------------------------------------

/// Bounds how much of a single response is scanned, so a runaway generation
/// cannot consume unbounded CPU. 4 MiB is far beyond any legitimate chat
/// completion.
pub const MAX_SCAN_BYTES: usize = 4 << 20;

/// Inspects a response while it is still streaming.
///
/// Matching a pattern across chunk boundaries is the hard part: an upstream can
/// split "ignore previous instructions" into several SSE deltas to evade a naive
/// per-chunk regexp. The detector therefore keeps a sliding tail of the text seen
/// so far and rescans an overlap window on every delta, which catches any match
/// shorter than the window regardless of how it was fragmented.
///
/// The detector owns copies of the inbound rules it needs instead of borrowing
/// the engine: cloning a compiled [`regex::Regex`] is cheap (the pattern is
/// shared), and it keeps the detector free of a lifetime parameter.
#[derive(Clone, Debug)]
pub struct StreamDetector {
    inbound: Vec<Rule>,
    in_mode: String,
    salt: String,
    /// The retained overlap in bytes.
    window: usize,
    /// The last `window` bytes of audited text.
    tail: Vec<u8>,
    /// Counts all bytes observed.
    total: usize,
    /// Accumulates deduplicated findings.
    findings: Vec<Finding>,
    /// Labels findings from this stream.
    field: String,
    /// Reports that the stream exceeded [`MAX_SCAN_BYTES`] and auditing stopped
    /// early.
    truncated: bool,
}

impl StreamDetector {
    /// Returns a detector using the engine's inbound rules.
    pub fn new(engine: &Engine, field: &str) -> Self {
        StreamDetector {
            inbound: engine.inbound.clone(),
            in_mode: engine.inbound_mode().to_string(),
            salt: engine.excerpt_hash_salt().to_string(),
            window: 256,
            tail: Vec::new(),
            total: 0,
            findings: Vec::new(),
            field: if field.is_empty() {
                "stream".to_string()
            } else {
                field.to_string()
            },
            truncated: false,
        }
    }

    /// Feeds a text delta and returns any newly matched findings.
    pub fn observe(&mut self, delta: &str) -> Vec<Finding> {
        if self.truncated || delta.is_empty() {
            return Vec::new();
        }
        self.total += delta.len();
        if self.total > MAX_SCAN_BYTES {
            self.truncated = true;
            return Vec::new();
        }
        self.tail.extend_from_slice(delta.as_bytes());
        if self.tail.len() > self.window {
            let cut = self.tail.len() - self.window;
            self.tail.drain(..cut);
        }
        // Rescan the retained window; dedupe keeps repeated matches from
        // inflating the finding list. Matching runs over the raw bytes, so a
        // chunk boundary that splits a rune behaves the way Go's byte-oriented
        // regexp does.
        let mut fresh: Vec<Finding> = Vec::new();
        for r in &self.inbound {
            if r.re.is_none() && r.keywords.is_empty() {
                continue;
            }
            if let Some(matched) = r.match_bytes(&self.tail) {
                fresh.push(self.finding(r, &matched));
            }
        }
        if fresh.is_empty() {
            return Vec::new();
        }
        let before = self.findings.len();
        let mut merged = std::mem::take(&mut self.findings);
        merged.extend(fresh);
        self.findings = dedupe_findings(merged);
        if self.findings.len() == before {
            return Vec::new();
        }
        self.findings[before..].to_vec()
    }

    /// Finalises the streaming decision.
    pub fn decision(&self) -> Decision {
        let mut dec = build_decision(self.findings.clone(), &self.in_mode, false);
        dec.findings = dedupe_findings(self.findings.clone());
        dec.rules_evaluated = self.inbound.len() as i64;
        dec
    }

    /// Reports whether scanning stopped early.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Returns how many bytes were examined.
    pub fn scanned_bytes(&self) -> usize {
        self.total
    }

    fn finding(&self, rule: &Rule, matched: &[u8]) -> Finding {
        Finding {
            rule: rule.id.clone(),
            direction: rule.direction.clone(),
            severity: rule.severity.clone(),
            field: self.field.clone(),
            detail: rule.detail.clone(),
            excerpt_hash: excerpt_hash_with(&self.salt, matched),
            match_len: matched.len() as i64,
            trust_exempt: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Sanitiser
// ---------------------------------------------------------------------------

/// The fixed mask a redacted secret is replaced with.
pub const REDACTED_MASK: &str = "[redacted]";

/// Masks secrets in text before it is persisted to the audit log.
#[derive(Clone, Debug, Default)]
pub struct Redactor {
    pub(super) rules: Vec<regex::bytes::Regex>,
}

impl Redactor {
    /// Compiles extra operator patterns on top of the built-in credential
    /// patterns.
    pub fn new(extra: &[String]) -> Result<Redactor, PolicyError> {
        let mut rules = Vec::new();
        // The built-in patterns are known-good; a compile failure here would be
        // a bug in the rule table, which `default_secret_patterns_compile` in the
        // tests covers.
        if let Some(re) = super::engine::compile_any(&super::rules::default_secret_patterns())
            .map_err(|err| PolicyError::RedactPatterns(err.to_string()))?
        {
            rules.push(re);
        }
        if !extra.is_empty() {
            if let Some(re2) = super::engine::compile_any(extra)
                .map_err(|err| PolicyError::RedactPatterns(err.to_string()))?
            {
                rules.push(re2);
            }
        }
        Ok(Redactor { rules })
    }

    /// Replaces every known secret pattern with a fixed mask.
    pub fn redact(&self, s: &str) -> String {
        let mut out = s.as_bytes().to_vec();
        for re in &self.rules {
            out = re.replace_all(&out, REDACTED_MASK.as_bytes()).into_owned();
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Reports whether any pattern is compiled (useful for callers that hold an
    /// optional redactor and want to skip the copy).
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}
