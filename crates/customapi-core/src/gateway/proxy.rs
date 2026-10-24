//! The proxy pipeline: one entry point for every proxied endpoint.
//!
//! Order of stages is the contract (see PORTING.md and the Go implementation):
//! authenticate, rate/concurrency limit, read the body, decide model and
//! protocol, check quota, prepare (local metering, routing, model rename and
//! translation, outbound policy), forward (with failover), then inbound
//! inspection, metering and the final audit record.
//!
//! Accounting is never skipped: a request the gateway itself rejects still
//! commits one quota unit and still writes an audit record.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use chrono::Utc;
use futures_util::StreamExt;

use crate::audit;
use crate::config;
use crate::meter;
use crate::metering;
use crate::policy;
use crate::protocol;
use crate::route;
use crate::store;

use super::helpers::{
    is_credential_header, is_hop_by_hop, new_request_id, pin_header_of, redact_bytes, sha256_hex,
    upstream_err_text, upstream_status_text, version_short, LimiterGuard,
};
use super::protocols::{
    channel_protocol, failure_class, provider_of, provider_out_of_credit, record_latency,
    to_finding_refs, verdict_of, RebuildError, CLASS_QUOTA, CREDIT_PROBE_BYTES,
};
use super::{
    audit_error, bearer_token, json_error, sanitize_header_value, Server, MAX_BODY_BYTES,
    QUOTA_UNITS_PER_CURRENCY,
};

/// Everything the pipeline learns about one exchange.
#[derive(Debug)]
pub(crate) struct ProxyContext {
    pub start: Instant,
    pub start_wall: chrono::DateTime<Utc>,
    pub id: String,

    pub user: config::User,
    pub channel: config::Channel,

    pub body: Vec<u8>,
    pub forward_body: Vec<u8>,
    pub endpoint: String,
    pub protocol: String,
    pub canonical: String,
    pub upstream_model: String,
    pub stream: bool,

    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub tokenizer_family: metering::TokenizerFamily,

    pub usage: metering::Usage,
    pub deviation: metering::Deviation,
    pub fingerprint: audit::Fingerprint,
    pub cost: audit::Cost,

    pub outbound_decision: policy::Decision,
    pub inbound_decision: policy::Decision,

    pub outbound_ns: i64,
    pub inbound_ns: i64,
    pub upstream_ms: i64,
    pub ttfb_ms: i64,

    pub status_code: i32,
    pub decision: audit::Decision,
    pub err_text: String,
    pub route_note: String,

    pub response_hash: String,
    pub request_hash: String,

    pub quota_after: i64,

    /// The model names the upstream claimed, so a substitution can be recorded
    /// even when no probe was run.
    pub reported_models: Vec<String>,

    /// Every admissible channel in preference order, so a transport failure can
    /// fail over to the next one.
    pub ranked: Vec<route::Candidate>,
    /// Channels already attempted, so failover cannot loop.
    pub tried: std::collections::HashSet<String>,
    /// Counts upstream calls made for this request.
    pub attempt: i32,
    /// Explains a channel switch in the audit record.
    pub failover_note: String,

    /// The credential that served the request, which is what makes per-key
    /// accounting possible.
    pub provider: String,
    pub key: String,

    /// The protocol the chosen channel speaks.
    pub upstream_protocol: String,
    /// Whether the body had to be converted to reach that protocol.
    ///
    /// The two differ in one important case: a provider that declared the
    /// client's protocol among its surfaces (`supports=[openai, responses]` with
    /// `protocol=openai`) is sent the request unchanged, so `translated` is false,
    /// and yet the upstream still speaks a different protocol from the client, so
    /// the reply must be presented in the client's shape. Every use of
    /// `translated` that concerns the *reply* should ask
    /// [`ProxyContext::reply_needs_conversion`] instead; `translated` going into
    /// the audit record answers the narrower question "was the request body
    /// rewritten?".
    pub translated: bool,

    /// Records that the request depends on provider-side conversation state, so
    /// the gateway can neither audit the full prompt nor confirm its token count.
    pub server_state: bool,
}

impl Default for ProxyContext {
    /// A blank context. `Instant` has no `Default`, so the start time is taken
    /// here; every real context overwrites it immediately with the same value.
    fn default() -> Self {
        ProxyContext {
            start: Instant::now(),
            start_wall: chrono::DateTime::<Utc>::default(),
            id: String::new(),
            user: config::User::default(),
            channel: config::Channel::default(),
            body: Vec::new(),
            forward_body: Vec::new(),
            endpoint: String::new(),
            protocol: String::new(),
            canonical: String::new(),
            upstream_model: String::new(),
            stream: false,
            prompt_tokens: 0,
            completion_tokens: 0,
            tokenizer_family: String::new(),
            usage: metering::Usage::default(),
            deviation: metering::Deviation::default(),
            fingerprint: audit::Fingerprint::default(),
            cost: audit::Cost::default(),
            outbound_decision: policy::Decision::default(),
            inbound_decision: policy::Decision::default(),
            outbound_ns: 0,
            inbound_ns: 0,
            upstream_ms: 0,
            ttfb_ms: 0,
            status_code: 0,
            decision: String::new(),
            err_text: String::new(),
            route_note: String::new(),
            response_hash: String::new(),
            request_hash: String::new(),
            quota_after: 0,
            reported_models: Vec::new(),
            ranked: Vec::new(),
            tried: std::collections::HashSet::new(),
            attempt: 0,
            failover_note: String::new(),
            provider: String::new(),
            key: String::new(),
            upstream_protocol: String::new(),
            translated: false,
            server_state: false,
        }
    }
}

