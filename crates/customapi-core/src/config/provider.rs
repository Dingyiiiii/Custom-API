//! Providers: the vendor layer above flat channels.
//!
//! A "channel" in this gateway is one upstream endpoint with one credential, and
//! it is what routing, metering and auditing operate on. Operators, however,
//! think in terms of *providers*: one vendor, usually with several API keys to
//! spread load or to survive a key being rate-limited or revoked.
//!
//! A provider therefore declares the vendor (base URL, protocol, trust, model
//! aliases) once and lists its keys. At load time each key is expanded into its
//! own channel named `<provider>/<key>`, so every downstream component keeps
//! working on the flat channel list it already understands, while the operator
//! gets to manage keys as a group.
//!
//! The expansion is what makes multi-key failover work without touching the
//! router: a failed key is one unhealthy channel, and the router already knows
//! how to move to the next candidate.

use super::{Channel, Config};
use crate::json::Duration as JsonDuration;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Provider is an upstream vendor with one or more API keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// Stable identifier used in channel names and audit records.
    pub name: String,
    /// Vendor API root, e.g. `https://api.openai.com`.
    pub base_url: String,
    /// Wire protocol to prefer when talking to this provider: "openai"
    /// (chat completions), "responses" or "anthropic".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub protocol: String,
    /// Lists every protocol this provider can serve. When empty it defaults to
    /// just `protocol`.
    ///
    /// It exists so that cross-protocol routing can be deliberate: a provider
    /// that only implements the chat surface must not be handed a Responses
    /// request just because both are "OpenAI-ish".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supports: Vec<String>,
    /// Maps a canonical (user-facing) model name to this provider's own model
    /// identifier. An empty map means "pass through unchanged".
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub models: HashMap<String, String>,
    /// Extra canonical names that resolve to a model served by this provider.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub aliases: HashMap<String, String>,
    /// Relaxes content inspection for this provider.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub trust: String,
    /// Biases routing across providers.
    #[serde(
        default,
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub weight: f64,
    /// Allows disabling a whole provider without deleting its keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Overrides the per-request upstream timeout.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_duration")]
    pub timeout: JsonDuration,
    /// Extra headers attached to every request to this provider.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    /// Names the routing groups this provider belongs to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// The provider's credentials.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<ApiKey>,
    /// Controls automatic model discovery from this provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discover: Option<DiscoverPolicy>,
    /// Free-form operator annotations.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

impl Provider {
    /// Reports whether the provider may receive traffic.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Returns the effective trust level.
    pub fn trust_level(&self) -> &str {
        crate::config::trust_level(&self.trust)
    }

    /// Returns the protocols this provider can serve, defaulting to the
    /// preferred protocol alone.
    pub fn supported_protocols(&self) -> Vec<String> {
        if !self.supports.is_empty() {
            return self.supports.clone();
        }
        if !self.protocol.is_empty() {
            return vec![self.protocol.clone()];
        }
        vec!["openai".to_string()]
    }

    /// Reports whether the provider claims to serve a protocol.
    pub fn speaks_protocol(&self, protocol: &str) -> bool {
        self.supported_protocols().iter().any(|p| p == protocol)
    }

    /// Resolves the discovery switch against the global default.
    pub fn discover_enabled(&self, global_default: bool) -> bool {
        match self.discover.as_ref().and_then(|d| d.enabled) {
            Some(v) => v,
            None => global_default,
        }
    }
}

/// ApiKey is one credential belonging to a provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKey {
    /// Identifies the key within its provider; it appears in audit records so an
    /// operator can tell which key served a request.
    #[serde(default)]
    pub name: String,
    /// The credential itself. It is normally supplied through the env file
    /// rather than this field, using
    /// `CUSTOMS_PROVIDER_<PROVIDER>_KEY_<NAME>_APIKEY`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// Biases key selection within the provider. Zero means 1.
    #[serde(
        default,
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub weight: f64,
    /// Allows parking a key without deleting it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Documents the operator's knowledge of this key's allowance, which the
    /// front end shows as a hint. The gateway does not enforce it: only the
    /// provider knows the real limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<KeyLimits>,
    /// Free-form annotation.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl ApiKey {
    /// Reports whether the key may be used.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

