//! Provider management views: what the console shows about upstreams.
//!
//! A provider is expanded into one channel per enabled key, so the per-key view
//! is where an operator actually works: which key is being rate-limited, which
//! one costs more, and which one has stopped answering.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;

use crate::audit;
use crate::config;
use crate::discovery;
use crate::protocol;

use super::{json_error, json_response, Server};

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// One provider as the front end sees it.
#[derive(Debug, Serialize)]
struct ProviderView {
    #[serde(flatten)]
    summary: config::ProviderSummary,
    /// The expanded channels, one per enabled key.
    channels: Vec<ProviderChannelView>,
    /// Per-channel runtime state.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    health: Vec<ChannelHealth>,
    /// Every model this provider can serve, from the catalogue.
    ///
    /// It is a list rather than a count because the front end needs the names;
    /// the embedded summary keeps the count under `models` for callers that only
    /// want the number.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    model_list: Vec<String>,
    /// The client protocols this provider can accept by conversion, i.e. every
    /// protocol it speaks other than its own.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    supports_translation: Vec<String>,
}

/// One expanded channel.
#[derive(Debug, Default, Serialize)]
struct ProviderChannelView {
    channel: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    key: String,
    protocol: String,
    protocols: Vec<String>,
    has_key: bool,
    enabled: bool,
    #[serde(serialize_with = "crate::json::go_f64")]
    weight: f64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    groups: Vec<String>,
    model_count: usize,
    #[serde(skip_serializing_if = "String::is_empty")]
    discovered: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stale: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    discover_error: String,
}

/// The live state of one channel.
#[derive(Debug, Default, Serialize)]
struct ChannelHealth {
    channel: String,
    /// The consecutive failure count.
    failures: i32,
    /// False while the circuit breaker is open.
    healthy: bool,
    /// Taken from the audit ledger.
    requests: i64,
    errors: i64,
    /// 1 - clamped worst deviation, from the ledger.
    #[serde(serialize_with = "crate::json::go_f64")]
    usage_integrity: f64,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Lists configured providers with their keys and live health.
pub(crate) async fn handle_providers(State(server): State<Arc<Server>>) -> Response {
    let cfg = server.config();
    let summaries = cfg.summarize_providers();
    let catalogue = server.catalogue().channels();
    let mut by_channel: HashMap<String, discovery::ChannelModels> = HashMap::new();
    for models in catalogue.iter() {
        by_channel.insert(models.channel.clone(), models.clone());
    }
    let unhealthy = server.health().unhealthy();

    // Aggregate the audit ledger once, for per-channel request counts.
    let records = server.read_audit();
    let (_, channel_stats) = aggregate_for_view(&records);
    let stat_by_channel: HashMap<String, ChannelStat> = channel_stats
        .into_iter()
        .map(|cs| (cs.channel.clone(), cs))
        .collect();

    let mut out: Vec<ProviderView> = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let mut view = ProviderView {
            summary: summary.clone(),
            channels: Vec::new(),
            health: Vec::new(),
            model_list: Vec::new(),
            supports_translation: Vec::new(),
        };
        for key in &summary.keys {
            let configured = cfg.channel_by_name(&key.channel);
            let mut channel_view = ProviderChannelView {
                channel: key.channel.clone(),
                key: key.name.clone(),
                protocol: summary.protocol.clone(),
                protocols: summary.protocols.clone(),
                has_key: key.has_key,
                enabled: key.enabled && summary.enabled,
                groups: summary.groups.clone(),
                ..ProviderChannelView::default()
            };
            if let Some(channel) = configured {
                channel_view.weight = channel.weight;
                if let Some(models) = by_channel.get(&key.channel) {
                    channel_view.model_count = models.models.len();
                    channel_view.stale = models.stale;
                    channel_view.discover_error = models.error.clone();
                    channel_view.discovered =
                        models.discovered.format("%Y-%m-%d %H:%M:%S").to_string();
                }
                for entry in catalogue.iter().filter(|m| m.channel == key.channel) {
                    for model in &entry.models {
                        append_unique(&mut view.model_list, &model.model);
                    }
                }
            }
            view.channels.push(channel_view);

            let mut health = ChannelHealth {
                channel: key.channel.clone(),
                healthy: !unhealthy.contains(&key.channel),
                failures: server.health().failure_count(&key.channel),
                ..ChannelHealth::default()
            };
            if let Some(stats) = stat_by_channel.get(&key.channel) {
                health.requests = stats.requests;
                health.errors = stats.errors;
                health.usage_integrity = stats.integrity;
            }
            view.health.push(health);
        }
        // A provider can accept any protocol it speaks *and* any protocol the
        // gateway can translate into one of those.
        for candidate in ["openai", "responses", "anthropic"] {
            if candidate == summary.protocol {
                continue;
            }
            if !contains_string(&summary.protocols, candidate) {
                view.supports_translation.push(candidate.to_string());
            }
        }
        view.model_list.sort();
        out.push(view);
    }

