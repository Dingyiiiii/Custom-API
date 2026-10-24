//! Streaming (server-sent events) forwarding.
//!
//! Two strategies exist because of an inherent trade-off the project plan calls
//! out:
//!
//! * **live** - forward each event immediately for minimum latency, inspect the
//!   text as it flows and abort mid-stream if a critical rule fires. A match is
//!   only noticed after the offending bytes have already been forwarded, so the
//!   client may see them.
//! * **buffered** - hold the whole response, audit it, then replay. Zero leak, but
//!   the user loses streaming latency. Chosen automatically when the channel is
//!   untrusted or the response side is in enforce mode with a channel that has
//!   previously produced findings.
//!
//! The mode is announced to the client in `X-Customs-Stream-Mode`.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::stream::{self, StreamExt};

use crate::audit;
use crate::config;
use crate::metering;
use crate::policy;
use crate::protocol;

use super::helpers::LimiterGuard;
use super::protocols::{
    channel_protocol, new_stream_accumulator, StreamAccumulator, ToolCallParts,
};
use super::proxy::{ProxyContext, UpstreamBodyStream, UpstreamResponse};
use super::Server;

/// The head-of-stream verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamMode {
    /// Forward events as they arrive without buffering.
    Live,
    /// Collect the whole response so a blocking rule can be applied before
    /// anything reaches the client.
    DeepBuffer,
}

impl StreamMode {
    /// The value advertised in `X-Customs-Stream-Mode`.
    fn name(self) -> &'static str {
        match self {
            StreamMode::Live => "live",
            StreamMode::DeepBuffer => "buffered",
        }
    }
}

/// A stream rewriter that can cross a task boundary.
///
/// [`protocol::new_stream_translator`] returns `Box<dyn StreamTranslator>`, and
/// that trait object carries no `Send` bound, so it cannot live inside a
/// `Body`/task. The concrete rewriters are plain state machines and are `Send`;
/// the gateway therefore instantiates them directly. The direction table is kept
/// in step with `protocol::stream_translation_supported` (asserted by a test).
trait SendTranslator: protocol::StreamTranslator + Send {}
impl<T: protocol::StreamTranslator + Send> SendTranslator for T {}

/// Builds a rewriter for one direction, or `None` when the direction is not
/// implemented and the stream must be buffered instead.
fn new_translator(from: &str, to: &str) -> Option<Box<dyn SendTranslator>> {
    match (from, to) {
        ("openai", "responses") => Some(Box::new(protocol::ChatToResponsesStream::new())),
        ("openai", "anthropic") => Some(Box::new(protocol::ChatToAnthropicStream::new())),
        _ => None,
    }
}

impl Server {
    /// Proxies a server-sent-event response.
    pub(crate) async fn forward_stream(
        self: &Arc<Self>,
        pc: ProxyContext,
        upstream: UpstreamResponse,
        hold: Option<LimiterGuard>,
    ) -> Response {
        let mut mode = self.choose_stream_mode(&pc);
        // A streamed reply in a foreign protocol must be rewritten event by
        // event. When the direction is not implemented, live forwarding would
        // hand the client a dialect it cannot parse, so the stream is buffered
        // and converted as a whole instead. Correctness first, latency second.
        let upstream_protocol = if pc.upstream_protocol.is_empty() {
            channel_protocol(&pc.channel).to_string()
        } else {
            pc.upstream_protocol.clone()
        };
        let translator = if pc.protocol != upstream_protocol {
            match new_translator(&upstream_protocol, &pc.protocol) {
                Some(translator) => Some(translator),
                None => {
                    mode = StreamMode::DeepBuffer;
                    None
                }
            }
        } else {
            None
        };

        match mode {
            StreamMode::DeepBuffer => self.forward_stream_buffered(pc, upstream).await,
            StreamMode::Live => {
                self.forward_stream_live(pc, upstream, translator, hold)
                    .await
            }
        }
    }

    /// Decides between live and deep-buffered streaming.
    fn choose_stream_mode(&self, pc: &ProxyContext) -> StreamMode {
        let cfg = self.config();
        if cfg.policy.inbound.mode == "report" {
            return StreamMode::Live;
        }
        // Deep buffering costs latency, so it is reserved for the cases where
        // leaking a hostile response is unacceptable: an untrusted channel whose
        // content rules are enforced.
        if pc.channel.trust_level() != config::TRUST_TRUSTED {
            return StreamMode::DeepBuffer;
        }
        StreamMode::Live
    }