/// KeyLimits is operator-recorded information about a key's allowance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyLimits {
    /// Requests-per-minute allowance.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i32")]
    pub rpm: i32,
    /// Tokens-per-minute allowance.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i32")]
    pub tpm: i32,
    /// Spend cap in `currency`.
    #[serde(
        default,
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub monthly_budget: f64,
    /// Currency of `monthly_budget`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub currency: String,
}

/// DiscoverPolicy controls model discovery for a provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoverPolicy {
    /// Turns discovery on. Missing means "inherit the global setting".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Overrides the discovery endpoint, defaulting to `/v1/models`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// Overrides how often discovery runs.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_duration")]
    pub interval: JsonDuration,
    /// Filters out discovered model names (substring or glob).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// When non-empty, restricts discovery to these patterns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// When true, exposes discovered models as `<provider>/<model>` in addition
    /// to any bare name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<bool>,
}

// ---------------------------------------------------------------------------
// Routing policy
// ---------------------------------------------------------------------------

/// RoutingPolicy configures provider selection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingPolicy {
    /// Makes the router choose a channel that already speaks the client's
    /// protocol before considering any translation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer_same_protocol: Option<bool>,
    /// Permits routing to a channel that speaks a different protocol, which
    /// requires a conversion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_translation: Option<bool>,
    /// Bounds how many channels one request may try.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i32")]
    pub max_attempts: i32,
    /// Lists the failure classes worth another channel: "5xx", "429",
    /// "timeout", "network", "quota".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retry_on: Vec<String>,
    /// Restricts routing to these provider groups. Empty means all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// Skips these providers entirely.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_providers: Vec<String>,
}

impl RoutingPolicy {
    /// Reports whether this policy carries any explicit setting.
    pub fn is_empty(&self) -> bool {
        self.prefer_same_protocol.is_none()
            && self.allow_translation.is_none()
            && self.max_attempts == 0
            && self.retry_on.is_empty()
            && self.groups.is_empty()
            && self.exclude_providers.is_empty()
    }

    /// Returns the effective same-protocol preference.
    pub fn same_protocol_preferred(&self) -> bool {
        self.prefer_same_protocol.unwrap_or(true)
    }

    /// Returns the effective translation permission.
    pub fn translation_allowed(&self) -> bool {
        self.allow_translation.unwrap_or(true)
    }

    /// Returns the effective attempt bound.
    pub fn attempts(&self) -> i32 {
        if self.max_attempts <= 0 {
            3
        } else {
            self.max_attempts
        }
    }

    /// Reports whether a failure class is retryable.
    pub fn should_retry(&self, class: &str) -> bool {
        if self.retry_on.is_empty() {
            // "quota" belongs in the defaults: a provider whose account cannot
            // pay is exactly the provider an operator wants skipped, and the
            // request is unaffected - which is why several providers are
            // configured in the first place.
            return matches!(class, "5xx" | "429" | "timeout" | "network" | "quota");
        }
        self.retry_on.iter().any(|c| c == class)
    }
}

// ---------------------------------------------------------------------------
// Discovery (global)
// ---------------------------------------------------------------------------

/// DiscoveryPolicy configures model discovery globally.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryPolicy {
    /// Turns discovery on for providers that do not override it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// How often discovery runs.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_duration")]
    pub interval: JsonDuration,
    /// Bounds one provider's discovery call.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_duration")]
    pub timeout: JsonDuration,
    /// Where the merged catalogue is persisted.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cache_path: String,
    /// Marks a cached entry as stale for display purposes.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_duration")]
    pub stale_after: JsonDuration,
}

