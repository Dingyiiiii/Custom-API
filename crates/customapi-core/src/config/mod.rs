//! Configuration loading and validation.
//!
//! The configuration is intentionally one flat JSON file plus environment
//! overrides, so the whole gateway state is auditable and easy to diff.
//!
//! The module is a direct port of `internal/config` in the Go implementation:
//! the JSON field names, the defaults, the validation messages and the
//! environment-variable conventions are identical, because operators keep the
//! same files when they move between the two binaries.

mod envfile;
mod provider;

pub use envfile::{apply_env_pairs, load_env_file};
pub use provider::{
    upstream_url, ApiKey, DiscoverPolicy, DiscoveryPolicy, KeyLimits, KeySummary, Provider,
    ProviderSummary, RoutingPolicy,
};

use crate::json::Duration;
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Trust model
// ---------------------------------------------------------------------------

/// Trust controls how much auditing a subject is exempted from.
///
/// The project's core premise is "few subjects are trusted, everything else is
/// untrusted by default". Trust never disables metering or audit persistence;
/// it only relaxes content inspection.
pub const TRUST_UNTRUSTED: &str = "untrusted";

/// Trusted subjects get relaxed content inspection (fewer false positives,
/// lower latency) but keep usage verification and audit recording enabled.
pub const TRUST_TRUSTED: &str = "trusted";

/// Reports whether a string is a recognised trust level.
pub fn trust_valid(trust: &str) -> bool {
    trust == TRUST_UNTRUSTED || trust == TRUST_TRUSTED
}

/// Resolves an empty trust string to the default level.
pub fn trust_level(trust: &str) -> &str {
    if trust.is_empty() {
        TRUST_UNTRUSTED
    } else {
        trust
    }
}

// ---------------------------------------------------------------------------
// Channel (upstream access point)
// ---------------------------------------------------------------------------

/// Channel describes one upstream access point.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    /// Stable identifier used in routing and audit records.
    pub name: String,
    /// Upstream API root, e.g. `https://api.example.com`.
    pub base_url: String,
    /// Upstream credential. It is never exposed to users.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// Wire protocol spoken by this channel: "openai", "responses" or
    /// "anthropic". Empty means "openai".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub protocol: String,
    /// Maps the canonical (user-facing) model name to the upstream's own model
    /// identifier. An empty map means "pass through unchanged".
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub models: HashMap<String, String>,
    /// Relaxes content inspection for this channel.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub trust: String,
    /// Biases the router. Zero means the default (1.0).
    #[serde(
        default,
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub weight: f64,
    /// Allows disabling a channel without deleting it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Overrides the per-request upstream timeout.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_duration")]
    pub timeout: Duration,
    /// Extra headers attached to upstream requests.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    /// Free-form operator annotations (provider, notes...).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

impl Channel {
    /// Reports whether the channel may receive traffic.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Returns the effective trust level.
    pub fn trust_level(&self) -> &str {
        trust_level(&self.trust)
    }

    /// Translates a canonical model name to the channel's own name.
    ///
    /// Returns `None` when the channel is known not to serve the model, which
    /// is what lets the router exclude a channel rather than sending it a model
    /// it does not have and returning an upstream error to the user.
    ///
    /// A channel with an empty `models` map is treated as an explicit
    /// pass-through: it accepts whatever model name it is given. A channel with
    /// a non-empty map has declared its catalogue and is held to it.
    pub fn upstream_model(&self, canonical: &str) -> Option<String> {
        if self.models.is_empty() {
            return Some(canonical.to_string());
        }
        self.models.get(canonical).cloned()
    }

    /// The inverse of [`Channel::upstream_model`], used when fingerprinting a
    /// response whose model field was rewritten by the upstream.
    pub fn canonical_model(&self, upstream: &str) -> String {
        for (k, v) in &self.models {
            if v == upstream {
                return k.clone();
            }
        }
        upstream.to_string()
    }

    /// Returns the timeout override, or the given fallback when unset.
    pub fn timeout_or(&self, fallback: Duration) -> Duration {
        if self.timeout.0.is_zero() {
            fallback
        } else {
            self.timeout
        }
    }
}

// ---------------------------------------------------------------------------
// User
// ---------------------------------------------------------------------------

/// User is a gateway client.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    /// Stable identifier recorded in audit records.
    pub name: String,
    /// Bearer credential the client presents.
    #[serde(default)]
    pub token: String,
    /// Total allowance in quota units; 0 means unlimited.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub quota: i64,
    /// Resets the consumed counter on a schedule: "none", "daily", "weekly" or
    /// "monthly".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub period: String,
    /// Restricts this user to a set of canonical models. Empty means all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Sustained request rate in requests per second. Zero disables it.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_f64")]
    #[serde(serialize_with = "crate::json::go_f64")]
    pub rate_limit: f64,
    /// Rate limiter bucket size. Zero means 2x `rate_limit`.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i32")]
    pub burst: i32,
    /// Caps in-flight requests. Zero means unlimited.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i32")]
    pub max_concurrent: i32,
    /// Relaxes inbound/outbound content inspection for this user.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub trust: String,
    /// Allows suspending a user without deleting it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl User {
    /// Reports whether the user may issue requests.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Returns the effective trust level.
    pub fn trust_level(&self) -> &str {
        trust_level(&self.trust)
    }

    /// Reports whether the user may call the given canonical model.
    pub fn allows_model(&self, model: &str) -> bool {
        if self.models.is_empty() {
            return true;
        }
        if self.models.iter().any(|m| m == model) {
            return true;
        }
        // Wildcard support: "gpt-*" style prefixes.
        self.models.iter().any(|m| {
            m.strip_suffix('*')
                .is_some_and(|prefix| model.starts_with(prefix))
        })
    }
}

// ---------------------------------------------------------------------------
// Pricing
// ---------------------------------------------------------------------------

