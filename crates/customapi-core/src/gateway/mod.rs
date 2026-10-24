//! The customs proxy: the single insertion point between the user-facing
//! gateway and the upstream access points.
//!
//! Responsibilities, matching the project plan's sections A, B and C:
//!
//! * **A outbound** - inspect the user's request, detect abuse, intercept
//!   dangerous tool calls, redact secrets before persisting evidence.
//! * **B inbound** - verify the upstream's reported usage, fingerprint the model
//!   that actually answered, detect prompt injection and leaked credentials, and
//!   withhold a response that fails policy.
//! * **C metering** - recount tokens locally, price the exchange, and write an
//!   independent, tamper-evident audit record for every request.
//!
//! The module layout mirrors the Go package one file at a time:
//!
//! | Rust file             | Go source            | contents                                  |
//! |-----------------------|----------------------|-------------------------------------------|
//! | `mod.rs`              | `gateway.go`         | server state, routers, middleware, health |
//! | `proxy.rs`            | `proxy.go`           | the proxy pipeline, failover, audit write |
//! | `stream.rs`           | `stream.go`          | live/buffered SSE forwarding              |
//! | `protocols.rs`        | `protocols.go`       | failure classes, retries, per-protocol IO |
//! | `api.rs`              | `api.go`             | user and management endpoints             |
//! | `admin_providers.rs`  | `admin_providers.go` | provider views and aggregates             |
//! | `web.rs`              | `web.go`             | SPA serving from disk                     |
//! | `helpers.rs`          | `helpers.go`         | rate limiting, sampling, digests, headers |
//!
//! Concurrency differs from Go in exactly one way: Go keeps a `sync.RWMutex`
//! around a `*config.Config` that callers mutate through the pointer. Rust hands
//! out immutable snapshots instead, so [`Server::config`] returns an
//! `Arc<Config>` and a reload (or a test that changes an upstream URL) goes
//! through [`Server::update_config`].

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration as StdDuration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::any;
use axum::Router;

use crate::audit;
use crate::config::{self, Config};
use crate::discovery;
use crate::meter;
use crate::policy;
use crate::route;
use crate::store;
use crate::version;

mod admin_providers;
mod api;
mod helpers;
mod protocols;
mod proxy;
mod stream;
mod web;

#[cfg(test)]
mod tests;

pub use web::{DEFAULT_WEB_DIRS, PLACEHOLDER_HTML, WEB_DIR_ENV};

/// Bounds a request body: 32 MiB comfortably covers a long conversation with
/// inline images.
pub const MAX_BODY_BYTES: u64 = 32 << 20;

/// Converts a currency amount into quota units.
///
/// Quota is kept in integer units so accounting is exact and survives JSON
/// round-trips. The convention here is 1 unit = 1e-6 of the catalogue currency.
pub const QUOTA_UNITS_PER_CURRENCY: f64 = 1e6;

/// The header that lets a client pin a channel.
pub const PIN_HEADER: &str = "X-Customs-Channel";

/// Fails to build a gateway.
///
/// Go builds these errors with `fmt.Errorf` and the sub-package error values, so
/// the rendered text stays the contract; the variants exist so a caller can tell
/// a configuration problem from a storage problem.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// The configuration is missing or unusable.
    #[error("gateway: config is required")]
    NoConfig,
    /// The policy engine could not compile its rules.
    #[error("{0}")]
    Policy(String),
    /// The audit ledger could not be opened.
    #[error("{0}")]
    Audit(String),
    /// The quota state could not be loaded.
    #[error("{0}")]
    Store(String),
    /// The upstream client could not be built.
    #[error("gateway: upstream client: {0}")]
    Client(String),
}

impl From<audit::AuditError> for GatewayError {
    fn from(e: audit::AuditError) -> Self {
        GatewayError::Audit(e.to_string())
    }
}