impl DiscoveryPolicy {
    /// Reports whether this policy carries any explicit setting.
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.interval.0.is_zero()
            && self.timeout.0.is_zero()
            && self.cache_path.is_empty()
            && self.stale_after.0.is_zero()
    }

    /// Resolves the global switch.
    pub fn enabled_default(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Returns the discovery interval.
    pub fn interval_or_default(&self) -> std::time::Duration {
        if self.interval.0.is_zero() {
            std::time::Duration::from_secs(30 * 60)
        } else {
            self.interval.0
        }
    }

    /// Returns the per-provider discovery timeout.
    pub fn timeout_or_default(&self) -> std::time::Duration {
        if self.timeout.0.is_zero() {
            std::time::Duration::from_secs(20)
        } else {
            self.timeout.0
        }
    }

    /// Returns the staleness threshold.
    pub fn stale_after_or_default(&self) -> std::time::Duration {
        if self.stale_after.0.is_zero() {
            std::time::Duration::from_secs(24 * 3600)
        } else {
            self.stale_after.0
        }
    }
}

// ---------------------------------------------------------------------------
// Expansion: providers -> channels
// ---------------------------------------------------------------------------

/// Turns the configured providers into flat channels.
///
/// Naming: a provider with one key produces the channel name `<provider>`,
/// preserving the simple case and keeping existing audit queries working. A
/// provider with several keys produces `<provider>/<key>` for each, because a
/// bare provider name would be ambiguous.
pub fn expand_providers(config: &Config) -> Vec<Channel> {
    let mut out = Vec::new();
    for provider in &config.providers {
        if !provider.is_enabled() {
            continue;
        }
        // A provider may legitimately have no inline keys, for example when the
        // credential arrives through the environment. Emit one channel so the
        // provider still participates, with the key filled in later.
        let empty_key = ApiKey::default();
        let keys: Vec<&ApiKey> = if provider.keys.is_empty() {
            vec![&empty_key]
        } else {
            provider.keys.iter().collect()
        };
        for key in keys {
            if !key.is_enabled() {
                continue;
            }
            // A named key keeps its name even when it is the only one: the key
            // identity is what lets an operator tell which credential served a
            // request, so it must not disappear just because the provider
            // happens to have a single key. Only an unnamed single key uses the
            // bare provider name.
            let name = if key.name.is_empty() {
                provider.name.clone()
            } else {
                format!("{}/{}", provider.name, key.name)
            };
            let mut channel = Channel {
                name,
                base_url: provider.base_url.clone(),
                api_key: key.api_key.clone(),
                protocol: provider.protocol.clone(),
                models: merge_string_maps(&provider.models, &provider.aliases),
                trust: provider.trust.clone(),
                weight: provider.weight * weight_or(key.weight),
                timeout: provider.timeout,
                headers: provider.headers.clone(),
                metadata: provider.metadata.clone(),
                ..Channel::default()
            };
            // Carry the provenance so audit records and the admin API can
            // explain which provider and key served a request.
            channel
                .metadata
                .insert("provider".to_string(), provider.name.clone());
            if !key.name.is_empty() {
                channel.metadata.insert("key".to_string(), key.name.clone());
            }
            channel.metadata.insert(
                "protocols".to_string(),
                provider.supported_protocols().join(","),
            );
            if !provider.groups.is_empty() {
                channel
                    .metadata
                    .insert("groups".to_string(), provider.groups.join(","));
            }
            out.push(channel);
        }
    }
    out
}

/// Returns a weight, treating zero as 1.
fn weight_or(weight: f64) -> f64 {
    if weight <= 0.0 {
        1.0
    } else {
        weight
    }
}

/// Overlays `b` onto a copy of `a`. Aliases win over model names, because an
/// alias is the more specific operator intent.
fn merge_string_maps(
    a: &HashMap<String, String>,
    b: &HashMap<String, String>,
) -> HashMap<String, String> {
    if a.is_empty() && b.is_empty() {
        return HashMap::new();
    }
    let mut out = a.clone();
    for (k, v) in b {
        out.insert(k.clone(), v.clone());
    }
    out
}

impl Config {
    /// Returns the effective channel list: providers expanded, plus any channels
    /// configured directly.
    pub fn all_channels(&self) -> Vec<Channel> {
        let mut out = expand_providers(self);
        out.extend(self.channels.iter().cloned());
        out
    }

