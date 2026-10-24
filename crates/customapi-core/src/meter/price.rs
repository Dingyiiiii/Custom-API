//! Price quoting and normalisation.
//!
//! Two concerns live here, mirroring `internal/meter/price.go`:
//!
//! * quoting a token mix against a normalized [`config::PriceModel`], including
//!   every promotion that fires for *this* mix at *this* instant;
//! * reducing the shapes an upstream publishes prices in to that same
//!   normalized model, which is the "归一化口径" the project reuses from
//!   api-price-monitor.

use crate::config;
use chrono::{DateTime, FixedOffset, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

// ---------------------------------------------------------------------------
// Token mix and quote
// ---------------------------------------------------------------------------

/// A token mix to price.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Tokens {
    /// Prompt tokens.
    pub input: i64,
    /// Completion tokens.
    pub output: i64,
}

impl Tokens {
    /// Builds a token mix.
    pub const fn new(input: i64, output: i64) -> Self {
        Tokens { input, output }
    }

    /// Returns the combined token count.
    ///
    /// The addition wraps rather than panicking, because Go's `int64` addition
    /// does: a request carrying absurd counts must stay comparable between the
    /// two implementations instead of aborting the process on one side only.
    pub fn total(self) -> i64 {
        self.input.wrapping_add(self.output)
    }
}

/// The priced result for a token mix.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Quote {
    /// Undiscounted cost.
    pub list_cost: f64,
    /// Cost after promotions.
    pub cost: f64,
    /// Currency of both amounts.
    pub currency: String,
    /// Effective discount, 0..100.
    pub discount_pct: f64,
    /// Discounted input component.
    pub input_cost: f64,
    /// Discounted output component.
    pub output_cost: f64,
    /// Promotions that fired, for auditability.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied: Vec<AppliedPromotion>,
    /// How the price was obtained.
    pub basis: String,
}

/// One promotion that reduced the price.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AppliedPromotion {
    /// Promotion kind (`off_peak`, `volume`, `subscription`).
    pub kind: String,
    /// Multiplier this promotion contributed.
    pub multiplier: f64,
    /// Operator-facing justification.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

// ---------------------------------------------------------------------------
// Pricer
// ---------------------------------------------------------------------------

/// Prices token mixes using a catalogue of normalized prices.
///
/// The catalogue is behind a lock rather than `&mut self` because the
/// intelligence layer refreshes it at runtime (`set`) while request handlers
/// read it (`lookup`, `quote`) from other tasks; Go shares one `*Pricer`
/// between the two without synchronisation, which Rust will not accept.
pub struct Pricer {
    /// Maps "channel\x00model" to a normalized price.
    catalogue: RwLock<HashMap<String, config::PriceModel>>,
    /// Timezone used for off-peak windows.
    location: FixedOffset,
    /// Currency used when a price omits its own.
    default_currency: String,
}

impl std::fmt::Debug for Pricer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entries = self.catalogue.read().map(|c| c.len()).unwrap_or(0);
        f.debug_struct("Pricer")
            .field("entries", &entries)
            .field("location", &self.location)
            .field("default_currency", &self.default_currency)
            .finish()
    }
}

impl Pricer {
    /// Builds a pricer from price entries.
    ///
    /// `tz` is the offset used to evaluate off-peak windows; `None` means UTC,
    /// matching Go's nil `*time.Location`. A DST-aware zone would need chrono-tz,
    /// which this workspace does not depend on (see the porting report).
    pub fn new(entries: Vec<config::PriceEntry>, tz: Option<FixedOffset>) -> Pricer {
        let mut catalogue = HashMap::with_capacity(entries.len());
        for entry in entries {
            let mut pm = entry.price;
            if pm.currency.is_empty() {
                pm.currency = DEFAULT_CURRENCY.to_string();
            }
            catalogue.insert(catalogue_key(&entry.channel, &entry.model), pm);
        }
        Pricer {
            catalogue: RwLock::new(catalogue),
            location: tz.unwrap_or_else(utc_offset),
            default_currency: DEFAULT_CURRENCY.to_string(),
        }
    }

    /// Returns the timezone used for off-peak windows.
    pub fn location(&self) -> FixedOffset {
        self.location
    }

    /// Returns the currency used when a price omits one.
    pub fn default_currency(&self) -> &str {
        &self.default_currency
    }

    /// Returns the normalized price for a channel/model pair.
    pub fn lookup(&self, channel: &str, model: &str) -> Option<config::PriceModel> {
        read_lock(&self.catalogue)
            .get(&catalogue_key(channel, model))
            .cloned()
    }