impl ProxyContext {
    /// Reports whether the upstream's reply is in a protocol other than the one
    /// the client used, and therefore has to be presented in the client's shape
    /// before it is returned.
    ///
    /// It is not the same question as [`ProxyContext::translated`]: a request
    /// served on a surface the provider declared natively travels unchanged, but
    /// its reply still arrives in that surface's shape.
    pub(crate) fn reply_needs_conversion(&self) -> bool {
        !self.upstream_protocol.is_empty()
            && !self.protocol.is_empty()
            && self.upstream_protocol != self.protocol
    }

    /// Records the model name an upstream claimed.
    pub(crate) fn observe_upstream_model(&mut self, reported: &str) {
        if reported.is_empty() {
            return;
        }
        if self.reported_models.len() < 8 {
            self.reported_models.push(reported.to_string());
        }
    }
}

/// An HTTP-shaped failure that came out of the pipeline before forwarding.
#[derive(Debug, Clone)]
pub(crate) struct RouteFailure {
    pub status: StatusCode,
    pub code: String,
    pub msg: String,
}

/// Why preparation failed.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) enum PrepareError {
    /// A rejection with its own status code and error name.
    Route(RouteFailure),
    /// A gateway fault.
    Other(String),
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// The single entry point for every proxied endpoint.
pub(crate) async fn handle_proxy(State(server): State<Arc<Server>>, req: Request) -> Response {
    let headers = req.headers().clone();
    let query = req.uri().query().unwrap_or_default().to_string();
    let mut pc = ProxyContext {
        start: Instant::now(),
        start_wall: Utc::now(),
        id: new_request_id(),
        endpoint: req.uri().path().to_string(),
        ..ProxyContext::default()
    };
    server.stats.requests.fetch_add(1, Ordering::Relaxed);

    // ---------------------------------------------------------------- auth
    let cfg = server.config();
    let token = bearer_token(&headers);
    let Some(user) = cfg.user_by_token(&token) else {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "the provided API key is not valid for this gateway",
        );
    };
    pc.user = user.clone();
    if !user.is_enabled() {
        return json_error(
            StatusCode::FORBIDDEN,
            "user_suspended",
            "this API key is suspended",
        );
    }

    // ------------------------------------------------------- rate limiting
    if user.rate_limit > 0.0 && !server.allow(&user) {
        return json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_exceeded",
            "request rate limit exceeded; slow down",
        );
    }
    let hold: Option<LimiterGuard> = if user.max_concurrent > 0 {
        match server.acquire(&user.name, user.max_concurrent) {
            Some(guard) => Some(guard),
            None => {
                return json_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "concurrency_limit_exceeded",
                    "too many concurrent requests for this key",
                )
            }
        }
    } else {
        None
    };

    // ---------------------------------------------------------------- body
    let body = match read_request_body(req.into_body(), MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(BodyReadError::TooLarge) => {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                &format!("request body exceeds {MAX_BODY_BYTES} bytes"),
            )
        }
        Err(BodyReadError::Read) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "body_read_error",
                "could not read request body",
            )
        }
    };
    server
        .stats
        .bytes_in
        .fetch_add(body.len() as i64, Ordering::Relaxed);
    pc.body = body;

    // -------------------------------------------------------------- model
    pc.canonical = protocol::model(&pc.body);
    if pc.canonical.is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "missing_model",
            "the request body must contain a model field",
        );
    }
    if !user.allows_model(&pc.canonical) {
        return json_error(
            StatusCode::FORBIDDEN,
            "model_not_allowed",
            &format!("model {:?} is not enabled for this API key", pc.canonical),
        );
    }
    pc.protocol = protocol::detect_protocol(&pc.endpoint, &pc.body);
    pc.stream = protocol::is_streaming(&pc.body);

    // -------------------------------------------------------------- quota
    if let Err(e) = server.store.check(&user, &pc.canonical) {
        // A suspended key is an authorisation failure, not a rate problem; only
        // an exhausted allowance is a 429.
        let (status, code) = match e {
            store::StoreError::UserSuspended(_) => (StatusCode::FORBIDDEN, "user_suspended"),
            store::StoreError::ModelNotAllowed(_) => (StatusCode::FORBIDDEN, "model_not_allowed"),
            _ => (StatusCode::TOO_MANY_REQUESTS, "insufficient_quota"),
        };
        return server.refuse(&mut pc, status, code, &e.to_string());
    }

    // ------------------------------------------ tokenize, route, audit
    match server.prepare(&mut pc, &headers).await {
        Ok(()) => {}
        Err(PrepareError::Route(failure)) => {
            server.finish_audit(
                &mut pc,
                audit::DECISION_UPSTREAM_ERROR,
                failure.status.as_u16() as i32,
                &failure.msg,
            );
            return audit_error(failure.status, &failure.code, &failure.msg, &pc.id);
        }
        Err(PrepareError::Other(message)) => {
            server.finish_audit(
                &mut pc,
                audit::DECISION_UPSTREAM_ERROR,
                StatusCode::BAD_GATEWAY.as_u16() as i32,
                &message,
            );
            return json_error(StatusCode::BAD_GATEWAY, "gateway_error", &message);
        }
    }

    // --------------------------------------------------- outbound policy
    if pc.outbound_decision.blocked() {
        server
            .stats
            .blocked_outbound
            .fetch_add(1, Ordering::Relaxed);
        let message = server.block_message(&pc.outbound_decision, true);
        return server.refuse(
            &mut pc,
            StatusCode::FORBIDDEN,
            "content_policy_violation",
            &message,
        );
    }

    // ----------------------------------------------------------- forward
    server.forward(pc, &headers, &query, hold).await
}

