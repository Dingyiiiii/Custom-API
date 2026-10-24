//! Tests translated from `internal/discovery/discovery_test.go`.
//!
//! The Go tests use `httptest.Server`; the equivalent here is a minimal HTTP/1.1
//! server on a loopback `TcpListener`, so no test ever leaves the machine.

use super::discovery::{match_pattern, truncate};
use super::*;
use crate::config::{ApiKey, Channel, Config, DiscoverPolicy, DiscoveryPolicy, Provider, User};
use chrono::{DateTime, TimeZone, Utc};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Locks a test mutex, ignoring poisoning.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A throwaway directory with recursive cleanup.
struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    fn new() -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("customs-discovery-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    fn join(&self, name: &str) -> String {
        self.path.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A loopback model-list server.
///
/// It answers `path` with an OpenAI-shaped list, any other path with 404, and any
/// non-200 status with a JSON error body. The last request head is captured so a
/// test can assert on headers.
struct TestServer {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    head: Arc<Mutex<String>>,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn start(models: &[&str], status: u16, path: &str) -> TestServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let head = Arc::new(Mutex::new(String::new()));
        let models: Vec<String> = models.iter().map(|m| m.to_string()).collect();
        let path = path.to_string();
        let hits_task = hits.clone();
        let head_task = head.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let hits = hits_task.clone();
                let head = head_task.clone();
                let models = models.clone();
                let path = path.clone();
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut buf = [0u8; 4096];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                raw.extend_from_slice(&buf[..n]);
                                if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                    let text = String::from_utf8_lossy(&raw).into_owned();
                    *lock(&head) = text.clone();
                    hits.fetch_add(1, Ordering::SeqCst);
                    let request_path = text
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    let (status_line, body) = if request_path != path {
                        (
                            "HTTP/1.1 404 Not Found".to_string(),
                            "not found".to_string(),
                        )
                    } else if status != 200 {
                        (
                            format!("HTTP/1.1 {status} Error"),
                            "{\"error\":{\"message\":\"nope\"}}".to_string(),
                        )
                    } else {
                        let data: Vec<serde_json::Value> = models
                            .iter()
                            .map(|m| {
                                serde_json::json!({
                                    "id": m, "object": "model", "owned_by": "vendor"
                                })
                            })
                            .collect();
                        let payload = serde_json::json!({"object": "list", "data": data});
                        (
                            "HTTP/1.1 200 OK".to_string(),
                            serde_json::to_string(&payload).expect("serialise list"),
                        )
                    };
                    let response = format!(
                        "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        TestServer {
            addr,
            hits,
            head,
            handle,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn header(&self, name: &str) -> Option<String> {
        let head = lock(&self.head).clone();
        for line in head.lines().skip(1) {
            if let Some((key, value)) = line.split_once(':') {
                if key.trim().eq_ignore_ascii_case(name) {
                    return Some(value.trim().to_string());
                }
            }
        }
        None
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn new_service(cache_path: &str) -> Service {
    Service::new(Options {
        policy: DiscoveryPolicy {
            cache_path: cache_path.to_string(),
            ..DiscoveryPolicy::default()
        },
        ..Options::default()
    })
}

fn provider(name: &str, base_url: &str, protocol: &str, key: &str) -> Provider {
    Provider {
        name: name.to_string(),
        base_url: base_url.to_string(),
        protocol: protocol.to_string(),
        keys: vec![ApiKey {
            name: key.to_string(),
            api_key: format!("sk-{key}"),
            ..ApiKey::default()
        }],
        ..Provider::default()
    }
}

fn config_with(providers: Vec<Provider>) -> Config {
    let mut cfg = Config {
        channels: Vec::new(),
        users: vec![User {
            name: "u".to_string(),
            token: "t".to_string(),
            ..User::default()
        }],
        providers,
        ..Config::default()
    };
    cfg.apply_defaults();
    cfg
}

fn view_names(views: &[ModelView]) -> Vec<String> {
    views.iter().map(|v| v.model.clone()).collect()
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_refresh_discovers_models() {
    let srv = TestServer::start(&["gpt-4o", "gpt-4o-mini", "o3-mini"], 200, "/v1/models").await;
    let svc = new_service("");

    let cfg = config_with(vec![provider("vendor", &srv.url(), "openai", "k")]);
    svc.refresh(&cfg).await;

    let views = svc.models(None);
    assert_eq!(view_names(&views), vec!["gpt-4o", "gpt-4o-mini", "o3-mini"]);
    for v in &views {
        assert!(
            !v.configured,
            "a discovered model should not be marked configured"
        );
        // A named key keeps its name in the channel, so the credential that
        // serves this model is identifiable from the catalogue.
        assert_eq!(v.channels.len(), 1, "{:?}", v.channels);
        assert_eq!(v.channels[0].channel, "vendor/k");
        assert_eq!(v.channels[0].source, SOURCE_DISCOVERED);
    }
}

/// The authority order: an operator mapping is never overwritten by what the
/// provider reports.
#[tokio::test]
async fn test_configured_models_win_over_discovered() {
    let srv = TestServer::start(&["gpt-4o"], 200, "/v1/models").await;
    let svc = new_service("");

    let mut cfg = config_with(vec![Provider {
        models: HashMap::from([("gpt-4".to_string(), "gpt-4o".to_string())]),
        ..provider("vendor", &srv.url(), "openai", "k")
    }]);
    cfg.apply_defaults();
    svc.refresh(&cfg).await;

    let views = svc.models(None);
    let by_name: HashMap<String, ModelView> =
        views.iter().map(|v| (v.model.clone(), v.clone())).collect();
    let configured = by_name
        .get("gpt-4")
        .unwrap_or_else(|| panic!("the operator's canonical name is missing: {views:?}"));
    assert!(
        configured.configured,
        "the operator's model should be marked configured"
    );
    assert_eq!(
        configured.channels[0].upstream_model, "gpt-4o",
        "the upstream name should be the provider's own"
    );
    // The raw discovered name is also present, because it is genuinely servable;
    // hiding it would make a working model unreachable.
    assert!(
        by_name.contains_key("gpt-4o"),
        "the discovered name should also be listed: {views:?}"
    );
}

#[tokio::test]
async fn test_discovery_failure_is_recorded_not_fatal() {
    let srv = TestServer::start(&[], 401, "/v1/models").await;
    let svc = new_service("");

    let cfg = config_with(vec![Provider {
        models: HashMap::from([("known".to_string(), "known-upstream".to_string())]),
        ..provider("vendor", &srv.url(), "openai", "k")
    }]);
    svc.refresh(&cfg).await;

    // A failed discovery must not remove the operator's declared models: the
    // model is servable, the catalogue is merely unverified.
    let views = svc.models(None);
    assert_eq!(view_names(&views), vec!["known"]);

    let snapshot = svc.snapshot();
    let error = snapshot
        .errors
        .get("vendor")
        .unwrap_or_else(|| panic!("the failure should be recorded: {snapshot:?}"));
    assert!(
        error.contains("401"),
        "the error should name the status: {error:?}"
    );

    let channels = svc.channels();
    assert_eq!(channels.len(), 1, "{channels:?}");
    assert!(!channels[0].error.is_empty(), "per-channel error missing");
}

/// The filters operators use to keep a provider's hundreds of models out of the
/// catalogue.
#[tokio::test]
async fn test_discovery_include_exclude() {
    let srv = TestServer::start(
        &["gpt-4o", "gpt-4o-mini", "whisper-1", "dall-e-3"],
        200,
        "/v1/models",
    )
    .await;
    let svc = new_service("");

    let cfg = config_with(vec![Provider {
        discover: Some(DiscoverPolicy {
            enabled: Some(true),
            include: vec!["gpt-4o*".to_string()],
            exclude: vec!["gpt-4o-mini".to_string()],
            ..DiscoverPolicy::default()
        }),
        ..provider("vendor", &srv.url(), "openai", "k")
    }]);
    svc.refresh(&cfg).await;

    let views = svc.models(None);
    assert_eq!(view_names(&views), vec!["gpt-4o"], "filters not applied");
}

#[tokio::test]
async fn test_discovery_disabled_for_provider() {
    let srv = TestServer::start(&["should-not-appear"], 200, "/v1/models").await;
    let svc = new_service("");

    let cfg = config_with(vec![Provider {
        models: HashMap::from([("mine".to_string(), "mine".to_string())]),
        discover: Some(DiscoverPolicy {
            enabled: Some(false),
            ..DiscoverPolicy::default()
        }),
        ..provider("vendor", &srv.url(), "openai", "k")
    }]);
    svc.refresh(&cfg).await;

    assert_eq!(
        srv.hits(),
        0,
        "discovery ran for a provider that disabled it"
    );
    let views = svc.models(None);
    assert_eq!(
        view_names(&views),
        vec!["mine"],
        "configured models should survive"
    );
}

/// Each key produces its own catalogue entry, so the front end can show which key
/// reaches what.
#[tokio::test]
async fn test_multi_key_provider_is_discovered_per_key() {
    let srv = TestServer::start(&["m1"], 200, "/v1/models").await;
    let svc = new_service("");

    let mut vendor = provider("vendor", &srv.url(), "openai", "k1");
    vendor.keys.push(ApiKey {
        name: "k2".to_string(),
        api_key: "sk-k2".to_string(),
        ..ApiKey::default()
    });
    let cfg = config_with(vec![vendor]);
    svc.refresh(&cfg).await;

    let channels = svc.channels();
    assert_eq!(channels.len(), 2, "{channels:?}");
    let mut keys = Vec::new();
    for cm in &channels {
        keys.push(cm.key.clone());
        assert_eq!(
            cm.models.len(),
            1,
            "channel {} models: {:?}",
            cm.channel,
            cm.models
        );
    }
    keys.sort();
    assert_eq!(keys, vec!["k1".to_string(), "k2".to_string()]);
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_cache_round_trip() {
    let dir = TempDir::new();
    let path = dir.join("cache.json");
    let srv = TestServer::start(&["m1", "m2"], 200, "/v1/models").await;

    let svc = new_service(&path);
    let cfg = config_with(vec![provider("vendor", &srv.url(), "openai", "k")]);
    svc.refresh(&cfg).await;

    assert!(
        std::path::Path::new(&path).exists(),
        "cache was not written"
    );

    // A fresh service must load the cache, so the UI has data after a restart
    // even before the first refresh completes.
    let fresh = new_service(&path);
    fresh.load().expect("load");
    let views = fresh.models(None);
    assert_eq!(
        view_names(&views),
        vec!["m1", "m2"],
        "cache did not restore the catalogue"
    );
}

#[test]
fn test_load_missing_cache_is_not_an_error() {
    let dir = TempDir::new();
    let svc = new_service(&dir.join("absent.json"));
    svc.load().expect("a missing cache must not be an error");
}

#[test]
fn test_load_corrupt_cache_fails() {
    let dir = TempDir::new();
    let path = dir.join("bad.json");
    std::fs::write(&path, "{not json").expect("write");
    let svc = new_service(&path);
    let err = svc.load().expect_err("a corrupt cache should be reported");
    assert!(
        err.to_string().starts_with("discovery: parse cache "),
        "{err}"
    );
    assert!(err.to_string().contains("bad.json"), "{err}");
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// One model served by two channels appears once, listing both, which is what
/// lets a user ask "where can I get this model".
#[tokio::test]
async fn test_model_view_merges_channels() {
    let srv_a = TestServer::start(&["shared-model"], 200, "/v1/models").await;
    let srv_b = TestServer::start(&["shared-model"], 200, "/v1/models").await;
    let svc = new_service("");

    let cfg = config_with(vec![
        provider("a", &srv_a.url(), "openai", "k"),
        provider("b", &srv_b.url(), "anthropic", "k"),
    ]);
    svc.refresh(&cfg).await;

    let views = svc.models(None);
    assert_eq!(
        views.len(),
        1,
        "the same model on two channels should merge"
    );
    let v = &views[0];
    assert_eq!(v.channels.len(), 2, "{:?}", v.channels);
    assert_eq!(
        v.protocols,
        vec!["anthropic".to_string(), "openai".to_string()],
        "both protocols should be listed"
    );

    let view = svc.lookup("shared-model").expect("lookup");
    assert_eq!(view.channels.len(), 2);
    assert!(
        svc.known("shared-model"),
        "Known should report a catalogued model"
    );
    assert!(
        !svc.known("nonexistent"),
        "Known should reject an unknown model"
    );
}

/// An old list is flagged, so the UI can warn that a provider has not been
/// reachable.
#[tokio::test]
async fn test_stale_is_marked() {
    let srv = TestServer::start(&["m"], 200, "/v1/models").await;
    let dir = TempDir::new();

    let now = Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap();
    let clock = Arc::new(Mutex::new(now));
    let service_clock = clock.clone();
    let svc = Service::new(Options {
        policy: DiscoveryPolicy {
            cache_path: dir.join("c.json"),
            stale_after: crate::json::Duration::new(Duration::from_secs(3600)),
            ..DiscoveryPolicy::default()
        },
        now: Some(Arc::new(move || *lock(&service_clock))),
        ..Options::default()
    });
    let cfg = config_with(vec![provider("vendor", &srv.url(), "openai", "k")]);
    svc.refresh(&cfg).await;

    let channels = svc.channels();
    assert_eq!(channels.len(), 1);
    assert!(!channels[0].stale, "a fresh list must not be stale");

    // Move the clock past the threshold.
    let later = *lock(&clock) + chrono::TimeDelta::hours(2);
    *lock(&clock) = later;
    let channels = svc.channels();
    assert_eq!(channels.len(), 1);
    assert!(channels[0].stale, "an old list must be marked stale");
}

/// A channel whose discovery produced nothing is flagged, so the UI can say
/// "operator-supplied, unverified".
#[tokio::test]
async fn test_configured_only_is_reported() {
    let srv = TestServer::start(&["whatever"], 200, "/v1/models").await;
    let svc = new_service("");

    let cfg = config_with(vec![Provider {
        models: HashMap::from([("m".to_string(), "m".to_string())]),
        discover: Some(DiscoverPolicy {
            enabled: Some(false),
            ..DiscoverPolicy::default()
        }),
        ..provider("vendor", &srv.url(), "openai", "k")
    }]);
    svc.refresh(&cfg).await;

    let channels = svc.channels();
    assert_eq!(channels.len(), 1, "{channels:?}");
    assert!(channels[0].configured_only, "expected configured_only");
}

/// The per-user filter used by `/v1/models`.
#[tokio::test]
async fn test_models_allow_filter() {
    let srv = TestServer::start(&["allowed", "denied"], 200, "/v1/models").await;
    let svc = new_service("");

    let cfg = config_with(vec![provider("vendor", &srv.url(), "openai", "k")]);
    svc.refresh(&cfg).await;

    let allow = |model: &str| model == "allowed";
    let views = svc.models(Some(&allow));
    assert_eq!(view_names(&views), vec!["allowed"], "filter not applied");
}

#[test]
fn test_match_pattern() {
    let cases: [(&str, &str, bool); 6] = [
        ("gpt-4o", "gpt-4o", true),
        ("gpt-4o", "gpt-4o-mini", false),
        ("gpt-4o*", "gpt-4o-mini", true),
        ("gpt-4o*", "claude", false),
        ("*", "anything", true),
        ("", "anything", false),
    ];
    for (pattern, name, want) in cases {
        assert_eq!(
            match_pattern(pattern, name),
            want,
            "match_pattern({pattern:?}, {name:?})"
        );
    }
}

// ---------------------------------------------------------------------------
// Rust-side boundary tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn openai_requests_carry_a_bearer_token_and_operator_headers() {
    let srv = TestServer::start(&["m"], 200, "/v1/models").await;
    let svc = new_service("");

    let mut vendor = provider("vendor", &srv.url(), "openai", "k");
    vendor.headers = HashMap::from([("X-Trace".to_string(), "abc".to_string())]);
    let cfg = config_with(vec![vendor]);
    svc.refresh(&cfg).await;

    assert_eq!(srv.header("authorization").as_deref(), Some("Bearer sk-k"));
    assert_eq!(srv.header("accept").as_deref(), Some("application/json"));
    assert_eq!(srv.header("x-trace").as_deref(), Some("abc"));
    assert!(srv.header("x-api-key").is_none());
}

#[tokio::test]
async fn anthropic_requests_carry_the_api_key_header() {
    let srv = TestServer::start(&["m"], 200, "/v1/models").await;
    let svc = new_service("");

    let cfg = config_with(vec![provider("vendor", &srv.url(), "anthropic", "k")]);
    svc.refresh(&cfg).await;

    assert_eq!(srv.header("x-api-key").as_deref(), Some("sk-k"));
    assert_eq!(
        srv.header("anthropic-version").as_deref(),
        Some("2023-06-01")
    );
    assert!(srv.header("authorization").is_none());
}

#[tokio::test]
async fn custom_discovery_path_is_used() {
    let srv = TestServer::start(&["m"], 200, "/custom/models").await;
    let svc = new_service("");

    let cfg = config_with(vec![Provider {
        discover: Some(DiscoverPolicy {
            path: "/custom/models".to_string(),
            ..DiscoverPolicy::default()
        }),
        ..provider("vendor", &srv.url(), "openai", "k")
    }]);
    svc.refresh(&cfg).await;

    let views = svc.models(None);
    assert_eq!(view_names(&views), vec!["m"]);
}

#[tokio::test]
async fn a_base_url_carrying_the_version_segment_is_not_doubled() {
    let srv = TestServer::start(&["m"], 200, "/v1/models").await;
    let svc = new_service("");

    let cfg = config_with(vec![provider(
        "vendor",
        &format!("{}/v1", srv.url()),
        "openai",
        "k",
    )]);
    svc.refresh(&cfg).await;

    assert_eq!(view_names(&svc.models(None)), vec!["m"]);
}

#[tokio::test]
async fn exclusion_wins_over_inclusion() {
    let srv = TestServer::start(&["gpt-4o", "gpt-4o-mini", "claude"], 200, "/v1/models").await;
    let svc = new_service("");
    let cfg = config_with(vec![Provider {
        discover: Some(DiscoverPolicy {
            include: vec!["gpt-4*".to_string()],
            exclude: vec!["gpt-4o-mini".to_string()],
            ..DiscoverPolicy::default()
        }),
        ..provider("vendor", &srv.url(), "openai", "k")
    }]);
    svc.refresh(&cfg).await;

    // An exclusion carves a model out of an inclusion; the pattern that matches
    // nothing on either list contributes nothing.
    assert_eq!(view_names(&svc.models(None)), vec!["gpt-4o"]);
}

#[test]
fn truncate_keeps_utf8_intact() {
    assert_eq!(truncate("  hello  ", 160), "hello");
    assert_eq!(truncate("abcdef", 3), "abc...");
    // Four Chinese characters are twelve bytes, so a byte limit of 7 lands in the
    // middle of one: the cut must move back to a character boundary.
    let text = "中文中文";
    let cut = truncate(text, 7);
    assert_eq!(cut, "中文...");
    assert!(cut.is_char_boundary(0));
}

#[test]
fn cache_json_matches_the_go_shape() {
    let snapshot = Snapshot {
        updated_at: Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap(),
        channels: vec![ChannelModels {
            channel: "vendor/k".to_string(),
            provider: "vendor".to_string(),
            key: "k".to_string(),
            protocol: "openai".to_string(),
            models: vec![ModelEntry {
                model: "gpt-4o".to_string(),
                channel: "vendor/k".to_string(),
                provider: "vendor".to_string(),
                key: "k".to_string(),
                upstream_model: "gpt-4o".to_string(),
                protocol: "openai".to_string(),
                protocols: vec!["openai".to_string()],
                source: SOURCE_DISCOVERED.to_string(),
                discovered_at: Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap(),
                ..ModelEntry::default()
            }],
            discovered: Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap(),
            ..ChannelModels::default()
        }],
        providers: [("vendor".to_string(), 1)].into_iter().collect(),
        errors: Default::default(),
    };
    let json = serde_json::to_string(&snapshot).expect("serialise");
    for key in [
        "\"updated_at\":\"2026-03-01T12:00:00Z\"",
        "\"channels\":[{\"channel\":\"vendor/k\",\"provider\":\"vendor\",\"key\":\"k\",\"protocol\":\"openai\"",
        "\"upstream_model\":\"gpt-4o\"",
        "\"protocols\":[\"openai\"]",
        "\"source\":\"discovered\"",
        "\"discovered_at\":\"2026-03-01T12:00:00Z\"",
        "\"providers\":{\"vendor\":1}",
    ] {
        assert!(json.contains(key), "missing {key} in {json}");
    }
    // The zero-valued flags and maps are omitted exactly like Go's omitempty.
    assert!(!json.contains("\"stale\""), "{json}");
    assert!(!json.contains("\"errors\""), "{json}");
    assert!(!json.contains("\"configured_only\""), "{json}");
    assert!(!json.contains("\"created\""), "{json}");
    assert!(!json.contains("\"error\""), "{json}");
}

/// The exact bytes Go writes for a failed discovery run: a nil model slice is
/// `null`, not `[]`.
#[test]
fn a_go_written_cache_with_null_collections_loads() {
    let json = r#"{
  "updated_at": "0001-01-01T00:00:00Z",
  "channels": [
    {
      "channel": "vendor/k",
      "protocol": "openai",
      "models": null,
      "discovered": "0001-01-01T00:00:00Z"
    }
  ]
}"#;
    let snapshot: Snapshot = serde_json::from_str(json).expect("Go-written cache parses");
    assert_eq!(snapshot.channels.len(), 1);
    assert!(snapshot.channels[0].models.is_empty());
    assert!(snapshot.providers.is_empty());

    // And an empty snapshot, which Go writes as `"channels": null`.
    let empty: Snapshot =
        serde_json::from_str(r#"{"updated_at":"0001-01-01T00:00:00Z","channels":null}"#)
            .expect("null channel list parses");
    assert!(empty.channels.is_empty());
    assert!(empty.errors.is_empty());
}

#[test]
fn an_empty_snapshot_serialises_like_go() {
    let json = serde_json::to_string(&Snapshot::default()).expect("serialise");
    assert!(json.contains("\"channels\":null"), "{json}");
    assert!(!json.contains("\"providers\""), "{json}");
    assert!(!json.contains("\"errors\""), "{json}");
}

#[test]
fn a_cache_without_optional_maps_still_parses() {
    // An older or hand-edited cache: the maps are absent and the timestamps are
    // Go's zero value.
    let json = r#"{
        "updated_at": "0001-01-01T00:00:00Z",
        "channels": [
            {"channel": "c", "protocol": "openai", "models": [
                {"model": "m", "channel": "c", "source": "configured"}
            ]}
        ]
    }"#;
    let snapshot: Snapshot = serde_json::from_str(json).expect("parse");
    assert_eq!(snapshot.channels.len(), 1);
    assert_eq!(snapshot.channels[0].discovered, crate::config::zero_time());
    assert!(snapshot.providers.is_empty());
    assert!(snapshot.errors.is_empty());
}

#[tokio::test]
async fn channels_include_configured_models_missing_from_the_snapshot() {
    let svc = new_service("");
    let channel = Channel {
        name: "direct".to_string(),
        base_url: "http://127.0.0.1:1".to_string(),
        models: HashMap::from([("m".to_string(), "up-m".to_string())]),
        ..Channel::default()
    };
    svc.set_configured(&[channel]);

    // No refresh ran, so the snapshot is empty; the configured models must still
    // produce a channel entry and a view.
    let channels = svc.channels();
    assert_eq!(channels.len(), 1, "{channels:?}");
    assert_eq!(channels[0].channel, "direct");
    assert!(channels[0].configured_only);
    assert_eq!(channels[0].provider, "");
    assert_eq!(channels[0].models.len(), 1);

    let views = svc.models(None);
    assert_eq!(view_names(&views), vec!["m"]);
    assert!(views[0].configured);
}

#[tokio::test]
async fn a_direct_channel_without_a_provider_still_gets_a_discovery_job() {
    // A directly-configured channel has no provider record, so discovery runs
    // against its own base_url; here that host is the loopback server.
    let srv = TestServer::start(&["m"], 200, "/v1/models").await;
    let svc = new_service("");
    let channel = Channel {
        name: "direct".to_string(),
        base_url: srv.url(),
        protocol: "openai".to_string(),
        ..Channel::default()
    };
    let mut cfg = Config {
        channels: vec![channel],
        users: vec![User {
            name: "u".to_string(),
            token: "t".to_string(),
            ..User::default()
        }],
        ..Config::default()
    };
    cfg.apply_defaults();
    svc.refresh(&cfg).await;

    let views = svc.models(None);
    assert_eq!(view_names(&views), vec!["m"]);
    // With no metadata the catalogue cannot name a provider or a key.
    assert_eq!(views[0].channels[0].provider, "");
    assert_eq!(views[0].channels[0].key, "");
}

#[tokio::test]
async fn discovery_errors_do_not_stop_other_channels() {
    let good = TestServer::start(&["good-model"], 200, "/v1/models").await;
    let bad = TestServer::start(&[], 500, "/v1/models").await;
    let svc = new_service("");

    let cfg = config_with(vec![
        provider("good", &good.url(), "openai", "k"),
        provider("bad", &bad.url(), "openai", "k"),
    ]);
    svc.refresh(&cfg).await;

    let names = view_names(&svc.models(None));
    assert_eq!(names, vec!["good-model"]);
    let snapshot = svc.snapshot();
    assert!(snapshot.errors.contains_key("bad"), "{snapshot:?}");
    assert!(!snapshot.errors.contains_key("good"), "{snapshot:?}");
    assert_eq!(snapshot.providers.get("good"), Some(&1));
    assert_eq!(snapshot.providers.get("bad"), Some(&0));
}

#[tokio::test]
async fn duplicate_discovered_names_are_listed_once_per_channel() {
    // The same name reported twice by one upstream must not produce two entries.
    let srv = TestServer::start(&["m", "m"], 200, "/v1/models").await;
    let svc = new_service("");
    let cfg = config_with(vec![provider("vendor", &srv.url(), "openai", "k")]);
    svc.refresh(&cfg).await;

    let views = svc.models(None);
    assert_eq!(view_names(&views), vec!["m"]);
    assert_eq!(views[0].channels.len(), 1);
}

#[tokio::test]
async fn run_with_no_interval_refreshes_once() {
    let srv = TestServer::start(&["m"], 200, "/v1/models").await;
    let svc = new_service("");
    let cfg = Arc::new(config_with(vec![provider(
        "vendor",
        &srv.url(),
        "openai",
        "k",
    )]));

    // A zero interval means "once, now": the loop must return instead of
    // spinning, which is what lets a caller use `run` for a refresh-then-exit.
    svc.run(cfg, Duration::ZERO).await;
    assert_eq!(view_names(&svc.models(None)), vec!["m"]);
}

#[tokio::test]
async fn service_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Service>();
    assert_send_sync::<Snapshot>();
    assert_send_sync::<ModelView>();
    // Keep the imported chrono symbol in use for the clock helper's type.
    let _: fn() -> DateTime<Utc> = Utc::now;
}