impl From<store::StoreError> for GatewayError {
    fn from(e: store::StoreError) -> Self {
        GatewayError::Store(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Live counters
// ---------------------------------------------------------------------------

/// Live counters, exposed on `/healthz` and the management API.
#[derive(Debug, Default)]
pub struct Stats {
    pub(crate) requests: AtomicI64,
    pub(crate) blocked_outbound: AtomicI64,
    pub(crate) blocked_inbound: AtomicI64,
    pub(crate) upstream_errors: AtomicI64,
    pub(crate) streamed_reqs: AtomicI64,
    pub(crate) over_billed: AtomicI64,
    pub(crate) fingerprint_miss: AtomicI64,
    pub(crate) degraded: AtomicI64,
    pub(crate) bytes_in: AtomicI64,
    pub(crate) bytes_out: AtomicI64,
}

impl Stats {
    /// Renders the counters for reporting.
    ///
    /// Go returns a `map[string]int64`; `encoding/json` sorts map keys, so the
    /// snapshot is a `BTreeMap` here for the same stable output.
    pub fn snapshot(&self) -> BTreeMap<String, i64> {
        let mut out = BTreeMap::new();
        out.insert("requests".to_string(), self.requests.load(Ordering::Relaxed));
        out.insert(
            "blocked_outbound".to_string(),
            self.blocked_outbound.load(Ordering::Relaxed),
        );
        out.insert(
            "blocked_inbound".to_string(),
            self.blocked_inbound.load(Ordering::Relaxed),
        );
        out.insert(
            "upstream_errors".to_string(),
            self.upstream_errors.load(Ordering::Relaxed),
        );
        out.insert(
            "streamed_requests".to_string(),
            self.streamed_reqs.load(Ordering::Relaxed),
        );
        out.insert(
            "over_billed".to_string(),
            self.over_billed.load(Ordering::Relaxed),
        );
        out.insert(
            "fingerprint_miss".to_string(),
            self.fingerprint_miss.load(Ordering::Relaxed),
        );
        out.insert(
            "degraded".to_string(),
            self.degraded.load(Ordering::Relaxed),
        );
        out.insert("bytes_in".to_string(), self.bytes_in.load(Ordering::Relaxed));
        out.insert(
            "bytes_out".to_string(),
            self.bytes_out.load(Ordering::Relaxed),
        );
        out
    }
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Configures a [`Server`].
#[derive(Default)]
pub struct Options {
    /// The loaded configuration.
    pub config: Option<Config>,
    /// Resolves relative paths in the configuration.
    pub base_dir: PathBuf,
    /// Seeds the routing scorebook.
    pub scores: Option<HashMap<String, HashMap<String, route::Scores>>>,
    /// Overrides the upstream HTTP client (used by tests).
    pub client: Option<reqwest::Client>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// The customs gateway.
///
/// All mutable fields are guarded: the configuration can be reloaded while
/// requests are in flight, and counters are updated from many tasks. Handlers
/// receive an `Arc<Server>`, which is what makes the state shareable across
/// axum's tasks.
pub struct Server {
    cfg: RwLock<Arc<Config>>,
    /// The configuration directory, used to resolve relative paths.
    base: PathBuf,

    engine: Arc<policy::Engine>,
    redactor: Arc<policy::Redactor>,
    pricer: Arc<meter::Pricer>,
    selector: Arc<route::Selector>,
    scores: Arc<route::MapScoreBook>,
    health: Arc<route::HealthTracker>,
    store: Arc<store::Manager>,
    ledger: Arc<audit::Writer>,
    /// The discovered and configured model catalogue. It is a view only:
    /// routing never consults it, so a discovery outage cannot take models out
    /// of service.
    catalogue: Arc<discovery::Service>,
    /// Stops the background discovery loop on [`Server::close`].
    discovery_task: Mutex<Option<tokio::task::JoinHandle<()>>>,

    client: reqwest::Client,

    /// Rate limiting state per user name.
    limiters: Mutex<HashMap<String, Arc<Mutex<helpers::Limiter>>>>,

    /// Running counters.
    stats: Stats,

    /// The front-end directory, located once at construction.
    web_dir: Option<PathBuf>,

    /// Process start time.
    started_at: Instant,
    /// Set once the server is serving.
    ready: AtomicBool,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("base", &self.base)
            .field("channels", &self.config().all_channels().len())
            .field("ready", &self.ready.load(Ordering::Relaxed))
            .finish()
    }
}

impl Server {
    /// Builds a gateway server, opening the audit ledger and restoring quota
    /// state.
    ///
    /// The returned `Arc` is what every handler clones, mirroring the Go
    /// `*Server` that all methods hang off.
    pub fn new(opts: Options) -> Result<Arc<Server>, GatewayError> {
        let Some(cfg) = opts.config else {
            return Err(GatewayError::NoConfig);
        };
        let base = if opts.base_dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            opts.base_dir.clone()
        };

        let engine = Arc::new(
            policy::Engine::new(policy::Options {
                policy: cfg.policy.clone(),
                fail_mode: cfg.server.fail_mode.clone(),
                hash_salt: String::new(),
                disable_defaults: false,
            })
            .map_err(|e| GatewayError::Policy(e.to_string()))?,
        );
        let redactor = Arc::new(
            policy::Redactor::new(&cfg.policy.outbound.redact_patterns)
                .map_err(|e| GatewayError::Policy(e.to_string()))?,
        );

        let pricer = Arc::new(meter::Pricer::new(
            cfg.prices.clone(),
            Some(meter::utc_offset()),
        ));
        let scores = Arc::new(route::MapScoreBook::new(opts.scores.unwrap_or_default()));
        let scores_dyn: Arc<dyn route::ScoreBook> = scores.clone();

        let ledger_path = config::resolve(&base, &cfg.server.audit_path);
        let ledger = Arc::new(audit::Writer::open(audit::WriterOptions {
            path: ledger_path,
            sync: false,
            truncate: false,
        })?);

        let st = Arc::new(store::Manager::new(
            &cfg.users,
            config::resolve(&base, &cfg.server.state_path),
        ));
        if let Err(e) = st.load() {
            let _ = ledger.close();
            return Err(GatewayError::Store(e.to_string()));
        }
        {
            let saver = st.clone();
            st.set_on_change(Some(Arc::new(move || {
                if let Err(e) = saver.save() {
                    log::error!("state save failed: {e}");
                }
            })));
        }

        let client = match opts.client {
            Some(c) => c,
            None => new_upstream_client()?,
        };

        let catalogue = Arc::new(discovery::Service::new(discovery::Options {
            policy: cfg.discovery.clone(),
            client: None,
            now: None,
        }));

        let selector = Arc::new(route::Selector::new(route::Options {
            pricer: Some(pricer.clone()),
            scores: Some(scores_dyn),
            strategy: cfg.server.route_strategy.clone(),
            seed: 0,
        }));

        let web_dir = web::locate_web_dir(&base);

        let server = Arc::new(Server {
            cfg: RwLock::new(Arc::new(cfg)),
            base,
            engine,
            redactor,
            pricer,
            selector,
            scores,
            health: Arc::new(route::HealthTracker::new(3, StdDuration::from_secs(20))),
            store: st,
            ledger,
            catalogue,
            discovery_task: Mutex::new(None),
            client,
            limiters: Mutex::new(HashMap::new()),
            stats: Stats::default(),
            web_dir,
            started_at: Instant::now(),
            ready: AtomicBool::new(false),
        });

        // Seed the catalogue with the configured models immediately.
        //
        // Discovery is asynchronous and may never succeed (an unreachable
        // upstream is a normal state), but a model the operator declared is
        // servable regardless. Seeding here means the catalogue is never empty
        // while waiting for a network round trip.
        let seeded = server.config().all_channels();
        server.catalogue.set_configured(&seeded);
        Ok(server)
    }

    // -- accessors ---------------------------------------------------------

    /// Returns the active configuration.
    ///
    /// Go hands out the mutable `*config.Config`; a snapshot keeps the Rust side
    /// free of data races while still letting a test (or a SIGHUP reload) change
    /// the contents through [`Server::update_config`].
    pub fn config(&self) -> Arc<Config> {
        self.cfg.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replaces the active configuration.
    pub fn set_config(&self, cfg: Config) {
        *self.cfg.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(cfg);
    }

    /// Mutates a copy of the active configuration and installs it.
    pub fn update_config<F: FnOnce(&mut Config)>(&self, f: F) {
        let mut next = (*self.config()).clone();
        f(&mut next);
        self.set_config(next);
    }

    /// Returns the configuration base directory.
    pub fn base_dir(&self) -> &Path {
        &self.base
    }

    /// Exposes the quota manager.
    pub fn store(&self) -> &Arc<store::Manager> {
        &self.store
    }

    /// Exposes the audit writer.
    pub fn ledger(&self) -> &Arc<audit::Writer> {
        &self.ledger
    }

    /// Exposes the model catalogue.
    pub fn catalogue(&self) -> &Arc<discovery::Service> {
        &self.catalogue
    }

    /// Exposes the policy engine.
    pub fn engine(&self) -> &Arc<policy::Engine> {
        &self.engine
    }

    /// Exposes the price catalogue.
    pub fn pricer(&self) -> &Arc<meter::Pricer> {
        &self.pricer
    }

    /// Exposes the routing scorecard.
    pub fn scores(&self) -> &Arc<route::MapScoreBook> {
        &self.scores
    }

    /// Exposes the channel health tracker.
    pub fn health(&self) -> &Arc<route::HealthTracker> {
        &self.health
    }

    /// Exposes the routing selector.
    pub fn selector(&self) -> &Arc<route::Selector> {
        &self.selector
    }

    /// Exposes the upstream client (tests point it at a mock).
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Exposes the counters.
    pub fn stats_snapshot(&self) -> BTreeMap<String, i64> {
        self.stats.snapshot()
    }

    /// Returns how long the process has been running.
    pub fn uptime(&self) -> StdDuration {
        self.started_at.elapsed()
    }

    /// Returns the front-end directory, when one was found.
    pub fn web_dir(&self) -> Option<&Path> {
        self.web_dir.as_deref()
    }

    /// Re-seeds the catalogue from the current channel list.
    ///
    /// It exists for the admin "refresh models" path and for tests that mutate
    /// the provider set after construction. Routing reads the configuration
    /// directly, so only the catalogue needs refreshing.
    pub fn refresh_channels(&self) {
        let channels = self.config().all_channels();
        self.catalogue.set_configured(&channels);
    }

    /// Signals that the server is serving.
    pub fn mark_ready(&self) {
        self.ready.store(true, Ordering::SeqCst);
    }

    /// Reports whether [`Server::mark_ready`] has been called.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    /// Begins background model discovery.
    ///
    /// It is separate from [`Server::new`] so that a caller can decide when
    /// discovery runs: the market data it gathers is useful for the UI, but a
    /// gateway must be able to serve traffic whether or not an upstream is
    /// reachable.
    pub fn start_discovery(self: &Arc<Self>) {
        let cfg = self.config();
        if let Err(e) = self.catalogue.load() {
            log::warn!("discovery cache load failed: {e}");
        }
        let catalogue = self.catalogue.clone();
        let interval = cfg.discovery.interval_or_default();
        let handle = tokio::spawn(async move {
            catalogue.run(cfg, interval).await;
        });
        *self
            .discovery_task
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    /// Releases resources.
    pub async fn close(&self) {
        if let Some(handle) = self
            .discovery_task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            handle.abort();
        }
        if let Err(e) = self.store.save() {
            log::error!("final state save failed: {e}");
        }
        if let Err(e) = self.ledger.close() {
            log::error!("audit close failed: {e}");
        }
    }

    /// Returns the configured ledger path.
    pub fn audit_path(&self) -> String {
        config::resolve(&self.base, &self.config().server.audit_path)
    }

    /// Returns the anchor file beside the ledger.
    pub fn anchor_path(&self) -> String {
        let path = self.audit_path();
        let dir = Path::new(&path)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        dir.join("anchor.jsonl").to_string_lossy().into_owned()
    }

    // -- routers -----------------------------------------------------------

    /// Returns the user-facing HTTP router.
    pub fn handler(self: &Arc<Self>) -> Router {
        let state = self.clone();
        Router::new()
            .route("/healthz", any(api::handle_health))
            .route("/readyz", any(api::handle_ready))
            .route("/v1/models", any(api::handle_models))
            .route("/v1/customs/usage", any(api::handle_my_usage))
            .route("/v1/customs/prices", any(api::handle_price_compare))
            // The proxy endpoints. Any other /v1 path is forwarded verbatim so
            // that endpoints the gateway does not model (embeddings, images,
            // audio) still work, just with less inspection.
            .route("/v1/chat/completions", any(proxy::handle_proxy))
            .route("/v1/completions", any(proxy::handle_proxy))
            .route("/v1/messages", any(proxy::handle_proxy))
            // The Responses API is served natively, not aliased to chat
            // completions: it has its own request, response and streaming event
            // shapes.
            .route("/v1/responses", any(proxy::handle_proxy))
            // The remaining paths are dispatched by prefix in the fallback,
            // exactly like `http.ServeMux` longest-prefix matching.
            .fallback(api::handle_user_fallback)
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(
                state.clone(),
                with_access_log,
            ))
            .layer(middleware::from_fn(with_recovery))
    }

    /// Returns the management API router.
    ///
    /// Reachability is deliberately narrow. The router refuses non-loopback
    /// callers unless the operator opts in with `server.admin_remote`, because
    /// the management API can suspend users, read the audit ledger and rewrite
    /// the price catalogue. Loopback callers are trusted without a token:
    /// anything that can reach the loopback interface already runs on this host.
    /// When `admin_remote` is on, every non-loopback caller must present
    /// `server.admin_token` as a bearer credential.
    pub fn admin_handler(self: &Arc<Self>) -> Router {
        let state = self.clone();
        Router::new()
            .route("/admin/healthz", any(api::handle_health))
            .route("/admin/version", any(api::handle_version))
            .route("/admin/stats", any(api::handle_stats))
            .route("/admin/config", any(api::handle_config_summary))
            .route("/admin/users", any(api::handle_users))
            .route("/admin/channels", any(api::handle_channels))
            .route("/admin/audit", any(api::handle_audit))
            .route("/admin/audit/verify", any(api::handle_audit_verify))
            .route("/admin/usage", any(api::handle_usage))
            .route("/admin/prices", any(api::handle_prices))
            .route("/admin/prices/compare", any(api::handle_price_compare))
            .route("/admin/scores", any(api::handle_scores))
            .route("/admin/fingerprints", any(api::handle_fingerprints))
            .route("/admin/providers", any(admin_providers::handle_providers))
            .route("/admin/models", any(admin_providers::handle_admin_models))
            .route(
                "/admin/models/refresh",
                any(admin_providers::handle_models_refresh),
            )
            // Everything else - `/admin/users/<name>`, `/admin/`, the console's
            // own files - is dispatched by prefix in the fallback.
            .fallback(api::handle_admin_fallback)
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(
                state.clone(),
                with_admin_access,
            ))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                with_access_log,
            ))
            .layer(middleware::from_fn(with_recovery))
    }
}