    // Directly-configured channels have no provider wrapper; report them so
    // nothing is invisible in the UI.
    let mut direct: Vec<ProviderChannelView> = Vec::new();
    for channel in cfg.all_channels() {
        if cfg.provider_by_channel_name(&channel.name).is_some() {
            // Providers are reported above, with their keys and health.
            continue;
        }
        let mut view = ProviderChannelView {
            channel: channel.name.clone(),
            protocol: super::protocols::channel_protocol(&channel).to_string(),
            has_key: !channel.api_key.is_empty(),
            enabled: channel.is_enabled(),
            weight: channel.weight,
            ..ProviderChannelView::default()
        };
        if let Some(models) = by_channel.get(&channel.name) {
            view.model_count = models.models.len();
            view.stale = models.stale;
            view.discover_error = models.error.clone();
        }
        direct.push(view);
    }

    let protocols: Vec<serde_json::Value> = [
        ("openai", "/v1/chat/completions"),
        ("responses", "/v1/responses"),
        ("anthropic", "/v1/messages"),
    ]
    .iter()
    .map(|(id, endpoint)| {
        serde_json::json!({
            "id": id,
            "name": protocol::protocol_display_name(id),
            "endpoint": endpoint,
        })
    })
    .collect();

    let payload = serde_json::json!({
        "direct_channels": direct,
        "groups": cfg.group_names(),
        "protocols": protocols,
        "providers": out,
        "routing": {
            "allow_translation": cfg.routing.translation_allowed(),
            "max_attempts": cfg.routing.attempts(),
            "prefer_same_protocol": cfg.routing.same_protocol_preferred(),
            "retry_on": cfg.routing.retry_on,
            "strategy": server.selector().strategy(),
        },
    });
    json_response(StatusCode::OK, &payload)
}

/// Serves the per-channel catalogue with staleness.
pub(crate) async fn handle_admin_models(
    State(server): State<Arc<Server>>,
    req: Request,
) -> Response {
    let snapshot = server.catalogue().snapshot();
    let mut views = server.catalogue().models(None);

    if let Some(provider) = super::api::query_param(&req, "provider") {
        if !provider.is_empty() {
            views.retain(|view| {
                view.channels
                    .iter()
                    .any(|channel| channel.provider == provider)
            });
        }
    }

    let payload = serde_json::json!({
        "channels": server.catalogue().channels(),
        "errors": snapshot.errors,
        "models": views,
        "providers": snapshot.providers,
        "updated_at": crate::json::GoTime(snapshot.updated_at),
    });
    json_response(StatusCode::OK, &payload)
}

/// Forces a discovery run and returns the new catalogue.
pub(crate) async fn handle_models_refresh(
    State(server): State<Arc<Server>>,
    req: Request,
) -> Response {
    if req.method() != axum::http::Method::POST {
        return json_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "use POST",
        );
    }
    let cfg = server.config();
    server.catalogue().refresh(&cfg).await;
    let payload = serde_json::json!({
        "models": server.catalogue().models(None).len(),
        "ok": true,
    });
    json_response(StatusCode::OK, &payload)
}

// ---------------------------------------------------------------------------
// Aggregates
// ---------------------------------------------------------------------------

/// The subset of aggregate data the provider view needs.
#[derive(Debug, Clone)]
struct ChannelStat {
    channel: String,
    requests: i64,
    errors: i64,
    integrity: f64,
}

