//! `customsctl prices ...`: price comparison and raw-price normalisation.
//!
//! Both commands are offline and deterministic given the configuration, which
//! makes them the operator's way to answer "what does a request cost here" and
//! "what does this upstream price mean in our units".

use std::cmp::Ordering;
use std::error::Error;
use std::path::Path;

use chrono::{DateTime, FixedOffset, Local, Offset, Utc};
use clap::{Args, Subcommand};
use customapi_core::{config, meter};

use crate::commands::emit;
use crate::ui::TabWriter;
use crate::util::go_json_pretty;

/// `customsctl prices <command>`.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Compare the catalogue's prices for one model.
    Compare(CompareArgs),
    /// Convert a raw upstream price into the catalogue shape.
    Normalize(NormalizeArgs),
}

/// Flags accepted by `prices compare`.
#[derive(Args, Debug, Clone)]
pub struct CompareArgs {
    /// configuration file
    #[arg(long, short = 'c', value_name = "FILE")]
    pub config: Option<String>,
    /// canonical model name
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,
    /// input tokens
    #[arg(long, value_name = "N", default_value_t = 1000)]
    pub input: i64,
    /// output tokens
    #[arg(long, value_name = "N", default_value_t = 500)]
    pub output: i64,
    /// RFC3339 instant for time-dependent promotions
    #[arg(long, value_name = "TIME")]
    pub at: Option<String>,
}

/// Flags accepted by `prices normalize`.
#[derive(Args, Debug, Clone)]
pub struct NormalizeArgs {
    /// multiplier|flat|per_request|tiered|manual
    #[arg(long, value_name = "SCHEME", default_value = "multiplier")]
    pub scheme: String,
    /// base price per million (multiplier scheme)
    #[arg(long, value_name = "PRICE", default_value_t = 2.0)]
    pub base: f64,
    /// model multiplier
    #[arg(long = "model-ratio", value_name = "RATIO", default_value_t = 1.0)]
    pub model_ratio: f64,
    /// group multiplier
    #[arg(long = "group-ratio", value_name = "RATIO", default_value_t = 1.0)]
    pub group_ratio: f64,
    /// output/input ratio
    #[arg(long = "completion-ratio", value_name = "RATIO", default_value_t = 1.0)]
    pub completion_ratio: f64,
    /// flat input price per million
    #[arg(long = "input-per-m", value_name = "PRICE", default_value_t = 0.0)]
    pub input_per_m: f64,
    /// price per request
    #[arg(long = "per-request", value_name = "PRICE", default_value_t = 0.0)]
    pub per_request: f64,
    /// typical input tokens (per_request scheme)
    #[arg(long = "typical-input", value_name = "N", default_value_t = 1000)]
    pub typical_input: i64,
    /// typical output tokens (per_request scheme)
    #[arg(long = "typical-output", value_name = "N", default_value_t = 500)]
    pub typical_output: i64,
    /// USD/CNY exchange rate
    #[arg(long = "usd-cny", value_name = "RATE", default_value_t = 7.1)]
    pub usd_cny: f64,
}

/// One rendered row of the comparison table.
struct Row {
    channel: String,
    trust: String,
    input: f64,
    output: f64,
    cost: f64,
    list: f64,
    discount: f64,
    currency: String,
    promos: Vec<String>,
}

/// The timezone promotions are evaluated in: Go passes `time.Local`.
fn local_offset() -> FixedOffset {
    Local::now().offset().fix()
}

/// Parses `--at`, or returns "now".
fn instant(at: Option<&str>) -> Result<DateTime<Utc>, Box<dyn Error>> {
    match at.filter(|s| !s.is_empty()) {
        None => Ok(Utc::now()),
        Some(text) => Ok(DateTime::parse_from_rfc3339(text)
            .map_err(|e| format!("invalid --at: {e}"))?
            .with_timezone(&Utc)),
    }
}

/// Runs `prices compare`.
pub fn compare(args: &CompareArgs) -> Result<(), Box<dyn Error>> {
    let path = args
        .config
        .clone()
        .filter(|p| !p.is_empty())
        .ok_or("--config is required")?;
    let cfg = config::load(Path::new(&path))?;
    let at = instant(args.at.as_deref())?;
    let offset = local_offset();
    let tokens = meter::Tokens::new(args.input, args.output);
    let model = args.model.clone().unwrap_or_default();

    let mut rows: Vec<Row> = Vec::new();
    for entry in &cfg.prices {
        if !model.is_empty() && entry.model != model {
            continue;
        }
        let trust = cfg
            .channel_by_name(&entry.channel)
            .map(|ch| ch.trust_level().to_string())
            // A price row can name a channel that no longer exists; Go's zero
            // Channel resolves to the untrusted level.
            .unwrap_or_else(|| config::trust_level("").to_string());
        let quote = meter::price_with(&entry.price, tokens, at, Some(offset));
        let promos = quote
            .applied
            .iter()
            .map(|applied| format!("{}×{:.3}", applied.kind, applied.multiplier))
            .collect();
        rows.push(Row {
            channel: entry.channel.clone(),
            trust,
            input: entry.price.input_per_m,
            output: entry.price.output_per_m,
            cost: quote.cost,
            list: quote.list_cost,
            discount: quote.discount_pct,
            currency: quote.currency,
            promos,
        });
    }
    // Cheapest first. Go uses an unstable sort; a stable one keeps equal-cost
    // rows in configuration order, which is reproducible.
    rows.sort_by(|a, b| a.cost.partial_cmp(&b.cost).unwrap_or(Ordering::Equal));

    let mut table = TabWriter::new();
    table.line(&format!(
        "MODEL\t{}\t({} in / {} out tokens)",
        model, args.input, args.output
    ));
    table.line("CHANNEL\tTRUST\tIN/M\tOUT/M\tCUR\tLIST\tCOST\tDISC%\tPROMOTIONS");
    for row in &rows {
        table.line(&format!(
            "{}\t{}\t{:.4}\t{:.4}\t{}\t{:.6}\t{:.6}\t{:.2}\t{}",
            row.channel,
            row.trust,
            row.input,
            row.output,
            row.currency,
            row.list,
            row.cost,
            row.discount,
            row.promos.join(",")
        ));
    }
    emit(&table.finish());
    Ok(())
}