// ---------------------------------------------------------------------------
// Upstream client
// ---------------------------------------------------------------------------

/// Builds the HTTP client tuned for proxying LLM traffic.
///
/// LLM responses are streamed, so compression stays off: it would have to be
/// undone before auditing, and it would make byte accounting dishonest. A
/// per-request deadline replaces a client-wide timeout, because a streamed
/// completion legitimately takes minutes while a connect must not.
pub fn new_upstream_client() -> Result<reqwest::Client, GatewayError> {
    reqwest::Client::builder()
        .connect_timeout(StdDuration::from_secs(15))
        .pool_idle_timeout(StdDuration::from_secs(90))
        .pool_max_idle_per_host(50)
        .redirect(reqwest::redirect::Policy::none())
        .no_gzip()
        .no_deflate()
        .no_brotli()
        .build()
        .map_err(|e| GatewayError::Client(e.to_string()))
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// Turns a panic in a handler into a JSON 500 instead of a dropped connection.
async fn with_recovery(req: Request, next: Next) -> Response {
    use futures_util::FutureExt;
    match std::panic::AssertUnwindSafe(next.run(req))
        .catch_unwind()
        .await
    {
        Ok(response) => response,
        Err(_) => {
            log::error!("panic in handler");
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal error",
            )
        }
    }
}

