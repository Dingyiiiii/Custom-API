//! Protocol-aware decisions the pipeline makes: failure classification, channel
//! failover, body rebuilding, and the per-protocol audit/metering adapters.
//!
//! Splitting these out of the proxy keeps the pipeline in `proxy.rs` readable:
//! that file asks "should I retry?" and "what does this body mean?", while this
//! file knows how each wire protocol is shaped.

use std::collections::HashSet;

use crate::audit;
use crate::config::{self, Channel};
use crate::metering;
use crate::policy;
use crate::protocol;

use super::proxy::{ProxyContext, RouteFailure};

// ---------------------------------------------------------------------------
// Failure classification
// ---------------------------------------------------------------------------

/// Bytes of a 403/402 body read to decide whether the provider is out of credit.
///
/// An error body is short; a provider that sends something enormous does not get
/// to make the gateway read it.
pub(crate) const CREDIT_PROBE_BYTES: usize = 8192;

/// The failure classes `routing.retry_on` understands.
pub(crate) const CLASS_TIMEOUT: &str = "timeout";
pub(crate) const CLASS_NETWORK: &str = "network";
pub(crate) const CLASS_429: &str = "429";
pub(crate) const CLASS_5XX: &str = "5xx";
pub(crate) const CLASS_QUOTA: &str = "quota";

/// Maps a transport or status failure onto the retry vocabulary used by
/// `routing.retry_on`.
///
/// `body` may be absent, in which case only the status is available. When the
/// body is present it is consulted for one specific case: a 4xx that means "this
/// provider cannot serve anyone right now" rather than "this request is
/// invalid". Exhausted credit is exactly that - the caller's request is fine,
/// this provider is out of money - and it must move to the next provider, which
/// is the whole reason several providers are configured.
pub(crate) fn failure_class(
    status: u16,
    error: Option<&reqwest::Error>,
    body: Option<&[u8]>,
) -> &'static str {
    if let Some(err) = error {
        if err.is_timeout() || err.to_string().to_ascii_lowercase().contains("timeout") {
            return CLASS_TIMEOUT;
        }
        return CLASS_NETWORK;
    }
    match status {
        429 => CLASS_429,
        500..=u16::MAX => CLASS_5XX,
        402 | 403 => {
            // 403 is normally deliberate and terminal (a bad key, a banned
            // model), and retrying it everywhere would multiply failures across
            // providers. It is retryable only when the provider says the reason
            // is its own account state, never on a bare 403.
            if provider_out_of_credit(body.unwrap_or(&[])) {
                CLASS_QUOTA
            } else {
                ""
            }
        }
        400..=499 => {
            // Any other 4xx is the client's fault or a per-model failure;
            // retrying another provider would most likely fail the same way and
            // would still be billed when it does not.
            ""
        }
        _ => "",
    }
}

/// Reports whether an error body says the provider's own account cannot pay for
/// the request.
///
/// The check is deliberately narrow and text-based: these bodies come from
/// aggregators running software this project does not control, and the field
/// names differ (new-api uses code `insufficient_user_quota`, others say
/// `insufficient_quota`, `exceeded your current quota`, or `余额不足`). A false
/// positive costs one wasted attempt on the next provider; a false negative
/// means a dead provider is never skipped, which is the failure the operator
/// actually notices.
pub(crate) fn provider_out_of_credit(body: &[u8]) -> bool {
    if body.is_empty() {
        return false;
    }
    // Only scan a bounded prefix: an error body is short, and this runs on the
    // request path.
    let body = if body.len() > 4096 { &body[..4096] } else { body };
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    const MARKERS: [&str; 12] = [
        "insufficient_user_quota",
        "insufficient_quota",
        "insufficient balance",
        "insufficient_balance",
        "insufficient credit",
        "no credit",
        "exceeded your current quota",
        "quota exhausted",
        "credit_limit",
        "余额不足",
        "预扣费额度失败",
        "余额为",
    ];
    MARKERS.iter().any(|marker| text.contains(marker))
}