    /// Inserts or replaces a price at runtime.
    ///
    /// Used by the intelligence layer when it refreshes the catalogue without a
    /// restart.
    pub fn set(&self, channel: &str, model: &str, mut pm: config::PriceModel) {
        if pm.currency.is_empty() {
            pm.currency = self.default_currency.clone();
        }
        write_lock(&self.catalogue).insert(catalogue_key(channel, model), pm);
    }

    /// Returns every priced model for a channel, in sorted order.
    ///
    /// Go returns them in map-iteration order; sorting here keeps listings and
    /// tests deterministic without changing the set.
    pub fn models(&self, channel: &str) -> Vec<String> {
        let prefix = format!("{channel}\u{0}");
        let catalogue = read_lock(&self.catalogue);
        let mut out: Vec<String> = catalogue
            .keys()
            .filter_map(|key| key.strip_prefix(&prefix))
            .map(str::to_string)
            .collect();
        out.sort();
        out
    }

    /// Prices a token mix at a point in time.
    ///
    /// Time-dependent promotions use `at`; volume promotions use `tokens.total()`.
    pub fn quote(&self, channel: &str, model: &str, tokens: Tokens, at: DateTime<Utc>) -> Quote {
        let Some(pm) = self.lookup(channel, model) else {
            return Quote {
                basis: "unknown".to_string(),
                currency: self.default_currency.clone(),
                ..Quote::default()
            };
        };
        price_with(&pm, tokens, at, Some(self.location))
    }
}

/// Currency assumed when neither the price nor the pricer states one.
const DEFAULT_CURRENCY: &str = "CNY";

/// Joins a channel and a model into a catalogue key.
fn catalogue_key(channel: &str, model: &str) -> String {
    format!("{channel}\u{0}{model}")
}

/// Returns the UTC offset.
///
/// Go writes `time.UTC`; chrono spells the same thing as a zero `FixedOffset`.
pub fn utc_offset() -> FixedOffset {
    FixedOffset::east_opt(0).expect("UTC is a valid offset")
}

/// Locks for reading, ignoring poisoning.
///
/// A panic inside a request handler must not turn the price catalogue into a
/// permanently unusable object: the data behind the lock is a plain map whose
/// invariant cannot be broken by a panic, so the poison is ignored.
fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

/// Locks for writing, ignoring poisoning (see [`read_lock`]).
fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Quoting
// ---------------------------------------------------------------------------

/// Applies a normalized price plus its promotions to a token mix.
///
/// `loc` is the timezone off-peak windows are evaluated in; `None` means UTC.
pub fn price_with(
    pm: &config::PriceModel,
    tokens: Tokens,
    at: DateTime<Utc>,
    loc: Option<FixedOffset>,
) -> Quote {
    let loc = loc.unwrap_or_else(utc_offset);
    let currency = if pm.currency.is_empty() {
        DEFAULT_CURRENCY.to_string()
    } else {
        pm.currency.clone()
    };
    let in_cost = tokens.input as f64 / 1e6 * pm.input_per_m;
    let out_cost = tokens.output as f64 / 1e6 * pm.output_per_m;
    let list = in_cost + out_cost;

    let mut mult = 1.0;
    let mut applied: Vec<AppliedPromotion> = Vec::new();
    for promo in &pm.promotions {
        if let Some(m) = promotion_multiplier(promo, pm, tokens, at, loc) {
            mult *= m;
            applied.push(AppliedPromotion {
                kind: promo.kind.clone(),
                multiplier: m,
                note: promo.note.clone(),
            });
        }
    }
    let cost = list * mult;
    let mut discount = 0.0;
    if list > 0.0 {
        discount = (1.0 - cost / list) * 100.0;
    }
    Quote {
        list_cost: round6(list),
        cost: round6(cost),
        currency,
        discount_pct: round4(discount),
        input_cost: round6(in_cost * mult),
        output_cost: round6(out_cost * mult),
        applied,
        basis: "catalogue".to_string(),
    }
}

/// Returns the blended price per million tokens for a mix.
///
/// This is the number the comparison UI shows: it collapses an input/output rate
/// pair plus every active promotion into one scalar that can be sorted.
pub fn effective_unit_price(
    pm: &config::PriceModel,
    tokens: Tokens,
    at: DateTime<Utc>,
    loc: Option<FixedOffset>,
) -> f64 {
    let tokens = if tokens.total() == 0 {
        // Fall back to a 1:1 mix so the number is still meaningful.
        Tokens {
            input: 500_000,
            output: 500_000,
        }
    } else {
        tokens
    };
    let q = price_with(pm, tokens, at, loc);
    q.cost / (tokens.total() as f64 / 1e6)
}

