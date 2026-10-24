//! The catalogue service: upstream model lists, merged with operator intent.

use super::io::{is_not_exist, read_file, write_file_atomic};
use crate::config::{self, Channel, Config, DiscoveryPolicy, Provider};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Catalogue shapes
// ---------------------------------------------------------------------------

/// Records where a catalogue entry came from.
///
/// Go spells this `type Source string` with two constants and allows any value;
/// it is a `String` here so that a cache file written by a future version still
/// parses.
pub type Source = String;

/// Operator-declared, and always wins.
pub const SOURCE_CONFIGURED: &str = "configured";
/// Came from the provider's model-list endpoint.
pub const SOURCE_DISCOVERED: &str = "discovered";

/// One servable model on one channel.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Canonical, user-facing name.
    pub model: String,
    /// The channel that serves it.
    pub channel: String,
    /// The credential that reaches it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// Name of the key within the provider.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// The name sent to the provider.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub upstream_model: String,
    /// The channel's native protocol.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub protocol: String,
    /// Every protocol the channel can accept: its native one plus any the
    /// provider declared it also serves.
    ///
    /// It is what lets the catalogue tell a chat-only channel apart from one that
    /// also speaks the Responses or Messages surface.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_default"
    )]
    pub protocols: Vec<String>,
    /// Whether this entry was configured or discovered.
    pub source: Source,
    /// Provider-reported owner, when it reports one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owned_by: String,
    /// Provider-reported creation time, when available.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub created: i64,
    /// When the entry was last confirmed by discovery.
    ///
    /// Go's `omitempty` does not apply to a `time.Time`, so the zero timestamp is
    /// always written.
    #[serde(default = "config::zero_time")]
    pub discovered_at: DateTime<Utc>,
    /// A discovery failure for this provider, so an operator can see why a
    /// provider contributed nothing instead of assuming it has no models.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// Groups the models one channel serves.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChannelModels {
    /// Channel name.
    pub channel: String,
    /// Provider that owns the channel.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// Key within the provider.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// Native protocol.
    pub protocol: String,
    /// The models this channel serves.
    #[serde(
        default,
        serialize_with = "vec_null_when_empty",
        deserialize_with = "null_default"
    )]
    pub models: Vec<ModelEntry>,
    /// When this channel's list was last refreshed.
    #[serde(default = "config::zero_time")]
    pub discovered: DateTime<Utc>,
    /// Whether the list is older than the configured threshold.
    #[serde(default, skip_serializing_if = "crate::json::is_false")]
    pub stale: bool,
    /// Set when the last refresh failed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// Whether the channel declares models but discovery produced nothing, so the
    /// list is operator-supplied rather than verified.
    #[serde(default, skip_serializing_if = "crate::json::is_false")]
    pub configured_only: bool,
}

/// The whole catalogue at one point in time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// When discovery last ran to completion.
    #[serde(default = "config::zero_time")]
    pub updated_at: DateTime<Utc>,
    /// Per-channel model lists.
    #[serde(
        default,
        serialize_with = "vec_null_when_empty",
        deserialize_with = "null_default"
    )]
    pub channels: Vec<ChannelModels>,
    /// Discovered models per provider.
    ///
    /// Sorted (a `BTreeMap`) because Go's `encoding/json` sorts map keys and the
    /// snapshot is a file operators may diff.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "null_default"
    )]
    pub providers: BTreeMap<String, i32>,
    /// Provider-level failures from the last run.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "null_default"
    )]
    pub errors: BTreeMap<String, String>,
}

/// The merged, user-facing view of one model name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelView {
    /// Canonical model name.
    pub model: String,
    /// Every channel that can serve it, with the protocol each speaks.
    #[serde(
        default,
        serialize_with = "vec_null_when_empty",
        deserialize_with = "null_default"
    )]
    pub channels: Vec<ModelChannel>,
    /// De-duplicated set of protocols available for this model.
    #[serde(
        default,
        serialize_with = "vec_null_when_empty",
        deserialize_with = "null_default"
    )]
    pub protocols: Vec<String>,
    /// Whether at least one provider declares it explicitly.
    pub configured: bool,
    /// The channel with the lowest effective unit price, when prices are known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cheapest: String,
    /// The cheapest effective unit price.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_f64")]
    pub unit_price: f64,
    /// Currency of [`ModelView::unit_price`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub currency: String,
}