/// Off-peak discounts apply a multiplier inside a daily time window.
pub const PROMO_OFF_PEAK: &str = "off_peak";
/// Volume discounts apply a multiplier once usage exceeds a threshold.
pub const PROMO_VOLUME: &str = "volume";
/// Subscription plans are amortised over an allowance.
pub const PROMO_SUBSCRIBE: &str = "subscription";

/// PriceModel is the normalized price for one canonical model on one channel.
///
/// Every upstream pricing scheme (multiplier, flat, per-request, tiered
/// expression) is reduced to a pair of per-million-token rates so that channels
/// can be compared directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceModel {
    /// Price per million input tokens, in `currency`.
    #[serde(serialize_with = "crate::json::go_f64")]
    pub input_per_m: f64,
    /// Price per million output tokens, in `currency`.
    #[serde(serialize_with = "crate::json::go_f64")]
    pub output_per_m: f64,
    /// Unit of the two rates above, e.g. "CNY" or "USD".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub currency: String,
    /// How the raw upstream price was expressed: "multiplier", "flat",
    /// "per_request", "tiered" or "manual".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scheme: String,
    /// Raw ratio used by multiplier schemes, kept for auditability.
    #[serde(
        default,
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub completion_ratio: f64,
    /// Discounts that depend on time or volume.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub promotions: Vec<Promotion>,
    /// Documents where the number came from (URL or "manual").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// When the price was last observed.
    #[serde(default = "zero_time", serialize_with = "crate::json::go_time")]
    pub updated_at: DateTime<Utc>,
}

impl Default for PriceModel {
    /// The Go zero value, including its zero `time.Time`.
    ///
    /// A derived `Default` would leave `updated_at` at the Unix epoch, which is
    /// what chrono defaults to, and the catalogue would then claim a price had
    /// been observed in 1970 instead of never.
    fn default() -> Self {
        PriceModel {
            input_per_m: 0.0,
            output_per_m: 0.0,
            currency: String::new(),
            scheme: String::new(),
            completion_ratio: 0.0,
            promotions: Vec::new(),
            source: String::new(),
            updated_at: zero_time(),
        }
    }
}

/// Promotion is one discount rule.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Promotion {
    /// Discount mechanism: "off_peak", "volume" or "subscription".
    pub kind: String,
    /// Scales the base price (0.8 == 20% off). Zero means 1.0.
    #[serde(
        default,
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub multiplier: f64,
    /// Bounds an off-peak window as minutes past local midnight. A window may
    /// wrap past midnight (start > end).
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i32")]
    pub start_minute: i32,
    /// End of the off-peak window.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i32")]
    pub end_minute: i32,
    /// Volume threshold for [`PROMO_VOLUME`].
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub min_tokens: i64,
    /// Monthly fee of a [`PROMO_SUBSCRIBE`] plan.
    #[serde(
        default,
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub monthly_fee: f64,
    /// Included tokens of a [`PROMO_SUBSCRIBE`] plan.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub included_tokens: i64,
    /// Human-readable justification shown in the comparison UI.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// Policy configures the audit engine.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Outbound rules guard the upstream against user abuse.
    #[serde(default)]
    pub outbound: OutboundPolicy,
    /// Inbound rules guard the user against a hostile upstream.
    #[serde(default)]
    pub inbound: InboundPolicy,
    /// Configures usage verification.
    #[serde(default)]
    pub metering: MeteringPolicy,
}

/// OutboundPolicy configures request-side inspection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutboundPolicy {
    /// Rejects a request containing any of these substrings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub block_keywords: Vec<String>,
    /// Rejects a request matching any regexp.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub block_patterns: Vec<String>,
    /// Rejects these tool/function names outright.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_tools: Vec<String>,
    /// When non-empty, switches to allowlist mode: only these tools may be
    /// called and everything else is denied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_tools: Vec<String>,
    /// Flags dangerous paths inside tool arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_path_patterns: Vec<String>,
    /// Replaced before the request is persisted to audit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redact_patterns: Vec<String>,
    /// "enforce" or "report"; report records but never blocks.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
}

/// InboundPolicy configures response-side inspection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboundPolicy {
    /// Detects prompt-injection attempts in model output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub injection_patterns: Vec<String>,
    /// Detects leaked credentials in model output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_patterns: Vec<String>,
    /// "off", "report" or "block" for upstream-suggested tool calls.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool_call_policy: String,
    /// "enforce" or "report".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
}

/// MeteringPolicy configures usage verification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeteringPolicy {
    /// Absolute token difference below which a deviation is treated as
    /// estimator noise, however large it looks in relative terms.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i32")]
    pub min_absolute_delta: i32,
    /// Allowed relative deviation between the upstream's reported usage and the
    /// gateway's own recount before it is flagged.
    #[serde(
        default,
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub tolerance: f64,
    /// Fraction of requests that get an online behavioural probe.
    #[serde(
        default,
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub fingerprint_sample_rate: f64,
    /// JS-divergence above which a channel is considered to serve a different
    /// model.
    #[serde(
        default,
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub fingerprint_threshold: f64,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Server configures listeners and storage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// User-facing address, e.g. `127.0.0.1:8080`.
    #[serde(default)]
    pub listen: String,
    /// Management API/UI address.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub admin_listen: String,
    /// Append-only audit log file.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub audit_path: String,
    /// JSONL metering ledger file.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub meter_path: String,
    /// Persists quota counters between restarts.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub state_path: String,
    /// Price catalogue maintained by the intelligence layer.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub price_path: String,
    /// Channel scorecard produced by the intelligence layer.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub score_path: String,
    /// Bounds a whole upstream exchange.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_duration")]
    pub upstream_timeout: Duration,
    /// "open" or "closed" when the audit engine itself errors.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fail_mode: String,
    /// "debug", "info", "warn" or "error".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub log_level: String,
    /// "cheapest", "best_score", "value", "weighted" or "fixed".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub route_strategy: String,
    /// Allows the management API to answer callers that are not on the loopback
    /// interface.
    #[serde(default, skip_serializing_if = "crate::json::is_false")]
    pub admin_remote: bool,
    /// Bearer credential for the management API.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub admin_token: String,
}