/// Evaluates a promotion, reporting the multiplier when it applies.
fn promotion_multiplier(
    p: &config::Promotion,
    pm: &config::PriceModel,
    tokens: Tokens,
    at: DateTime<Utc>,
    loc: FixedOffset,
) -> Option<f64> {
    let mut m = p.multiplier;
    if m <= 0.0 {
        m = 1.0;
    }
    match p.kind.as_str() {
        config::PROMO_OFF_PEAK => {
            if in_window(at.with_timezone(&loc), p.start_minute, p.end_minute) {
                Some(m)
            } else {
                None
            }
        }
        config::PROMO_VOLUME => {
            if tokens.total() >= p.min_tokens && p.min_tokens > 0 {
                Some(m)
            } else {
                None
            }
        }
        config::PROMO_SUBSCRIBE => {
            if p.included_tokens <= 0 {
                return None;
            }
            // Amortised subscription: the monthly fee buys `included_tokens`, so
            // the effective multiplier is (fee / included tokens) relative to
            // the pay-as-you-go rate for this mix.
            let list_cost = tokens.input as f64 / 1e6 * pm.input_per_m
                + tokens.output as f64 / 1e6 * pm.output_per_m;
            if list_cost <= 0.0 {
                return None;
            }
            let per_token = p.monthly_fee / p.included_tokens as f64;
            let sub_cost = per_token * tokens.total() as f64;
            Some(sub_cost / list_cost)
        }
        _ => None,
    }
}

/// Reports whether a time falls in `[start, end)` minutes past midnight,
/// supporting windows that wrap past midnight.
fn in_window<Tz: chrono::TimeZone>(t: DateTime<Tz>, start: i32, end: i32) -> bool {
    if start == end {
        return true; // a full-day window
    }
    let minutes = t.hour() as i32 * 60 + t.minute() as i32;
    if start < end {
        return minutes >= start && minutes < end;
    }
    minutes >= start || minutes < end
}

/// Rounds to six decimal places, the precision the quote JSON carries.
fn round6(v: f64) -> f64 {
    // f64::round rounds half away from zero, like Go's math.Round.
    (v * 1e6).round() / 1e6
}

/// Rounds to four decimal places, the precision a discount percentage carries.
fn round4(v: f64) -> f64 {
    (v * 1e4).round() / 1e4
}

// ---------------------------------------------------------------------------
// Price normalisation
// ---------------------------------------------------------------------------

/// An upstream price in whatever shape the upstream published it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawPrice {
    /// Selects the conversion: "multiplier", "flat", "per_request", "tiered" or
    /// "manual".
    pub scheme: String,
    /// Paid price per million input tokens, for schemes where the upstream
    /// states a cash rate directly.
    pub input_per_m: f64,
    /// Output/input ratio, for multiplier schemes where the upstream expresses
    /// input as a multiple of a reference price.
    pub completion_ratio: f64,
    /// The upstream's per-model multiplier against a base price.
    pub model_ratio: f64,
    /// The reference price per million tokens that `model_ratio` multiplies.
    /// new-api calls this the "模型固定价格/倍率" base.
    pub base_price_per_m: f64,
    /// A per-user-group multiplier applied on top.
    pub group_ratio: f64,
    /// Charge per request, for `per_request` schemes.
    pub per_request_price: f64,
    /// Currency of the raw numbers.
    pub currency: String,
    /// Token mix used to convert a per-request price into per-million rates.
    pub typical_input: i64,
    /// Output half of the typical mix.
    pub typical_output: i64,
    /// Tiered pricing rows.
    pub tiers: Vec<Tier>,
    /// Converts from the raw currency to the catalogue currency.
    pub exchange_rate: f64,
    /// Documents where the number came from.
    pub source: String,
}

/// One row of a tiered price expression.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tier {
    /// Inclusive upper bound of this tier; 0 means "and above" when it is the
    /// last tier.
    pub up_to_tokens: i64,
    /// Input rate within the tier.
    pub input_per_m: f64,
    /// Output rate within the tier.
    pub output_per_m: f64,
    /// Alternative to explicit rates.
    pub multiplier: f64,
}

/// Reports a price-normalisation failure.
///
/// The message texts are copied verbatim from the Go implementation because
/// operators and tests grep for them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MeterError {
    /// A per-request price with no typical token mix cannot be converted.
    #[error("meter: per_request scheme needs a typical token mix")]
    PerRequestNeedsTypicalMix,
    /// A per-request price whose typical mix sums to zero.
    #[error("meter: per_request scheme needs a non-zero token mix")]
    PerRequestZeroTypicalMix,
    /// A tiered price with no rows.
    #[error("meter: tiered scheme needs at least one tier")]
    TieredNeedsTiers,
    /// A tiered price whose rows all fall outside the typical mix.
    #[error("meter: tiered scheme produced no weight")]
    TieredNoWeight,
    /// A scheme name the catalogue does not know.
    #[error("meter: unknown price scheme {0:?}")]
    UnknownScheme(String),
}