    /// Maps a channel back to its provider.
    pub fn provider_by_channel_name(&self, channel: &str) -> Option<Provider> {
        for provider in &self.providers {
            if provider.name == channel || channel.starts_with(&format!("{}/", provider.name)) {
                return Some(provider.clone());
            }
        }
        None
    }

    /// Returns every routing group referenced by any provider.
    pub fn group_names(&self) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for provider in &self.providers {
            for group in &provider.groups {
                if !seen.contains(group) {
                    seen.push(group.clone());
                }
            }
        }
        seen.sort();
        seen
    }

    /// Checks the provider layer.
    pub(crate) fn validate_providers(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for (i, provider) in self.providers.iter().enumerate() {
            let mut where_ = format!("providers[{i}]");
            if provider.name.is_empty() {
                errors.push(format!("{where_}.name must not be empty"));
                continue;
            }
            where_ = format!("provider {:?}", provider.name);
            if seen.contains(&provider.name) {
                errors.push(format!("duplicate {where_}"));
            }
            seen.push(provider.name.clone());
            if provider.base_url.is_empty() {
                errors.push(format!("{where_}: base_url must not be empty"));
            }
            if !provider.protocol.is_empty()
                && !crate::config::is_known_protocol(&provider.protocol)
            {
                errors.push(format!(
                    "{where_}: protocol {:?} must be openai, responses or anthropic",
                    provider.protocol
                ));
            }
            for supported in &provider.supports {
                if !crate::config::is_known_protocol(supported) {
                    errors.push(format!(
                        "{where_}: supports {supported:?} must be openai, responses or anthropic"
                    ));
                }
            }
            if !crate::config::trust_valid(provider.trust_level()) {
                errors.push(format!("{where_}: invalid trust {:?}", provider.trust));
            }
            let mut key_names: Vec<String> = Vec::new();
            for (j, key) in provider.keys.iter().enumerate() {
                if key.name.is_empty() && provider.keys.len() > 1 {
                    errors.push(format!(
                        "{where_}: keys[{j}].name is required when a provider has several keys"
                    ));
                }
                if !key.name.is_empty() {
                    if key_names.contains(&key.name) {
                        errors.push(format!("{where_}: duplicate key name {:?}", key.name));
                    }
                    key_names.push(key.name.clone());
                    // A key name becomes part of a channel name, which is used
                    // in audit records and as a route target, so it must not
                    // contain the separator.
                    if key.name.contains('/') {
                        errors.push(format!(
                            "{where_}: key name {:?} must not contain '/'",
                            key.name
                        ));
                    }
                }
            }
        }
        // A provider name must not collide with a directly-configured channel,
        // because both become entries in the same flat channel list.
        for channel in &self.channels {
            if seen.contains(&channel.name) {
                errors.push(format!(
                    "channel {:?} collides with a provider of the same name",
                    channel.name
                ));
            }
        }
        errors
    }

    /// Overlays credentials from the environment or env file.
    ///
    /// Accepted names:
    ///
    /// - `CUSTOMS_PROVIDER_<PROVIDER>_APIKEY` (single-key provider)
    /// - `CUSTOMS_PROVIDER_<PROVIDER>_BASEURL`
    /// - `CUSTOMS_PROVIDER_<PROVIDER>_KEY_<NAME>_APIKEY` (one of several keys)
    ///
    /// `<PROVIDER>` is compared after normalisation, so a provider named after
    /// its host ("naapi.cc") is addressable as `CUSTOMS_PROVIDER_NAAPI_CC_...`:
    /// an environment variable name cannot contain a dot, and forcing operators
    /// to rename providers would mean the name in the audit ledger no longer
    /// matched the name of the thing it came from.
    pub fn apply_provider_env(&mut self, environ: &[(String, String)]) {
        for (key, value) in environ {
            let Some(rest) = key.strip_prefix("CUSTOMS_PROVIDER_") else {
                continue;
            };
            // Key-specific form first: ..._KEY_<NAME>_APIKEY
            if let Some(idx) = rest.find("_KEY_") {
                if rest.ends_with("_APIKEY") && idx > 0 {
                    let provider_name = &rest[..idx];
                    let key_name = &rest[idx + "_KEY_".len()..rest.len() - "_APIKEY".len()];
                    for provider in &mut self.providers {
                        if !env_name_matches(&provider.name, provider_name) {
                            continue;
                        }
                        for provider_key in &mut provider.keys {
                            if provider_key.name.eq_ignore_ascii_case(key_name) {
                                provider_key.api_key = value.clone();
                            }
                        }
                    }
                    continue;
                }
            }
            if let Some(provider_name) = rest.strip_suffix("_APIKEY") {
                for provider in &mut self.providers {
                    if !env_name_matches(&provider.name, provider_name) {
                        continue;
                    }
                    if provider.keys.is_empty() {
                        provider.keys.push(ApiKey::default());
                    }
                    provider.keys[0].api_key = value.clone();
                }
            } else if let Some(provider_name) = rest.strip_suffix("_BASEURL") {
                for provider in &mut self.providers {
                    if env_name_matches(&provider.name, provider_name) {
                        provider.base_url = value.clone();
                    }
                }
            }
        }
    }

    /// Renders providers without exposing credentials.
    ///
    /// The key prefix is included deliberately: it is enough for an operator to
    /// tell two keys apart when checking which one is rate-limited, and a prefix
    /// alone does not grant access. Full keys are never returned.
    pub fn summarize_providers(&self) -> Vec<ProviderSummary> {
        let mut out = Vec::new();
        for provider in &self.providers {
            let mut summary = ProviderSummary {
                name: provider.name.clone(),
                base_url: provider.base_url.clone(),
                protocol: or_default(&provider.protocol, "openai"),
                protocols: provider.supported_protocols(),
                trust: provider.trust_level().to_string(),
                enabled: provider.is_enabled(),
                weight: provider.weight,
                groups: provider.groups.clone(),
                models: provider.models.len() as i32,
                keys: Vec::new(),
                discover: provider.discover.clone(),
                metadata: provider.metadata.clone(),
                can_translate: provider.supported_protocols().len() > 1,
                channel_prefix: String::new(),
            };
            if provider.keys.is_empty() {
                summary.keys.push(KeySummary {
                    name: String::new(),
                    has_key: false,
                    prefix: String::new(),
                    weight: 0.0,
                    enabled: true,
                    limits: None,
                    note: String::new(),
                    channel: provider.name.clone(),
                });
            }
            for key in &provider.keys {
                let channel = if key.name.is_empty() {
                    provider.name.clone()
                } else {
                    format!("{}/{}", provider.name, key.name)
                };
                summary.keys.push(KeySummary {
                    name: key.name.clone(),
                    has_key: !key.api_key.is_empty(),
                    prefix: key_prefix(&key.api_key),
                    weight: key.weight,
                    enabled: key.is_enabled(),
                    limits: key.limits.clone(),
                    note: key.note.clone(),
                    channel,
                });
            }
            out.push(summary);
        }
        out
    }
}