    /// Streams live, auditing as the text flows.
    async fn forward_stream_live(
        self: &Arc<Self>,
        pc: ProxyContext,
        upstream: UpstreamResponse,
        translator: Option<Box<dyn SendTranslator>>,
        hold: Option<LimiterGuard>,
    ) -> Response {
        let (status, headers, body) = upstream.into_stream();
        debug_assert_eq!(status, StatusCode::OK);

        let acc = new_stream_accumulator(&pc);
        let detector = policy::StreamDetector::new(&self.engine, "stream");
        let state = LiveState {
            server: self.clone(),
            pc,
            stream: body,
            parser: protocol::SseParser::new(),
            acc,
            det: detector,
            translator,
            seen_text: 0,
            first_byte: true,
            source_open: true,
            finished: false,
            aborted: false,
            abort_message: String::new(),
            hold,
        };
        let body_stream = stream::unfold(state, |mut state| async move {
            loop {
                if state.finished {
                    return None;
                }
                // Drain whatever the parser can already form.
                if let Some(event) = state.parser.next_event() {
                    match state.handle_event(event) {
                        Step::Emit(bytes) => {
                            return Some((Ok::<Bytes, std::io::Error>(bytes), state))
                        }
                        Step::EmitAndFinish(bytes) => {
                            state.finalize();
                            return Some((Ok::<Bytes, std::io::Error>(bytes), state));
                        }
                        Step::Finished => {
                            state.finalize();
                            return None;
                        }
                        Step::Continue => continue,
                    }
                }
                if !state.source_open {
                    state.finalize();
                    return None;
                }
                match state.stream.next().await {
                    Some(Ok(chunk)) => state.parser.push(&chunk),
                    Some(Err(_)) | None => {
                        state.source_open = false;
                        state.parser.finish();
                    }
                }
            }
        });

        let mut response = Response::new(Body::from_stream(body_stream));
        *response.status_mut() = StatusCode::OK;
        copy_headers(response.headers_mut(), &headers);
        insert_header(
            response.headers_mut(),
            "x-customs-stream-mode",
            StreamMode::Live.name(),
        );
        response
    }