/// Logs one line per request, like the Go access log.
async fn with_access_log(State(server): State<Arc<Server>>, req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let response = next.run(req).await;
    let status = response.status().as_u16();
    // Health checks are noisy; log them only at debug level.
    if path.ends_with("healthz") || path.ends_with("readyz") {
        log::debug!(
            "http method={method} path={path} status={status} dur_ms={}",
            start.elapsed().as_millis()
        );
    } else {
        log::info!(
            "http method={method} path={path} status={status} dur_ms={}",
            start.elapsed().as_millis()
        );
    }
    let _ = &server;
    response
}

/// Enforces the management listener's reachability policy.
///
/// Only the API is gated, not the console's own files. Gating those too looks
/// tidier but does not work: the browser would be refused the page whose job is
/// to ask for the credential, so a remote operator could never authenticate. The
/// shell is a static bundle containing no configuration, no credentials and no
/// data; every byte of actual content comes from `/admin/*`, which is gated.
async fn with_admin_access(State(server): State<Arc<Server>>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let peer = peer_addr(&req);
    if let Some(refusal) = server.admin_gate(&path, peer, req.headers()) {
        return refusal;
    }
    next.run(req).await
}

impl Server {
    /// Applies the management access gate, returning the refusal when the
    /// request must not proceed.
    ///
    /// It is separated from the middleware so the security-relevant decisions
    /// can be asserted directly, exactly as the Go tests do through
    /// `AdminHandler` with a crafted `RemoteAddr`.
    pub(crate) fn admin_gate(
        &self,
        path: &str,
        peer: Option<SocketAddr>,
        headers: &HeaderMap,
    ) -> Option<Response> {
        // The prefix is checked without its trailing slash so that a bare
        // `/admin` cannot be used to probe for the API's existence.
        if !path.starts_with("/admin") || self.admin_allowed(peer, headers) {
            return None;
        }
        log::warn!(
            "admin access refused remote={} path={}",
            peer_text(peer),
            path
        );
        if !self.config().server.admin_remote {
            return Some(json_error(
                StatusCode::FORBIDDEN,
                "admin_forbidden",
                "the management API only answers the loopback interface; \
                 set server.admin_remote and server.admin_token to allow remote callers",
            ));
        }
        // Remote access is enabled, so the caller is simply not (or not
        // correctly) authenticated. The distinction matters to the console: it
        // asks for a credential on admin_unauthorized, whereas admin_forbidden
        // means no credential can help.
        let mut response = json_error(
            StatusCode::UNAUTHORIZED,
            "admin_unauthorized",
            "the management API requires the server.admin_token credential",
        );
        response.headers_mut().insert(
            axum::http::header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"customapi-admin\""),
        );
        Some(response)
    }

    /// Reports whether a request may reach the management API.
    ///
    /// It deliberately inspects the peer address rather than `client_ip`:
    /// `client_ip` honours `X-Forwarded-For`, which is caller-supplied, so
    /// trusting it here would let anyone claim to be 127.0.0.1 and walk straight
    /// into the management API.
    pub fn admin_allowed(&self, peer: Option<SocketAddr>, headers: &HeaderMap) -> bool {
        if peer.is_some_and(|a| a.ip().is_loopback()) {
            return true;
        }
        let cfg = self.config();
        if !cfg.server.admin_remote {
            return false;
        }
        // A remote caller must authenticate. An empty configured token is a
        // misconfiguration rather than an invitation, so it denies instead of
        // matching an empty credential.
        let want = &cfg.server.admin_token;
        if want.is_empty() {
            return false;
        }
        helpers::constant_time_eq(bearer_token(headers).as_bytes(), want.as_bytes())
    }

    /// Builds the body of `/healthz`.
    pub(crate) fn health_body(&self) -> serde_json::Value {
        let cfg = self.config();
        serde_json::json!({
            "audit_seq": self.ledger.seq(),
            "channels": cfg.all_channels().len(),
            "fail_mode": self.engine.fail_mode(),
            "inbound_rules": self.engine.inbound_rule_count(),
            "outbound_rules": self.engine.outbound_rule_count(),
            "stats": self.stats.snapshot(),
            "status": "ok",
            "uptime_s": self.uptime().as_secs() as i64,
            "users": cfg.users.len(),
            "version": version::VERSION,
        })
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// Emits an OpenAI-compatible error envelope so any client library understands a
/// gateway rejection.
///
/// `encoding/json` sorts map keys, so the members are written alphabetically to
/// keep the bytes identical to the Go implementation.
pub(crate) fn json_error(status: StatusCode, code: &str, message: &str) -> Response {
    let payload = serde_json::json!({
        "error": { "code": code, "message": message, "type": code },
    });
    json_response(status, &payload)
}

/// Emits an error envelope that also carries the audit id, so a user can quote it
/// and an operator can find the evidence.
pub(crate) fn audit_error(status: StatusCode, code: &str, message: &str, record_id: &str) -> Response {
    let payload = serde_json::json!({
        "customs_audit_id": record_id,
        "error": { "code": code, "message": message, "type": code },
    });
    let mut response = json_response(status, &payload);
    if let Ok(value) = HeaderValue::from_str(record_id) {
        response.headers_mut().insert("x-customs-audit-id", value);
    }
    response
}

/// Renders a JSON response the way Go's `json.Encoder` does, including the
/// trailing newline.
pub(crate) fn json_response(status: StatusCode, value: &serde_json::Value) -> Response {
    let mut body = serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec());
    body.push(b'\n');
    Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Emits a response with a pre-rendered body and content type.
pub(crate) fn bytes_response(status: StatusCode, content_type: &str, body: Vec<u8>) -> Response {
    Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Extracts the caller credential, accepting both the OpenAI `Authorization`
/// header and the Anthropic `x-api-key` header.
pub fn bearer_token(headers: &HeaderMap) -> String {
    if let Some(value) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        for prefix in ["Bearer ", "bearer "] {
            if let Some(rest) = value.strip_prefix(prefix) {
                return rest.trim().to_string();
            }
        }
    }
    if let Some(key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        if !key.is_empty() {
            return key.trim().to_string();
        }
    }
    if let Some(key) = headers.get("api-key").and_then(|v| v.to_str().ok()) {
        if !key.is_empty() {
            return key.trim().to_string();
        }
    }
    String::new()
}

/// Extracts the best-effort caller address.
///
/// `X-Forwarded-For` is honoured because the gateway normally sits behind the
/// user-facing relay, which is the component that knows the real peer. It is
/// never used for the management access decision.
///
/// The audit ledger deliberately does not carry it: Go defines `clientIP` but
/// never stores it either, and a record's bytes are part of the interoperability
/// contract. A deployment that wants the address in its logs can call this.
pub fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if !xff.is_empty() {
            let first = xff.split(',').next().unwrap_or("");
            return first.trim().to_string();
        }
    }
    peer_text(peer)
}

