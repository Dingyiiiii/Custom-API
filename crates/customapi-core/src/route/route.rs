//! Channel selection, circuit breaking and integrity weighting.

use crate::config::Channel;
use crate::meter::{self, Pricer, Quote, Tokens};
use chrono::{DateTime, TimeDelta, Utc};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Scores
// ---------------------------------------------------------------------------

/// The intelligence layer's verdict on one channel/model pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scores {
    /// Benchmark score in [0,1]; 0 means "unmeasured".
    #[serde(
        default,
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub capability: f64,
    /// Fraction of probes that matched the baseline, in [0,1]; 1 means "always
    /// the genuine model".
    #[serde(
        default,
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub fingerprint_match: f64,
    /// Observed success rate in [0,1].
    #[serde(
        default,
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub reliability: f64,
    /// 1 - clamp(worst observed deviation), in [0,1]; a channel that inflates
    /// usage is penalised regardless of price.
    #[serde(
        default,
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub usage_integrity: f64,
    /// Per-suite capability detail.
    ///
    /// A `BTreeMap` (not a `HashMap`) keeps the serialised order sorted, which is
    /// what Go's `encoding/json` does for maps; the scorecard is a file the
    /// operator may diff.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "null_default",
        serialize_with = "crate::json::go_f64_map"
    )]
    pub benchmarks: BTreeMap<String, f64>,
    /// When the score was computed.
    ///
    /// Go's `omitempty` does not apply to a `time.Time` struct, so the zero
    /// timestamp is always written; the field mirrors that.
    #[serde(
        default = "crate::config::zero_time",
        serialize_with = "crate::json::go_time"
    )]
    pub updated_at: DateTime<Utc>,
    /// How many observations back this score.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i32")]
    pub samples: i32,
}

impl Default for Scores {
    /// The Go zero value, including its zero `time.Time`.
    ///
    /// A derived `Default` would leave `updated_at` at the Unix epoch, which is
    /// what chrono defaults to; the scorecard then claimed a score had been
    /// measured in 1970 instead of never.
    fn default() -> Scores {
        Scores {
            capability: 0.0,
            fingerprint_match: 0.0,
            reliability: 0.0,
            usage_integrity: 0.0,
            benchmarks: BTreeMap::new(),
            updated_at: crate::config::zero_time(),
            samples: 0,
        }
    }
}

/// Resolves scores by channel and model.
///
/// Go declares this as an interface and then type-asserts `*MapScoreBook` inside
/// `DegradeChannel`. Rust cannot assert on a trait object without `Any`, so the
/// feedback edge is a trait method instead: the default implementation is a
/// no-op, which is exactly what Go's failed assertion does.
pub trait ScoreBook: Send + Sync {
    /// Returns the score for a channel/model pair, if one is known.
    fn score(&self, channel: &str, model: &str) -> Option<Scores>;

    /// Records a metering deviation for every model of a channel.
    fn degrade(&self, channel: &str, deviation: &Deviation) {
        let _ = (channel, deviation);
    }
}

/// An in-memory [`ScoreBook`], used by tests and by the intelligence layer's
/// initial load.
pub struct MapScoreBook {
    by_key: RwLock<HashMap<String, Scores>>,
    /// Returned when no entry exists.
    pub fallback: Scores,
    /// Enables the fallback.
    pub use_fallback: bool,
}

impl Default for MapScoreBook {
    fn default() -> Self {
        MapScoreBook {
            by_key: RwLock::new(HashMap::new()),
            fallback: Scores::default(),
            use_fallback: false,
        }
    }
}

impl MapScoreBook {
    /// Builds a score book from a nested map.
    pub fn new(input: HashMap<String, HashMap<String, Scores>>) -> MapScoreBook {
        let mut by_key = HashMap::new();
        for (channel, models) in input {
            for (model, score) in models {
                by_key.insert(score_key(&channel, &model), score);
            }
        }
        MapScoreBook {
            by_key: RwLock::new(by_key),
            ..MapScoreBook::default()
        }
    }

