//! Price collection: fetch an upstream's published price table and normalize it
//! into the catalogue shape.

use chrono::Utc;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::Duration;

use super::IntelError;
use crate::config;
use crate::meter;

/// Describes where to fetch prices for a channel.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PriceSource {
    /// The channel the prices belong to.
    pub channel: String,
    /// `"newapi"` (a new-api `/api/pricing` endpoint), `"openai"` (a
    /// `/v1/models` plus a static table) or `"manual"`.
    pub kind: String,
    /// The endpoint to fetch.
    #[serde(default)]
    pub url: String,
    /// An optional credential for the pricing endpoint.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// Overrides the currency assumed for the fetched numbers.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub currency: String,
    /// The exchange rate used when converting.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_f64")]
    pub usd_cny: f64,
}

/// The pricing payload exposed by new-api compatible gateways.
///
/// Every field tolerates `null` and absence, because Go's decoder does: a
/// JSON null leaves a Go slice/map at nil without an error, while serde would
/// refuse it. `enable_groups` is part of the payload but, as in Go, unused.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct NewApiPricing {
    // `success` and `enable_groups` are part of the payload but, as in Go, never
    // read: the collector trusts the row shapes, not the envelope flag.
    #[allow(dead_code)]
    success: bool,
    data: Option<Vec<NewApiRow>>,
    group_ratio: Option<BTreeMap<String, f64>>,
    /// Lets the payload tell us its own FX rate.
    usd_exchange_rate: f64,
}

/// One row of the new-api price table.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct NewApiRow {
    model_name: String,
    /// 0 = multiplier against a reference price, 1 = flat per-million price.
    quota_type: i32,
    model_ratio: f64,
    completion_ratio: f64,
    model_price: f64,
}

/// Fetches and normalizes prices for one source.
///
/// The conversion rules mirror api-price-monitor: `quota_type` 0 is a multiplier
/// against the group ratio, `quota_type` 1 is a flat per-million price, and
/// anything else is skipped rather than guessed at.
///
/// Go takes a `context.Context`; cancellation here is tokio's, so dropping the
/// future aborts the fetch. `client` is injected for tests: `None` builds a
/// client with Go's 30s timeout.
pub async fn collect_prices(
    client: Option<&reqwest::Client>,
    src: &PriceSource,
    group: &str,
) -> Result<Vec<config::PriceEntry>, IntelError> {
    let default_client;
    let client = match client {
        Some(client) => client,
        None => {
            default_client = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new());
            &default_client
        }
    };
    match src.kind.as_str() {
        "newapi" => collect_new_api(client, src, group).await,
        "manual" => Ok(Vec::new()),
        other => Err(IntelError::UnknownPriceSourceKind(other.to_string())),
    }
}

/// Fetches a new-api `/api/pricing` payload and converts every recognized row.
async fn collect_new_api(
    client: &reqwest::Client,
    src: &PriceSource,
    group: &str,
) -> Result<Vec<config::PriceEntry>, IntelError> {
    let mut request = client.get(&src.url);
    if !src.api_key.is_empty() {
        request = request.header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", src.api_key),
        );
    }
    let response = request.send().await.map_err(|source| IntelError::Fetch {
        url: src.url.clone(),
        source,
    })?;
    if response.status().as_u16() != 200 {
        return Err(IntelError::FetchStatus {
            url: src.url.clone(),
            status: response.status().as_u16(),
        });
    }
    // Go caps the read at 8 MiB so a hostile pricing endpoint cannot exhaust
    // memory; a read error is returned raw, as Go does.
    let bytes = response.bytes().await.map_err(IntelError::Body)?.to_vec();
    let body = super::bench::limit_body(bytes, 8 << 20);
    let payload: NewApiPricing =
        serde_json::from_slice(&body).map_err(|source| IntelError::ParsePricing {
            url: src.url.clone(),
            source,
        })?;

    let mut group_ratio = 1.0;
    if !group.is_empty() {
        if let Some(ratio) = payload.group_ratio.as_ref().and_then(|map| map.get(group)) {
            group_ratio = *ratio;
        }
    } else if let Some(map) = payload.group_ratio.as_ref() {
        // Pick the default group when the caller did not specify one.
        if !map.is_empty() {
            for name in ["default", "vip", "svip"] {
                if let Some(ratio) = map.get(name) {
                    group_ratio = *ratio;
                    break;
                }
            }
        }
    }
    let mut fx = src.usd_cny;
    if fx == 0.0 {
        fx = payload.usd_exchange_rate;
    }
    if fx == 0.0 {
        fx = 7.1;
    }
    let currency = if src.currency.is_empty() {
        "USD".to_string()
    } else {
        src.currency.clone()
    };

    let now = Utc::now();
    let rows = payload.data.unwrap_or_default();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if row.model_name.is_empty() {
            continue;
        }
        let raw = match row.quota_type {
            0 => meter::RawPrice {
                // new-api's reference: $2 per 1M tokens.
                scheme: "multiplier".to_string(),
                base_price_per_m: 2.0,
                model_ratio: row.model_ratio,
                completion_ratio: row.completion_ratio,
                group_ratio,
                currency: currency.clone(),
                exchange_rate: meter::currency_rate(&currency, "CNY", fx),
                source: src.url.clone(),
                ..meter::RawPrice::default()
            },
            1 => meter::RawPrice {
                // The flat scheme is expressed as a direct per-million rate,
                // which `manual` carries without any ratio arithmetic.
                scheme: "manual".to_string(),
                input_per_m: row.model_price,
                completion_ratio: row.completion_ratio,
                currency: currency.clone(),
                exchange_rate: meter::currency_rate(&currency, "CNY", fx),
                source: src.url.clone(),
                ..meter::RawPrice::default()
            },
            _ => continue,
        };
        let Ok(mut price) = meter::normalize(raw) else {
            // A price the normalizer rejects is treated like an unknown quota
            // type: skipped, because a wrong price would corrupt routing.
            continue;
        };
        price.updated_at = now;
        out.push(config::PriceEntry {
            channel: src.channel.clone(),
            model: row.model_name,
            price,
        });
    }
    out.sort_by(|a, b| a.model.cmp(&b.model));
    Ok(out)
}