/// Reduces a [`RawPrice`] to the catalogue shape.
///
/// This is the "归一化口径" the project reuses from api-price-monitor, extended
/// with tiered and per-request handling.
///
/// Go returns a partially-filled model together with the error; no caller reads
/// that partial value, so this port returns `Err` alone and documents the
/// difference here rather than forcing every caller to destructure a tuple.
pub fn normalize(r: RawPrice) -> Result<config::PriceModel, MeterError> {
    let mut rate = r.exchange_rate;
    if rate == 0.0 {
        rate = 1.0;
    }
    let mut pm = config::PriceModel {
        currency: r.currency.clone(),
        source: r.source.clone(),
        scheme: r.scheme.clone(),
        ..config::PriceModel::default()
    };
    match r.scheme.as_str() {
        "multiplier" => {
            // price = base * modelRatio * groupRatio, with output derived from
            // the completion ratio.
            let base = r.base_price_per_m;
            let mr = if r.model_ratio == 0.0 {
                1.0
            } else {
                r.model_ratio
            };
            let gr = if r.group_ratio == 0.0 {
                1.0
            } else {
                r.group_ratio
            };
            let inp = base * mr * gr;
            let cr = if r.completion_ratio == 0.0 {
                1.0
            } else {
                r.completion_ratio
            };
            pm.input_per_m = inp * rate;
            pm.output_per_m = inp * cr * rate;
            pm.completion_ratio = cr;
        }
        "flat" => {
            pm.input_per_m = r.input_per_m * rate;
            let cr = if r.completion_ratio == 0.0 {
                1.0
            } else {
                r.completion_ratio
            };
            pm.output_per_m = pm.input_per_m * cr;
            pm.completion_ratio = cr;
        }
        "per_request" => {
            let inp = r.typical_input;
            let out = r.typical_output;
            if inp == 0 && out == 0 {
                return Err(MeterError::PerRequestNeedsTypicalMix);
            }
            let total = (inp.wrapping_add(out)) as f64;
            if total <= 0.0 {
                return Err(MeterError::PerRequestZeroTypicalMix);
            }
            // Spread the per-request charge across the typical mix, attributing
            // it proportionally to the two sides.
            let per_token = r.per_request_price / total * rate;
            pm.input_per_m = per_token * 1e6;
            pm.output_per_m = per_token * 1e6;
            pm.completion_ratio = 1.0;
        }
        "tiered" => {
            if r.tiers.is_empty() {
                return Err(MeterError::TieredNeedsTiers);
            }
            // Collapse tiers to a volume-weighted average over the typical mix so
            // that a single comparable pair of rates exists. The full tier table
            // is preserved by the caller if it wants exact billing.
            let mut in_sum = 0.0;
            let mut out_sum = 0.0;
            let mut weight = 0.0;
            let mut remaining = r.typical_input.wrapping_add(r.typical_output);
            if remaining <= 0 {
                remaining = 1_000_000;
            }
            for tier in &r.tiers {
                let mut span = tier.up_to_tokens;
                if span <= 0 || span > remaining {
                    span = remaining;
                }
                if span <= 0 {
                    continue;
                }
                let w = span as f64;
                in_sum += tier.input_per_m * rate * w;
                out_sum += tier.output_per_m * rate * w;
                weight += w;
                remaining -= span;
                if remaining <= 0 {
                    break;
                }
            }
            if weight == 0.0 {
                return Err(MeterError::TieredNoWeight);
            }
            pm.input_per_m = in_sum / weight;
            pm.output_per_m = out_sum / weight;
        }
        "manual" => {
            pm.input_per_m = r.input_per_m * rate;
            let cr = if r.completion_ratio == 0.0 {
                1.0
            } else {
                r.completion_ratio
            };
            pm.output_per_m = pm.input_per_m * cr;
            pm.completion_ratio = cr;
        }
        other => return Err(MeterError::UnknownScheme(other.to_string())),
    }
    Ok(pm)
}

/// Returns a conversion factor between two currencies using a configurable
/// USD/CNY rate.
pub fn currency_rate(from: &str, to: &str, usd_cny: f64) -> f64 {
    if from == to {
        return 1.0;
    }
    let usd_cny = if usd_cny <= 0.0 { 7.1 } else { usd_cny };
    match (from, to) {
        ("USD", "CNY") => usd_cny,
        ("CNY", "USD") => 1.0 / usd_cny,
        _ => 1.0,
    }
}