/// Renders a peer address the way Go's `r.RemoteAddr` looks.
pub(crate) fn peer_text(peer: Option<SocketAddr>) -> String {
    match peer {
        Some(addr) => addr.to_string(),
        // Go's `httptest` invents a routable peer address, and a request that
        // arrives without socket information is exactly as untrusted.
        None => "192.0.2.1:1234".to_string(),
    }
}

/// Returns the socket peer of a request, when the server injected one.
pub(crate) fn peer_addr(req: &Request) -> Option<SocketAddr> {
    req.extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|info| info.0)
}

/// Reports whether a `host:port` peer address is the loopback interface.
///
/// Ported verbatim from Go, including the `localhost` special case, because the
/// management gate's unit test asserts the same table.
pub fn is_loopback_addr(remote_addr: &str) -> bool {
    let host = match split_host_port(remote_addr) {
        Some((host, _)) => host,
        None => remote_addr,
    };
    if host == "localhost" {
        return true;
    }
    host.trim_matches(|c| c == '[' || c == ']')
        .parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Splits `host:port`, accepting IPv6 literals in brackets.
fn split_host_port(addr: &str) -> Option<(&str, &str)> {
    if addr.is_empty() {
        return None;
    }
    if let Some(rest) = addr.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':')?;
        if port.is_empty() {
            return None;
        }
        return Some((host, port));
    }
    let (host, port) = addr.rsplit_once(':')?;
    if host.is_empty() || port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((host, port))
}

/// Strips characters that would break header framing.
pub(crate) fn sanitize_header_value(value: &str) -> String {
    value.chars().filter(|c| *c != '\r' && *c != '\n').collect()
}

#[cfg(test)]
mod internal_tests {
    use super::*;

    #[test]
    fn loopback_detection_matches_go_table() {
        for (addr, want) in [
            ("127.0.0.1:8080", true),
            ("127.0.0.5:1", true),
            ("[::1]:8080", true),
            ("localhost:80", true),
            ("10.10.1.5:80", false),
            ("[fe80::1]:80", false),
            ("127.0.0.1", true),
            ("", false),
            ("garbage", false),
        ] {
            assert_eq!(is_loopback_addr(addr), want, "is_loopback_addr({addr:?})");
        }
    }

    #[test]
    fn request_id_shape() {
        let id = helpers::new_request_id();
        assert!(id.starts_with("req_"), "{id}");
        assert_eq!(id.len(), 4 + 24, "{id}");
        assert!(id[4..].chars().all(|c| c.is_ascii_hexdigit()), "{id}");
    }
}