/// Reads a request body, enforcing the size cap.
async fn read_request_body(body: Body, max_bytes: u64) -> Result<Vec<u8>, BodyReadError> {
    use http_body_util::BodyExt;
    let mut body = body;
    let mut out: Vec<u8> = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| BodyReadError::Read)?;
        if let Some(data) = frame.data_ref() {
            out.extend_from_slice(data);
            if out.len() as u64 > max_bytes {
                return Err(BodyReadError::TooLarge);
            }
        }
    }
    Ok(out)
}

/// Why the request body could not be used.
enum BodyReadError {
    TooLarge,
    Read,
}

// ---------------------------------------------------------------------------
// Pipeline stages
// ---------------------------------------------------------------------------

impl Server {
    /// Meters the request, routes it, rewrites the body and runs the outbound
    /// audit.
    async fn prepare(
        &self,
        pc: &mut ProxyContext,
        headers: &HeaderMap,
    ) -> Result<(), PrepareError> {
        let cfg = self.config();

        // --- metering the request -------------------------------------------
        self.prepare_request(pc).map_err(PrepareError::Route)?;

        // --- routing ---------------------------------------------------------
        // The token mix of *this* request determines the price, so the router
        // receives the real counts rather than an average.
        let req = route::Request {
            model: pc.canonical.clone(),
            tokens: meter::Tokens {
                input: pc.prompt_tokens,
                output: 0,
            },
            at: Some(Utc::now()),
            strategy: String::new(),
            pin: pin_header_of(headers),
            exclude: std::collections::HashSet::new(),
            unhealthy: self.health.unhealthy(),
            client_protocol: pc.protocol.clone(),
            prefer_same_protocol: cfg.routing.same_protocol_preferred(),
            allow_translation: cfg.routing.translation_allowed(),
            groups: cfg.routing.groups.clone(),
            exclude_providers: cfg.routing.exclude_providers.clone(),
        };
        let decision = self
            .selector
            .select(&cfg.all_channels(), &req)
            .map_err(|e| {
                PrepareError::Route(RouteFailure {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    code: "no_channel".to_string(),
                    msg: e.to_string(),
                })
            })?;
        pc.channel = decision.chosen.channel.clone();
        pc.upstream_model = decision.chosen.upstream_model.clone();
        pc.route_note = decision.reason.clone();
        // Keep the full ranked list so a transport failure can fail over.
        pc.ranked = decision.ranked.clone();
        pc.tried = std::collections::HashSet::from([pc.channel.name.clone()]);
        let (provider, key) = provider_of(&cfg, &pc.channel.name);
        pc.provider = provider;
        pc.key = key;

        // --- body construction: model rename, then translation ---------------
        match self.rebuild_forward_body(pc) {
            Ok(()) => {}
            Err(RebuildError::Translation(message)) => {
                // A feature the target protocol cannot express is a
                // client-visible problem, not a gateway fault, so it gets a 400
                // naming the field.
                return Err(PrepareError::Route(RouteFailure {
                    status: StatusCode::BAD_REQUEST,
                    code: "translation_unsupported".to_string(),
                    msg: message,
                }));
            }
            Err(RebuildError::Other(message)) => {
                return Err(PrepareError::Route(RouteFailure {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    code: "no_channel".to_string(),
                    msg: message,
                }));
            }
        }
        pc.request_hash = sha256_hex(&redact_bytes(Some(self.redactor.as_ref()), &pc.body));

        // --- outbound policy --------------------------------------------------
        let started = Instant::now();
        let inspect = self.build_inspect_request_for(pc);
        pc.outbound_decision = self.engine.check_outbound(Some(&inspect));
        pc.outbound_ns = started.elapsed().as_nanos() as i64;
        if pc.outbound_decision.degraded {
            self.stats.degraded.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Meters and audits a request the gateway itself rejected.
    ///
    /// A refusal still goes through accounting. The project plan requires that
    /// quota deduction and audit recording are never skipped, for any subject and
    /// any outcome, so that the ledger is a complete record of what the gateway
    /// was asked to do - not just of what it forwarded. Charging one quota unit
    /// makes the refusal visible in a user's usage statement without pretending
    /// that upstream tokens were spent.
    fn refuse(
        &self,
        pc: &mut ProxyContext,
        status: StatusCode,
        code: &str,
        message: &str,
    ) -> Response {
        pc.usage.prompt_tokens = pc.prompt_tokens;
        pc.usage.total_tokens = pc.prompt_tokens;
        pc.usage.family = pc.tokenizer_family.clone();
        pc.usage.method = "local_only".to_string();
        pc.cost = audit::Cost {
            basis: "not_billed".to_string(),
            ..audit::Cost::default()
        };
        if let Ok(state) = self.store.commit(&pc.user.name, 1, 0.0, "", Utc::now()) {
            pc.quota_after = state.remaining();
        }
        self.finish_audit(
            pc,
            audit::DECISION_BLOCK_USER,
            status.as_u16() as i32,
            message,
        );
        audit_error(status, code, message, &pc.id)
    }

    /// Builds a user-facing explanation from a decision.
    pub(crate) fn block_message(&self, decision: &policy::Decision, outbound: bool) -> String {
        let side = if outbound { "request" } else { "response" };
        match decision.highest() {
            Some(finding) => format!(
                "the {} was blocked by the customs gateway (rule {}, severity {})",
                side, finding.rule, finding.severity
            ),
            None => format!("the {} was blocked by the customs gateway", side),
        }
    }
}

// ---------------------------------------------------------------------------
// Upstream interaction
// ---------------------------------------------------------------------------

impl Server {
    /// Builds the request sent to the channel.
    fn build_upstream_request(
        &self,
        pc: &ProxyContext,
        headers: &HeaderMap,
        query: &str,
    ) -> Result<reqwest::Request, String> {
        let path = self.upstream_path(pc);
        let target = config::upstream_url(&pc.channel.base_url, &path);
        if target.is_empty() {
            return Err(format!(
                "gateway: bad channel base_url {:?}",
                pc.channel.base_url
            ));
        }
        let url = if query.is_empty() {
            target
        } else {
            format!("{target}?{query}")
        };

        let mut outgoing = HeaderMap::new();
        for (name, value) in headers.iter() {
            if is_hop_by_hop(name)
                || is_credential_header(name)
                || name.as_str() == "content-length"
            {
                continue;
            }
            let Ok(text) = value.to_str() else { continue };
            let sanitized = sanitize_header_value(text);
            if let Ok(value) = HeaderValue::from_str(&sanitized) {
                outgoing.append(name.clone(), value);
            }
        }
        // Content-Length must describe the body actually being sent. The gateway
        // rewrites the model name, so the forwarded body can differ in length
        // from what the client sent; copying the client's Content-Length would
        // make the upstream wait for bytes that never arrive. reqwest derives the
        // correct value from the body.
        if !outgoing.contains_key(axum::http::header::CONTENT_TYPE) {
            outgoing.insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
        }
        apply_channel_auth(&mut outgoing, &pc.channel);
        for (name, value) in &pc.channel.headers {
            let sanitized = sanitize_header_value(value);
            if let (Ok(name), Ok(value)) = (
                axum::http::header::HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&sanitized),
            ) {
                outgoing.insert(name, value);
            }
        }
        if let Ok(value) = HeaderValue::from_str(&format!("customapi/{}", version_short())) {
            outgoing.insert("x-customs-gateway", value);
        }

        let cfg = self.config();
        self.client
            .post(&url)
            .timeout(cfg.upstream_timeout().0)
            .headers(outgoing)
            .body(pc.forward_body.clone())
            .build()
            .map_err(|e| format!("gateway: bad upstream request: {e}"))
    }

    /// Maps the inbound endpoint onto the surface that will serve it.
    ///
    /// Two different things can put a request on a surface other than the
    /// channel's preferred one:
    ///
    /// * the provider declared the client's protocol among the surfaces it
    ///   serves, in which case the request goes to that endpoint unchanged;
    /// * the payload was converted, in which case it must go to the endpoint that
    ///   accepts the converted protocol, because sending a chat body to
    ///   `/v1/messages` is simply rejected.
    ///
    /// Treating the first case as the second is what made a Responses request for
    /// an aggregator that declares both surfaces arrive on the chat endpoint: the
    /// body was correct for Responses and the URL was not.
    fn upstream_path(&self, pc: &ProxyContext) -> String {
        if !pc.protocol.is_empty()
            && pc.upstream_protocol.is_empty()
            && pc.channel.declares_protocol(&pc.protocol)
        {
            return endpoint_for_protocol(&pc.protocol, &pc.endpoint);
        }
        let target = if pc.upstream_protocol.is_empty() {
            channel_protocol(&pc.channel).to_string()
        } else {
            pc.upstream_protocol.clone()
        };
        if target == pc.protocol {
            return pc.endpoint.clone();
        }
        endpoint_for_protocol(&target, &pc.endpoint)
    }

    /// Performs the upstream exchange and writes the client response.
    ///
    /// Failures that another candidate might survive (5xx, 429, timeout, network)
    /// cause a failover to the next ranked channel, up to `routing.max_attempts`.
    /// Each attempt is a real upstream call that may be billed, so the budget is
    /// enforced strictly and a client-side error (4xx other than 429) never
    /// retries: another provider would fail the same way, or would succeed and
    /// charge for a request the user did not want.
    pub(crate) async fn forward(
        self: &Arc<Self>,
        mut pc: ProxyContext,
        headers: &HeaderMap,
        query: &str,
        hold: Option<LimiterGuard>,
    ) -> Response {
        let policy = self.config().routing.clone();
        loop {
            pc.attempt += 1;
            let upstream_request = match self.build_upstream_request(&pc, headers, query) {
                Ok(request) => request,
                Err(message) => {
                    self.health.failure(&pc.channel.name);
                    if self.retry_after_failure(&mut pc, None, None) {
                        continue;
                    }
                    self.finish_audit(
                        &mut pc,
                        audit::DECISION_UPSTREAM_ERROR,
                        StatusCode::BAD_GATEWAY.as_u16() as i32,
                        &message,
                    );
                    return audit_error(
                        StatusCode::BAD_GATEWAY,
                        "upstream_request_error",
                        &message,
                        &pc.id,
                    );
                }
            };

            let upstream_start = Instant::now();
            let response = match self.client.execute(upstream_request).await {
                Ok(response) => response,
                Err(err) => {
                    self.health.failure(&pc.channel.name);
                    self.stats.upstream_errors.fetch_add(1, Ordering::Relaxed);
                    if self.retry_after_failure(&mut pc, None, Some(&err)) {
                        continue;
                    }
                    let message = format!("upstream request failed: {}", upstream_err_text(&err));
                    self.finish_audit(
                        &mut pc,
                        audit::DECISION_UPSTREAM_ERROR,
                        StatusCode::BAD_GATEWAY.as_u16() as i32,
                        &message,
                    );
                    return audit_error(
                        StatusCode::BAD_GATEWAY,
                        "upstream_unavailable",
                        &message,
                        &pc.id,
                    );
                }
            };
            pc.upstream_ms = upstream_start.elapsed().as_millis() as i64;

            // A retryable status is decided before the body is consumed, so a
            // failing channel is abandoned without paying to read its error page.
            //
            // The exception is the credit case below, which cannot be classified
            // from the status alone: a 403 may mean "your key is banned"
            // (terminal, do not retry) or "this account is out of money" (move to
            // the next provider). Only the latter is worth reading a body for, so
            // only that status is read here.
            let status = response.status();
            if status.as_u16() >= 400
                && policy.should_retry(failure_class(status.as_u16(), None, None))
                && self.retry_after_failure(&mut pc, Some(status.as_u16()), None)
            {
                continue;
            }
            let mut upstream = UpstreamResponse::new(response);
            if status == StatusCode::FORBIDDEN || status == StatusCode::PAYMENT_REQUIRED {
                // Read once, bounded, and hand the bytes to both the classifier
                // and the client - the body the client sees must be the
                // provider's own explanation, not a reconstruction.
                let peek = upstream.peek(CREDIT_PROBE_BYTES).await;
                if provider_out_of_credit(&peek)
                    && policy.should_retry(CLASS_QUOTA)
                    && self.retry_for_class(&mut pc, CLASS_QUOTA)
                {
                    continue;
                }
            }
            self.health.success(&pc.channel.name);

            if pc.stream && status == StatusCode::OK {
                self.stats.streamed_reqs.fetch_add(1, Ordering::Relaxed);
                return self.forward_stream(pc, upstream, hold).await;
            }
            return self.forward_buffered(&mut pc, upstream).await;
        }
    }

    /// Decides whether to move to the next channel.
    ///
    /// `response` carries the status when one arrived; the status is then taken
    /// as zero and only the error is classified.
    fn retry_after_failure(
        &self,
        pc: &mut ProxyContext,
        status: Option<u16>,
        error: Option<&reqwest::Error>,
    ) -> bool {
        self.retry_for_class(pc, failure_class(status.unwrap_or(0), error, None))
    }

    /// Moves to the next channel when the given failure class is retryable.
    ///
    /// It exists so a caller that has already classified a failure - the credit
    /// check has to read the body to know the class - does not have to
    /// reconstruct a response object just to have it re-derived.
    fn retry_for_class(&self, pc: &mut ProxyContext, class: &str) -> bool {
        if class.is_empty() || !self.config().routing.should_retry(class) {
            return false;
        }
        if !self.switch_channel(pc, pc.attempt) {
            return false;
        }
        // Clear per-attempt accounting so the audit record describes the attempt
        // that actually succeeded, not the sum of the failures before it.
        pc.response_hash.clear();
        pc.usage = metering::Usage::default();
        pc.deviation = metering::Deviation::default();
        true
    }

    /// Handles a non-streaming (or failed) upstream response.
    async fn forward_buffered(
        self: &Arc<Self>,
        pc: &mut ProxyContext,
        mut upstream: UpstreamResponse,
    ) -> Response {
        let body = match upstream.read_all(MAX_BODY_BYTES as usize + 1).await {
            Ok(body) => body,
            Err(()) => {
                self.finish_audit(
                    pc,
                    audit::DECISION_UPSTREAM_ERROR,
                    StatusCode::BAD_GATEWAY.as_u16() as i32,
                    "upstream body read failed",
                );
                return audit_error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_read_error",
                    "upstream body read failed",
                    &pc.id,
                );
            }
        };
        let status = upstream.status;
        pc.status_code = status.as_u16() as i32;
        pc.response_hash = sha256_hex(&redact_bytes(Some(self.redactor.as_ref()), &body));

        if status.as_u16() >= 400 {
            self.stats.upstream_errors.fetch_add(1, Ordering::Relaxed);
            // Even a failed response is audited and metered: the upstream may
            // still bill for a partial generation.
            self.inspect_inbound(pc, &body);
            self.meter_response(pc, &body);
            self.finish_audit(
                pc,
                audit::DECISION_UPSTREAM_ERROR,
                status.as_u16() as i32,
                &upstream_status_text(status.as_u16()),
            );
            let mut response = Response::new(Body::from(body.clone()));
            *response.status_mut() = status;
            copy_headers(response.headers_mut(), &upstream.headers);
            set_content_length(response.headers_mut(), body.len());
            return response;
        }

        self.inspect_inbound(pc, &body);
        self.meter_response(pc, &body);

        if pc.inbound_decision.blocked() {
            self.stats.blocked_inbound.fetch_add(1, Ordering::Relaxed);
            let message = self.block_message(&pc.inbound_decision, false);
            self.finish_audit(
                pc,
                audit::DECISION_BLOCK_UPSTREAM,
                StatusCode::BAD_GATEWAY.as_u16() as i32,
                &message,
            );
            return audit_error(
                StatusCode::BAD_GATEWAY,
                "upstream_content_blocked",
                &message,
                &pc.id,
            );
        }

        // Present the reply in the client's protocol whenever the upstream speaks
        // a different one. The audit and metering above already ran on that view,
        // so what the user receives and what was accounted for are the same
        // bytes.
        let mut out = body.clone();
        if pc.reply_needs_conversion() {
            match protocol::translate_response_body(&pc.upstream_protocol, &pc.protocol, &body) {
                Ok(converted) => out = converted,
                Err(e) => {
                    // A reply that cannot be converted is a gateway-side failure,
                    // not something to hand to the client in the wrong shape.
                    let message = format!(
                        "the upstream reply could not be converted from {} to {}: {}",
                        pc.upstream_protocol, pc.protocol, e
                    );
                    self.finish_audit(
                        pc,
                        audit::DECISION_UPSTREAM_ERROR,
                        StatusCode::BAD_GATEWAY.as_u16() as i32,
                        &message,
                    );
                    return audit_error(
                        StatusCode::BAD_GATEWAY,
                        "translation_failed",
                        &message,
                        &pc.id,
                    );
                }
            }
        }

        let mut response = Response::new(Body::from(out.clone()));
        *response.status_mut() = status;
        copy_headers(response.headers_mut(), &upstream.headers);
        // The converted body has a different length, so the upstream's
        // Content-Length would truncate or hang the client.
        set_content_length(response.headers_mut(), out.len());
        if pc.reply_needs_conversion() {
            if let Ok(value) =
                HeaderValue::from_str(&format!("{}->{}", pc.upstream_protocol, pc.protocol))
            {
                response.headers_mut().insert("x-customs-translated", value);
            }
        }
        self.stats
            .bytes_out
            .fetch_add(out.len() as i64, Ordering::Relaxed);
        self.finish_audit(pc, audit::DECISION_ALLOW, status.as_u16() as i32, "");
        response
    }