// ---------------------------------------------------------------------------
// Failover
// ---------------------------------------------------------------------------

impl super::Server {
    /// Moves the context to the next candidate after a failure.
    ///
    /// Returns false when no further attempt should be made. Every attempt is a
    /// real upstream call that may be billed, so the attempt budget is honoured
    /// strictly.
    pub(crate) fn switch_channel(&self, pc: &mut ProxyContext, attempt: i32) -> bool {
        let pol = self.config().routing.clone();
        if attempt >= pol.attempts() {
            return false;
        }
        // Find the current channel in the ranking and take the next one that has
        // not been tried yet.
        let mut next = match pc
            .ranked
            .iter()
            .position(|c| c.channel.name == pc.channel.name)
        {
            Some(index) if index + 1 < pc.ranked.len() => index + 1,
            _ => return false,
        };
        if pc.tried.is_empty() {
            pc.tried = HashSet::new();
        }
        // Skip anything already attempted, so a degraded channel revisited by
        // the ranker cannot cause a loop.
        while next < pc.ranked.len() && pc.tried.contains(&pc.ranked[next].channel.name) {
            next += 1;
        }
        if next >= pc.ranked.len() {
            return false;
        }
        let candidate = pc.ranked[next].clone();
        pc.tried.insert(pc.channel.name.clone());

        let previous = pc.channel.name.clone();
        pc.channel = candidate.channel.clone();
        pc.upstream_model = candidate.upstream_model.clone();
        // The channel identity is part of the audit record and it must follow the
        // switch: a record naming the channel that answered while naming the
        // provider that failed is worse than useless when the question is "which
        // provider do I stop paying for?". Deriving it here rather than at
        // selection time is what keeps the two in step.
        let (provider, key) = provider_of(&self.config(), &pc.channel.name);
        pc.provider = provider;
        pc.key = key;
        pc.failover_note = format!(
            "failed over from {:?} to {:?} after attempt {}",
            previous, candidate.channel.name, attempt
        );
        log::info!(
            "channel failover id={} from={} to={} attempt={}",
            pc.id,
            previous,
            candidate.channel.name,
            attempt + 1
        );

        // The new channel may speak a different protocol, so the body has to be
        // re-derived from the original request rather than reused.
        self.rebuild_forward_body(pc).is_ok()
    }
}

/// Why a forwarded body could not be rebuilt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RebuildError {
    /// The target protocol cannot express a feature of the request, which is a
    /// client-visible problem (HTTP 400 naming the field).
    Translation(String),
    /// Anything else (translation disabled, malformed request for the target
    /// protocol) is a channel-selection problem.
    Other(String),
}

impl std::fmt::Display for RebuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RebuildError::Translation(msg) | RebuildError::Other(msg) => f.write_str(msg),
        }
    }
}

impl super::Server {
    /// Recomputes the forwarded body for the current channel, applying model
    /// renaming and any required protocol translation.
    pub(crate) fn rebuild_forward_body(
        &self,
        pc: &mut ProxyContext,
    ) -> Result<(), RebuildError> {
        let cfg = self.config();
        let mut body = pc.body.clone();
        let upstream_protocol = channel_protocol(&pc.channel).to_string();

        // Step 1: bring the payload into the protocol the channel speaks.
        //
        // The router has already decided that this channel may serve the
        // request: either it speaks the client's protocol, or it declared the
        // surface (in which case sending the body unchanged is the whole point of
        // the declaration), or conversion is permitted. So the payload is
        // converted whenever the two protocols differ, and a request that cannot
        // be expressed in the target protocol fails here with a 400 naming the
        // field rather than being forwarded in a shape the upstream will misread.
        if upstream_protocol == pc.protocol {
            pc.translated = false;
            pc.upstream_protocol = upstream_protocol;
        } else if pc.channel.declares_protocol(&pc.protocol) {
            // The provider serves this surface itself, so the body is already in
            // the right shape. Nothing to convert, and nothing to present back.
            pc.translated = false;
            pc.upstream_protocol = pc.protocol.clone();
        } else {
            if !cfg.routing.translation_allowed() {
                return Err(RebuildError::Other(format!(
                    "channel {:?} speaks {} but the request is {}, and translation is disabled",
                    pc.channel.name, upstream_protocol, pc.protocol
                )));
            }
            let translated = protocol::translate_request_body(
                &pc.protocol,
                &upstream_protocol,
                &body,
            )
            .map_err(|e| match e.translation() {
                Some(t) => RebuildError::Translation(t.to_string()),
                None => RebuildError::Other(e.to_string()),
            })?;
            body = translated;
            pc.translated = true;
            pc.upstream_protocol = upstream_protocol;
        }

        // Step 2: rename the model in place, on whichever body shape we now hold.
        if !pc.upstream_model.is_empty() && pc.upstream_model != pc.canonical {
            body = protocol::replace_top_level_string(&body, "model", &pc.upstream_model);
        }
        pc.forward_body = body;
        Ok(())
    }