/// Compares a configured name against the name as it appears in an environment
/// variable.
///
/// The comparison folds case and treats every character an environment variable
/// cannot carry (dot, dash, space) as an underscore, so "naapi.cc" and
/// "NAAPI_CC" are the same provider. That is deliberately generous rather than
/// strict: the alternative is a credential that silently never gets attached,
/// which surfaces as an authentication failure against the upstream rather than
/// as a configuration error.
fn env_name_matches(configured: &str, from_env: &str) -> bool {
    normalise_env_name(configured) == normalise_env_name(from_env)
}

/// Maps a name onto its environment-variable spelling.
fn normalise_env_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.to_uppercase().chars() {
        if ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

/// Returns a short, non-secret identifier for a credential.
fn key_prefix(key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return format!("{}***", chars[0]);
    }
    let head: String = chars[..6].iter().collect();
    let tail: String = chars[chars.len() - 2..].iter().collect();
    format!("{head}...{tail}")
}

fn or_default(value: &str, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

/// The redacted view of a provider for the admin API.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderSummary {
    /// Provider name.
    pub name: String,
    /// Vendor API root.
    pub base_url: String,
    /// Preferred protocol.
    pub protocol: String,
    /// Every protocol the provider serves.
    pub protocols: Vec<String>,
    /// Effective trust level.
    pub trust: String,
    /// Whether the provider may receive traffic.
    pub enabled: bool,
    /// Routing weight.
    #[serde(serialize_with = "crate::json::go_f64")]
    pub weight: f64,
    /// Routing groups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// Number of declared model aliases.
    pub models: i32,
    /// Redacted keys.
    pub keys: Vec<KeySummary>,
    /// Discovery settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discover: Option<DiscoverPolicy>,
    /// Operator annotations.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
    /// Whether the provider can serve more than one protocol.
    #[serde(rename = "can_translate")]
    pub can_translate: bool,
    /// Channel name prefix produced by expansion.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub channel_prefix: String,
}