    /// Runs the response-side policy checks on a buffered body.
    pub(crate) fn inspect_inbound(&self, pc: &mut ProxyContext, body: &[u8]) {
        let started = Instant::now();
        self.inspect_inbound_for(pc, body);
        pc.inbound_ns = started.elapsed().as_nanos() as i64;
    }

    /// Recomputes usage from a buffered response body.
    pub(crate) fn meter_response(&self, pc: &mut ProxyContext, body: &[u8]) {
        self.meter_response_for(pc, body);
    }

    /// Assembles the usage verdict, price and quota update.
    pub(crate) fn finalize_metering(&self, pc: &mut ProxyContext) {
        let cfg = self.config();
        pc.usage.prompt_tokens = pc.prompt_tokens;
        pc.usage.completion_tokens = pc.completion_tokens;
        pc.usage.total_tokens = pc.prompt_tokens + pc.completion_tokens;
        pc.usage.family = pc.tokenizer_family.clone();
        if pc.usage.method.is_empty() {
            pc.usage.method = "local_only".to_string();
        }
        pc.deviation = pc.usage.compare_with(
            cfg.policy.metering.tolerance,
            cfg.policy.metering.min_absolute_delta as i64,
        );
        if pc.deviation.over_threshold {
            self.stats.over_billed.fetch_add(1, Ordering::Relaxed);
            // A channel that inflates usage is untrustworthy regardless of how
            // good its price looks.
            self.selector.degrade_channel(
                &pc.channel.name,
                &route::Deviation {
                    worst: pc.deviation.worst,
                    over_threshold: pc.deviation.over_threshold,
                    direction: pc.deviation.direction.clone(),
                },
            );
        }

        let quote = self.pricer.quote(
            &pc.channel.name,
            &pc.canonical,
            meter::Tokens {
                input: pc.usage.prompt_tokens,
                output: pc.usage.completion_tokens,
            },
            Utc::now(),
        );
        pc.cost = audit::Cost {
            currency: quote.currency.clone(),
            input_cost: quote.input_cost,
            output_cost: quote.output_cost,
            total_cost: quote.cost,
            list_cost: quote.list_cost,
            discount_pct: quote.discount_pct,
            basis: quote.basis.clone(),
            price_model: pc.canonical.clone(),
        };
        let mut units = (quote.cost * QUOTA_UNITS_PER_CURRENCY) as i64;
        if units < 1 {
            // Every metered request costs at least one unit.
            units = 1;
        }
        if let Ok(state) = self.store.commit(
            &pc.user.name,
            units,
            quote.cost,
            &quote.currency,
            Utc::now(),
        ) {
            pc.quota_after = state.remaining();
        }
    }
}

