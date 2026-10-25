//! User-facing and management endpoints.
//!
//! The user surface is deliberately small: a health probe, the model catalogue
//! with live pricing, the caller's own quota, and a price comparison. Everything
//! else under `/v1/` is proxied. The management surface reports the same state
//! an operator needs to answer "who is using what, at what price, and did the
//! upstream tell the truth".

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use chrono::{DateTime, Utc};

use crate::audit;
use crate::discovery;
use crate::meter;
use crate::route::{self, ScoreBook};
use crate::store;
use crate::version;

use super::{bearer_token, bytes_response, json_error, json_response, web, Server};

/// The per-user model filter the catalogue applies, when the key is restricted.
type ModelFilter = Box<dyn Fn(&str) -> bool>;

// ---------------------------------------------------------------------------
// Dispatch: the user router's catch-all
// ---------------------------------------------------------------------------

/// Handles every path the user router did not match exactly.
///
/// `http.ServeMux` picks the longest matching prefix, so `/v1/anything` reaches
/// the proxy and everything else reaches the root handler, which answers 404 for
/// anything but `/`.
pub(crate) async fn handle_user_fallback(
    State(server): State<Arc<Server>>,
    req: Request,
) -> Response {
    let path = req.uri().path().to_string();
    if path.starts_with("/v1/") {
        return super::proxy::handle_proxy(State(server), req).await;
    }
    if path == "/" {
        return handle_root();
    }
    json_error(
        StatusCode::NOT_FOUND,
        "not_found",
        &format!("unknown endpoint {path}"),
    )
}

/// Handles every path the management router did not match exactly.
///
/// Ported from `http.ServeMux`'s longest-prefix matching: `/admin/users/<name>`
/// is a user action, everything under `/admin/` is the service map, the console's
/// own files live under `/ui/`, and the shell answers anything else.
pub(crate) async fn handle_admin_fallback(
    State(server): State<Arc<Server>>,
    req: Request,
) -> Response {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    if path.starts_with("/admin/users/") {
        return handle_user_action(State(server), req).await;
    }
    if path.starts_with("/admin/") {
        return handle_admin_index();
    }
    if path == "/admin" {
        return redirect(&method, "/admin/");
    }
    if let Some(relative) = path.strip_prefix("/ui/") {
        return web::serve_spa(server.web_dir(), relative).await;
    }
    if path == "/ui" {
        return redirect(&method, "/ui/");
    }
    web::serve_spa(server.web_dir(), &path).await
}

/// Emits the redirect `http.ServeMux` produces for a missing trailing slash.
fn redirect(method: &axum::http::Method, target: &str) -> Response {
    let status = if method == axum::http::Method::GET || method == axum::http::Method::HEAD {
        StatusCode::MOVED_PERMANENTLY
    } else {
        StatusCode::TEMPORARY_REDIRECT
    };
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    if let Ok(value) = HeaderValue::from_str(target) {
        response
            .headers_mut()
            .insert(axum::http::header::LOCATION, value);
    }
    response
}

// ---------------------------------------------------------------------------
// Health and identity
// ---------------------------------------------------------------------------

/// `GET /healthz` and `/admin/healthz`.
pub(crate) async fn handle_health(State(server): State<Arc<Server>>) -> Response {
    json_response(StatusCode::OK, &server.health_body())
}

/// `GET /readyz`.
pub(crate) async fn handle_ready(State(server): State<Arc<Server>>) -> Response {
    if server.is_ready() {
        return bytes_response(
            StatusCode::OK,
            "text/plain; charset=utf-8",
            b"ready\n".to_vec(),
        );
    }
    json_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "not_ready",
        "still starting",
    )
}

/// `GET /` on the user listener.
fn handle_root() -> Response {
    let payload = serde_json::json!({
        "endpoints": [
            "/healthz", "/readyz", "/v1/models",
            "/v1/chat/completions", "/v1/completions", "/v1/responses", "/v1/messages",
        ],
        "service": "customapi-customs",
        "version": version::identity(),
    });
    json_response(StatusCode::OK, &payload)
}