    /// Inserts a score.
    pub fn set(&self, channel: &str, model: &str, score: Scores) {
        write_lock(&self.by_key).insert(score_key(channel, model), score);
    }

    /// Returns the number of recorded channel/model pairs.
    pub fn len(&self) -> usize {
        read_lock(&self.by_key).len()
    }

    /// Reports whether the book is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ScoreBook for MapScoreBook {
    fn score(&self, channel: &str, model: &str) -> Option<Scores> {
        if let Some(score) = read_lock(&self.by_key).get(&score_key(channel, model)) {
            return Some(score.clone());
        }
        if self.use_fallback {
            return Some(self.fallback.clone());
        }
        None
    }

    fn degrade(&self, channel: &str, deviation: &Deviation) {
        let prefix = format!("{channel}\u{0}");
        let mut book = write_lock(&self.by_key);
        // Collect first: Go mutates the map it is ranging over, which Rust
        // cannot do, and the two notations produce the same result.
        let keys: Vec<String> = book
            .keys()
            .filter(|key| key.len() > prefix.len() && key.starts_with(&prefix))
            .cloned()
            .collect();
        // Clamp the measured deviation into an integrity factor in [0,1]. Go
        // clamps +Inf (and -Inf through Abs) to 1e6 before taking the minimum;
        // NaN propagates through math.Min, so it propagates here too.
        let worst = {
            let abs = deviation.worst.abs();
            if abs.is_infinite() {
                1e6
            } else {
                abs
            }
        };
        let measured = if worst.is_nan() {
            f64::NAN
        } else if worst < 1.0 {
            1.0 - worst
        } else {
            0.0
        };
        let now = Utc::now();
        for key in keys {
            let Some(score) = book.get_mut(&key) else {
                continue;
            };
            if score.usage_integrity == 0.0 {
                score.usage_integrity = 1.0;
            }
            // Blend with the existing estimate so a single anomaly does not
            // collapse a channel's score, but a pattern does.
            if score.samples > 0 {
                let samples = score.samples as f64;
                score.usage_integrity =
                    (score.usage_integrity * samples + measured) / (samples + 1.0);
            } else {
                score.usage_integrity = measured;
            }
            score.samples += 1;
            score.updated_at = now;
        }
    }
}

/// Accepts an explicit `null` where a map is expected.
///
/// Go writes `null` for a nil map, and the scorecard is written by the
/// intelligence layer before every field has been filled in.
fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Joins a channel and a model into a score key.
fn score_key(channel: &str, model: &str) -> String {
    format!("{channel}\u{0}{model}")
}

// ---------------------------------------------------------------------------
// Candidate scoring
// ---------------------------------------------------------------------------

/// One channel considered for a request.
#[derive(Debug, Clone, Default)]
pub struct Candidate {
    /// The resolved configuration.
    pub channel: Channel,
    /// Model name to send upstream.
    pub upstream_model: String,
    /// The normalized price, when known.
    pub price: crate::config::PriceModel,
    /// Whether [`Candidate::price`] is meaningful.
    pub has_price: bool,
    /// Priced cost for this request's token mix.
    pub quote: Quote,
    /// Blended cost per million tokens.
    pub unit_price: f64,
    /// The intelligence verdict, when known.
    pub scores: Scores,
    /// Composite ranking score for strategy "value".
    pub value: f64,
    /// Configured routing weight.
    pub weight: f64,
    /// Explains the ranking contribution.
    pub reason: String,
    /// Wire protocol the channel speaks.
    pub protocol: String,
    /// Whether reaching this channel requires converting the request and
    /// response between protocols.
    pub translated: bool,
}