/// Runs `prices normalize`.
pub fn normalize(args: &NormalizeArgs) -> Result<(), Box<dyn Error>> {
    let mut price = meter::normalize(meter::RawPrice {
        scheme: args.scheme.clone(),
        input_per_m: args.input_per_m,
        completion_ratio: args.completion_ratio,
        model_ratio: args.model_ratio,
        base_price_per_m: args.base,
        group_ratio: args.group_ratio,
        per_request_price: args.per_request,
        currency: "USD".to_string(),
        typical_input: args.typical_input,
        typical_output: args.typical_output,
        tiers: Vec::new(),
        exchange_rate: args.usd_cny,
        source: String::new(),
    })?;
    // Go's `meter.Normalize` never touches `UpdatedAt`, so it stays at the zero
    // time and the JSON says `0001-01-01T00:00:00Z`. `PriceModel::default()`
    // leaves the Unix epoch instead, which would print `1970-01-01T00:00:00Z`;
    // normalisation never stamps the field, so pinning it to Go's zero value is
    // exact rather than a guess.
    price.updated_at = config::zero_time();
    println!("{}", go_json_pretty(&price));
    let tokens = meter::Tokens::new(args.typical_input, args.typical_output);
    println!(
        "\neffective unit price: {:.6} {} per 1M tokens (for the given mix)",
        meter::effective_unit_price(&price, tokens, Utc::now(), Some(local_offset())),
        price.currency
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a row with only the fields the ordering test cares about.
    fn row(channel: &str, cost: f64) -> Row {
        Row {
            channel: channel.to_string(),
            trust: "trusted".to_string(),
            input: 0.0,
            output: 0.0,
            cost,
            list: 0.0,
            discount: 0.0,
            currency: "CNY".to_string(),
            promos: Vec::new(),
        }
    }

    #[test]
    fn rows_sort_by_cost_ascending() {
        let mut rows = [row("b", 0.025), row("a", 0.02)];
        rows.sort_by(|a, b| a.cost.partial_cmp(&b.cost).unwrap_or(Ordering::Equal));
        assert_eq!(rows[0].channel, "a");
        assert_eq!(rows[1].channel, "b");
    }

    #[test]
    fn equal_costs_keep_the_configuration_order() {
        // Go's `sort.Slice` is unstable; a stable sort is the reproducible
        // choice for two rows at the same price.
        let mut rows = [row("first", 0.01), row("second", 0.01)];
        rows.sort_by(|a, b| a.cost.partial_cmp(&b.cost).unwrap_or(Ordering::Equal));
        assert_eq!(rows[0].channel, "first");
    }

    #[test]
    fn promotion_labels_use_three_decimals() {
        let label = format!("{}×{:.3}", "volume", 0.9);
        assert_eq!(label, "volume×0.900");
    }

    #[test]
    fn invalid_at_is_rejected_with_the_go_prefix() {
        let err = instant(Some("not-a-time")).unwrap_err().to_string();
        assert!(err.starts_with("invalid --at: "), "{err}");
    }

    #[test]
    fn an_unstamped_price_prints_the_go_zero_time() {
        let price = meter::normalize(meter::RawPrice {
            scheme: "multiplier".to_string(),
            base_price_per_m: 2.0,
            model_ratio: 1.0,
            group_ratio: 1.0,
            completion_ratio: 1.0,
            currency: "USD".to_string(),
            exchange_rate: 7.1,
            ..Default::default()
        })
        .unwrap();
        let mut price = price;
        price.updated_at = config::zero_time();
        let json = go_json_pretty(&price);
        assert!(
            json.contains(r#""updated_at": "0001-01-01T00:00:00Z""#),
            "{json}"
        );
        assert!(json.contains(r#""input_per_m": 14.2"#), "{json}");
        assert!(json.contains(r#""completion_ratio": 1"#), "{json}");
    }

    #[test]
    fn a_missing_channel_renders_as_untrusted() {
        assert_eq!(config::trust_level(""), "untrusted");
    }
}