    /// Audits a complete stream before delivering any of it.
    ///
    /// Nothing is written to the client until the whole response has been
    /// accumulated and inspected, which is what makes a blocking rule able to
    /// withhold the response with a proper error status. The cost is the loss of
    /// incremental delivery, so this mode is reserved for untrusted channels
    /// whose content rules are enforced.
    async fn forward_stream_buffered(
        self: &Arc<Self>,
        mut pc: ProxyContext,
        upstream: UpstreamResponse,
    ) -> Response {
        let (status, headers, body) = upstream.into_stream();
        debug_assert_eq!(status, StatusCode::OK);
        let mut stream = body;
        let mut parser = protocol::SseParser::new();
        let mut acc = new_stream_accumulator(&pc);
        let mut detector = policy::StreamDetector::new(&self.engine, "stream");

        let mut events: Vec<protocol::SseEvent> = Vec::new();
        let mut seen_text = 0usize;
        let mut first_byte = true;
        let mut source_open = true;
        while source_open {
            match stream.next().await {
                Some(Ok(chunk)) => parser.push(&chunk),
                Some(Err(_)) | None => {
                    source_open = false;
                    parser.finish();
                }
            }
            while let Some(event) = parser.next_event() {
                if first_byte {
                    pc.ttfb_ms = pc.start.elapsed().as_millis() as i64;
                    first_byte = false;
                }
                acc.observe(&event);
                let full_len = acc.text().len();
                if full_len > seen_text {
                    let delta = acc.text()[seen_text..].to_string();
                    detector.observe(&delta);
                    seen_text = full_len;
                }
                let model = protocol::detect_stream_model(&event.raw);
                if !model.is_empty() {
                    pc.observe_upstream_model(&model);
                }
                events.push(event);
            }
        }

        pc.inbound_decision = detector.decision();
        // The rendered body is in the client's protocol, so the same
        // non-streaming audit rules apply and no protocol-specific constructor is
        // needed here.
        let rendered = acc.render();
        if let Some(full) = rendered.as_deref() {
            let decision = self
                .engine
                .check_inbound(Some(&inbound_from_body(&pc, full)));
            if decision.findings.len() > pc.inbound_decision.findings.len() {
                pc.inbound_decision = decision;
            }
        }
        self.meter_stream(&mut pc, acc.as_ref());

        if pc.inbound_decision.blocked() {
            self.stats.blocked_inbound.fetch_add(1, Ordering::Relaxed);
            let message = self.block_message(&pc.inbound_decision, false);
            self.finish_audit(
                &mut pc,
                audit::DECISION_BLOCK_UPSTREAM,
                StatusCode::BAD_GATEWAY.as_u16() as i32,
                &message,
            );
            return super::audit_error(
                StatusCode::BAD_GATEWAY,
                "upstream_content_blocked",
                &message,
                &pc.id,
            );
        }

        // A buffered stream still has to reach the client in its own protocol.
        // When the direction has no event rewriter, the accumulated reply is
        // converted as a single body and delivered as one event, which is correct
        // but not incremental.
        let mut out_events = events;
        let upstream_protocol = if pc.upstream_protocol.is_empty() {
            channel_protocol(&pc.channel).to_string()
        } else {
            pc.upstream_protocol.clone()
        };
        if pc.protocol != upstream_protocol {
            match new_translator(&upstream_protocol, &pc.protocol) {
                Some(mut translator) => {
                    let mut rewritten = Vec::with_capacity(out_events.len());
                    for event in &out_events {
                        rewritten.extend(translator.observe(event));
                    }
                    rewritten.extend(translator.finish());
                    out_events = rewritten;
                }
                None => {
                    let converted = match rendered.as_deref() {
                        Some(full) => protocol::translate_response_body(
                            &upstream_protocol,
                            &pc.protocol,
                            full,
                        ),
                        None => Err(protocol::err_unsupported(&upstream_protocol, &pc.protocol)),
                    };
                    match converted {
                        Ok(converted) => out_events = vec![protocol::SseEvent::data(converted)],
                        Err(e) => {
                            let message = format!("the streamed reply could not be converted: {e}");
                            self.finish_audit(
                                &mut pc,
                                audit::DECISION_UPSTREAM_ERROR,
                                StatusCode::BAD_GATEWAY.as_u16() as i32,
                                &message,
                            );
                            return super::audit_error(
                                StatusCode::BAD_GATEWAY,
                                "translation_failed",
                                &message,
                                &pc.id,
                            );
                        }
                    }
                }
            }
        }

        let mut payload: Vec<u8> = Vec::new();
        {
            let mut writer = protocol::SseWriter::new(&mut payload);
            for event in &out_events {
                if writer.write_event(event).is_err() {
                    break;
                }
                self.stats
                    .bytes_out
                    .fetch_add(event.raw.len() as i64, Ordering::Relaxed);
            }
        }

        let mut response = Response::new(Body::from(payload));
        *response.status_mut() = StatusCode::OK;
        copy_headers(response.headers_mut(), &headers);
        insert_header(
            response.headers_mut(),
            "x-customs-stream-mode",
            StreamMode::DeepBuffer.name(),
        );
        if pc.reply_needs_conversion() {
            insert_header(
                response.headers_mut(),
                "x-customs-translated",
                &format!("{}->{}", upstream_protocol, pc.protocol),
            );
        }
        pc.status_code = StatusCode::OK.as_u16() as i32;
        self.finish_audit(
            &mut pc,
            audit::DECISION_ALLOW,
            StatusCode::OK.as_u16() as i32,
            "",
        );
        response
    }

    /// Finalises metering for a streamed response.
    ///
    /// The local recount is derived from the accumulated stream, because that is
    /// the number the gateway can defend. When the upstream also reported usage
    /// (most do on the final event) the two are compared exactly as in the
    /// buffered path.
    pub(crate) fn meter_stream(&self, pc: &mut ProxyContext, acc: &dyn StreamAccumulator) {
        // The accumulator knows the real model only after the first event, so the
        // tokenizer family is chosen from what the upstream disclosed rather than
        // from the canonical name.
        let model = if acc.model().is_empty() {
            pc.canonical.clone()
        } else {
            acc.model().to_string()
        };
        let estimator = metering::Estimator::for_model(&model);
        let mut completion = estimator.estimate(acc.text()) + estimator.estimate(acc.reasoning());
        for ToolCallParts { name, arguments } in acc.tool_calls() {
            completion += estimator.estimate(&name) + estimator.estimate(&arguments);
        }
        if let Some(upstream) = acc.usage() {
            pc.usage.upstream_prompt_tokens = upstream.prompt_tokens;
            pc.usage.upstream_completion_tokens = upstream.completion_tokens;
            pc.usage.upstream_total_tokens = upstream.total_tokens;
            pc.usage.upstream_reported = true;
            pc.usage.method = "stream_chunks".to_string();
        } else {
            pc.usage.method = "local_only".to_string();
        }
        pc.completion_tokens = completion;
        self.finalize_metering(pc);
    }
}