/// The redacted view of one API key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KeySummary {
    /// Key name within the provider.
    pub name: String,
    /// Whether a credential is present.
    pub has_key: bool,
    /// Non-secret prefix that distinguishes two keys.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    /// Key weight.
    #[serde(
        default,
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub weight: f64,
    /// Whether the key may be used.
    pub enabled: bool,
    /// Operator-recorded limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<KeyLimits>,
    /// Operator note.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// Channel name this key expands to.
    pub channel: String,
}

// ---------------------------------------------------------------------------
// Upstream URL construction
// ---------------------------------------------------------------------------

/// The version prefix every wire protocol this gateway speaks lives under.
const API_VERSION_SEGMENT: &str = "/v1";

/// Joins a channel's `base_url` with a protocol path.
///
/// Operators write base_url both ways, and both are reasonable:
///
/// - `https://host` - the vendor's host root
/// - `https://host/v1` - the documented "API root", which is how relay services
///   and most vendor docs print it
///
/// Appending `/v1/chat/completions` to the second form yields
/// `https://host/v1/v1/chat/completions` and a 404 that looks like the
/// aggregator is broken. So a base_url that already ends in the version segment
/// is treated as the API root, and the path is appended without repeating it:
/// `/v1/chat/completions` becomes `/chat/completions`. A base_url carrying
/// additional path segments (`https://host/openai/v1`) keeps them, because only
/// the trailing version segment is dropped.
pub fn upstream_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    // An unset base is returned as the empty string rather than as a bare path:
    // validation rejects an empty base_url, and a caller that forgot to check
    // should see "no host" rather than a relative URL.
    if base.is_empty() {
        return String::new();
    }
    if path.is_empty() {
        return base.to_string();
    }
    let mut p = path.to_string();
    if !p.starts_with('/') {
        p = format!("/{p}");
    }
    if base.ends_with(API_VERSION_SEGMENT) && p.starts_with("/v1/") {
        // Drop one copy of the version segment: the base already carries it.
        p = p["/v1".len()..].to_string();
    }
    format!("{base}{p}")
}