// ---------------------------------------------------------------------------
// Upstream response wrapper
// ---------------------------------------------------------------------------

/// The type of the upstream body stream.
pub(crate) type UpstreamBodyStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// One upstream response, with the ability to read a bounded prefix and still
/// hand the whole body on.
pub(crate) struct UpstreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    stream: UpstreamBodyStream,
}

impl UpstreamResponse {
    fn new(response: reqwest::Response) -> Self {
        let status = response.status();
        let headers = response.headers().clone();
        UpstreamResponse {
            status,
            headers,
            stream: Box::pin(response.bytes_stream()),
        }
    }

    /// Reads up to `limit` bytes without consuming them.
    ///
    /// Go does this with `io.MultiReader(bytes.NewReader(peek), resp.Body)`;
    /// chaining the prefix back onto the stream is the same operation.
    pub(crate) async fn peek(&mut self, limit: usize) -> Vec<u8> {
        let mut peeked: Vec<u8> = Vec::new();
        while peeked.len() < limit {
            match self.stream.next().await {
                Some(Ok(chunk)) => peeked.extend_from_slice(&chunk),
                _ => break,
            }
        }
        let rest = std::mem::replace(&mut self.stream, Box::pin(futures_util::stream::empty()));
        let prefix = Bytes::from(peeked.clone());
        self.stream = Box::pin(
            futures_util::stream::once(async move { Ok::<Bytes, reqwest::Error>(prefix) })
                .chain(rest),
        );
        peeked
    }