/// The input to the router.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// Canonical model name.
    pub model: String,
    /// Token mix to price.
    pub tokens: Tokens,
    /// Pricing instant; `None` means now.
    pub at: Option<DateTime<Utc>>,
    /// Overrides the configured default strategy: "cheapest", "best_score",
    /// "value", "weighted" or "fixed".
    pub strategy: String,
    /// Names a channel to use regardless of strategy.
    pub pin: String,
    /// Channels that must not be used (for example after a failure, or when the
    /// user already tried them).
    ///
    /// Go models both this and [`Request::unhealthy`] as `map[string]bool`; a set
    /// is the same thing with the redundant boolean removed.
    pub exclude: HashSet<String>,
    /// Channels that must not be used because they are marked unhealthy.
    pub unhealthy: HashSet<String>,

    /// The protocol the caller spoke: "openai", "responses" or "anthropic".
    pub client_protocol: String,
    /// Ranks channels that already speak the client's protocol above any channel
    /// reachable only by translation. Translation is lossy in places, so it is a
    /// fallback rather than a default.
    pub prefer_same_protocol: bool,
    /// Permits considering channels that speak a different protocol. When false,
    /// only same-protocol channels are candidates.
    pub allow_translation: bool,
    /// Restricts routing to providers in these groups.
    pub groups: Vec<String>,
    /// Skips these providers entirely.
    pub exclude_providers: Vec<String>,
}

/// The router's output.
#[derive(Debug, Clone, Default)]
pub struct Decision {
    /// The selected candidate.
    pub chosen: Candidate,
    /// Every admissible candidate in preference order, which the failover loop
    /// uses to try alternatives.
    pub ranked: Vec<Candidate>,
    /// Strategy actually applied.
    pub strategy: String,
    /// Human-readable explanation recorded in the audit log.
    pub reason: String,
}

/// Configures a [`Selector`].
#[derive(Default)]
pub struct Options {
    /// Price catalogue, shared with the intelligence layer.
    pub pricer: Option<Arc<Pricer>>,
    /// Capability scores, shared with the intelligence layer.
    pub scores: Option<Arc<dyn ScoreBook>>,
    /// Default strategy; empty means "value".
    pub strategy: String,
    /// Makes the weighted strategy deterministic in tests.
    pub seed: i64,
}

/// Performs channel selection.
///
/// A `Selector` is safe for concurrent use: the only mutable state is the RNG
/// behind the weighted strategy (guarded, because selection runs on every
/// request task) and the protocol remembered for wording explanations.
pub struct Selector {
    pricer: Option<Arc<Pricer>>,
    scores: Option<Arc<dyn ScoreBook>>,
    strategy: String,
    /// Used by the weighted strategy.
    rng: Mutex<StdRng>,
    /// Guards the value metric against a zero price.
    cheapness_floor: f64,
    /// Remembers the client protocol, which is only used to word explanations.
    last_protocol: Mutex<String>,
}

impl std::fmt::Debug for Selector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Selector")
            .field("strategy", &self.strategy)
            .field("has_pricer", &self.pricer.is_some())
            .field("has_scores", &self.scores.is_some())
            .finish()
    }
}