/// `GET /admin/version`.
pub(crate) async fn handle_version() -> Response {
    let payload = serde_json::json!({
        "built": version::BUILD_TIME,
        "commit": version::COMMIT,
        "version": version::identity(),
    });
    json_response(StatusCode::OK, &payload)
}

// ---------------------------------------------------------------------------
// User-facing: model catalogue with live pricing
// ---------------------------------------------------------------------------

/// Serves the model catalogue.
///
/// It is OpenAI-shaped (`{"object":"list","data":[...]}`) so any client library
/// can enumerate models, with customs extensions attached under `customs` for
/// clients that understand them. The catalogue is built from the providers'
/// declared models plus whatever discovery found, so a model appears here only if
/// some channel can actually serve it.
pub(crate) async fn handle_models(State(server): State<Arc<Server>>, req: Request) -> Response {
    let cfg = server.config();
    let token = bearer_token(req.headers());
    let Some(user) = cfg.user_by_token(&token) else {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "the provided API key is not valid for this gateway",
        );
    };

    // A user restricted to a set of models sees only those, so the catalogue
    // never advertises something the caller would be refused.
    let allow: Option<ModelFilter> = if user.models.is_empty() {
        None
    } else {
        let models = user.models.clone();
        Some(Box::new(move |model: &str| {
            if models.iter().any(|m| m == model) {
                return true;
            }
            models.iter().any(|m| {
                m.strip_suffix('*')
                    .is_some_and(|prefix| model.starts_with(prefix))
            })
        }))
    };
    let views = server
        .catalogue()
        .models(allow.as_ref().map(|f| f.as_ref() as &dyn Fn(&str) -> bool));

    // Serve a plain list by default; include the routing detail only when the
    // caller asks, because the extra keys confuse some strict clients.
    let verbose = query_param(&req, "detail").as_deref() == Some("true");

    let mut entries: Vec<serde_json::Value> = Vec::with_capacity(views.len());
    for mut view in views {
        server.price_for_model(&mut view);
        let customs = if verbose {
            serde_json::json!({
                "channels": view.channels,
                "protocols": view.protocols,
                "configured": view.configured,
                "cheapest": view.cheapest,
                "unit_price": crate::json::GoFloat(view.unit_price),
                "currency": view.currency,
            })
        } else {
            serde_json::json!({
                "channels": view.channels.len(),
                "protocols": view.protocols,
            })
        };
        entries.push(serde_json::json!({
            "id": view.model,
            "object": "model",
            "owned_by": "customapi-customs",
            "customs": customs,
        }));
    }
    let payload = serde_json::json!({ "data": entries, "object": "list" });
    json_response(StatusCode::OK, &payload)
}

/// Reports the caller's own quota and usage.
pub(crate) async fn handle_my_usage(State(server): State<Arc<Server>>, req: Request) -> Response {
    let token = bearer_token(req.headers());
    let Some(user) = server.config().user_by_token(&token) else {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "invalid API key",
        );
    };
    let state = server.store().get(&user.name).unwrap_or_default();
    let payload = serde_json::json!({
        "consumed": state.consumed,
        "currency": state.currency,
        "last_seen": crate::json::GoTime(state.last_seen),
        "lifetime_consumed": state.lifetime_consumed,
        "period": state.period,
        "period_start": crate::json::GoTime(state.period_start),
        "quota": state.quota,
        "remaining": state.remaining(),
        "requests": state.requests,
        "spend": crate::json::GoFloat(state.spend),
        "unlimited": state.quota <= 0,
        "user": state.name,
    });
    json_response(StatusCode::OK, &payload)
}

// ---------------------------------------------------------------------------
// Management: stats and configuration
// ---------------------------------------------------------------------------