    /// Parses the client body for its protocol, recounts the prompt and builds
    /// the outbound audit payload.
    pub(crate) fn prepare_request(&self, pc: &mut ProxyContext) -> Result<(), RouteFailure> {
        match pc.protocol.as_str() {
            "anthropic" => {
                let req: protocol::AnthropicRequest =
                    serde_json::from_slice(&pc.body).map_err(|_| malformed_body())?;
                let (tokens, family) = metering::count_request_anthropic(&pc.canonical, Some(&req));
                pc.prompt_tokens = tokens;
                pc.tokenizer_family = family;
            }
            "responses" => {
                let req: protocol::ResponsesRequest =
                    serde_json::from_slice(&pc.body).map_err(|_| malformed_body())?;
                let (tokens, family) = metering::count_request_responses(&pc.canonical, Some(&req));
                pc.prompt_tokens = tokens;
                pc.tokenizer_family = family;
                // Server-side conversation state means the gateway cannot see the
                // whole prompt, so its recount and its input audit would both be
                // partial. Record it rather than let that pass silently.
                if req.uses_server_state() {
                    pc.server_state = true;
                }
            }
            _ => {
                let req: protocol::ChatRequest =
                    serde_json::from_slice(&pc.body).map_err(|_| malformed_body())?;
                let (tokens, family) = metering::count_request(&pc.canonical, Some(&req));
                pc.prompt_tokens = tokens;
                pc.tokenizer_family = family;
            }
        }
        Ok(())
    }

    /// Extracts the outbound audit surface for the protocol.
    pub(crate) fn build_inspect_request_for(&self, pc: &ProxyContext) -> policy::InspectRequest {
        let trusted = pc.user.trust_level() == config::TRUST_TRUSTED;
        let base = || policy::InspectRequest {
            protocol: pc.protocol.clone(),
            model: pc.canonical.clone(),
            actor: pc.user.name.clone(),
            trusted,
            text: Vec::new(),
            tools: Vec::new(),
            tool_args: Vec::new(),
        };
        match pc.protocol.as_str() {
            "anthropic" => {
                let Ok(req) = serde_json::from_slice::<protocol::AnthropicRequest>(&pc.body) else {
                    return base();
                };
                policy::inspect_request_from_anthropic(
                    &pc.user.name,
                    &pc.canonical,
                    trusted,
                    Some(&req),
                )
            }
            "responses" => {
                let Ok(req) = serde_json::from_slice::<protocol::ResponsesRequest>(&pc.body) else {
                    return base();
                };
                let mut out = base();
                out.protocol = "responses".to_string();
                for item in req.input_text() {
                    out.text.push(policy::LabelledText {
                        field: item.field,
                        text: item.text,
                    });
                }
                out.tools = req.tool_names();
                // Tool schemas are part of the prompt and can carry dangerous
                // paths, so they go through the same argument rules as the chat
                // path.
                for (i, tool) in req.tools.iter().enumerate() {
                    if let Some(schema) = tool.tool_schema() {
                        if !schema.is_empty() {
                            out.tool_args.push(policy::LabelledText {
                                field: format!("tools[{i}].parameters"),
                                text: schema.as_str().to_string(),
                            });
                        }
                    }
                }
                out
            }
            _ => {
                let Ok(req) = serde_json::from_slice::<protocol::ChatRequest>(&pc.body) else {
                    return base();
                };
                policy::inspect_request_from_openai(&pc.user.name, &pc.canonical, trusted, Some(&req))
            }
        }
    }
}