impl Selector {
    /// Builds a selector.
    pub fn new(opts: Options) -> Selector {
        let mut strategy = opts.strategy;
        if strategy.is_empty() {
            strategy = "value".to_string();
        }
        let seed = if opts.seed == 0 {
            // Go uses the wall clock so that two gateways started in the same
            // second do not send identical traffic patterns upstream.
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E37_79B9_7F4A_7C15)
        } else {
            opts.seed as u64
        };
        Selector {
            pricer: opts.pricer,
            scores: opts.scores,
            strategy,
            rng: Mutex::new(StdRng::seed_from_u64(seed)),
            cheapness_floor: 1e-9,
            last_protocol: Mutex::new(String::new()),
        }
    }

    /// Returns the configured default strategy.
    pub fn strategy(&self) -> &str {
        &self.strategy
    }

    /// Builds and ranks the admissible candidates for a request.
    pub fn candidates(
        &self,
        channels: &[Channel],
        req: &Request,
    ) -> Result<Vec<Candidate>, RouteError> {
        let at = req.at.unwrap_or_else(Utc::now);
        self.note_client_protocol(&req.client_protocol);
        let mut out: Vec<Candidate> = Vec::new();
        for ch in channels {
            if !ch.is_enabled() {
                continue;
            }
            if req.exclude.contains(&ch.name) {
                continue;
            }
            if req.unhealthy.contains(&ch.name) {
                continue;
            }
            let Some(upstream) = ch.upstream_model(&req.model) else {
                continue;
            };
            if upstream.is_empty() {
                continue;
            }
            // Group and provider filtering happens before scoring so that an
            // excluded provider never influences the ranking.
            if !req.groups.is_empty() && !channel_in_groups(ch, &req.groups) {
                continue;
            }
            if !req.exclude_providers.is_empty() && channel_in_providers(ch, &req.exclude_providers)
            {
                continue;
            }
            let protocol = channel_protocol_name(ch).to_string();
            let mut translated = false;
            if !req.client_protocol.is_empty() && protocol != req.client_protocol {
                // The provider does not prefer the protocol the caller used. Two
                // ways it can still serve the request:
                //
                //   - it declared that surface (supports=[..., responses]), in
                //     which case the request goes to that endpoint unchanged;
                //   - it did not declare it, in which case the only route is to
                //     convert the payload, which needs the operator's permission.
                //
                // Anything else is refused. Handing a converted payload to an
                // endpoint that does not implement the target surface produces
                // an upstream 404 at best and a wrong answer at worst.
                if !channel_supports_request(ch, &req.client_protocol) {
                    if !req.allow_translation {
                        continue;
                    }
                    translated = true;
                }
            }
            let mut c = Candidate {
                channel: ch.clone(),
                upstream_model: upstream,
                weight: ch.weight,
                protocol,
                translated,
                ..Candidate::default()
            };
            if let Some(pricer) = &self.pricer {
                if let Some(pm) = pricer.lookup(&ch.name, &req.model) {
                    c.price = pm;
                    c.has_price = true;
                    c.quote = meter::price_with(&c.price, req.tokens, at, None);
                    let total = req.tokens.total();
                    if total > 0 {
                        c.unit_price = c.quote.cost / (total as f64 / 1e6);
                    } else {
                        c.unit_price = meter::effective_unit_price(&c.price, req.tokens, at, None);
                    }
                }
            }
            if let Some(scores) = &self.scores {
                if let Some(sc) = scores.score(&ch.name, &req.model) {
                    c.scores = sc;
                }
            }
            c.value = self.value(&c);
            out.push(c);
        }
        if out.is_empty() {
            return Err(RouteError::NoChannel(req.model.clone()));
        }
        self.rank(&mut out, &req.strategy, req.prefer_same_protocol);
        Ok(out)
    }

    /// Returns the best candidate plus the full ranked list.
    pub fn select(&self, channels: &[Channel], req: &Request) -> Result<Decision, RouteError> {
        let cands = self.candidates(channels, req)?;
        let strategy = if req.strategy.is_empty() {
            self.strategy.clone()
        } else {
            req.strategy.clone()
        };
        if !req.pin.is_empty() {
            if let Some(c) = cands.iter().find(|c| c.channel.name == req.pin) {
                return Ok(Decision {
                    chosen: c.clone(),
                    ranked: cands,
                    strategy: "pinned".to_string(),
                    reason: format!("channel {:?} pinned by request", req.pin),
                });
            }
            return Err(RouteError::PinUnavailable(req.pin.clone()));
        }
        let reason = self.explain(&cands[0], &cands, &strategy);
        let chosen = cands[0].clone();
        Ok(Decision {
            chosen,
            ranked: cands,
            strategy,
            reason,
        })
    }

    /// Computes the composite value metric.
    ///
    /// `value = capability_quality * integrity / unit_price`
    ///
    /// Both quality terms default to a neutral 1.0 when unmeasured, so a new
    /// channel is neither advantaged nor punished by missing data. Integrity is
    /// squared so a channel caught inflating usage (0.5) is penalised harder than
    /// its linear share.
    fn value(&self, c: &Candidate) -> f64 {
        let mut quality = 1.0;
        if c.scores.capability > 0.0 {
            quality *= c.scores.capability;
        }
        if c.scores.fingerprint_match > 0.0 {
            quality *= c.scores.fingerprint_match;
        }
        if c.scores.reliability > 0.0 {
            quality *= c.scores.reliability;
        }
        if c.scores.usage_integrity > 0.0 {
            quality *= c.scores.usage_integrity * c.scores.usage_integrity;
        }
        if !c.has_price || c.unit_price <= 0.0 {
            // Unknown price: treat as moderately expensive so a priced channel
            // wins on a tie, but the channel is not excluded.
            return quality / 1.0;
        }
        let mut v = quality / go_max(c.unit_price, self.cheapness_floor);
        // Scale by the operator's channel weight so a preferred channel can be
        // nudged upward without pinning it.
        if c.weight > 0.0 {
            v *= c.weight;
        }
        v
    }

    /// Orders candidates in place according to the strategy.
    ///
    /// When the caller asked for same-protocol preference, the list is first
    /// partitioned so that channels speaking the client's protocol come before
    /// any channel that would require translation. The configured strategy then
    /// orders within each partition, which means a translation is only ever
    /// attempted when no same-protocol channel is available - the property that
    /// keeps the common path lossless.
    fn rank(&self, cands: &mut Vec<Candidate>, strategy: &str, prefer_same: bool) {
        let strategy = if strategy.is_empty() {
            self.strategy.as_str()
        } else {
            strategy
        };
        match strategy {
            "cheapest" => cands.sort_by(|a, b| self.less_unit_price(a, b)),
            "best_score" => cands.sort_by(|a, b| {
                let (x, y) = (self.capability(b), self.capability(a));
                x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
            }),
            "weighted" => {
                // A weighted draw over the configured weights, with the drawn
                // channel first and the rest in value order for failover.
                cands.sort_by(|a, b| {
                    b.value
                        .partial_cmp(&a.value)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                if cands.len() > 1 {
                    let total: f64 = cands
                        .iter()
                        .filter(|c| c.weight > 0.0)
                        .map(|c| c.weight)
                        .sum();
                    if total > 0.0 {
                        let pick = {
                            let mut rng = lock(&self.rng);
                            rng.random::<f64>() * total
                        };
                        let mut acc = 0.0;
                        let mut idx = 0;
                        for (i, c) in cands.iter().enumerate() {
                            acc += c.weight;
                            if pick <= acc {
                                idx = i;
                                break;
                            }
                        }
                        if idx != 0 {
                            let chosen = cands.remove(idx);
                            cands.insert(0, chosen);
                        }
                    }
                }
            }
            "fixed" => {
                // Preserve configuration order.
            }
            _ => {
                // "value", and any unrecognised name, which the configuration
                // layer rejects but a caller can still pass.
                cands.sort_by(|a, b| {
                    b.value
                        .partial_cmp(&a.value)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
        }

        // Same-protocol preference is a partition, not a tie-break: a channel
        // that serves the client's protocol without conversion is worth more than
        // any amount of cheapness, because translation is lossy (tool calls,
        // reasoning fields and server-side state degrade) while a price
        // difference is merely money. The strategy still decides the order
        // *within* each tier, so "cheapest" keeps meaning cheapest among
        // comparable channels.
        //
        // This is applied after the strategy sort rather than inside it so that
        // every strategy gets the same guarantee; an earlier revision accepted
        // the flag and never read it, which silently disabled the feature.
        if prefer_same {
            cands.sort_by_key(|c| c.translated);
        }
    }

    /// Orders by price, treating unknown prices as most expensive.
    fn less_unit_price(&self, a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        if a.has_price != b.has_price {
            return if a.has_price {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        if !a.has_price {
            return a.channel.name.cmp(&b.channel.name);
        }
        if a.unit_price != b.unit_price {
            return a
                .unit_price
                .partial_cmp(&b.unit_price)
                .unwrap_or(Ordering::Equal);
        }
        a.channel.name.cmp(&b.channel.name)
    }

    /// Returns the capability score with a neutral default.
    fn capability(&self, c: &Candidate) -> f64 {
        if c.scores.capability > 0.0 {
            c.scores.capability
        } else {
            0.5
        }
    }

    /// Builds the audit-facing justification for a choice.
    fn explain(&self, chosen: &Candidate, ranked: &[Candidate], strategy: &str) -> String {
        let price = if chosen.has_price {
            format!("{:.4} {}/M", chosen.unit_price, chosen.quote.currency)
        } else {
            "unpriced".to_string()
        };
        let capability = if chosen.scores.capability > 0.0 {
            format!("{:.3}", chosen.scores.capability)
        } else {
            "unmeasured".to_string()
        };
        let integrity = if chosen.scores.usage_integrity > 0.0 {
            format!("{:.3}", chosen.scores.usage_integrity)
        } else {
            "unmeasured".to_string()
        };
        let mut via = String::new();
        if chosen.translated {
            let from = lock(&self.last_protocol).clone();
            via = format!(" (translated {} -> {})", or_none(&from), chosen.protocol);
        }
        format!(
            "strategy={} picked {:?} of {} candidate(s): protocol={}{} unit_price={} capability={} usage_integrity={} value={}",
            strategy,
            chosen.channel.name,
            ranked.len(),
            chosen.protocol,
            via,
            price,
            capability,
            integrity,
            format_g4(chosen.value)
        )
    }

    /// Remembers the client protocol for explanations.
    ///
    /// It is set by [`Selector::candidates`], which is the only entry point that
    /// knows the request.
    fn note_client_protocol(&self, protocol: &str) {
        *lock(&self.last_protocol) = protocol.to_string();
    }

    /// Lowers a channel's effective score in response to a metering violation.
    ///
    /// This is the feedback edge from metering back into routing: a channel that
    /// inflates its reported usage is penalised in proportion to how badly it
    /// lied, and the penalty decays over time rather than being permanent, so an
    /// upstream that fixes a billing bug is not punished forever.
    pub fn degrade_channel(&self, channel: &str, d: &Deviation) {
        let Some(book) = &self.scores else {
            return;
        };
        book.degrade(channel, d);
    }
}

impl Default for Selector {
    fn default() -> Selector {
        Selector::new(Options::default())
    }
}

fn or_none(v: &str) -> &str {
    if v.is_empty() {
        "?"
    } else {
        v
    }
}

/// Go's `math.Max`: NaN wins over the other operand, which Rust's `f64::max`
/// does not do.
fn go_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a > b {
        a
    } else {
        b
    }
}

/// Locks, ignoring poisoning.
///
/// A panicking request task must not turn the selector into a permanently
/// unusable object; the guarded state is a random number generator and a string,
/// neither of which can be left inconsistent.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Locks for reading, ignoring poisoning (see [`lock`]).
fn read_lock<T>(m: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

/// Locks for writing, ignoring poisoning (see [`lock`]).
fn write_lock<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Reports a routing failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    /// No channel could serve the requested model.
    #[error("route: no channel can serve model {0:?}")]
    NoChannel(String),
    /// A pinned channel was not among the candidates.
    #[error("route: pinned channel {0:?} is not available")]
    PinUnavailable(String),
}

// ---------------------------------------------------------------------------
// Health tracking
// ---------------------------------------------------------------------------

/// Records channel failures so the router can avoid them.
///
/// It implements a simple consecutive-failure circuit breaker: a channel is
/// considered unhealthy after [`HealthTracker::threshold`] failures in a row and
/// recovers either after [`HealthTracker::cooldown`] elapses or on an explicit
/// success.
///
/// It is safe for concurrent use, because every request task reports its own
/// outcome.
pub struct HealthTracker {
    /// Consecutive failures that trip the breaker.
    pub threshold: i32,
    /// How long an open breaker lasts.
    pub cooldown: Duration,
    state: Mutex<HealthState>,
}

struct HealthState {
    now: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    failures: HashMap<String, i32>,
    opened_at: HashMap<String, DateTime<Utc>>,
}

impl std::fmt::Debug for HealthTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("HealthTracker")
            .field("threshold", &self.threshold)
            .field("cooldown", &self.cooldown)
            .field("failures", &state.failures)
            .finish()
    }
}

impl HealthTracker {
    /// Builds a tracker with sensible defaults.
    pub fn new(threshold: i32, cooldown: Duration) -> HealthTracker {
        let threshold = if threshold <= 0 { 3 } else { threshold };
        let cooldown = if cooldown.is_zero() {
            Duration::from_secs(30)
        } else {
            cooldown
        };
        HealthTracker {
            threshold,
            cooldown,
            state: Mutex::new(HealthState {
                now: Box::new(Utc::now),
                failures: HashMap::new(),
                opened_at: HashMap::new(),
            }),
        }
    }

    /// Overrides the clock, for tests.
    pub fn set_clock<F>(&self, f: F)
    where
        F: Fn() -> DateTime<Utc> + Send + Sync + 'static,
    {
        lock(&self.state).now = Box::new(f);
    }

    /// Records a failure and reports whether the channel is now unhealthy.
    pub fn failure(&self, channel: &str) -> bool {
        let mut state = lock(&self.state);
        let now = (state.now)();
        let count = {
            let entry = state.failures.entry(channel.to_string()).or_insert(0);
            *entry += 1;
            *entry
        };
        if count >= self.threshold {
            state.opened_at.entry(channel.to_string()).or_insert(now);
            return true;
        }
        false
    }

    /// Clears a channel's failure state.
    pub fn success(&self, channel: &str) {
        let mut state = lock(&self.state);
        state.failures.remove(channel);
        state.opened_at.remove(channel);
    }

    /// Returns the set of currently unhealthy channels.
    pub fn unhealthy(&self) -> HashSet<String> {
        let mut state = lock(&self.state);
        let now = (state.now)();
        let cooldown = TimeDelta::from_std(self.cooldown).unwrap_or(TimeDelta::MAX);
        let mut out = HashSet::new();
        let mut expired = Vec::new();
        for (channel, at) in state.opened_at.iter() {
            if now.signed_duration_since(*at) < cooldown {
                out.insert(channel.clone());
            } else {
                // Cooldown elapsed: allow a half-open retry.
                expired.push(channel.clone());
            }
        }
        for channel in expired {
            state.opened_at.remove(&channel);
            state.failures.insert(channel, 0);
        }
        out
    }

    /// Returns the consecutive failure count for a channel.
    pub fn failure_count(&self, channel: &str) -> i32 {
        let state = lock(&self.state);
        state.failures.get(channel).copied().unwrap_or(0)
    }
}

impl Default for HealthTracker {
    fn default() -> HealthTracker {
        HealthTracker::new(0, Duration::ZERO)
    }
}

// ---------------------------------------------------------------------------
// Usage-integrity degradation
// ---------------------------------------------------------------------------

/// The metering layer's measure of how far an upstream's reported usage diverged
/// from the gateway's recount.
///
/// It is redeclared here so that the routing module does not depend on the
/// metering module, avoiding an import cycle (metering -> protocol,
/// route -> config+meter). That was already true in Go; the same separation is
/// kept so the two module graphs stay isomorphic.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Deviation {
    /// Largest absolute relative deviation observed.
    pub worst: f64,
    /// Whether that deviation exceeded the tolerance.
    pub over_threshold: bool,
    /// "over" or "under".
    pub direction: String,
}

// ---------------------------------------------------------------------------
// Protocol and group helpers
// ---------------------------------------------------------------------------

/// Returns the channel's effective wire protocol.
fn channel_protocol_name(ch: &Channel) -> &str {
    if ch.protocol.is_empty() {
        "openai"
    } else {
        &ch.protocol
    }
}

/// Returns the protocol surfaces a channel announced, or `None` when it announced
/// none.
///
/// Go distinguishes a nil slice (no declaration) from an empty one because the
/// difference carries meaning; a `Vec` cannot, so the distinction is spelled with
/// `Option`. Absent metadata and absent `protocols` both mean "silence".
fn declared_protocols(ch: &Channel) -> Option<Vec<String>> {
    if ch.metadata.is_empty() {
        return None;
    }
    let listed = ch.metadata.get("protocols")?;
    let parsed: Vec<String> = listed
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    if parsed.is_empty() {
        None
    } else {
        Some(parsed)
    }
}

/// Reports whether a channel can serve a request that arrived in the given
/// protocol.
///
/// A channel always handles the protocol it prefers. Beyond that it is held to
/// its declaration, and the declaration is exhaustive: a provider that writes
/// protocol=openai with supports=[openai] has enumerated its surfaces and is
/// saying "chat only", so a Responses request must not reach it. Absent metadata
/// is silence rather than a list, and a directly-configured channel is trusted
/// there because the operator wrote the channel by hand.
fn channel_supports_request(ch: &Channel, protocol: &str) -> bool {
    if channel_protocol_name(ch) == protocol {
        return true;
    }
    match declared_protocols(ch) {
        // Go writes `ch.Metadata == nil` here; an empty map is indistinguishable
        // from an absent one in Rust, so both are treated as silence.
        None => ch.metadata.is_empty(),
        Some(declared) => declared.iter().any(|p| p == protocol),
    }
}

/// Reports whether a channel belongs to any of the groups.
fn channel_in_groups(ch: &Channel, groups: &[String]) -> bool {
    let Some(listed) = ch.metadata.get("groups") else {
        return false;
    };
    let member: HashSet<&str> = listed.split(',').map(str::trim).collect();
    groups.iter().any(|g| member.contains(g.as_str()))
}

/// Reports whether a channel belongs to any of the providers.
fn channel_in_providers(ch: &Channel, providers: &[String]) -> bool {
    let name = ch.metadata.get("provider").unwrap_or(&ch.name);
    providers
        .iter()
        .any(|p| name == p || ch.name.starts_with(&format!("{p}/")))
}

// ---------------------------------------------------------------------------
// Go-compatible float formatting
// ---------------------------------------------------------------------------

/// Formats a float the way Go's `fmt.Sprintf("%.4g", v)` does.
pub fn format_g4(v: f64) -> String {
    format_g(v, 4)
}

/// Formats a float the way Go's `%.*g` verb does.
///
/// The audit trail compares routing explanations between the two
/// implementations, and Go's `%g` has no direct Rust counterpart: it picks
/// between fixed and exponential notation by the exponent of the *rounded* value
/// and strips trailing zeros. `{:.3e}` gets the rounding right (including the
/// carry that pushes 9999.9 to 1e4), so the exponent and mantissa are read back
/// from it and re-rendered in Go's notation.
pub fn format_g(v: f64, precision: usize) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 {
            "+Inf".to_string()
        } else {
            "-Inf".to_string()
        };
    }
    if v == 0.0 {
        return if v.is_sign_negative() {
            "-0".to_string()
        } else {
            "0".to_string()
        };
    }
    let precision = precision.max(1);
    let negative = v.is_sign_negative();
    let magnitude = v.abs();
    let formatted = format!("{:.*e}", precision - 1, magnitude);
    let (mantissa, exponent) = formatted
        .split_once('e')
        .expect("`{:e}` always emits an exponent");
    let exponent: i32 = exponent.parse().expect("exponent is an integer");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };

    let mut body = if exponent < -4 || exponent >= precision as i32 {
        // Exponential notation: Go pads the exponent to at least two digits and
        // always prints its sign.
        let mut out = String::new();
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        if exponent < 0 {
            out.push_str(&format!("e-{:02}", -exponent));
        } else {
            out.push_str(&format!("e+{:02}", exponent));
        }
        out
    } else if exponent >= 0 {
        let int_len = exponent as usize + 1;
        if digits.len() <= int_len {
            let mut out = digits.to_string();
            out.push_str(&"0".repeat(int_len - digits.len()));
            out
        } else {
            format!("{}.{}", &digits[..int_len], &digits[int_len..])
        }
    } else {
        format!("0.{}{}", "0".repeat((-exponent - 1) as usize), digits)
    };
    if negative {
        body.insert(0, '-');
    }
    body
}