impl Channel {
    /// Returns the protocol surfaces a channel announced, or an empty list when
    /// it announced none.
    ///
    /// The distinction carries meaning throughout the system: declared-but-
    /// incomplete is a statement about what the upstream implements ("these are
    /// my surfaces"), while absent is silence, and silence is not evidence that
    /// a surface exists. Provider expansion writes this from
    /// [`Provider::supports`], so both the router and the proxy read the
    /// operator's declaration from one place.
    pub fn declared_protocols(&self) -> Vec<String> {
        let Some(listed) = self.metadata.get("protocols") else {
            return Vec::new();
        };
        listed
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Reports whether the channel announced this protocol as one it serves
    /// directly, which means a request in that protocol can go to that surface
    /// unchanged rather than being converted.
    pub fn declares_protocol(&self, protocol: &str) -> bool {
        self.declared_protocols().iter().any(|p| p == protocol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::provider_config;

    #[test]
    fn one_unnamed_key_uses_bare_provider_name() {
        let config = provider_config();
        let channels = expand_providers(&config);
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].name, "vendor");
    }

    #[test]
    fn named_keys_expand_to_slash_names() {
        let mut config = provider_config();
        config.providers[0].keys = vec![
            ApiKey {
                name: "a".into(),
                api_key: "sk-a".into(),
                weight: 2.0,
                ..ApiKey::default()
            },
            ApiKey {
                name: "b".into(),
                api_key: "sk-b".into(),
                ..ApiKey::default()
            },
        ];
        let channels = expand_providers(&config);
        assert_eq!(channels[0].name, "vendor/a");
        assert_eq!(channels[1].name, "vendor/b");
        assert_eq!(
            channels[0].metadata.get("key").map(String::as_str),
            Some("a")
        );
        assert_eq!(
            channels[0].metadata.get("provider").map(String::as_str),
            Some("vendor")
        );
    }

    #[test]
    fn disabled_keys_are_skipped() {
        let mut config = provider_config();
        config.providers[0].keys = vec![ApiKey {
            name: "a".into(),
            enabled: Some(false),
            ..ApiKey::default()
        }];
        assert!(expand_providers(&config).is_empty());
    }

    #[test]
    fn aliases_win_over_models() {
        let mut config = provider_config();
        config.providers[0]
            .models
            .insert("gpt-4o".into(), "up-4o".into());
        config.providers[0]
            .aliases
            .insert("gpt-4o".into(), "alias-4o".into());
        let channels = expand_providers(&config);
        assert_eq!(
            channels[0].models.get("gpt-4o").map(String::as_str),
            Some("alias-4o")
        );
    }

    #[test]
    fn provider_env_overlays_credentials_by_normalised_name() {
        let mut config = provider_config();
        config.providers[0].name = "naapi.cc".into();
        config.apply_provider_env(&[(
            "CUSTOMS_PROVIDER_NAAPI_CC_APIKEY".to_string(),
            "sk-env".to_string(),
        )]);
        assert_eq!(config.providers[0].keys[0].api_key, "sk-env");
    }

    #[test]
    fn provider_env_overlays_named_key() {
        let mut config = provider_config();
        config.providers[0].keys = vec![ApiKey {
            name: "main".into(),
            ..ApiKey::default()
        }];
        config.apply_provider_env(&[(
            "CUSTOMS_PROVIDER_VENDOR_KEY_MAIN_APIKEY".to_string(),
            "sk-main".to_string(),
        )]);
        assert_eq!(config.providers[0].keys[0].api_key, "sk-main");
    }

    /// Mirrors Go's `TestProviderEnvNameAcceptsHostStyleNames`.
    #[test]
    fn provider_env_accepts_host_style_names() {
        let mut config = Config {
            providers: vec![
                Provider {
                    name: "naapi.cc".into(),
                    base_url: "https://naapi.cc/v1".into(),
                    keys: vec![ApiKey {
                        name: "main".into(),
                        ..ApiKey::default()
                    }],
                    ..Provider::default()
                },
                Provider {
                    name: "0egaeshop.xyz".into(),
                    base_url: "https://0egaeshop.xyz/v1".into(),
                    keys: vec![ApiKey {
                        name: "main".into(),
                        ..ApiKey::default()
                    }],
                    ..Provider::default()
                },
                Provider {
                    name: "vsllm.com".into(),
                    base_url: "https://vsllm.com/v1".into(),
                    keys: Vec::new(),
                    ..Provider::default()
                },
            ],
            ..Config::default()
        };
        config.apply_provider_env(&[
            (
                "CUSTOMS_PROVIDER_NAAPI_CC_KEY_MAIN_APIKEY".into(),
                "sk-naapi".into(),
            ),
            (
                "CUSTOMS_PROVIDER_0EGAESHOP_XYZ_KEY_MAIN_APIKEY".into(),
                "sk-0ega".into(),
            ),
            (
                "CUSTOMS_PROVIDER_VSLLM_COM_APIKEY".into(),
                "sk-vsllm".into(),
            ),
        ]);
        assert_eq!(config.providers[0].keys[0].api_key, "sk-naapi");
        assert_eq!(config.providers[1].keys[0].api_key, "sk-0ega");
        assert_eq!(config.providers[2].keys[0].api_key, "sk-vsllm");

        // Channels are named after the provider, and that name reaches the
        // audit ledger, so it must survive expansion verbatim.
        let names: Vec<String> = config.all_channels().into_iter().map(|c| c.name).collect();
        for want in ["naapi.cc/main", "0egaeshop.xyz/main", "vsllm.com"] {
            assert!(
                names.contains(&want.to_string()),
                "missing {want}: {names:?}"
            );
        }
    }

    /// Mirrors Go's `TestUpstreamURLJoinsEitherBaseURLForm`.
    #[test]
    fn upstream_url_joins_either_base_url_form() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "https://naapi.cc",
                "/v1/chat/completions",
                "https://naapi.cc/v1/chat/completions",
            ),
            (
                "https://naapi.cc/",
                "/v1/chat/completions",
                "https://naapi.cc/v1/chat/completions",
            ),
            (
                "https://naapi.cc/v1",
                "/v1/chat/completions",
                "https://naapi.cc/v1/chat/completions",
            ),
            (
                "https://naapi.cc/v1/",
                "/v1/chat/completions",
                "https://naapi.cc/v1/chat/completions",
            ),
            (
                "https://naapi.cc/v1",
                "/v1/models",
                "https://naapi.cc/v1/models",
            ),
            (
                "https://naapi.cc",
                "/v1/models",
                "https://naapi.cc/v1/models",
            ),
            (
                "https://naapi.cc/v1",
                "/v1/responses",
                "https://naapi.cc/v1/responses",
            ),
            (
                "https://naapi.cc/v1",
                "/v1/messages",
                "https://naapi.cc/v1/messages",
            ),
            (
                "https://host/openai/v1",
                "/v1/chat/completions",
                "https://host/openai/v1/chat/completions",
            ),
            (
                "https://host/openai",
                "/v1/chat/completions",
                "https://host/openai/v1/chat/completions",
            ),
            (
                "https://host/api",
                "/api/pricing",
                "https://host/api/api/pricing",
            ),
            (
                "https://host/v1",
                "/chat/completions",
                "https://host/v1/chat/completions",
            ),
            ("", "/v1/models", ""),
            ("https://host/v1", "", "https://host/v1"),
        ];
        for (base, path, want) in cases {
            assert_eq!(&upstream_url(base, path), want, "base={base} path={path}");
        }
        // Every combination must contain exactly one version segment, whatever
        // the operator wrote.
        for base in ["https://h", "https://h/", "https://h/v1", "https://h/v1/"] {
            let got = upstream_url(base, "/v1/chat/completions");
            assert_eq!(got.matches("/v1/").count(), 1, "{got}");
        }
    }

    #[test]
    fn upstream_url_does_not_double_the_version_segment() {
        assert_eq!(
            upstream_url("https://host", "/v1/chat/completions"),
            "https://host/v1/chat/completions"
        );
        assert_eq!(
            upstream_url("https://host/v1", "/v1/chat/completions"),
            "https://host/v1/chat/completions"
        );
        assert_eq!(
            upstream_url("https://host/openai/v1", "/v1/responses"),
            "https://host/openai/v1/responses"
        );
        assert_eq!(upstream_url("", "/v1/models"), "");
        assert_eq!(upstream_url("https://host/", ""), "https://host");
    }

    #[test]
    fn declared_protocols_round_trip() {
        let mut config = provider_config();
        config.providers[0].supports = vec!["openai".into(), "responses".into()];
        let channels = expand_providers(&config);
        assert_eq!(
            channels[0].declared_protocols(),
            vec!["openai", "responses"]
        );
        assert!(channels[0].declares_protocol("responses"));
        assert!(!channels[0].declares_protocol("anthropic"));
    }

    #[test]
    fn routing_defaults() {
        let policy = RoutingPolicy::default();
        assert!(policy.same_protocol_preferred());
        assert!(policy.translation_allowed());
        assert_eq!(policy.attempts(), 3);
        assert!(policy.should_retry("quota"));
        assert!(!policy.should_retry("4xx"));
    }
}