/// The failure a malformed client body produces.
fn malformed_body() -> RouteFailure {
    RouteFailure {
        status: axum::http::StatusCode::BAD_REQUEST,
        code: "invalid_request".to_string(),
        msg: "malformed JSON body".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Per-protocol inbound inspection and metering
// ---------------------------------------------------------------------------

impl super::Server {
    /// Runs the response-side audit for the protocol the upstream actually
    /// replied in.
    pub(crate) fn inspect_inbound_for(&self, pc: &mut ProxyContext, body: &[u8]) {
        let trusted = pc.channel.trust_level() == config::TRUST_TRUSTED;
        // The reply is presented in the client's protocol before it is inspected,
        // so the rules always see the shape the user will receive. That holds
        // whether the protocol difference was bridged by conversion or by the
        // provider serving the client's surface itself - a client that asked for
        // Responses must not be handed a chat body just because the provider
        // happened to prefer chat.
        let mut presented: Vec<u8> = body.to_vec();
        if pc.reply_needs_conversion() {
            match protocol::translate_response_body(
                &pc.upstream_protocol,
                &pc.protocol,
                &presented,
            ) {
                Ok(converted) => presented = converted,
                Err(e) => log::warn!(
                    "response translation failed id={} from={} to={} err={}",
                    pc.id,
                    pc.upstream_protocol,
                    pc.protocol,
                    e
                ),
            }
        }
        let body = presented.as_slice();
        match pc.protocol.as_str() {
            "anthropic" => {
                let pr: protocol::AnthropicResponse = match serde_json::from_slice(body) {
                    Ok(pr) => pr,
                    Err(_) => {
                        pc.inbound_decision = self.engine.check_inbound(Some(&policy::InspectResponse {
                            protocol: "anthropic".to_string(),
                            channel: pc.channel.name.clone(),
                            trusted,
                            text: Vec::new(),
                            tool_calls: Vec::new(),
                        }));
                        return;
                    }
                };
                if let Some(u) = pr.usage.as_ref() {
                    pc.usage.upstream_prompt_tokens =
                        u.input_tokens + u.cache_read_input_tokens + u.cache_creation_input_tokens;
                    pc.usage.upstream_completion_tokens = u.output_tokens;
                    pc.usage.upstream_total_tokens =
                        pc.usage.upstream_prompt_tokens + pc.usage.upstream_completion_tokens;
                    pc.usage.upstream_reported = true;
                }
                if !pr.model.is_empty() {
                    pc.observe_upstream_model(&pr.model);
                }
                pc.inbound_decision =
                    self.engine
                        .check_inbound(Some(&policy::inspect_response_from_anthropic(
                            &pc.channel.name,
                            trusted,
                            Some(&pr),
                        )));
            }
            "responses" => {
                let pr: protocol::ResponsesResponse = match serde_json::from_slice(body) {
                    Ok(pr) => pr,
                    Err(_) => {
                        pc.inbound_decision = self.engine.check_inbound(Some(&policy::InspectResponse {
                            protocol: "responses".to_string(),
                            channel: pc.channel.name.clone(),
                            trusted,
                            text: Vec::new(),
                            tool_calls: Vec::new(),
                        }));
                        return;
                    }
                };
                if let Some(u) = pr.usage.as_ref() {
                    let converted = u.to_usage();
                    pc.usage.upstream_prompt_tokens = converted.prompt_tokens;
                    pc.usage.upstream_completion_tokens = converted.completion_tokens;
                    pc.usage.upstream_total_tokens = converted.total_tokens;
                    pc.usage.upstream_reported = true;
                }
                if !pr.model.is_empty() {
                    pc.observe_upstream_model(&pr.model);
                }
                pc.inbound_decision =
                    self.engine
                        .check_inbound(Some(&inspect_responses(&pc.channel.name, trusted, &pr)));
            }
            _ => {
                let pr: protocol::ChatResponse = match serde_json::from_slice(body) {
                    Ok(pr) => pr,
                    Err(_) => {
                        pc.inbound_decision = self.engine.check_inbound(Some(&policy::InspectResponse {
                            protocol: "openai".to_string(),
                            channel: pc.channel.name.clone(),
                            trusted,
                            text: Vec::new(),
                            tool_calls: Vec::new(),
                        }));
                        return;
                    }
                };
                if let Some(u) = pr.usage.as_ref() {
                    pc.usage.upstream_prompt_tokens = u.prompt_tokens;
                    pc.usage.upstream_completion_tokens = u.completion_tokens;
                    pc.usage.upstream_total_tokens = u.total_tokens;
                    pc.usage.upstream_reported = true;
                }
                if !pr.model.is_empty() {
                    pc.observe_upstream_model(&pr.model);
                }
                pc.inbound_decision = self.engine.check_inbound(Some(
                    &policy::inspect_response_from_openai(&pc.channel.name, trusted, Some(&pr)),
                ));
            }
        }
        if pc.inbound_decision.degraded {
            self.stats.degraded.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Recounts the response for the protocol in use.
    pub(crate) fn meter_response_for(&self, pc: &mut ProxyContext, body: &[u8]) {
        // Recount in the client's protocol: the number the user is charged for
        // should match the response they receive, and a translated reply may
        // carry slightly different framing from the upstream's own shape.
        let mut measured: Vec<u8> = body.to_vec();
        if pc.reply_needs_conversion() {
            if let Ok(converted) =
                protocol::translate_response_body(&pc.upstream_protocol, &pc.protocol, &measured)
            {
                measured = converted;
            }
        }
        let completion = match pc.protocol.as_str() {
            "anthropic" => serde_json::from_slice::<protocol::AnthropicResponse>(&measured)
                .map(|pr| metering::count_response_anthropic(&pc.canonical, Some(&pr)))
                .unwrap_or(0),
            "responses" => serde_json::from_slice::<protocol::ResponsesResponse>(&measured)
                .map(|pr| metering::count_response_responses(&pc.canonical, Some(&pr)))
                .unwrap_or(0),
            _ => serde_json::from_slice::<protocol::ChatResponse>(&measured)
                .map(|pr| metering::count_response(&pc.canonical, Some(&pr)))
                .unwrap_or(0),
        };
        pc.completion_tokens = completion;
        if pc.usage.upstream_reported {
            pc.usage.method = "upstream".to_string();
        }
        self.finalize_metering(pc);
    }
}

/// Builds the inbound audit payload for a Responses reply.
///
/// There is no constructor in the policy package for this protocol because the
/// Responses shape is the only one whose function calls live in a flat output
/// array rather than inside a message, so the mapping is expressed here.
pub(crate) fn inspect_responses(
    channel: &str,
    trusted: bool,
    resp: &protocol::ResponsesResponse,
) -> policy::InspectResponse {
    let mut ir = policy::InspectResponse {
        protocol: "responses".to_string(),
        channel: channel.to_string(),
        trusted,
        text: Vec::new(),
        tool_calls: Vec::new(),
    };
    for (i, item) in resp.output.iter().enumerate() {
        match item.r#type.as_str() {
            "message" => {
                for (j, part) in item.content.iter().enumerate() {
                    if !part.text.is_empty() {
                        ir.text.push(policy::LabelledText {
                            field: format!("output[{i}].content[{j}]"),
                            text: part.text.clone(),
                        });
                    }
                }
            }
            "reasoning" => {
                for (j, part) in item.summary.iter().enumerate() {
                    if !part.text.is_empty() {
                        ir.text.push(policy::LabelledText {
                            field: format!("output[{i}].summary[{j}]"),
                            text: part.text.clone(),
                        });
                    }
                }
            }
            "function_call" => {
                ir.tool_calls.push(policy::ToolCallRequest {
                    name: item.name.clone(),
                    arguments: item.arguments.clone(),
                    kind: "function_call".to_string(),
                });
                if !item.arguments.is_empty() {
                    ir.text.push(policy::LabelledText {
                        field: format!("output[{i}].arguments"),
                        text: item.arguments.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    ir
}

// ---------------------------------------------------------------------------
// Stream accumulation per protocol
// ---------------------------------------------------------------------------

/// The metering-relevant part of a streamed tool call.
#[derive(Debug, Clone, Default)]
pub struct ToolCallParts {
    pub name: String,
    pub arguments: String,
}

/// Abstracts the per-protocol streaming bookkeeping so the proxy's streaming
/// loop stays protocol-agnostic.
///
/// Two members (`terminal_error` and `tool_call_count`) are part of the Go
/// interface and are kept for parity even though the current pipeline does not
/// consult them.
#[allow(dead_code)]
pub(crate) trait StreamAccumulator: Send {
    /// Folds one SSE event into the accumulated state.
    fn observe(&mut self, ev: &protocol::SseEvent);
    /// Returns the assistant text seen so far.
    fn text(&self) -> &str;
    /// Returns any chain-of-thought or reasoning summary seen so far.
    ///
    /// It is accounted separately because it is generated and billed, but it is
    /// not assistant-visible text.
    fn reasoning(&self) -> &str;
    /// Returns the accumulated tool calls as name/argument pairs.
    fn tool_calls(&self) -> Vec<ToolCallParts>;
    /// Returns the model the upstream reported, if any.
    fn model(&self) -> String;
    /// Returns the upstream's usage claim.
    fn usage(&self) -> Option<protocol::Usage>;
    /// Reports whether the stream reached a terminal event.
    fn completed(&self) -> bool;
    /// Returns a failure the stream carried, if any.
    fn terminal_error(&self) -> Option<protocol::ApiError>;
    /// Converts the accumulated stream into a JSON body for the audit path, in
    /// the client's protocol.
    fn render(&self) -> Option<Vec<u8>>;
    /// The protocol the accumulator parsed.
    fn upstream(&self) -> String;
    /// Reports how many tool calls were accumulated.
    fn tool_call_count(&self) -> usize;
}

/// Adapts [`protocol::StreamAccumulator`] (chat and Messages).
pub(crate) struct ChatAccumulator {
    acc: protocol::StreamAccumulator,
}

impl ChatAccumulator {
    fn new(provider: &str) -> Self {
        ChatAccumulator {
            acc: protocol::StreamAccumulator::new(provider),
        }
    }
}

impl StreamAccumulator for ChatAccumulator {
    fn upstream(&self) -> String {
        self.acc.provider().to_string()
    }

    fn observe(&mut self, ev: &protocol::SseEvent) {
        self.acc.observe(ev);
    }

    fn text(&self) -> &str {
        &self.acc.text
    }

    fn reasoning(&self) -> &str {
        &self.acc.reasoning
    }

    fn tool_calls(&self) -> Vec<ToolCallParts> {
        self.acc
            .tool_calls_in_order()
            .into_iter()
            .map(|call| ToolCallParts {
                name: call.function.name,
                arguments: call.function.arguments,
            })
            .collect()
    }

    fn model(&self) -> String {
        self.acc.model.clone()
    }

    fn tool_call_count(&self) -> usize {
        self.acc.tool_calls.len()
    }

    fn usage(&self) -> Option<protocol::Usage> {
        if self.acc.has_usage {
            Some(self.acc.usage.clone())
        } else {
            None
        }
    }

    fn terminal_error(&self) -> Option<protocol::ApiError> {
        None
    }

    /// Reports whether a chat-style stream saw its terminator.
    ///
    /// Only the `[DONE]` sentinel is authoritative here; a `finish_reason` is not
    /// enough, because servers emit it on a chunk that may be followed by a usage
    /// chunk.
    fn completed(&self) -> bool {
        self.acc.saw_done
    }

    fn render(&self) -> Option<Vec<u8>> {
        render_stream(&self.acc)
    }
}

/// Serialises a chat/Messages accumulator back to a single JSON body so the
/// buffered response path can reuse the non-streaming inspectors.
///
/// The protocol comes from the accumulator itself rather than a parameter, so a
/// caller cannot accidentally render an Anthropic stream as a chat response.
pub(crate) fn render_stream(acc: &protocol::StreamAccumulator) -> Option<Vec<u8>> {
    if acc.provider() == "anthropic" {
        serde_json::to_vec(&acc.as_anthropic_response()).ok()
    } else {
        serde_json::to_vec(&acc.as_response()).ok()
    }
}

/// Adapts [`protocol::ResponsesStreamState`].
pub(crate) struct ResponsesAccumulator {
    state: protocol::ResponsesStreamState,
}

impl ResponsesAccumulator {
    fn new() -> Self {
        ResponsesAccumulator {
            state: protocol::ResponsesStreamState::new(),
        }
    }
}

impl StreamAccumulator for ResponsesAccumulator {
    fn upstream(&self) -> String {
        "responses".to_string()
    }

    fn observe(&mut self, ev: &protocol::SseEvent) {
        self.state.observe(ev);
    }

    fn text(&self) -> &str {
        &self.state.text
    }

    fn reasoning(&self) -> &str {
        &self.state.reasoning
    }

    fn tool_calls(&self) -> Vec<ToolCallParts> {
        self.state
            .calls_in_order()
            .into_iter()
            .map(|call| ToolCallParts {
                name: call.name,
                arguments: call.arguments,
            })
            .collect()
    }

    fn model(&self) -> String {
        self.state.model.clone()
    }

    fn tool_call_count(&self) -> usize {
        self.state.calls.len()
    }

    fn usage(&self) -> Option<protocol::Usage> {
        if self.state.has_usage {
            Some(self.state.usage.to_usage())
        } else {
            None
        }
    }

    /// Reports whether a terminal Responses event arrived.
    fn completed(&self) -> bool {
        self.state.terminal()
    }

    fn terminal_error(&self) -> Option<protocol::ApiError> {
        self.state.error.clone()
    }

    fn render(&self) -> Option<Vec<u8>> {
        serde_json::to_vec(&self.state.as_response()).ok()
    }
}

/// Builds an accumulator for the protocol the upstream is expected to speak.
///
/// That is deliberately NOT the client's protocol: when the request was
/// translated, the upstream replies in its own dialect, so the accumulator has
/// to parse that dialect while the translator converts each event for the
/// client. Rendering still happens in the client's shape.
pub(crate) fn new_stream_accumulator(pc: &ProxyContext) -> Box<dyn StreamAccumulator> {
    let upstream = if pc.upstream_protocol.is_empty() {
        channel_protocol(&pc.channel).to_string()
    } else {
        pc.upstream_protocol.clone()
    };
    match upstream.as_str() {
        "responses" => Box::new(ResponsesAccumulator::new()),
        other => Box::new(ChatAccumulator::new(other)),
    }
}

// ---------------------------------------------------------------------------
// Audit enrichment
// ---------------------------------------------------------------------------

/// Resolves the provider and key an audit record belongs to.
pub(crate) fn provider_of(cfg: &config::Config, channel: &str) -> (String, String) {
    if let Some(p) = cfg.provider_by_channel_name(channel) {
        let key = channel
            .strip_prefix(&format!("{}/", p.name))
            .unwrap_or("")
            .to_string();
        return (p.name, key);
    }
    if let Some(meta) = channel_metadata(cfg, channel) {
        return (
            meta.get("provider").cloned().unwrap_or_default(),
            meta.get("key").cloned().unwrap_or_default(),
        );
    }
    (String::new(), String::new())
}

/// Returns a channel's metadata map, if it has one.
fn channel_metadata(cfg: &config::Config, channel: &str) -> Option<std::collections::HashMap<String, String>> {
    cfg.all_channels()
        .into_iter()
        .find(|ch| ch.name == channel)
        .map(|ch| ch.metadata)
}

/// Stamps the audit latency breakdown, in milliseconds.
pub(crate) fn record_latency(pc: &ProxyContext) -> audit::Latency {
    audit::Latency {
        total_ms: pc.start.elapsed().as_millis() as i64,
        outbound_audit_ms: pc.outbound_ns / 1_000_000,
        inbound_audit_ms: pc.inbound_ns / 1_000_000,
        upstream_ms: pc.upstream_ms,
        ttfb_ms: pc.ttfb_ms,
    }
}

/// Converts policy findings into the compact audit references.
pub(crate) fn to_finding_refs(findings: &[policy::Finding]) -> Vec<audit::FindingRef> {
    findings
        .iter()
        .map(|f| audit::FindingRef {
            rule: f.rule.clone(),
            severity: f.severity.clone(),
            field: f.field.clone(),
            excerpt_hash: f.excerpt_hash.clone(),
            trust_exempt: f.trust_exempt,
        })
        .collect()
}

/// Resolves the wire protocol a channel speaks.
pub(crate) fn channel_protocol(ch: &Channel) -> &str {
    if ch.protocol.is_empty() {
        "openai"
    } else {
        &ch.protocol
    }
}

/// Classifies the usage comparison.
pub(crate) fn verdict_of(pc: &ProxyContext) -> String {
    if !pc.usage.upstream_reported {
        return "not_reported".to_string();
    }
    if !pc.deviation.verifiable {
        return "not_verifiable".to_string();
    }
    if !pc.deviation.over_threshold {
        return "consistent".to_string();
    }
    if pc.deviation.direction == "over" {
        return "over_billed".to_string();
    }
    "under_billed".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_markers_are_recognised() {
        let naapi = "{\"error\":{\"message\":\"预扣费额度失败, 用户剩余额度: 0.017783\",\"type\":\"new_api_error\",\"code\":\"insufficient_user_quota\"}}";
        let naapi = naapi.as_bytes();
        let openai = br#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","code":"insufficient_quota"}}"#;
        let english = br#"{"error":{"message":"Insufficient balance","type":"invalid_request_error"}}"#;
        for (name, body) in [
            ("new-api", naapi),
            ("openai", &openai[..]),
            ("insufficient balance", &english[..]),
        ] {
            assert!(provider_out_of_credit(body), "{name}");
        }
        for (name, body) in [
            ("empty", &b""[..]),
            ("bad key", &br#"{"error":{"message":"Invalid token","code":"invalid_api_key"}}"#[..]),
            (
                "model not allowed",
                &br#"{"error":{"message":"This model is not available in your region","code":"model_not_found"}}"#[..],
            ),
            (
                "rate limited",
                &br#"{"error":{"message":"Too many requests","code":"rate_limit_exceeded"}}"#[..],
            ),
        ] {
            assert!(!provider_out_of_credit(body), "{name}");
        }
    }

    #[test]
    fn failure_classes_match_the_retry_vocabulary() {
        let quota = br#"{"error":{"code":"insufficient_user_quota"}}"#;
        assert_eq!(failure_class(403, None, Some(&quota[..])), CLASS_QUOTA);
        assert_eq!(
            failure_class(403, None, Some(&br#"{"error":{"code":"invalid_api_key"}}"#[..])),
            ""
        );
        assert_eq!(failure_class(403, None, None), "");
        assert_eq!(failure_class(402, None, Some(&quota[..])), CLASS_QUOTA);
        assert_eq!(failure_class(429, None, None), CLASS_429);
        assert_eq!(failure_class(503, None, None), CLASS_5XX);
        assert_eq!(failure_class(400, None, None), "");
        assert_eq!(failure_class(200, None, None), "");
    }
}