/// `GET /admin/stats`.
pub(crate) async fn handle_stats(State(server): State<Arc<Server>>) -> Response {
    let cfg = server.config();
    let payload = serde_json::json!({
        "audit_head": server.ledger().head(),
        "audit_path": server.audit_path(),
        "audit_seq": server.ledger().seq(),
        "channels": cfg.all_channels().len(),
        "fail_mode": server.engine().fail_mode(),
        "inbound_mode": server.engine().inbound_mode(),
        "outbound_mode": server.engine().outbound_mode(),
        "stats": server.stats_snapshot(),
        "strategy": server.selector().strategy(),
        "uptime_s": server.uptime().as_secs() as i64,
        "users": cfg.users.len(),
    });
    json_response(StatusCode::OK, &payload)
}

/// `GET /admin/config` and `/admin/channels`.
pub(crate) async fn handle_config_summary(State(server): State<Arc<Server>>) -> Response {
    let cfg = server.config();
    let mut channels: Vec<serde_json::Value> = Vec::new();
    for channel in cfg.all_channels() {
        let mut models: Vec<String> = channel.models.keys().cloned().collect();
        models.sort();
        let mut view = serde_json::json!({
            "name": channel.name,
            "base_url": channel.base_url,
            "protocol": channel.protocol,
            "trust": channel.trust_level(),
            "enabled": channel.is_enabled(),
            "weight": crate::json::GoFloat(channel.weight),
            "has_key": !channel.api_key.is_empty(),
        });
        // Go declares `models` with `omitempty`, so a channel with no declared
        // catalogue carries no `models` key at all.
        if !models.is_empty() {
            view["models"] = serde_json::json!(models);
        }
        channels.push(view);
    }
    // Secrets are never echoed; only their presence is reported.
    let payload = serde_json::json!({
        "channels": channels,
        "policy": {
            "inbound_mode": cfg.policy.inbound.mode,
            "inbound_rules": server.engine().inbound_rule_count(),
            "outbound_mode": cfg.policy.outbound.mode,
            "outbound_rules": server.engine().outbound_rule_count(),
            "tolerance": crate::json::GoFloat(cfg.policy.metering.tolerance),
            "tool_policy": cfg.policy.inbound.tool_call_policy,
        },
        "server": {
            "admin_listen": cfg.server.admin_listen,
            "fail_mode": cfg.server.fail_mode,
            "listen": cfg.server.listen,
            "route_strategy": cfg.server.route_strategy,
            "upstream_timeout": crate::json::format_duration(cfg.upstream_timeout().0),
        },
    });
    json_response(StatusCode::OK, &payload)
}

/// `GET /admin/channels` is the configuration summary.
pub(crate) async fn handle_channels(State(server): State<Arc<Server>>) -> Response {
    handle_config_summary(State(server)).await
}

/// A small service map for the management root.
pub(crate) fn handle_admin_index() -> Response {
    let payload = serde_json::json!({
        "endpoints": [
            "/admin/healthz", "/admin/version", "/admin/stats", "/admin/config",
            "/admin/users", "/admin/channels", "/admin/audit", "/admin/audit/verify",
            "/admin/usage", "/admin/prices", "/admin/prices/compare",
            "/admin/scores", "/admin/fingerprints",
        ],
        "service": "customapi-customs-admin",
    });
    json_response(StatusCode::OK, &payload)
}

// ---------------------------------------------------------------------------
// Management: users and quota
// ---------------------------------------------------------------------------

/// `GET|POST /admin/users`.
pub(crate) async fn handle_users(State(server): State<Arc<Server>>, req: Request) -> Response {
    match *req.method() {
        axum::http::Method::GET => {
            let users = server.store().list();
            let payload = serde_json::json!({ "count": users.len(), "users": users });
            json_response(StatusCode::OK, &payload)
        }
        axum::http::Method::POST => handle_user_action(State(server), req).await,
        _ => json_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "use GET or POST",
        ),
    }
}

/// The body accepted by the user management endpoint.
#[derive(Debug, Default, serde::Deserialize)]
struct UserActionRequest {
    #[serde(default)]
    user: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    quota: i64,
    #[serde(default)]
    period: String,
}