    /// Reads the whole body, stopping once `limit` bytes have arrived.
    ///
    /// The caller limits the read; a body larger than the limit is truncated
    /// exactly as Go's `io.ReadAll(io.LimitReader(...))` truncates it.
    pub(crate) async fn read_all(&mut self, limit: usize) -> Result<Vec<u8>, ()> {
        let mut out: Vec<u8> = Vec::new();
        while out.len() < limit {
            match self.stream.next().await {
                Some(Ok(chunk)) => out.extend_from_slice(&chunk),
                Some(Err(_)) => return Err(()),
                None => break,
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    /// Returns the remaining body stream, for the streaming path.
    pub(crate) fn into_stream(self) -> (StatusCode, HeaderMap, UpstreamBodyStream) {
        (self.status, self.headers, self.stream)
    }
}

// ---------------------------------------------------------------------------
// Audit finalisation
// ---------------------------------------------------------------------------

impl Server {
    /// Writes the audit record and records outcome counters.
    ///
    /// Called on every terminal path, including blocked requests, because the
    /// plan requires accounting and audit to be unskippable.
    pub(crate) fn finish_audit(
        &self,
        pc: &mut ProxyContext,
        decision: &str,
        status: i32,
        err_text: &str,
    ) {
        pc.decision = decision.to_string();
        let status = if status == 0 { 200 } else { status };
        pc.status_code = status;
        pc.err_text = err_text.to_string();

        let cfg = self.config();
        let mut record = audit::Record {
            id: pc.id.clone(),
            time: pc.start_wall,
            end_time: Utc::now(),
            actor: pc.user.name.clone(),
            channel: pc.channel.name.clone(),
            provider: pc.provider.clone(),
            key: pc.key.clone(),
            model: pc.canonical.clone(),
            upstream_model: pc.upstream_model.clone(),
            protocol: pc.protocol.clone(),
            upstream_protocol: pc.upstream_protocol.clone(),
            translated: pc.translated,
            server_state: pc.server_state,
            stream: pc.stream,
            endpoint: pc.endpoint.clone(),
            decision: decision.to_string(),
            status_code: status as i64,
            error: err_text.to_string(),
            usage: audit::UsageSummary {
                prompt_tokens: pc.usage.prompt_tokens,
                completion_tokens: pc.usage.completion_tokens,
                total_tokens: pc.usage.total_tokens,
                upstream_prompt_tokens: pc.usage.upstream_prompt_tokens,
                upstream_completion_tokens: pc.usage.upstream_completion_tokens,
                upstream_total_tokens: pc.usage.upstream_total_tokens,
                upstream_reported: pc.usage.upstream_reported,
                tokenizer_family: pc.usage.family.clone(),
                method: pc.usage.method.clone(),
                prompt_delta: pc.deviation.prompt_delta,
                completion_delta: pc.deviation.completion_delta,
                verdict: verdict_of(pc),
                tolerance: cfg.policy.metering.tolerance,
                min_absolute_delta: cfg.policy.metering.min_absolute_delta as i64,
                worst_abs: pc.deviation.worst_abs,
            },
            cost: pc.cost.clone(),
            fingerprint: pc.fingerprint.clone(),
            request_hash: pc.request_hash.clone(),
            response_hash: pc.response_hash.clone(),
            route_reason: pc.route_note.clone(),
            attempts: pc.attempt as i64,
            failover_note: pc.failover_note.clone(),
            quota_after: pc.quota_after,
            latency_ms: record_latency(pc),
            outbound_findings: to_finding_refs(&pc.outbound_decision.findings),
            inbound_findings: to_finding_refs(&pc.inbound_decision.findings),
            ..audit::Record::default()
        };
        // Samples are only ever attached to exchanges that were *allowed*
        // through. A blocked request's payload is exactly the content the audit
        // ledger must not retain, and a finding's excerpt hash is sufficient
        // evidence that a specific input was seen.
        if pc.decision != audit::DECISION_BLOCK_USER
            && pc.decision != audit::DECISION_BLOCK_UPSTREAM
            && self.sample_wanted(pc)
        {
            record.sample = Some(self.build_sample(pc));
        }
        if let Err(e) = self.ledger.append(&mut record) {
            log::error!("audit append failed id={} err={}", pc.id, e);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Copies the upstream headers that matter to clients.
fn copy_headers(dst: &mut HeaderMap, src: &HeaderMap) {
    super::helpers::copy_response_headers(dst, src);
}

/// Sets an exact Content-Length.
fn set_content_length(headers: &mut HeaderMap, len: usize) {
    if let Ok(value) = HeaderValue::from_str(&len.to_string()) {
        headers.insert(axum::http::header::CONTENT_LENGTH, value);
    }
}

/// Maps a protocol onto the endpoint that serves it.
///
/// This is only consulted when the request must change surface - either because
/// the provider declared a different protocol for it, or because the payload was
/// converted. A request that stays on the protocol the caller used keeps the
/// caller's own path, so endpoints the gateway does not model (and the legacy
/// `/v1/completions`) are still forwarded verbatim rather than being rewritten.
fn endpoint_for_protocol(protocol: &str, client_path: &str) -> String {
    match protocol {
        "anthropic" => "/v1/messages".to_string(),
        "responses" => "/v1/responses".to_string(),
        "openai" => "/v1/chat/completions".to_string(),
        _ => {
            if client_path.is_empty() {
                "/v1/chat/completions".to_string()
            } else {
                client_path.to_string()
            }
        }
    }
}

/// Attaches the upstream credential.
///
/// The header differs per protocol, and a provider may require both an
/// Authorization bearer and an x-api-key, so the Anthropic path sets the key
/// header and leaves any Authorization the caller's own provider headers
/// specified alone.
fn apply_channel_auth(headers: &mut HeaderMap, channel: &config::Channel) {
    if channel.api_key.is_empty() {
        return;
    }
    if channel_protocol(channel) == "anthropic" {
        if let Ok(value) = HeaderValue::from_str(&sanitize_header_value(&channel.api_key)) {
            headers.insert("x-api-key", value);
        }
        if !headers.contains_key("anthropic-version") {
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        }
        return;
    }
    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", channel.api_key)) {
        headers.insert(axum::http::header::AUTHORIZATION, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_mapping_covers_every_protocol() {
        assert_eq!(endpoint_for_protocol("anthropic", "/v1/x"), "/v1/messages");
        assert_eq!(endpoint_for_protocol("responses", "/v1/x"), "/v1/responses");
        assert_eq!(
            endpoint_for_protocol("openai", "/v1/x"),
            "/v1/chat/completions"
        );
        assert_eq!(endpoint_for_protocol("other", "/v1/x"), "/v1/x");
        assert_eq!(endpoint_for_protocol("other", ""), "/v1/chat/completions");
    }

    #[test]
    fn reply_conversion_is_not_the_same_question_as_translation() {
        let pc = ProxyContext {
            protocol: "responses".to_string(),
            upstream_protocol: "openai".to_string(),
            translated: false,
            ..ProxyContext::default()
        };
        assert!(pc.reply_needs_conversion());
        let same = ProxyContext {
            protocol: "openai".to_string(),
            upstream_protocol: "openai".to_string(),
            ..ProxyContext::default()
        };
        assert!(!same.reply_needs_conversion());
    }
}