/// One channel serving a model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelChannel {
    /// Channel name.
    pub channel: String,
    /// Provider that owns the channel.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// Key within the provider.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// Native protocol.
    pub protocol: String,
    /// Upstream model name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub upstream_model: String,
    /// Where the entry came from.
    pub source: Source,
    /// Operator-declared trust level of the channel.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub trust: String,
    /// Client protocols this channel can accept by translation, i.e. every
    /// protocol it supports other than its own.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_default"
    )]
    pub translate_from: Vec<String>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Reports a discovery failure.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// The cache file exists but cannot be parsed.
    #[error("discovery: parse cache {path}: {message}")]
    ParseCache {
        /// Path that failed.
        path: String,
        /// Parser message.
        message: String,
    },
    /// The model-list request could not be issued or completed.
    #[error("model list request failed: {0}")]
    Request(String),
    /// The model-list response body could not be read.
    #[error("model list read failed: {0}")]
    Read(String),
    /// The upstream answered with a non-200 status.
    #[error("model list returned HTTP {status}: {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// Truncated body, for the operator to see why.
        body: String,
    },
    /// The model-list body was not the expected envelope.
    #[error("model list parse failed: {0}")]
    Parse(String),
    /// The snapshot could not be encoded.
    #[error("discovery: encode cache: {0}")]
    EncodeCache(String),
    /// A cache file could not be read or written.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// The largest model list the service will read from an upstream.
const MAX_MODEL_LIST_BYTES: usize = 8 << 20;

/// How much of an error body is quoted back to the operator.
const ERROR_BODY_LIMIT: usize = 160;

/// A clock the service reads `now` from; injectable for tests.
type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Configures the service.
#[derive(Default)]
pub struct Options {
    /// The global discovery policy.
    pub policy: DiscoveryPolicy,
    /// Overrides the HTTP client, which is how tests point discovery at a local
    /// server.
    pub client: Option<reqwest::Client>,
    /// Overrides the clock, for tests.
    pub now: Option<Clock>,
}

/// Discovers and caches provider model lists.
///
/// Unlike the Go type, progress messages go through the `log` crate rather than
/// an injected `slog` logger, which is what the porting rules ask for; the
/// message text is kept.
pub struct Service {
    snapshot: RwLock<Snapshot>,
    /// Performs discovery requests.
    client: reqwest::Client,
    /// Persists the snapshot across restarts.
    cache_path: String,
    /// Marks entries as stale in the view.
    stale_after: Duration,
    /// The clock, injectable for tests.
    now: Clock,
    /// Operator-declared models, which always win.
    configured: RwLock<HashMap<String, Vec<ModelEntry>>>,
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service")
            .field("cache_path", &self.cache_path)
            .field("stale_after", &self.stale_after)
            .finish()
    }
}

impl Service {
    /// Creates a discovery service.
    pub fn new(opts: Options) -> Service {
        let client = match opts.client {
            Some(client) => client,
            None => {
                let timeout = opts.policy.timeout_or_default();
                reqwest::Client::builder()
                    .timeout(timeout)
                    .build()
                    .unwrap_or_else(|err| {
                        log::warn!("discovery: http client build failed, using the default: {err}");
                        reqwest::Client::new()
                    })
            }
        };
        let stale_after = opts.policy.stale_after_or_default();
        let now: Clock = opts.now.unwrap_or_else(|| Arc::new(Utc::now));
        Service {
            snapshot: RwLock::new(Snapshot::default()),
            client,
            cache_path: opts.policy.cache_path,
            stale_after,
            now,
            configured: RwLock::new(HashMap::new()),
        }
    }