// ---------------------------------------------------------------------------
// Root
// ---------------------------------------------------------------------------

/// Config is the whole gateway configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Operator note carried in the file.
    #[serde(
        rename = "_comment",
        default,
        skip_serializing_if = "Comment::is_empty"
    )]
    pub comment: Comment,

    #[serde(default)]
    pub server: Server,
    #[serde(default)]
    pub policy: Policy,

    /// Upstream vendors with one or more API keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<Provider>,
    /// Single-endpoint upstreams configured directly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<Channel>,

    #[serde(default)]
    pub users: Vec<User>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prices: Vec<PriceEntry>,

    /// Configures provider selection across protocols.
    #[serde(default, skip_serializing_if = "RoutingPolicy::is_empty")]
    pub routing: RoutingPolicy,
    /// Configures automatic upstream model discovery.
    #[serde(default, skip_serializing_if = "DiscoveryPolicy::is_empty")]
    pub discovery: DiscoveryPolicy,
}

/// PriceEntry binds a price to a channel/model pair.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceEntry {
    /// Channel name.
    pub channel: String,
    /// Canonical model name.
    pub model: String,
    /// The price itself.
    pub price: PriceModel,
}

/// Comment is a documentation-only field that accepts a string or an array of
/// strings, so a configuration file can read naturally in either style.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Comment(pub Vec<String>);

impl Comment {
    /// Reports whether the comment is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Renders the comment for display.
    pub fn as_text(&self) -> String {
        self.0.join(" ")
    }
}

impl Serialize for Comment {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Comment {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let value = Option::<serde_json::Value>::deserialize(deserializer)?;
        match value {
            None | Some(serde_json::Value::Null) => Ok(Comment(Vec::new())),
            Some(serde_json::Value::String(s)) => Ok(Comment(vec![s])),
            Some(serde_json::Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        serde_json::Value::String(s) => out.push(s),
                        other => {
                            return Err(D::Error::custom(format!(
                            "_comment must be a string or an array of strings: unexpected {other}"
                        )))
                        }
                    }
                }
                Ok(Comment(out))
            }
            Some(other) => Err(D::Error::custom(format!(
                "_comment must be a string or an array of strings: unexpected {other}"
            ))),
        }
    }
}

/// Returns the Go zero timestamp (0001-01-01T00:00:00Z), used as the default
/// for fields whose Go counterpart is a zero `time.Time` that is still
/// serialised because `omitempty` does not apply to structs.
pub fn zero_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(1, 1, 1, 0, 0, 0)
        .single()
        .expect("year 1 is a valid UTC instant")
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// ConfigError reports a configuration failure.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The configuration file does not exist.
    #[error("config: file not found: {0}")]
    NoConfig(String),
    /// The configuration file could not be read.
    #[error("config: read {path}: {source}")]
    Read {
        /// Path that failed.
        path: String,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The configuration file could not be parsed.
    #[error("config: parse {path}: {message}")]
    Parse {
        /// Path that failed.
        path: String,
        /// Parser message.
        message: String,
    },
    /// The configuration parsed but is semantically invalid.
    #[error("config: {0}")]
    Invalid(String),
    /// The secrets file could not be opened.
    #[error("config: open env file {path}: {source}")]
    EnvFileOpen {
        /// Path that failed.
        path: String,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The secrets file is malformed.
    #[error("config: {0}")]
    EnvFile(String),
}

impl ConfigError {
    /// Reports whether this error means "no configuration file was found".
    pub fn is_no_config(&self) -> bool {
        matches!(self, ConfigError::NoConfig(_))
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Reads a configuration file, applies defaults and validates it.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    load_with_env_file(path, None)
}

/// [`load`] plus an optional KEY=VALUE secrets file.
///
/// Precedence, lowest to highest: configuration file, environment, env file.
/// The env file wins over the ambient environment because it is the more
/// specific, explicitly requested source.
pub fn load_with_env_file(path: &Path, env_file: Option<&Path>) -> Result<Config, ConfigError> {
    let raw = std::fs::read(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ConfigError::NoConfig(path.display().to_string())
        } else {
            ConfigError::Read {
                path: path.display().to_string(),
                source: err,
            }
        }
    })?;
    let mut config: Config = serde_json::from_slice(&raw).map_err(|err| ConfigError::Parse {
        path: path.display().to_string(),
        message: err.to_string(),
    })?;

    let mut environ: Vec<(String, String)> = std::env::vars().collect();
    if let Some(env_file) = env_file {
        let pairs = load_env_file(env_file)?;
        // The env file wins over the ambient environment, so it is applied last.
        environ.extend(pairs);
    }
    config.apply_env(&environ);
    config.apply_provider_env(&environ);
    config.apply_defaults();
    config.validate()?;
    Ok(config)
}

/// Interprets an environment value as a boolean, accepting the spellings an
/// operator is likely to reach for in an env file.
pub fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "y" | "t"
    )
}