/// Builds the response-side inspection payload for a rendered stream, so the
/// buffered path can reuse the non-streaming inspectors.
fn inbound_from_body(pc: &ProxyContext, body: &[u8]) -> policy::InspectResponse {
    let trusted = pc.channel.trust_level() == config::TRUST_TRUSTED;
    let empty = |protocol: &str| policy::InspectResponse {
        protocol: protocol.to_string(),
        channel: pc.channel.name.clone(),
        trusted,
        text: Vec::new(),
        tool_calls: Vec::new(),
    };
    match pc.protocol.as_str() {
        "anthropic" => match serde_json::from_slice::<protocol::AnthropicResponse>(body) {
            Ok(parsed) => {
                policy::inspect_response_from_anthropic(&pc.channel.name, trusted, Some(&parsed))
            }
            Err(_) => empty("anthropic"),
        },
        "responses" => match serde_json::from_slice::<protocol::ResponsesResponse>(body) {
            Ok(parsed) => super::protocols::inspect_responses(&pc.channel.name, trusted, &parsed),
            Err(_) => empty("responses"),
        },
        _ => match serde_json::from_slice::<protocol::ChatResponse>(body) {
            Ok(parsed) => {
                policy::inspect_response_from_openai(&pc.channel.name, trusted, Some(&parsed))
            }
            Err(_) => empty("openai"),
        },
    }
}