    /// Restores a cached snapshot from disk.
    ///
    /// A missing cache is not an error: the first discovery run populates it.
    pub fn load(&self) -> Result<(), DiscoveryError> {
        if self.cache_path.is_empty() {
            return Ok(());
        }
        let bytes = match read_file(&self.cache_path) {
            Ok(bytes) => bytes,
            Err(err) if is_not_exist(&err) => return Ok(()),
            Err(err) => return Err(DiscoveryError::Io(err)),
        };
        let snapshot: Snapshot =
            serde_json::from_slice(&bytes).map_err(|err| DiscoveryError::ParseCache {
                path: self.cache_path.clone(),
                message: err.to_string(),
            })?;
        *write_lock(&self.snapshot) = snapshot;
        Ok(())
    }

    /// Persists the current snapshot.
    pub fn save(&self) -> Result<(), DiscoveryError> {
        if self.cache_path.is_empty() {
            return Ok(());
        }
        let snapshot = read_lock(&self.snapshot).clone();
        let bytes = serde_json::to_vec_pretty(&snapshot)
            .map_err(|err| DiscoveryError::EncodeCache(err.to_string()))?;
        write_file_atomic(&self.cache_path, &bytes)?;
        Ok(())
    }

    /// Returns the current catalogue.
    pub fn snapshot(&self) -> Snapshot {
        read_lock(&self.snapshot).clone()
    }

    /// Installs the operator-declared models for each channel.
    ///
    /// Configured entries always win, so a provider that reports a model under a
    /// different name cannot hide the operator's mapping.
    pub fn set_configured(&self, channels: &[Channel]) {
        let discovered_at = (self.now)();
        let mut by_channel: HashMap<String, Vec<ModelEntry>> = HashMap::new();
        for ch in channels {
            if ch.models.is_empty() {
                continue;
            }
            let (provider, key) = provider_of_channel(ch);
            let mut names: Vec<&String> = ch.models.keys().collect();
            names.sort();
            let entries: Vec<ModelEntry> = names
                .into_iter()
                .map(|canonical| ModelEntry {
                    model: canonical.clone(),
                    channel: ch.name.clone(),
                    provider: provider.clone(),
                    key: key.clone(),
                    upstream_model: ch.models.get(canonical).cloned().unwrap_or_default(),
                    protocol: channel_protocol(ch).to_string(),
                    protocols: channel_protocols(ch),
                    source: SOURCE_CONFIGURED.to_string(),
                    discovered_at,
                    ..ModelEntry::default()
                })
                .collect();
            by_channel.insert(ch.name.clone(), entries);
        }
        *write_lock(&self.configured) = by_channel;
    }