impl Config {
    /// Overlays environment variables onto the configuration.
    ///
    /// Supported forms:
    ///
    /// - `CUSTOMS_SERVER_LISTEN`
    /// - `CUSTOMS_SERVER_ADMIN_REMOTE`
    /// - `CUSTOMS_SERVER_ADMIN_TOKEN`
    /// - `CUSTOMS_CHANNEL_<NAME>_APIKEY`
    /// - `CUSTOMS_CHANNEL_<NAME>_BASEURL`
    /// - `CUSTOMS_USER_<NAME>_TOKEN`
    pub fn apply_env(&mut self, environ: &[(String, String)]) {
        for (key, value) in environ {
            let Some(rest) = key.strip_prefix("CUSTOMS_") else {
                continue;
            };
            let parts: Vec<&str> = rest.split('_').collect();
            if parts.len() < 2 {
                continue;
            }
            match parts[0] {
                "SERVER" => {
                    let field = parts[1..].join("_");
                    match field.as_str() {
                        "LISTEN" => self.server.listen = value.clone(),
                        "ADMIN_LISTEN" => self.server.admin_listen = value.clone(),
                        "AUDIT_PATH" => self.server.audit_path = value.clone(),
                        "METER_PATH" => self.server.meter_path = value.clone(),
                        "STATE_PATH" => self.server.state_path = value.clone(),
                        "LOG_LEVEL" => self.server.log_level = value.clone(),
                        "FAIL_MODE" => self.server.fail_mode = value.clone(),
                        "ROUTE_STRATEGY" => self.server.route_strategy = value.clone(),
                        "ADMIN_REMOTE" => self.server.admin_remote = truthy(value),
                        "ADMIN_TOKEN" => self.server.admin_token = value.clone(),
                        _ => {}
                    }
                }
                "CHANNEL" => {
                    if parts.len() < 3 {
                        continue;
                    }
                    let field = parts[parts.len() - 1];
                    let name = parts[1..parts.len() - 1].join("_");
                    for channel in &mut self.channels {
                        if channel.name.eq_ignore_ascii_case(&name) {
                            match field {
                                "APIKEY" => channel.api_key = value.clone(),
                                "BASEURL" => channel.base_url = value.clone(),
                                _ => {}
                            }
                        }
                    }
                }
                "USER" => {
                    if parts.len() < 3 || parts[parts.len() - 1] != "TOKEN" {
                        continue;
                    }
                    let name = parts[1..parts.len() - 1].join("_");
                    for user in &mut self.users {
                        if user.name.eq_ignore_ascii_case(&name) {
                            user.token = value.clone();
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Fills unset fields with safe defaults.
    pub fn apply_defaults(&mut self) {
        let defaults = ServerDefaults::default();
        let server = &mut self.server;
        if server.listen.is_empty() {
            server.listen = defaults.listen;
        }
        if server.admin_listen.is_empty() {
            server.admin_listen = defaults.admin_listen;
        }
        if server.audit_path.is_empty() {
            server.audit_path = defaults.audit_path;
        }
        if server.meter_path.is_empty() {
            server.meter_path = defaults.meter_path;
        }
        if server.state_path.is_empty() {
            server.state_path = defaults.state_path;
        }
        if server.price_path.is_empty() {
            server.price_path = defaults.price_path;
        }
        if server.score_path.is_empty() {
            server.score_path = defaults.score_path;
        }
        if server.upstream_timeout.0.is_zero() {
            server.upstream_timeout = defaults.upstream_timeout;
        }
        if server.fail_mode.is_empty() {
            server.fail_mode = "open".to_string();
        }
        if server.log_level.is_empty() {
            server.log_level = "info".to_string();
        }
        if server.route_strategy.is_empty() {
            server.route_strategy = "value".to_string();
        }

        let policy = &mut self.policy;
        if policy.outbound.mode.is_empty() {
            policy.outbound.mode = "enforce".to_string();
        }
        if policy.inbound.mode.is_empty() {
            policy.inbound.mode = "enforce".to_string();
        }
        if policy.inbound.tool_call_policy.is_empty() {
            policy.inbound.tool_call_policy = "block".to_string();
        }
        if policy.metering.tolerance == 0.0 {
            policy.metering.tolerance = 0.25;
        }
        if policy.metering.min_absolute_delta == 0 {
            policy.metering.min_absolute_delta = 8;
        }
        if policy.metering.fingerprint_threshold == 0.0 {
            policy.metering.fingerprint_threshold = 0.35;
        }

        // Providers need the same normalisation as channels: the expansion
        // multiplies the provider weight by each key weight, so a provider left
        // at weight 0 would silently produce zero-weight channels that a
        // weighted strategy never selects.
        let upstream_timeout = if server.upstream_timeout.0.is_zero() {
            defaults.upstream_timeout
        } else {
            server.upstream_timeout
        };
        let discovery_interval = self.discovery.interval;
        for provider in &mut self.providers {
            if provider.protocol.is_empty() {
                provider.protocol = "openai".to_string();
            }
            if provider.trust.is_empty() {
                provider.trust = TRUST_UNTRUSTED.to_string();
            }
            if provider.weight == 0.0 {
                provider.weight = 1.0;
            }
            if provider.timeout.0.is_zero() {
                provider.timeout = upstream_timeout;
            }
            if let Some(discover) = provider.discover.as_mut() {
                if discover.interval.0.is_zero() {
                    discover.interval = discovery_interval;
                }
            }
        }
        for channel in &mut self.channels {
            if channel.protocol.is_empty() {
                channel.protocol = "openai".to_string();
            }
            if channel.trust.is_empty() {
                channel.trust = TRUST_UNTRUSTED.to_string();
            }
            if channel.weight == 0.0 {
                channel.weight = 1.0;
            }
            if channel.timeout.0.is_zero() {
                channel.timeout = upstream_timeout;
            }
        }
        for user in &mut self.users {
            if user.trust.is_empty() {
                user.trust = TRUST_UNTRUSTED.to_string();
            }
            if user.period.is_empty() {
                user.period = "none".to_string();
            }
        }
    }

    /// Rejects configurations that cannot work.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errors: Vec<String> = Vec::new();
        if self.server.listen.is_empty() {
            errors.push("server.listen must not be empty".to_string());
        }
        if !matches!(self.server.fail_mode.as_str(), "open" | "closed") {
            errors.push(format!(
                "server.fail_mode {:?} must be open or closed",
                self.server.fail_mode
            ));
        }
        if !matches!(
            self.server.route_strategy.as_str(),
            "cheapest" | "best_score" | "value" | "weighted" | "fixed"
        ) {
            errors.push(format!(
                "server.route_strategy {:?} is not recognised",
                self.server.route_strategy
            ));
        }
        if self.channels.is_empty() && self.providers.is_empty() {
            errors.push("at least one channel or provider is required".to_string());
        }
        let mut seen_channel: Vec<String> = Vec::new();
        for (i, channel) in self.channels.iter().enumerate() {
            if channel.name.is_empty() {
                errors.push(format!("channels[{i}].name must not be empty"));
            } else if seen_channel.contains(&channel.name) {
                errors.push(format!("duplicate channel {:?}", channel.name));
            }
            seen_channel.push(channel.name.clone());
            if channel.base_url.is_empty() {
                errors.push(format!(
                    "channel {:?}: base_url must not be empty",
                    channel.name
                ));
            }
            if !is_known_protocol(&channel.protocol) {
                errors.push(format!(
                    "channel {:?}: protocol {:?} must be openai, responses or anthropic",
                    channel.name, channel.protocol
                ));
            }
            if !trust_valid(&channel.trust) {
                errors.push(format!(
                    "channel {:?}: invalid trust {:?}",
                    channel.name, channel.trust
                ));
            }
        }
        let mut seen_user: Vec<String> = Vec::new();
        let mut seen_token: Vec<String> = Vec::new();
        for (i, user) in self.users.iter().enumerate() {
            if user.name.is_empty() {
                errors.push(format!("users[{i}].name must not be empty"));
            } else if seen_user.contains(&user.name) {
                errors.push(format!("duplicate user {:?}", user.name));
            }
            seen_user.push(user.name.clone());
            if user.token.is_empty() {
                errors.push(format!("user {:?}: token must not be empty", user.name));
            } else if seen_token.contains(&user.token) {
                errors.push(format!("duplicate user token for {:?}", user.name));
            }
            seen_token.push(user.token.clone());
            if !matches!(
                user.period.as_str(),
                "none" | "daily" | "weekly" | "monthly"
            ) {
                errors.push(format!(
                    "user {:?}: period {:?} must be none/daily/weekly/monthly",
                    user.name, user.period
                ));
            }
            if !trust_valid(&user.trust) {
                errors.push(format!(
                    "user {:?}: invalid trust {:?}",
                    user.name, user.trust
                ));
            }
        }
        errors.extend(self.validate_providers());
        errors.extend(self.validate_routing());
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(errors.join("; ")))
        }
    }

    /// Checks the routing policy.
    fn validate_routing(&self) -> Vec<String> {
        let mut errors = Vec::new();
        // "quota" is a provider-side refusal to serve (its account cannot pay),
        // not a client error, so it is a first-class retry class: the next
        // provider can serve the same request.
        let known = ["5xx", "429", "timeout", "network", "quota"];
        for class in &self.routing.retry_on {
            if !known.contains(&class.as_str()) {
                errors.push(format!(
                    "routing.retry_on {:?} must be one of 5xx, 429, timeout, network, quota",
                    class
                ));
            }
        }
        // A routing group that no provider belongs to would silently match
        // nothing, which is a configuration mistake worth surfacing.
        if !self.routing.groups.is_empty() {
            let available = self.group_names();
            for group in &self.routing.groups {
                if !available.contains(group) {
                    errors.push(format!(
                        "routing.groups references unknown group {:?}",
                        group
                    ));
                }
            }
        }
        errors
    }

    /// Returns the channel with the given name.
    pub fn channel_by_name(&self, name: &str) -> Option<Channel> {
        self.channels.iter().find(|c| c.name == name).cloned()
    }

    /// Returns the user owning the given bearer token.
    pub fn user_by_token(&self, token: &str) -> Option<User> {
        if token.is_empty() {
            return None;
        }
        self.users
            .iter()
            .find(|u| subtle_equal(&u.token, token))
            .cloned()
    }

    /// Returns the advertised price for a channel/model pair.
    pub fn price_for(&self, channel: &str, model: &str) -> Option<PriceModel> {
        self.prices
            .iter()
            .find(|p| p.channel == channel && p.model == model)
            .map(|p| p.price.clone())
    }

    /// Returns the configured upstream timeout or the default.
    pub fn upstream_timeout(&self) -> Duration {
        if self.server.upstream_timeout.0.is_zero() {
            Duration::from_secs(120)
        } else {
            self.server.upstream_timeout
        }
    }
}

/// Default server settings, mirroring `Config.ApplyDefaults` in Go.
struct ServerDefaults {
    listen: String,
    admin_listen: String,
    audit_path: String,
    meter_path: String,
    state_path: String,
    price_path: String,
    score_path: String,
    upstream_timeout: Duration,
}

impl Default for ServerDefaults {
    fn default() -> Self {
        ServerDefaults {
            listen: "127.0.0.1:8080".to_string(),
            admin_listen: "127.0.0.1:8081".to_string(),
            audit_path: "var/audit.jsonl".to_string(),
            meter_path: "var/meter.jsonl".to_string(),
            state_path: "var/state.json".to_string(),
            price_path: "etc/prices.json".to_string(),
            score_path: "var/scores.json".to_string(),
            upstream_timeout: Duration::from_secs(120),
        }
    }
}

/// Makes a path absolute relative to a base directory.
pub fn resolve(base: &Path, path: &str) -> String {
    if path.is_empty() {
        return path.to_string();
    }
    let p = Path::new(path);
    if p.is_absolute() {
        return path.to_string();
    }
    base.join(p).to_string_lossy().into_owned()
}

/// Makes a path absolute relative to a base directory, returning a PathBuf.
pub fn resolve_path(base: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if path.is_empty() || p.is_absolute() {
        return p.to_path_buf();
    }
    base.join(p)
}

/// Compares two secrets without leaking length-independent timing.
pub(crate) fn subtle_equal(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Reports whether a string names a protocol this gateway speaks.
pub fn is_known_protocol(protocol: &str) -> bool {
    matches!(protocol, "openai" | "responses" | "anthropic")
}

impl fmt::Display for Config {
    /// Renders a short configuration summary for logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} channel(s), {} user(s), {} price row(s)",
            self.all_channels().len(),
            self.users.len(),
            self.prices.len()
        )
    }
}

/// Helpers shared by in-crate tests.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A minimal provider-only configuration used by expansion tests.
    pub fn provider_config() -> Config {
        Config {
            providers: vec![Provider {
                name: "vendor".to_string(),
                base_url: "https://vendor.example.com".to_string(),
                protocol: "openai".to_string(),
                keys: vec![ApiKey {
                    name: String::new(),
                    api_key: "sk-vendor".to_string(),
                    ..ApiKey::default()
                }],
                ..Provider::default()
            }],
            ..Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> Config {
        Config {
            channels: vec![Channel {
                name: "upstream".to_string(),
                base_url: "https://api.example.com".to_string(),
                api_key: "sk-test".to_string(),
                ..Channel::default()
            }],
            users: vec![User {
                name: "alice".to_string(),
                token: "tok-alice".to_string(),
                ..User::default()
            }],
            ..Config::default()
        }
    }

    /// The Go zero value of a price row carries Go's zero `time.Time`, not the
    /// Unix epoch: a price that was never observed must not claim to have been
    /// observed in 1970, and the catalogue writes the field either way because
    /// `omitempty` does not apply to a struct.
    #[test]
    fn price_model_default_uses_the_go_zero_time() {
        let price = PriceModel::default();
        assert_eq!(price.updated_at, zero_time());
        assert_eq!(
            serde_json::to_value(&price).unwrap()["updated_at"],
            serde_json::json!("0001-01-01T00:00:00Z")
        );
    }

    /// Prices are floats on the Go side, so the file the catalogue writes has to
    /// spell them the way `encoding/json` does: `40`, not `40.0`.
    #[test]
    fn price_model_serialises_floats_like_go() {
        let price = PriceModel {
            input_per_m: 40.0,
            output_per_m: 1e-7,
            completion_ratio: 2.0,
            ..PriceModel::default()
        };
        let text = serde_json::to_string(&price).unwrap();
        assert!(text.contains(r#""input_per_m":40"#), "{text}");
        assert!(text.contains(r#""output_per_m":1e-7"#), "{text}");
        assert!(text.contains(r#""completion_ratio":2"#), "{text}");
    }

    #[test]
    fn defaults_are_applied() {
        let mut config = base_config();
        config.apply_defaults();
        assert_eq!(config.server.listen, "127.0.0.1:8080");
        assert_eq!(config.server.admin_listen, "127.0.0.1:8081");
        assert_eq!(config.server.route_strategy, "value");
        assert_eq!(config.policy.metering.tolerance, 0.25);
        assert_eq!(config.policy.metering.min_absolute_delta, 8);
        assert_eq!(config.channels[0].protocol, "openai");
        assert_eq!(config.channels[0].trust, TRUST_UNTRUSTED);
        assert_eq!(config.channels[0].weight, 1.0);
        assert_eq!(config.users[0].period, "none");
    }

    #[test]
    fn validation_reports_duplicates() {
        let mut config = base_config();
        config.channels.push(config.channels[0].clone());
        config.apply_defaults();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("duplicate channel"), "{err}");
    }

    #[test]
    fn user_model_wildcards() {
        let user = User {
            models: vec!["gpt-*".to_string(), "claude-3".to_string()],
            ..User::default()
        };
        assert!(user.allows_model("gpt-4o"));
        assert!(user.allows_model("claude-3"));
        assert!(!user.allows_model("gemini"));
    }

    #[test]
    fn channel_model_mapping() {
        let mut channel = Channel::default();
        assert_eq!(channel.upstream_model("gpt-4o").as_deref(), Some("gpt-4o"));
        channel
            .models
            .insert("gpt-4o".to_string(), "v-endpoint".to_string());
        assert_eq!(
            channel.upstream_model("gpt-4o").as_deref(),
            Some("v-endpoint")
        );
        assert_eq!(channel.upstream_model("other"), None);
        assert_eq!(channel.canonical_model("v-endpoint"), "gpt-4o");
    }

    #[test]
    fn comment_accepts_string_and_array() {
        let from_string: Config =
            serde_json::from_str(r#"{"_comment":"hello","server":{},"policy":{},"users":[]}"#)
                .unwrap();
        assert_eq!(from_string.comment.0, vec!["hello".to_string()]);
        let from_array: Config =
            serde_json::from_str(r#"{"_comment":["a","b"],"server":{},"policy":{},"users":[]}"#)
                .unwrap();
        assert_eq!(from_array.comment.as_text(), "a b");
    }

    fn write_config(name: &str, body: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("customs-config-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("customs.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Mirrors Go's `TestLoadMinimal`.
    #[test]
    fn load_minimal_applies_defaults() {
        let path = write_config(
            "minimal",
            r#"{
              "server": {"listen": "127.0.0.1:8080"},
              "channels": [{"name": "a", "base_url": "http://a.example", "api_key": "k1"}],
              "users": [{"name": "alice", "token": "tok-alice"}]
            }"#,
        );
        let cfg = load(&path).expect("load");
        assert_eq!(cfg.channels.len(), 1);
        assert_eq!(cfg.users.len(), 1);
        assert!(!cfg.server.admin_listen.is_empty());
        assert_eq!(cfg.server.route_strategy, "value");
        assert_eq!(cfg.server.fail_mode, "open");
        assert_eq!(cfg.policy.metering.tolerance, 0.25);
        assert_eq!(cfg.channels[0].trust_level(), TRUST_UNTRUSTED);
        assert_eq!(cfg.users[0].trust_level(), TRUST_UNTRUSTED);
        assert_eq!(cfg.channels[0].protocol, "openai");
    }

    /// Mirrors Go's `TestLoadMissingFile`.
    #[test]
    fn load_missing_file_reports_no_config() {
        let path = std::env::temp_dir().join("customs-config-missing-does-not-exist.json");
        let err = load(&path).unwrap_err();
        assert!(err.is_no_config(), "{err}");
    }

    /// Mirrors Go's `TestLoadRejectsInvalidJSON`.
    #[test]
    fn load_rejects_invalid_json() {
        let path = write_config("invalid", "{not json");
        assert!(load(&path).is_err());
    }

    /// Mirrors Go's validation table in `config_test.go`.
    #[test]
    fn validation_messages_match_go() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "channel_required",
                r#"{"server":{},"channels":[],"users":[{"name":"a","token":"t"}]}"#,
                "at least one channel",
            ),
            (
                "base_url",
                r#"{"server":{},"channels":[{"name":"a"}],"users":[]}"#,
                "base_url",
            ),
            (
                "duplicate_channel",
                r#"{"server":{},"channels":[{"name":"a","base_url":"http://x"},{"name":"a","base_url":"http://y"}],"users":[]}"#,
                "duplicate channel",
            ),
            (
                "duplicate_token",
                r#"{"server":{},"channels":[{"name":"a","base_url":"http://x"}],"users":[{"name":"u1","token":"same"},{"name":"u2","token":"same"}]}"#,
                "duplicate user token",
            ),
            (
                "empty_token",
                r#"{"server":{},"channels":[{"name":"a","base_url":"http://x"}],"users":[{"name":"u1"}]}"#,
                "token must not be empty",
            ),
            (
                "bad_protocol",
                r#"{"server":{},"channels":[{"name":"a","base_url":"http://x","protocol":"soap"}],"users":[]}"#,
                "protocol",
            ),
            (
                "fail_mode",
                r#"{"server":{"fail_mode":"maybe"},"channels":[{"name":"a","base_url":"http://x"}],"users":[]}"#,
                "fail_mode",
            ),
            (
                "strategy",
                r#"{"server":{"route_strategy":"vibes"},"channels":[{"name":"a","base_url":"http://x"}],"users":[]}"#,
                "route_strategy",
            ),
            (
                "period",
                r#"{"server":{},"channels":[{"name":"a","base_url":"http://x"}],"users":[{"name":"u","token":"t","period":"hourly"}]}"#,
                "period",
            ),
            (
                "trust",
                r#"{"server":{},"channels":[{"name":"a","base_url":"http://x","trust":"sorta"}],"users":[]}"#,
                "trust",
            ),
        ];
        for (name, body, want) in cases {
            let path = write_config(name, body);
            let err = load(&path).unwrap_err().to_string();
            assert!(err.contains(want), "case {name}: {err}");
        }
    }

    /// Mirrors Go's `TestTruthyAcceptsTheUsualSpellings`.
    #[test]
    fn truthy_accepts_the_usual_spellings() {
        for v in ["1", "true", "TRUE", "yes", "Yes", "on", " y ", "t"] {
            assert!(truthy(v), "truthy({v:?}) should be true");
        }
        for v in ["", "0", "false", "no", "off", "maybe", "2"] {
            assert!(!truthy(v), "truthy({v:?}) should be false");
        }
    }

    /// Mirrors Go's `TestEnvironmentOverridesSecrets`, driven through
    /// [`Config::apply_env`] so the test does not mutate process state.
    #[test]
    fn env_overrides_channel_and_user_secrets() {
        let mut config = base_config();
        config.channels[0].name = "upstream_a".to_string();
        config.channels[0].base_url = "http://old.example".to_string();
        config.users[0].name = "alice".to_string();
        config.users[0].token = "placeholder".to_string();
        config.apply_env(&[
            (
                "CUSTOMS_CHANNEL_UPSTREAM_A_APIKEY".to_string(),
                "sk-real-secret".to_string(),
            ),
            (
                "CUSTOMS_CHANNEL_UPSTREAM_A_BASEURL".to_string(),
                "http://new.example".to_string(),
            ),
            (
                "CUSTOMS_USER_ALICE_TOKEN".to_string(),
                "tok-from-env".to_string(),
            ),
        ]);
        assert_eq!(config.channels[0].api_key, "sk-real-secret");
        assert_eq!(config.channels[0].base_url, "http://new.example");
        assert_eq!(config.users[0].token, "tok-from-env");
    }

    /// Mirrors Go's `TestAdminRemoteIsOffUnlessConfigured` and the server
    /// overrides test.
    #[test]
    fn env_overrides_server_settings() {
        let mut config = base_config();
        assert!(!config.server.admin_remote);
        config.apply_env(&[
            (
                "CUSTOMS_SERVER_LISTEN".to_string(),
                "0.0.0.0:9999".to_string(),
            ),
            ("CUSTOMS_SERVER_LOG_LEVEL".to_string(), "debug".to_string()),
            ("CUSTOMS_SERVER_ADMIN_REMOTE".to_string(), "yes".to_string()),
            (
                "CUSTOMS_SERVER_ADMIN_TOKEN".to_string(),
                "console-token".to_string(),
            ),
        ]);
        assert_eq!(config.server.listen, "0.0.0.0:9999");
        assert_eq!(config.server.log_level, "debug");
        assert!(config.server.admin_remote);
        assert_eq!(config.server.admin_token, "console-token");
    }

    /// Mirrors Go's `TestUserByToken`.
    #[test]
    fn user_lookup_is_exact() {
        let config = Config {
            users: vec![
                User {
                    name: "a".into(),
                    token: "tok-a".into(),
                    ..User::default()
                },
                User {
                    name: "b".into(),
                    token: "tok-bb".into(),
                    ..User::default()
                },
            ],
            ..Config::default()
        };
        assert_eq!(
            config.user_by_token("tok-bb").map(|u| u.name),
            Some("b".into())
        );
        assert!(config.user_by_token("tok-b").is_none());
        assert!(config.user_by_token("").is_none());
    }

    /// Mirrors Go's `TestResolve`.
    #[test]
    fn resolve_keeps_absolute_and_empty_paths() {
        assert_eq!(resolve(Path::new("/base"), "rel/path"), "/base/rel/path");
        assert_eq!(resolve(Path::new("/base"), "/abs/path"), "/abs/path");
        assert_eq!(resolve(Path::new("/base"), ""), "");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = serde_json::from_str::<Config>(
            r#"{"server":{},"policy":{},"users":[],"unknown_field":1}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }

    /// The env file is the most specific source and is applied last, so it wins
    /// over the ambient environment. `load_with_env_file` relies on entry order
    /// for that, so the contract is pinned here rather than through process-wide
    /// environment mutation, which would race with other tests.
    #[test]
    fn env_file_pairs_win_over_the_ambient_environment() {
        let mut config = base_config();
        config.apply_env(&[
            (
                "CUSTOMS_SERVER_LISTEN".to_string(),
                "127.0.0.1:1111".to_string(),
            ),
            // The env file's line, appended after the ambient environment.
            (
                "CUSTOMS_SERVER_LISTEN".to_string(),
                "127.0.0.1:2222".to_string(),
            ),
        ]);
        assert_eq!(config.server.listen, "127.0.0.1:2222");
    }

    /// A malformed secrets file must fail the load rather than silently leaving a
    /// channel without credentials.
    #[test]
    fn load_with_env_file_surfaces_env_file_errors() {
        let config_path = write_config(
            "envfile-error",
            r#"{"server":{},"channels":[{"name":"a","base_url":"http://x"}],"users":[]}"#,
        );
        let env_path = config_path.with_file_name("customs.env");
        std::fs::write(&env_path, "THIS_IS_NOT_AN_ASSIGNMENT\n").unwrap();
        let err = load_with_env_file(&config_path, Some(&env_path))
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected KEY=VALUE"), "{err}");
    }

    /// The env file can supply a credential the configuration file omits.
    #[test]
    fn load_with_env_file_applies_secrets() {
        let config_path = write_config(
            "envfile-secret",
            r#"{"server":{},"channels":[{"name":"upstream_a","base_url":"http://x"}],"users":[{"name":"alice","token":"from-file"}]}"#,
        );
        let env_path = config_path.with_file_name("customs.env");
        std::fs::write(&env_path, "CUSTOMS_CHANNEL_UPSTREAM_A_APIKEY=sk-injected\n").unwrap();
        let config = load_with_env_file(&config_path, Some(&env_path)).expect("load");
        assert_eq!(config.channels[0].api_key, "sk-injected");
        assert_eq!(config.users[0].token, "from-file");
    }

    /// Provider validation keeps the Go messages: an unnamed key in a multi-key
    /// provider, a key name containing the channel separator, and a provider name
    /// colliding with a directly-configured channel.
    #[test]
    fn provider_validation_reports_key_problems() {
        let mut config = Config {
            providers: vec![Provider {
                name: "vendor".to_string(),
                base_url: "https://vendor.example".to_string(),
                keys: vec![
                    ApiKey::default(),
                    ApiKey {
                        name: "bad/name".to_string(),
                        ..ApiKey::default()
                    },
                ],
                ..Provider::default()
            }],
            channels: vec![Channel {
                name: "vendor".to_string(),
                base_url: "http://direct.example".to_string(),
                ..Channel::default()
            }],
            ..Config::default()
        };
        config.apply_defaults();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("keys[0].name is required"), "{err}");
        assert!(err.contains("must not contain '/'"), "{err}");
        assert!(err.contains("collides with a provider"), "{err}");
    }

    /// A retry class nobody recognises and a group no provider belongs to are
    /// configuration mistakes, not silent no-ops.
    #[test]
    fn routing_validation_rejects_unknown_groups_and_classes() {
        let mut config = base_config();
        config.routing.retry_on = vec!["5xx".to_string(), "teapot".to_string()];
        config.routing.groups = vec!["nope".to_string()];
        config.apply_defaults();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("routing.retry_on"), "{err}");
        assert!(err.contains("unknown group"), "{err}");
    }

    /// Providers inherit the server timeout and the global discovery interval;
    /// leaving them at zero would produce channels with no deadline that a
    /// weighted strategy never selects.
    #[test]
    fn provider_defaults_inherit_server_settings() {
        let mut config = Config {
            providers: vec![Provider {
                name: "vendor".to_string(),
                base_url: "https://vendor.example".to_string(),
                discover: Some(DiscoverPolicy::default()),
                ..Provider::default()
            }],
            ..Config::default()
        };
        config.server.upstream_timeout = Duration::from_secs(45);
        config.discovery.interval = Duration::from_secs(600);
        config.apply_defaults();

        let provider = &config.providers[0];
        assert_eq!(provider.protocol, "openai");
        assert_eq!(provider.trust, TRUST_UNTRUSTED);
        assert_eq!(provider.weight, 1.0);
        assert_eq!(provider.timeout, Duration::from_secs(45));
        assert_eq!(
            provider.discover.as_ref().map(|d| d.interval),
            Some(Duration::from_secs(600))
        );
    }
}