impl Server {
    /// Loads the audit ledger for aggregation. A missing ledger is not an error:
    /// a freshly started gateway simply has no records yet.
    pub(crate) fn read_audit(&self) -> Vec<audit::Record> {
        audit::read_all(&self.audit_path()).unwrap_or_default()
    }
}

/// Computes per-channel counters from the audit ledger.
fn aggregate_for_view(records: &[audit::Record]) -> (usize, Vec<ChannelStat>) {
    #[derive(Default)]
    struct Accumulator {
        requests: i64,
        errors: i64,
        verifiable: i64,
        sum: f64,
    }
    let mut index: HashMap<String, Accumulator> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for record in records {
        if !index.contains_key(&record.channel) {
            index.insert(record.channel.clone(), Accumulator::default());
            order.push(record.channel.clone());
        }
        let acc = index.get_mut(&record.channel).expect("just inserted");
        acc.requests += 1;
        if record.decision == audit::DECISION_UPSTREAM_ERROR {
            acc.errors += 1;
        }
        if record.usage.upstream_reported && record.usage.verdict != "not_reported" {
            acc.verifiable += 1;
            let mut integrity = 1.0;
            if record.usage.verdict == "over_billed" || record.usage.verdict == "under_billed" {
                let mut delta = record.usage.prompt_delta;
                let completion = record.usage.completion_delta;
                if completion.abs() > delta.abs() {
                    delta = completion;
                }
                integrity = 1.0 - delta.abs();
                if integrity < 0.0 {
                    integrity = 0.0;
                }
            }
            acc.sum += integrity;
        }
    }
    order.sort();
    let mut out: Vec<ChannelStat> = Vec::with_capacity(order.len());
    for name in order {
        let acc = &index[&name];
        let mut stats = ChannelStat {
            channel: name,
            requests: acc.requests,
            errors: acc.errors,
            integrity: 1.0,
        };
        if acc.verifiable > 0 {
            stats.integrity = acc.sum / acc.verifiable as f64;
        }
        out.push(stats);
    }
    (records.len(), out)
}

/// Reports whether a list contains a value.
fn contains_string(haystack: &[String], needle: &str) -> bool {
    haystack.iter().any(|item| item == needle)
}

/// Appends a value once, preserving order.
fn append_unique(list: &mut Vec<String>, value: &str) {
    if value.is_empty() || list.iter().any(|existing| existing == value) {
        return;
    }
    list.push(value.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_counts_errors_and_integrity() {
        let make = |channel: &str, decision: &str, verdict: &str, worst: f64| audit::Record {
            channel: channel.to_string(),
            decision: decision.to_string(),
            usage: audit::UsageSummary {
                upstream_reported: true,
                verdict: verdict.to_string(),
                completion_delta: worst,
                ..audit::UsageSummary::default()
            },
            ..audit::Record::default()
        };
        let records = vec![
            make("a", audit::DECISION_ALLOW, "consistent", 0.0),
            make("a", audit::DECISION_UPSTREAM_ERROR, "over_billed", 0.4),
            make("b", audit::DECISION_ALLOW, "not_reported", 0.0),
        ];
        let (total, stats) = aggregate_for_view(&records);
        assert_eq!(total, 3);
        assert_eq!(stats.len(), 2);
        let a = &stats[0];
        assert_eq!(a.channel, "a");
        assert_eq!(a.requests, 2);
        assert_eq!(a.errors, 1);
        // (1.0 + 0.6) / 2
        assert!((a.integrity - 0.8).abs() < 1e-9, "{}", a.integrity);
        assert_eq!(stats[1].integrity, 1.0);
    }

    #[test]
    fn append_unique_ignores_duplicates_and_empty() {
        let mut list = Vec::new();
        append_unique(&mut list, "x");
        append_unique(&mut list, "x");
        append_unique(&mut list, "");
        append_unique(&mut list, "y");
        assert_eq!(list, vec!["x".to_string(), "y".to_string()]);
        assert!(contains_string(&list, "y"));
        assert!(!contains_string(&list, "z"));
    }
}