    /// Performs discovery now and then on every interval until the task is
    /// aborted.
    ///
    /// The Go version takes a `context.Context`; the tokio equivalent is to spawn
    /// this future and abort the task, which is what the gateway does. [`run`]
    /// therefore loops until dropped.
    ///
    /// [`run`]: Service::run
    pub async fn run(&self, cfg: Arc<Config>, interval: Duration) {
        self.refresh(&cfg).await;
        if interval.is_zero() {
            return;
        }
        let mut ticker = tokio::time::interval(interval);
        // Go's Ticker drops ticks the consumer missed instead of bursting.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // tokio fires immediately; Go waits one interval first.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            self.refresh(&cfg).await;
        }
    }

    /// Queries every provider that has discovery enabled.
    pub async fn refresh(&self, cfg: &Config) {
        let channels = cfg.all_channels();
        self.set_configured(&channels);
        let global_enabled = cfg.discovery.enabled_default();

        // A provider with several keys is queried once per key but reported per
        // provider; a directly-configured channel has no discovery policy and
        // still gets an entry built from its declared models.
        let jobs: Vec<(Channel, Provider)> = channels
            .into_iter()
            .map(|ch| {
                let provider = cfg.provider_by_channel_name(&ch.name).unwrap_or_default();
                (ch, provider)
            })
            .collect();
        let mut results = futures_util::future::join_all(
            jobs.into_iter()
                .map(|(ch, provider)| self.discover_channel(ch, provider, global_enabled)),
        )
        .await;
        results.sort_by(|a, b| a.channel.cmp(&b.channel));

        let mut providers: BTreeMap<String, i32> = BTreeMap::new();
        let mut errors: BTreeMap<String, String> = BTreeMap::new();
        for cm in &results {
            let count = providers.entry(cm.provider.clone()).or_insert(0);
            *count += cm.models.len() as i32;
            if !cm.error.is_empty() {
                errors.insert(cm.provider.clone(), cm.error.clone());
            }
        }
        let total: i32 = providers.values().sum();
        let channel_count = results.len();
        let error_count = errors.len();
        let updated_at = (self.now)();
        {
            let mut snapshot = write_lock(&self.snapshot);
            *snapshot = Snapshot {
                updated_at,
                channels: results,
                providers,
                errors,
            };
        }

        if let Err(err) = self.save() {
            log::warn!("discovery cache save failed: {err}");
        }
        log::info!(
            "model discovery complete: channels={channel_count} models={total} providers_with_errors={error_count}"
        );
    }

    /// Queries one channel and merges the result with what the operator declared
    /// for it.
    async fn discover_channel(
        &self,
        ch: Channel,
        provider: Provider,
        global_enabled: bool,
    ) -> ChannelModels {
        let mut cm = ChannelModels {
            channel: ch.name.clone(),
            protocol: channel_protocol(&ch).to_string(),
            ..ChannelModels::default()
        };
        if !provider.name.is_empty() {
            cm.provider = provider.name.clone();
        }
        if let Some(key) = ch.metadata.get("key") {
            cm.key = key.clone();
        }
        if cm.provider.is_empty() {
            if let Some(name) = ch.metadata.get("provider") {
                cm.provider = name.clone();
            }
        }

        // Configured entries always come first and are never overwritten.
        let configured = read_lock(&self.configured)
            .get(&ch.name)
            .cloned()
            .unwrap_or_default();
        cm.models.extend(configured.iter().cloned());
        let mut seen: HashSet<String> = configured.iter().map(|e| e.model.clone()).collect();

        let discover = provider.name.is_empty() || provider.discover_enabled(global_enabled);
        if !discover {
            cm.configured_only = !configured.is_empty();
            cm.discovered = (self.now)();
            return cm;
        }

        let found = match self.fetch_models(&ch, &provider).await {
            Ok(found) => found,
            Err(err) => {
                cm.error = err.to_string();
                cm.configured_only = !configured.is_empty();
                cm.discovered = (self.now)();
                return cm;
            }
        };
        cm.discovered = (self.now)();
        let discovered = cm.discovered;
        let provider_name = cm.provider.clone();
        let key_name = cm.key.clone();
        let protocols = channel_protocols(&ch);
        for m in found {
            if m.id.is_empty() {
                continue;
            }
            // Only names the operator asked for are exposed under their bare
            // name; everything else is namespaced so that two providers serving
            // the same model name do not silently collide in the user-facing
            // catalogue.
            let canonical = m.id;
            if !self.allowed_by_filter(&provider, &canonical) {
                continue;
            }
            if !seen.insert(canonical.clone()) {
                continue;
            }
            cm.models.push(ModelEntry {
                model: canonical.clone(),
                channel: ch.name.clone(),
                provider: provider_name.clone(),
                key: key_name.clone(),
                upstream_model: canonical,
                protocol: cm.protocol.clone(),
                protocols: protocols.clone(),
                source: SOURCE_DISCOVERED.to_string(),
                owned_by: m.owned_by,
                created: m.created,
                discovered_at: discovered,
                error: String::new(),
            });
        }
        cm.configured_only = cm.models.is_empty() && !configured.is_empty();
        cm
    }

    /// Applies the provider's include/exclude patterns.
    ///
    /// Exclusion wins over inclusion, so an operator can write a broad include
    /// ("gpt-4*") and still carve out a single model.
    fn allowed_by_filter(&self, p: &Provider, model: &str) -> bool {
        let Some(discover) = &p.discover else {
            return true;
        };
        for pattern in &discover.exclude {
            if match_pattern(pattern, model) {
                return false;
            }
        }
        if discover.include.is_empty() {
            return true;
        }
        discover
            .include
            .iter()
            .any(|pattern| match_pattern(pattern, model))
    }

    /// Asks a channel for its model list.
    ///
    /// The endpoint and the authentication depend on the protocol: the Responses
    /// and chat APIs share `/v1/models` with a bearer token, while the Messages
    /// API exposes `/v1/models` with the `x-api-key` header.
    async fn fetch_models(
        &self,
        ch: &Channel,
        p: &Provider,
    ) -> Result<Vec<ProviderModel>, DiscoveryError> {
        let path = p
            .discover
            .as_ref()
            .map(|d| d.path.as_str())
            .filter(|path| !path.is_empty())
            .unwrap_or("/v1/models");
        // `upstream_url` tolerates a base_url that already carries the version
        // segment, which is how most aggregators document their endpoint.
        let url = config::upstream_url(&ch.base_url, path);
        let mut request = self
            .client
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/json");
        match channel_protocol(ch) {
            "anthropic" => {
                if !ch.api_key.is_empty() {
                    request = request.header("x-api-key", ch.api_key.clone());
                }
                request = request.header("anthropic-version", "2023-06-01");
            }
            _ => {
                if !ch.api_key.is_empty() {
                    request = request.header("Authorization", format!("Bearer {}", ch.api_key));
                }
            }
        }
        for (name, value) in &ch.headers {
            request = request.header(name.as_str(), value.as_str());
        }

        let mut response = request
            .send()
            .await
            .map_err(|err| DiscoveryError::Request(err.to_string()))?;
        let status = response.status().as_u16();
        let body = read_capped(&mut response)
            .await
            .map_err(|err| DiscoveryError::Read(err.to_string()))?;
        let text = String::from_utf8_lossy(&body);
        if status != 200 {
            return Err(DiscoveryError::Status {
                status,
                body: truncate(&text, ERROR_BODY_LIMIT),
            });
        }

        // Both protocols return {"data":[...]}; Anthropic uses the same envelope.
        let payload: ModelList =
            serde_json::from_str(&text).map_err(|err| DiscoveryError::Parse(err.to_string()))?;
        let mut out = Vec::with_capacity(payload.data.len());
        for mut model in payload.data {
            if model.id.is_empty() {
                model.id = model.name.clone();
            }
            if !model.id.is_empty() {
                out.push(model);
            }
        }
        Ok(out)
    }

    /// Merges the catalogue into a user-facing list.
    ///
    /// `allow`, when present, filters the result, which is how a user restricted
    /// to a set of models sees only those.
    pub fn models(&self, allow: Option<&dyn Fn(&str) -> bool>) -> Vec<ModelView> {
        let snapshot = read_lock(&self.snapshot).clone();
        let configured = read_lock(&self.configured).clone();

        let mut by_model: HashMap<String, ModelView> = HashMap::new();
        // `seen` deduplicates by channel plus upstream name: the snapshot and the
        // configured list can both carry the same model (discovery confirms what
        // the operator declared), and listing it twice would make any channel
        // count wrong.
        let mut seen: HashSet<String> = HashSet::new();
        for cm in &snapshot.channels {
            for entry in &cm.models {
                add_model(&mut by_model, &mut seen, allow, entry);
            }
        }
        // A configured model whose channel did not appear in the last snapshot
        // (because discovery failed outright) must still be offered: the model is
        // servable, the catalogue is just unverified.
        for entries in configured.values() {
            for entry in entries {
                add_model(&mut by_model, &mut seen, allow, entry);
            }
        }

        let mut out: Vec<ModelView> = Vec::with_capacity(by_model.len());
        for (_, mut view) in by_model {
            // A model's protocol list is the union over its channels of what each
            // channel can accept: its own protocol plus everything it declared it
            // can also serve. Reporting only the channel's own protocol told an
            // operator that a model was chat-only when the provider had in fact
            // declared the Responses surface too.
            let mut protocols: BTreeSet<String> = BTreeSet::new();
            for channel in &view.channels {
                if !channel.protocol.is_empty() {
                    protocols.insert(channel.protocol.clone());
                }
                for p in &channel.translate_from {
                    if !p.is_empty() {
                        protocols.insert(p.clone());
                    }
                }
            }
            view.protocols = protocols.into_iter().collect();
            view.channels.sort_by(|a, b| {
                (a.channel.as_str(), a.key.as_str()).cmp(&(b.channel.as_str(), b.key.as_str()))
            });
            out.push(view);
        }
        out.sort_by(|a, b| a.model.cmp(&b.model));
        out
    }

    /// Returns the per-channel catalogue with staleness marked.
    pub fn channels(&self) -> Vec<ChannelModels> {
        let (snapshot, configured, now) = {
            let snapshot = read_lock(&self.snapshot).clone();
            let configured = read_lock(&self.configured).clone();
            (snapshot, configured, (self.now)())
        };
        let stale_after = TimeDelta::from_std(self.stale_after).unwrap_or(TimeDelta::MAX);

        let mut out: Vec<ChannelModels> = Vec::with_capacity(snapshot.channels.len());
        for mut cm in snapshot.channels {
            if is_zero_time(&cm.discovered) {
                cm.discovered = snapshot.updated_at;
            }
            cm.stale = !self.stale_after.is_zero()
                && !is_zero_time(&cm.discovered)
                && now.signed_duration_since(cm.discovered) > stale_after;
            out.push(cm);
        }
        // Channels the last run did not reach still deserve an entry built from
        // their configured models, so a provider outage does not make them
        // vanish.
        let present: HashSet<String> = out.iter().map(|cm| cm.channel.clone()).collect();
        for (channel, entries) in &configured {
            if present.contains(channel) {
                continue;
            }
            let mut cm = ChannelModels {
                channel: channel.clone(),
                models: entries.clone(),
                ..ChannelModels::default()
            };
            if let Some(first) = entries.first() {
                cm.protocol = first.protocol.clone();
                cm.provider = first.provider.clone();
                cm.key = first.key.clone();
            }
            cm.configured_only = true;
            out.push(cm);
        }
        out.sort_by(|a, b| a.channel.cmp(&b.channel));
        out
    }

    /// Reports every channel that can serve a model.
    pub fn lookup(&self, model: &str) -> Option<ModelView> {
        self.models(None).into_iter().find(|v| v.model == model)
    }

    /// Reports whether the catalogue knows a model at all.
    pub fn known(&self, model: &str) -> bool {
        self.lookup(model).is_some()
    }
}