/// `POST /admin/users` and `/admin/users/<name>`.
pub(crate) async fn handle_user_action(
    State(server): State<Arc<Server>>,
    req: Request,
) -> Response {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let mut subject = String::new();

    // Accept both POST /admin/users with a body and POST /admin/users/<name>
    // with a body.
    if let Some(name) = path.strip_prefix("/admin/users/") {
        if !name.is_empty() {
            subject = name.to_string();
        }
    }

    let mut request = UserActionRequest::default();
    if method == axum::http::Method::POST {
        let body = match read_body(req).await {
            Ok(body) => body,
            Err(response) => return response,
        };
        if !body.iter().all(|b| b.is_ascii_whitespace()) {
            match serde_json::from_slice::<UserActionRequest>(&body) {
                Ok(parsed) => request = parsed,
                Err(_) => {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_body",
                        "could not parse request body",
                    )
                }
            }
        }
    }
    if !request.user.is_empty() {
        subject = request.user;
    }
    if subject.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "missing_user", "user is required");
    }
    let action = request.action;
    if action.is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "missing_action",
            "action is required",
        );
    }

    let outcome = match action.as_str() {
        "grant" | "set_quota" => server.store().set_quota(&subject, request.quota),
        "add_quota" => server.store().add_quota(&subject, request.quota),
        "suspend" => server.store().set_enabled(&subject, false),
        "resume" => server.store().set_enabled(&subject, true),
        "set_period" => server.store().set_period(&subject, &request.period),
        _ => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "unknown_action",
                &format!("action {:?} is not recognised", action),
            )
        }
    };
    match outcome {
        Ok(state) => {
            let payload = serde_json::json!({ "action": action, "ok": true, "user": state });
            json_response(StatusCode::OK, &payload)
        }
        Err(e) => json_error(StatusCode::NOT_FOUND, "user_error", &e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Management: audit
// ---------------------------------------------------------------------------

/// `GET /admin/audit`.
pub(crate) async fn handle_audit(State(server): State<Arc<Server>>, req: Request) -> Response {
    let records = match audit::read_all(&server.audit_path()) {
        Ok(records) => records,
        Err(e) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "audit_read_error",
                &e.to_string(),
            )
        }
    };
    let q = Query::new(&req);
    let mut filtered = filter_records(
        records,
        q.get("actor"),
        q.get("channel"),
        q.get("model"),
        q.get("decision"),
        q.get("verdict"),
    );
    // Newest first, then paginate.
    filtered.sort_by_key(|record| std::cmp::Reverse(record.seq));

    let mut limit = int_param(q.get("limit"), 100);
    if limit > 1000 {
        limit = 1000;
    }
    let offset = int_param(q.get("offset"), 0).max(0) as usize;
    let limit = limit.max(0) as usize;
    let total = filtered.len();
    let start = offset.min(total);
    let end = (start + limit).min(total);
    let mut page = filtered[start..end].to_vec();

    // Strip samples unless explicitly asked for: an operator browsing the list
    // should not accidentally read user content.
    if q.get("include_samples") != Some("true".to_string()) {
        for record in &mut page {
            record.sample = None;
        }
    }
    let payload = serde_json::json!({
        "limit": limit,
        "offset": start,
        "records": page,
        "total": total,
    });
    json_response(StatusCode::OK, &payload)
}

/// `GET /admin/audit/verify`.
pub(crate) async fn handle_audit_verify(State(server): State<Arc<Server>>) -> Response {
    match audit::verify(&server.audit_path(), &server.anchor_path()) {
        Ok(result) => {
            let status = if result.ok {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            };
            json_response(status, &serde_json::to_value(&result).unwrap_or_default())
        }
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "verify_error",
            &e.to_string(),
        ),
    }
}