/// Builds an SSE event carrying an error payload.
fn error_event(message: &str) -> protocol::SseEvent {
    let payload = serde_json::json!({
        "error": {
            "code": "upstream_content_blocked",
            "message": message,
            "type": "upstream_content_blocked",
        },
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    protocol::SseEvent::data(bytes)
}

/// Copies the upstream headers that matter to clients.
fn copy_headers(dst: &mut HeaderMap, src: &HeaderMap) {
    super::helpers::copy_response_headers(dst, src);
}

/// Inserts one response header, ignoring an unusable value.
fn insert_header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

/// What the live state machine should do after one event.
enum Step {
    /// Send these bytes and continue.
    Emit(Bytes),
    /// Send these bytes, then finish.
    EmitAndFinish(Bytes),
    /// Finish without sending anything more.
    Finished,
    /// Nothing to send yet.
    Continue,
}

/// The live streaming state machine.
///
/// It owns everything the exchange still needs: the upstream stream, the SSE
/// parser, the accumulator, the detector and the audit context. When the client
/// disappears mid-stream the value is dropped, and [`LiveState::finalize`] runs
/// from `Drop` so the exchange is still metered and audited - accounting is never
/// skipped.
struct LiveState {
    server: Arc<Server>,
    pc: ProxyContext,
    stream: UpstreamBodyStream,
    parser: protocol::SseParser,
    acc: Box<dyn StreamAccumulator>,
    det: policy::StreamDetector,
    translator: Option<Box<dyn SendTranslator>>,
    seen_text: usize,
    first_byte: bool,
    source_open: bool,
    finished: bool,
    aborted: bool,
    abort_message: String,
    hold: Option<LimiterGuard>,
}

impl LiveState {
    /// Feeds one upstream event through the machine.
    fn handle_event(&mut self, event: protocol::SseEvent) -> Step {
        if self.first_byte {
            self.pc.ttfb_ms = self.pc.start.elapsed().as_millis() as i64;
            self.first_byte = false;
        }
        self.acc.observe(&event);
        let full_len = self.acc.text().len();
        if full_len > self.seen_text {
            // Feed only the new text: the detector keeps its own overlap window
            // so a pattern split across chunks is still caught, but re-scanning
            // the whole generation on every chunk would make a long response
            // quadratic.
            let delta = self.acc.text()[self.seen_text..].to_string();
            self.det.observe(&delta);
            self.seen_text = full_len;
        }
        let model = protocol::detect_stream_model(&event.raw);
        if !model.is_empty() {
            self.pc.observe_upstream_model(&model);
        }

        // Forward the event, rewritten when the client speaks a different
        // protocol from the upstream. Termination is protocol-specific: chat
        // streams end with the `[DONE]` sentinel, the Responses API with a named
        // terminal event, and the upstream's own signal decides.
        let terminated = event.is_done() || self.acc.completed();
        let mut outputs: Vec<protocol::SseEvent> = match self.translator.as_mut() {
            Some(translator) => translator.observe(&event),
            None => vec![event.clone()],
        };
        if terminated {
            // Flush any events the translation still owes, so a client is never
            // left waiting for a terminator the rewriter would have produced.
            if let Some(translator) = self.translator.as_mut() {
                outputs.extend(translator.finish());
            }
        }

        let mut buffer: Vec<u8> = Vec::new();
        {
            let mut writer = protocol::SseWriter::new(&mut buffer);
            for out in &outputs {
                if writer.write_event(out).is_err() {
                    break;
                }
            }
        }
        let written: i64 = outputs.iter().map(|o| o.raw.len() as i64).sum();
        self.server
            .stats
            .bytes_out
            .fetch_add(written, Ordering::Relaxed);

        if terminated {
            self.pc.status_code = StatusCode::OK.as_u16() as i32;
            return if buffer.is_empty() {
                Step::Finished
            } else {
                Step::EmitAndFinish(Bytes::from(buffer))
            };
        }

        let decision = self.det.decision();
        if decision.blocked() {
            self.aborted = true;
            self.abort_message = self.server.block_message(&decision, false);
            self.pc.inbound_decision = decision;
            // Terminate the stream explicitly so a well-behaved client stops
            // instead of waiting for a completion that will never arrive. The
            // `[DONE]` sentinel is harmless for a Responses client, which ignores
            // data it does not recognise.
            {
                let mut writer = protocol::SseWriter::new(&mut buffer);
                let _ = writer.write_event(&error_event(&self.abort_message));
                let _ = writer.write_event(&protocol::SseEvent::data(b"[DONE]".to_vec()));
            }
            self.pc.status_code = StatusCode::OK.as_u16() as i32;
            return Step::EmitAndFinish(Bytes::from(buffer));
        }

        if buffer.is_empty() {
            Step::Continue
        } else {
            Step::Emit(Bytes::from(buffer))
        }
    }

    /// Meters and audits the exchange exactly once.
    fn finalize(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.pc.inbound_decision = self.det.decision();
        if self.pc.status_code == 0 {
            self.pc.status_code = StatusCode::OK.as_u16() as i32;
        }
        self.server.meter_stream(&mut self.pc, self.acc.as_ref());
        if self.aborted {
            self.server
                .stats
                .blocked_inbound
                .fetch_add(1, Ordering::Relaxed);
            self.server.finish_audit(
                &mut self.pc,
                audit::DECISION_BLOCK_UPSTREAM,
                StatusCode::OK.as_u16() as i32,
                &self.abort_message,
            );
            return;
        }
        self.server.finish_audit(
            &mut self.pc,
            audit::DECISION_ALLOW,
            StatusCode::OK.as_u16() as i32,
            "",
        );
    }
}

impl Drop for LiveState {
    fn drop(&mut self) {
        // A client that disconnects mid-stream drops the body; the exchange must
        // still be metered and audited.
        self.finalize();
        let _ = self.hold.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_mode_names_match_the_wire() {
        assert_eq!(StreamMode::Live.name(), "live");
        assert_eq!(StreamMode::DeepBuffer.name(), "buffered");
    }

    #[test]
    fn translator_directions_match_the_protocol_module() {
        for (from, to) in [("openai", "responses"), ("openai", "anthropic")] {
            assert!(
                protocol::stream_translation_supported(from, to),
                "{from}->{to}"
            );
            assert!(new_translator(from, to).is_some(), "{from}->{to}");
        }
        assert!(new_translator("anthropic", "openai").is_none());
        assert!(new_translator("responses", "openai").is_none());
    }

    #[test]
    fn error_events_carry_the_gateway_envelope() {
        let event = error_event("blocked");
        let parsed: serde_json::Value = serde_json::from_slice(&event.data).unwrap();
        assert_eq!(parsed["error"]["type"], "upstream_content_blocked");
        assert_eq!(parsed["error"]["code"], "upstream_content_blocked");
        assert_eq!(parsed["error"]["message"], "blocked");
    }
}