// ---------------------------------------------------------------------------
// Upstream model list
// ---------------------------------------------------------------------------

/// One entry of an upstream model list.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ProviderModel {
    /// Identifier sent back as the upstream model name.
    #[serde(default)]
    pub(crate) id: String,
    /// Creation time, when the upstream reports one.
    #[serde(default)]
    pub(crate) created: i64,
    /// Provider-reported owner.
    #[serde(default)]
    pub(crate) owned_by: String,
    /// Some compatible servers nest the identifier under `name`.
    #[serde(default)]
    pub(crate) name: String,
}

/// The `{"data":[...]}` envelope both protocols use.
#[derive(Debug, Default, Deserialize)]
struct ModelList {
    #[serde(default)]
    data: Vec<ProviderModel>,
}

/// Reads a response body, refusing to buffer more than [`MAX_MODEL_LIST_BYTES`].
///
/// Go wraps the body in an `io.LimitReader`; a hostile or broken upstream must not
/// be able to make the gateway allocate without bound.
async fn read_capped(response: &mut reqwest::Response) -> Result<Vec<u8>, reqwest::Error> {
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = MAX_MODEL_LIST_BYTES.saturating_sub(body.len());
        if room == 0 {
            break;
        }
        if chunk.len() >= room {
            body.extend_from_slice(&chunk[..room]);
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Trims whitespace and shortens a string for an error message.
pub(crate) fn truncate(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.len() <= n {
        return s.to_string();
    }
    // Go slices bytes and may hand back invalid UTF-8; Rust cannot, so the cut
    // moves to the nearest character boundary instead of panicking.
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// Supports exact names and a trailing `*` wildcard, which covers how operators
/// actually filter (e.g. `gpt-4*`).
pub(crate) fn match_pattern(pattern: &str, name: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    pattern == name
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Adds one entry to the merged view, deduplicating and filtering as it goes.
fn add_model(
    by_model: &mut HashMap<String, ModelView>,
    seen: &mut HashSet<String>,
    allow: Option<&dyn Fn(&str) -> bool>,
    entry: &ModelEntry,
) {
    if let Some(allow) = allow {
        if !allow(&entry.model) {
            return;
        }
    }
    let key = format!(
        "{}\u{0}{}\u{0}{}",
        entry.model, entry.channel, entry.upstream_model
    );
    if !seen.insert(key) {
        return;
    }
    let view = by_model
        .entry(entry.model.clone())
        .or_insert_with(|| ModelView {
            model: entry.model.clone(),
            ..ModelView::default()
        });
    // Everything the channel can accept beyond the protocol it speaks is
    // reachable only through translation, and saying so explicitly is what makes
    // the console able to warn about it.
    let mut translate_from: Vec<String> = entry
        .protocols
        .iter()
        .filter(|p| !p.is_empty() && **p != entry.protocol)
        .cloned()
        .collect();
    translate_from.sort();
    view.channels.push(ModelChannel {
        channel: entry.channel.clone(),
        provider: entry.provider.clone(),
        key: entry.key.clone(),
        protocol: entry.protocol.clone(),
        upstream_model: entry.upstream_model.clone(),
        source: entry.source.clone(),
        trust: String::new(),
        translate_from,
    });
    if entry.source == SOURCE_CONFIGURED {
        view.configured = true;
    }
}

/// Returns the channel's native protocol.
fn channel_protocol(ch: &Channel) -> &str {
    if ch.protocol.is_empty() {
        "openai"
    } else {
        &ch.protocol
    }
}

/// Returns every protocol a channel can accept, native first.
///
/// The provider expansion records the declaration in `metadata["protocols"]`; a
/// directly-configured channel has no metadata and therefore supports only what
/// it speaks.
fn channel_protocols(ch: &Channel) -> Vec<String> {
    let native = channel_protocol(ch);
    let mut out = vec![native.to_string()];
    if let Some(listed) = ch.metadata.get("protocols") {
        for p in listed.split(',') {
            let p = p.trim();
            if p.is_empty() || p == native {
                continue;
            }
            out.push(p.to_string());
        }
    }
    out
}

/// Returns the provider and key names a channel was expanded from.
fn provider_of_channel(ch: &Channel) -> (String, String) {
    (
        ch.metadata.get("provider").cloned().unwrap_or_default(),
        ch.metadata.get("key").cloned().unwrap_or_default(),
    )
}

/// Accepts an explicit `null` where a collection is expected.
///
/// Go writes `null` for a nil slice or map, and several catalogue fields have no
/// `omitempty`, so a cache produced by the Go implementation can contain
/// `"models": null` or `"channels": null`. `#[serde(default)]` only covers a
/// *missing* field, so the nullable spelling needs handling of its own.
fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Writes an empty slice as `null`, the way Go serialises a nil slice.
///
/// The cache file is a document operators may diff against the Go
/// implementation's, so the two must not disagree on `[]` versus `null`.
fn vec_null_when_empty<S, T>(value: &[T], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
    T: Serialize,
{
    if value.is_empty() {
        serializer.serialize_none()
    } else {
        value.serialize(serializer)
    }
}

/// Reports whether a timestamp is Go's zero time.
fn is_zero_time(at: &DateTime<Utc>) -> bool {
    *at == config::zero_time()
}

/// Locks for reading, ignoring poisoning.
///
/// A panicking admin handler must not turn the catalogue into a permanently
/// unusable object; the guarded data is plain and cannot be left inconsistent.
fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

/// Locks for writing, ignoring poisoning (see [`read_lock`]).
fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}