/// Applies the query filters used by the audit browser.
fn filter_records(
    records: Vec<audit::Record>,
    actor: Option<String>,
    channel: Option<String>,
    model: Option<String>,
    decision: Option<String>,
    verdict: Option<String>,
) -> Vec<audit::Record> {
    let actor = actor.unwrap_or_default();
    let channel = channel.unwrap_or_default();
    let model = model.unwrap_or_default();
    let decision = decision.unwrap_or_default();
    let verdict = verdict.unwrap_or_default();
    if actor.is_empty()
        && channel.is_empty()
        && model.is_empty()
        && decision.is_empty()
        && verdict.is_empty()
    {
        return records;
    }
    records
        .into_iter()
        .filter(|r| {
            (actor.is_empty() || r.actor == actor)
                && (channel.is_empty() || r.channel == channel)
                && (model.is_empty() || r.model == model)
                && (decision.is_empty() || r.decision == decision)
                && (verdict.is_empty() || r.usage.verdict == verdict)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Management: usage aggregates
// ---------------------------------------------------------------------------

/// `GET /admin/usage`.
pub(crate) async fn handle_usage(State(server): State<Arc<Server>>, req: Request) -> Response {
    let records = match audit::read_all(&server.audit_path()) {
        Ok(records) => records,
        Err(e) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "audit_read_error",
                &e.to_string(),
            )
        }
    };
    let q = Query::new(&req);
    let records = filter_records(
        records,
        q.get("actor"),
        q.get("channel"),
        q.get("model"),
        None,
        None,
    );
    let (models, channels) = store::aggregate(&records);
    let users = server.store().list();
    let payload = serde_json::json!({
        "channels": channels,
        "models": models,
        "records": records.len(),
        "users": users,
    });
    json_response(StatusCode::OK, &payload)
}

// ---------------------------------------------------------------------------
// Prices
// ---------------------------------------------------------------------------

/// One row of the price table, shaped like the Go handler's local `row` struct.
///
/// A typed response keeps the bytes identical to the Go build: the field order is
/// the struct's, `scheme` and `source` disappear when empty because their Go tags
/// carry `omitempty`, and the three rates are floats that Go writes without a
/// trailing `.0`.
#[derive(serde::Serialize)]
struct PriceRow {
    channel: String,
    model: String,
    trust: String,
    #[serde(serialize_with = "crate::json::go_f64")]
    input_per_m: f64,
    #[serde(serialize_with = "crate::json::go_f64")]
    output_per_m: f64,
    #[serde(serialize_with = "crate::json::go_f64")]
    effective_unit_price: f64,
    currency: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    scheme: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    source: String,
}

/// The `/admin/prices` envelope.
#[derive(serde::Serialize)]
struct PricesView {
    /// Go declares `var rows []row`, so an empty catalogue serialises as `null`.
    #[serde(serialize_with = "crate::json::go_slice_or_null")]
    prices: Vec<PriceRow>,
}

/// `GET /admin/prices`.
pub(crate) async fn handle_prices(State(server): State<Arc<Server>>) -> Response {
    let cfg = server.config();
    let now = Utc::now();
    let pricer = server.pricer();
    let location = pricer.location();
    let mut rows: Vec<PriceRow> = Vec::new();
    for channel in cfg.all_channels() {
        for model in pricer.models(&channel.name) {
            let Some(price) = pricer.lookup(&channel.name, &model) else {
                continue;
            };
            rows.push(PriceRow {
                channel: channel.name.clone(),
                model,
                trust: channel.trust_level().to_string(),
                input_per_m: price.input_per_m,
                output_per_m: price.output_per_m,
                effective_unit_price: meter::effective_unit_price(
                    &price,
                    meter::Tokens::default(),
                    now,
                    Some(location),
                ),
                currency: price.currency.clone(),
                scheme: price.scheme.clone(),
                source: price.source.clone(),
            });
        }
    }
    // Go sorts by model, then by unit price.
    rows.sort_by(|a, b| {
        a.model.cmp(&b.model).then_with(|| {
            a.effective_unit_price
                .partial_cmp(&b.effective_unit_price)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    json_response(StatusCode::OK, &PricesView { prices: rows })
}

/// Answers "what would this cost, and which channel is cheapest" for a concrete
/// token mix - the comparison the plan wants users to be able to browse.
pub(crate) async fn handle_price_compare(
    State(server): State<Arc<Server>>,
    req: Request,
) -> Response {
    let q = Query::new(&req);
    let model = q.get("model").unwrap_or_default();
    let input = int_param(q.get("input_tokens"), 1000) as i64;
    let output = int_param(q.get("output_tokens"), 500) as i64;
    let mut at = Utc::now();
    if let Some(text) = q.get("at") {
        if let Ok(parsed) = DateTime::parse_from_rfc3339(&text) {
            at = parsed.with_timezone(&Utc);
        }
    }
    let tokens = meter::Tokens { input, output };

    let cfg = server.config();
    let pricer = server.pricer();
    let location = pricer.location();
    let mut offers: Vec<CompareOffer> = Vec::new();
    for channel in cfg.all_channels() {
        if !channel.is_enabled() {
            continue;
        }
        if !model.is_empty() && channel.upstream_model(&model).is_none() {
            continue;
        }
        let Some(price) = pricer.lookup(&channel.name, &model) else {
            continue;
        };
        let quote = meter::price_with(&price, tokens, at, Some(location));
        let unit = if tokens.total() > 0 {
            quote.cost / (tokens.total() as f64 / 1e6)
        } else {
            0.0
        };
        offers.push(CompareOffer {
            channel: channel.name.clone(),
            currency: quote.currency.clone(),
            cost: quote.cost,
            list_cost: quote.list_cost,
            discount_pct: quote.discount_pct,
            unit_price: unit,
            promotions: quote.applied.clone(),
            trust: channel.trust_level().to_string(),
        });
    }
    // Go sorts the offers by cost, ascending.
    offers.sort_by(|a, b| {
        a.cost
            .partial_cmp(&b.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Go builds this payload as a map, so its members come out sorted; the
    // cheapest channel is only present when at least one offer was priced.
    let view = CompareView {
        at: at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        cheapest: offers.first().map(|offer| offer.channel.clone()),
        input_tokens: input,
        model,
        offers,
        output_tokens: output,
    };
    json_response(StatusCode::OK, &view)
}

/// One priced channel in the comparison view, shaped like Go's `offer` struct.
#[derive(serde::Serialize)]
struct CompareOffer {
    channel: String,
    currency: String,
    #[serde(serialize_with = "crate::json::go_f64")]
    cost: f64,
    #[serde(serialize_with = "crate::json::go_f64")]
    list_cost: f64,
    #[serde(serialize_with = "crate::json::go_f64")]
    discount_pct: f64,
    #[serde(serialize_with = "crate::json::go_f64")]
    unit_price: f64,
    /// Go's tag carries `omitempty`, and a Go slice of applied promotions is nil
    /// unless one fired, so both cases render as absent.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    promotions: Vec<meter::AppliedPromotion>,
    trust: String,
}

/// The `/admin/prices/compare` envelope, in Go's sorted-key order.
#[derive(serde::Serialize)]
struct CompareView {
    at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cheapest: Option<String>,
    input_tokens: i64,
    model: String,
    /// Go declares `var offers []offer`, so "no channel can serve this" is `null`.
    #[serde(serialize_with = "crate::json::go_slice_or_null")]
    offers: Vec<CompareOffer>,
    output_tokens: i64,
}

// ---------------------------------------------------------------------------
// Intelligence views
// ---------------------------------------------------------------------------

/// `GET /admin/scores`.
pub(crate) async fn handle_scores(State(server): State<Arc<Server>>) -> Response {
    let cfg = server.config();
    let pricer = server.pricer();
    let mut rows: Vec<ScoreRow> = Vec::new();
    for channel in cfg.all_channels() {
        let mut models: BTreeMap<String, ()> = BTreeMap::new();
        for model in pricer.models(&channel.name) {
            models.insert(model, ());
        }
        for model in channel.models.keys() {
            models.insert(model.clone(), ());
        }
        for model in models.keys() {
            if let Some(scores) = server.scores().score(&channel.name, model) {
                rows.push(ScoreRow {
                    channel: channel.name.clone(),
                    model: model.clone(),
                    scores,
                });
            }
        }
    }
    // Go sorts by model, then by channel.
    rows.sort_by(|a, b| {
        a.model
            .cmp(&b.model)
            .then_with(|| a.channel.cmp(&b.channel))
    });
    json_response(StatusCode::OK, &ScoresView { scores: rows })
}

/// One row of the scorecard view: a channel/model pair and its scores.
#[derive(serde::Serialize)]
struct ScoreRow {
    channel: String,
    model: String,
    /// `route::Scores` writes its floats and timestamp the way Go does.
    scores: route::Scores,
}

/// The `/admin/scores` envelope.
#[derive(serde::Serialize)]
struct ScoresView {
    /// Go declares `var rows []row`, so "nothing measured yet" is `null`.
    #[serde(serialize_with = "crate::json::go_slice_or_null")]
    scores: Vec<ScoreRow>,
}

/// Reports the model-identity evidence collected from audit.
pub(crate) async fn handle_fingerprints(State(server): State<Arc<Server>>) -> Response {
    let records = match audit::read_all(&server.audit_path()) {
        Ok(records) => records,
        Err(e) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "audit_read_error",
                &e.to_string(),
            )
        }
    };
    #[derive(Default)]
    struct Aggregate {
        channel: String,
        model: String,
        checked: i64,
        mismatch: i64,
        mean_score: f64,
    }
    let mut index: BTreeMap<(String, String), Aggregate> = BTreeMap::new();
    for record in records {
        if !record.fingerprint.checked {
            continue;
        }
        let key = (record.channel.clone(), record.model.clone());
        let entry = index.entry(key).or_insert_with(|| Aggregate {
            channel: record.channel.clone(),
            model: record.model.clone(),
            ..Aggregate::default()
        });
        entry.checked += 1;
        entry.mean_score += (record.fingerprint.score - entry.mean_score) / entry.checked as f64;
        if record.fingerprint.verdict == "mismatch" {
            entry.mismatch += 1;
        }
    }
    let out: Vec<serde_json::Value> = index
        .into_values()
        .map(|a| {
            serde_json::json!({
                "channel": a.channel,
                "model": a.model,
                "checked": a.checked,
                "mismatch": a.mismatch,
                "mean_score": crate::json::GoFloat(a.mean_score),
            })
        })
        .collect();
    json_response(StatusCode::OK, &serde_json::json!({ "fingerprints": out }))
}

// ---------------------------------------------------------------------------
// Pricing enrichment
// ---------------------------------------------------------------------------

impl Server {
    /// Reports the cheapest effective unit price across the channels serving a
    /// model, which is what the comparison view sorts on.
    pub(crate) fn price_for_model(&self, view: &mut discovery::ModelView) {
        let now = Utc::now();
        let location = self.pricer.location();
        let mut best = 0.0f64;
        let mut best_channel = String::new();
        let mut currency = String::new();
        for candidate in &view.channels {
            let Some(price) = self.pricer.lookup(&candidate.channel, &view.model) else {
                continue;
            };
            let unit =
                meter::effective_unit_price(&price, meter::Tokens::default(), now, Some(location));
            if unit <= 0.0 {
                continue;
            }
            if best == 0.0 || unit < best {
                best = unit;
                best_channel = candidate.channel.clone();
                currency = price.currency.clone();
            }
        }
        view.cheapest = best_channel;
        view.unit_price = best;
        view.currency = currency;
    }
}

// ---------------------------------------------------------------------------
// Small parameter helpers
// ---------------------------------------------------------------------------

/// Read access to query parameters.
struct Query {
    pairs: Vec<(String, String)>,
}

impl Query {
    fn new(req: &Request) -> Query {
        let mut pairs = Vec::new();
        if let Some(query) = req.uri().query() {
            for pair in query.split('&') {
                if pair.is_empty() {
                    continue;
                }
                let (key, value) = match pair.split_once('=') {
                    Some((key, value)) => (key, value),
                    None => (pair, ""),
                };
                pairs.push((percent_decode(key), percent_decode(value)));
            }
        }
        Query { pairs }
    }

    /// Returns the first value for a key.
    fn get(&self, key: &str) -> Option<String> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }
}

/// Decodes one query component.
fn percent_decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .into_owned()
}

/// Reads one query parameter without building the whole map.
pub(crate) fn query_param(req: &Request, key: &str) -> Option<String> {
    Query::new(req).get(key)
}

/// Parses an integer query parameter, falling back to a default.
///
/// Go's `fmt.Sscanf("%d")` accepts leading whitespace and ignores everything
/// after the digits, so `limit=10abc` means ten; reproducing that here keeps the
/// admin API tolerant of the same sloppy input.
fn int_param(value: Option<String>, default: i32) -> i32 {
    let Some(text) = value else {
        return default;
    };
    let trimmed = text.trim_start();
    let mut chars = trimmed.char_indices().peekable();
    let mut end = 0;
    if let Some((_, sign)) = chars.peek() {
        if *sign == '-' || *sign == '+' {
            chars.next();
            end = 1;
        }
    }
    let mut digits = 0;
    while let Some((index, ch)) = chars.peek() {
        if !ch.is_ascii_digit() {
            break;
        }
        end = index + ch.len_utf8();
        digits += 1;
        chars.next();
    }
    if digits == 0 {
        return default;
    }
    trimmed[..end].parse::<i32>().unwrap_or(default)
}

/// Reads a request body for a management call, bounded like the proxy's.
#[allow(clippy::result_large_err)]
async fn read_body(req: Request) -> Result<Vec<u8>, Response> {
    use http_body_util::BodyExt;
    let mut body = req.into_body();
    let mut out: Vec<u8> = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(_) => {
                return Err(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_body",
                    "could not read request body",
                ))
            }
        };
        if let Some(data) = frame.data_ref() {
            out.extend_from_slice(data);
            if out.len() as u64 > super::MAX_BODY_BYTES {
                return Err(json_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "body_too_large",
                    "request body too large",
                ));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_param_falls_back_on_garbage() {
        assert_eq!(int_param(None, 100), 100);
        assert_eq!(int_param(Some(String::new()), 7), 7);
        assert_eq!(int_param(Some("42".to_string()), 0), 42);
        assert_eq!(int_param(Some(" 42 ".to_string()), 0), 42);
        assert_eq!(int_param(Some("nope".to_string()), 7), 7);
        assert_eq!(int_param(Some("-3".to_string()), 7), -3);
        // `fmt.Sscanf("%d")` stops at the first non-digit rather than failing.
        assert_eq!(int_param(Some("10abc".to_string()), 7), 10);
        assert_eq!(int_param(Some("+5".to_string()), 7), 5);
    }

    #[test]
    fn redirects_use_go_statuses() {
        let get = redirect(&axum::http::Method::GET, "/admin/");
        assert_eq!(get.status(), StatusCode::MOVED_PERMANENTLY);
        let post = redirect(&axum::http::Method::POST, "/admin/");
        assert_eq!(post.status(), StatusCode::TEMPORARY_REDIRECT);
    }

    #[test]
    fn route_records_are_filtered_independently() {
        let record = |actor: &str, channel: &str| audit::Record {
            actor: actor.to_string(),
            channel: channel.to_string(),
            ..audit::Record::default()
        };
        let records = vec![record("alice", "a"), record("bob", "b")];
        let filtered = filter_records(
            records.clone(),
            Some("alice".into()),
            None,
            None,
            None,
            None,
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].actor, "alice");
        assert_eq!(
            filter_records(records, None, None, None, None, None).len(),
            2
        );
    }
}
