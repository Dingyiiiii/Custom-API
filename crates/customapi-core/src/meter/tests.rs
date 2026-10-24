//! Tests translated from `internal/meter/price_test.go`.

use super::*;
use crate::config;
use crate::config::{PriceEntry, PriceModel, Promotion};
use chrono::{DateTime, FixedOffset, TimeZone, Utc};

fn noon() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap()
}

fn night() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 1, 2, 0, 0).unwrap()
}

fn almost(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

fn price_model(input_per_m: f64, output_per_m: f64, currency: &str) -> PriceModel {
    PriceModel {
        input_per_m,
        output_per_m,
        currency: currency.to_string(),
        ..PriceModel::default()
    }
}

fn utc() -> Option<FixedOffset> {
    Some(utc_offset())
}

// ---------------------------------------------------------------------------
// Basic pricing
// ---------------------------------------------------------------------------

#[test]
fn test_price_with_flat_rates() {
    let pm = price_model(10.0, 30.0, "CNY");
    let q = price_with(&pm, Tokens::new(1_000_000, 1_000_000), noon(), utc());
    assert!(
        almost(q.input_cost, 10.0) && almost(q.output_cost, 30.0),
        "component costs wrong: {q:?}"
    );
    assert!(almost(q.cost, 40.0), "total = {}, want 40", q.cost);
    assert!(
        almost(q.discount_pct, 0.0),
        "expected no discount, got {}",
        q.discount_pct
    );
}

#[test]
fn test_price_with_partial_million() {
    let pm = price_model(100.0, 100.0, "");
    let q = price_with(&pm, Tokens::new(1000, 0), noon(), utc());
    assert!(almost(q.cost, 0.1), "cost = {}, want 0.1", q.cost);
}

#[test]
fn test_price_zero_tokens() {
    let pm = price_model(10.0, 30.0, "");
    let q = price_with(&pm, Tokens::default(), noon(), utc());
    assert!(
        q.cost == 0.0 && q.list_cost == 0.0,
        "zero tokens should cost nothing: {q:?}"
    );
}

// ---------------------------------------------------------------------------
// Promotions
// ---------------------------------------------------------------------------

#[test]
fn test_off_peak_promotion() {
    let pm = PriceModel {
        promotions: vec![Promotion {
            kind: config::PROMO_OFF_PEAK.to_string(),
            multiplier: 0.5,
            start_minute: 0,
            end_minute: 8 * 60,
            note: "overnight half price".to_string(),
            ..Promotion::default()
        }],
        ..price_model(100.0, 100.0, "CNY")
    };
    let tokens = Tokens::new(1_000_000, 0);

    let night_q = price_with(&pm, tokens, night(), utc());
    assert!(
        almost(night_q.cost, 50.0),
        "off-peak cost = {}, want 50",
        night_q.cost
    );
    assert!(
        almost(night_q.list_cost, 100.0),
        "list cost = {}, want 100",
        night_q.list_cost
    );
    assert!(
        almost(night_q.discount_pct, 50.0),
        "discount = {}, want 50",
        night_q.discount_pct
    );
    assert!(
        night_q.applied.len() == 1 && night_q.applied[0].kind == config::PROMO_OFF_PEAK,
        "promotion not recorded: {:?}",
        night_q.applied
    );

    let noon_q = price_with(&pm, tokens, noon(), utc());
    assert!(
        almost(noon_q.cost, 100.0),
        "peak cost = {}, want 100",
        noon_q.cost
    );
    assert!(
        noon_q.applied.is_empty(),
        "promotion should not apply at noon: {:?}",
        noon_q.applied
    );
}

/// The window shape that a naive comparison gets wrong.
#[test]
fn test_off_peak_window_wrapping_midnight() {
    let pm = PriceModel {
        promotions: vec![Promotion {
            kind: config::PROMO_OFF_PEAK.to_string(),
            multiplier: 0.5,
            start_minute: 22 * 60,
            end_minute: 6 * 60, // 22:00 -> 06:00
            ..Promotion::default()
        }],
        ..price_model(100.0, 0.0, "")
    };
    let tokens = Tokens::new(1_000_000, 0);

    let in_window = [
        Utc.with_ymd_and_hms(2026, 3, 1, 23, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2026, 3, 1, 2, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2026, 3, 1, 5, 59, 0).unwrap(),
    ];
    let out_window = [
        Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2026, 3, 1, 6, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2026, 3, 1, 21, 59, 0).unwrap(),
    ];
    for at in in_window {
        let q = price_with(&pm, tokens, at, utc());
        assert!(
            almost(q.cost, 50.0),
            "expected the wrapped window to apply at {at}, got {}",
            q.cost
        );
    }
    for at in out_window {
        let q = price_with(&pm, tokens, at, utc());
        assert!(
            almost(q.cost, 100.0),
            "expected the wrapped window not to apply at {at}, got {}",
            q.cost
        );
    }
}

#[test]
fn test_volume_promotion() {
    let pm = PriceModel {
        promotions: vec![Promotion {
            kind: config::PROMO_VOLUME.to_string(),
            multiplier: 0.8,
            min_tokens: 1_000_000,
            ..Promotion::default()
        }],
        ..price_model(100.0, 0.0, "")
    };
    let small = price_with(&pm, Tokens::new(100_000, 0), noon(), utc());
    assert!(
        almost(small.cost, 10.0),
        "below the threshold should not discount: {}",
        small.cost
    );
    let big = price_with(&pm, Tokens::new(2_000_000, 0), noon(), utc());
    assert!(
        almost(big.cost, 160.0),
        "above the threshold should discount 20%: {}",
        big.cost
    );
}

/// Verifies the break-even modelling: a monthly fee buying an allowance is
/// converted into an effective multiplier.
#[test]
fn test_subscription_promotion() {
    // List price: 100 CNY per million. Plan: 50 CNY for 2M tokens, so the
    // plan's effective rate is 25 per million == a 0.25 multiplier against the
    // list price. A 1M-token request therefore costs 25. This is the
    // subscription break-even the comparison view needs: the same request costs
    // 100 on pay-as-you-go and 25 inside the plan.
    let pm = PriceModel {
        promotions: vec![Promotion {
            kind: config::PROMO_SUBSCRIBE.to_string(),
            monthly_fee: 50.0,
            included_tokens: 2_000_000,
            note: "monthly plan".to_string(),
            ..Promotion::default()
        }],
        ..price_model(100.0, 0.0, "")
    };
    let q = price_with(&pm, Tokens::new(1_000_000, 0), noon(), utc());
    assert!(almost(q.list_cost, 100.0), "list cost = {}", q.list_cost);
    assert!(
        almost(q.cost, 25.0),
        "subscription cost = {}, want 25",
        q.cost
    );
    assert!(
        q.applied.len() == 1 && q.applied[0].kind == config::PROMO_SUBSCRIBE,
        "subscription promotion not recorded: {:?}",
        q.applied
    );
    assert!(
        almost(q.applied[0].multiplier, 0.25),
        "effective multiplier = {}, want 0.25",
        q.applied[0].multiplier
    );
}

#[test]
fn test_stacked_promotions() {
    let pm = PriceModel {
        promotions: vec![
            Promotion {
                kind: config::PROMO_OFF_PEAK.to_string(),
                multiplier: 0.5,
                start_minute: 0,
                end_minute: 8 * 60,
                ..Promotion::default()
            },
            Promotion {
                kind: config::PROMO_VOLUME.to_string(),
                multiplier: 0.9,
                min_tokens: 1000,
                ..Promotion::default()
            },
        ],
        ..price_model(100.0, 0.0, "")
    };
    let q = price_with(&pm, Tokens::new(1_000_000, 0), night(), utc());
    assert!(
        almost(q.cost, 45.0),
        "stacked promotions = {}, want 45",
        q.cost
    );
    assert!(
        q.applied.len() == 2,
        "expected both promotions recorded: {:?}",
        q.applied
    );
}

/// The comparison metric collapses a rate pair into one number that orders
/// channels correctly.
#[test]
fn test_effective_unit_price_is_sortable() {
    let cheap = price_model(1.0, 2.0, "");
    let dear = price_model(10.0, 20.0, "");
    let tokens = Tokens::new(1_000_000, 1_000_000);
    let uc = effective_unit_price(&cheap, tokens, noon(), utc());
    let ud = effective_unit_price(&dear, tokens, noon(), utc());
    assert!(
        uc < ud,
        "effective unit price did not order channels: {uc} vs {ud}"
    );
    assert!(almost(uc, 1.5), "blended unit price = {uc}, want 1.5");
}

#[test]
fn test_effective_unit_price_default_mix() {
    let pm = price_model(10.0, 20.0, "");
    // With no token information the metric must still be meaningful (a 1:1 mix).
    let got = effective_unit_price(&pm, Tokens::default(), noon(), utc());
    assert!(almost(got, 15.0), "default mix unit price = {got}, want 15");
}

// ---------------------------------------------------------------------------
// Catalogue
// ---------------------------------------------------------------------------

#[test]
fn test_pricer_lookup_and_set() {
    let p = Pricer::new(
        vec![
            PriceEntry {
                channel: "a".to_string(),
                model: "gpt-4o".to_string(),
                price: price_model(10.0, 30.0, "CNY"),
            },
            PriceEntry {
                channel: "b".to_string(),
                model: "gpt-4o".to_string(),
                price: price_model(5.0, 15.0, ""),
            },
        ],
        utc(),
    );

    let pm = p.lookup("a", "gpt-4o");
    assert!(
        pm.is_some() && pm.as_ref().unwrap().input_per_m == 10.0,
        "{pm:?}"
    );
    assert!(
        p.lookup("a", "unknown").is_none(),
        "expected a miss for an unknown model"
    );
    // An omitted currency defaults rather than being blank.
    let pm = p.lookup("b", "gpt-4o").unwrap();
    assert_eq!(pm.currency, "CNY", "expected the default currency");

    p.set("c", "gpt-4o", price_model(1.0, 0.0, ""));
    let pm = p.lookup("c", "gpt-4o");
    assert!(
        pm.is_some() && pm.as_ref().unwrap().input_per_m == 1.0,
        "set did not take effect"
    );

    let models = p.models("a");
    assert_eq!(models, vec!["gpt-4o".to_string()], "models(a) = {models:?}");
}

#[test]
fn test_pricer_quote_unknown_model() {
    let p = Pricer::new(Vec::new(), utc());
    let q = p.quote("a", "nope", Tokens::new(100, 0), noon());
    assert_eq!(q.basis, "unknown");
    assert!(
        q.cost == 0.0,
        "an unpriced model should cost 0, got {}",
        q.cost
    );
}

// ---------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------

/// Reproduces the api-price-monitor conversion:
/// price = base * modelRatio * groupRatio, output = input * completionRatio.
#[test]
fn test_normalize_multiplier_scheme() {
    let pm = normalize(RawPrice {
        scheme: "multiplier".to_string(),
        base_price_per_m: 2.0,
        model_ratio: 1.5,
        group_ratio: 0.8,
        completion_ratio: 3.0,
        currency: "USD".to_string(),
        exchange_rate: 7.0,
        ..RawPrice::default()
    })
    .expect("normalize");

    let want_in = 2.0 * 1.5 * 0.8 * 7.0; // 16.8
    assert!(
        almost(pm.input_per_m, want_in),
        "input = {}, want {want_in}",
        pm.input_per_m
    );
    assert!(
        almost(pm.output_per_m, want_in * 3.0),
        "output = {}, want {}",
        pm.output_per_m,
        want_in * 3.0
    );
    assert!(
        almost(pm.completion_ratio, 3.0),
        "completion ratio not preserved: {}",
        pm.completion_ratio
    );
}

#[test]
fn test_normalize_defaults_to_one() {
    let pm = normalize(RawPrice {
        scheme: "multiplier".to_string(),
        base_price_per_m: 2.0,
        currency: "USD".to_string(),
        ..RawPrice::default()
    })
    .expect("normalize");
    assert!(
        almost(pm.input_per_m, 2.0) && almost(pm.output_per_m, 2.0),
        "expected ratio 1 defaults, got {pm:?}"
    );
}

#[test]
fn test_normalize_flat_scheme() {
    let pm = normalize(RawPrice {
        scheme: "flat".to_string(),
        input_per_m: 10.0,
        completion_ratio: 2.0,
        ..RawPrice::default()
    })
    .expect("normalize");
    assert!(
        almost(pm.input_per_m, 10.0) && almost(pm.output_per_m, 20.0),
        "flat scheme wrong: {pm:?}"
    );
}

/// A per-request charge becomes an equivalent per-million rate for the stated
/// typical mix.
#[test]
fn test_normalize_per_request() {
    let pm = normalize(RawPrice {
        scheme: "per_request".to_string(),
        per_request_price: 1.0,
        typical_input: 10_000,
        typical_output: 10_000,
        ..RawPrice::default()
    })
    .expect("normalize");
    // 1.0 per request over 20k tokens == 50 per million.
    assert!(
        almost(pm.input_per_m, 50.0),
        "per-request conversion = {}, want 50",
        pm.input_per_m
    );
    assert!(
        almost(pm.output_per_m, 50.0),
        "output = {}",
        pm.output_per_m
    );
    assert!(almost(pm.completion_ratio, 1.0));
}

#[test]
fn test_normalize_per_request_needs_mix() {
    let err = normalize(RawPrice {
        scheme: "per_request".to_string(),
        per_request_price: 1.0,
        ..RawPrice::default()
    })
    .unwrap_err();
    assert_eq!(err, MeterError::PerRequestNeedsTypicalMix);
    assert_eq!(
        err.to_string(),
        "meter: per_request scheme needs a typical token mix"
    );
}

/// The volume-weighted collapse of a tier table.
#[test]
fn test_normalize_tiered() {
    let pm = normalize(RawPrice {
        scheme: "tiered".to_string(),
        tiers: vec![
            Tier {
                up_to_tokens: 1_000_000,
                input_per_m: 10.0,
                ..Tier::default()
            },
            Tier {
                up_to_tokens: 0,
                input_per_m: 5.0,
                ..Tier::default()
            }, // and above
        ],
        typical_input: 2_000_000,
        ..RawPrice::default()
    })
    .expect("normalize");
    // Half at 10, half at 5 -> 7.5.
    assert!(
        almost(pm.input_per_m, 7.5),
        "tiered average = {}, want 7.5",
        pm.input_per_m
    );
}

#[test]
fn test_normalize_tiered_needs_tiers() {
    let err = normalize(RawPrice {
        scheme: "tiered".to_string(),
        ..RawPrice::default()
    })
    .unwrap_err();
    assert_eq!(err, MeterError::TieredNeedsTiers);
}

#[test]
fn test_normalize_unknown_scheme() {
    let err = normalize(RawPrice {
        scheme: "magic".to_string(),
        ..RawPrice::default()
    })
    .unwrap_err();
    assert_eq!(err, MeterError::UnknownScheme("magic".to_string()));
    assert_eq!(err.to_string(), "meter: unknown price scheme \"magic\"");
}

#[test]
fn test_normalize_applies_exchange_rate() {
    let usd = normalize(RawPrice {
        scheme: "flat".to_string(),
        input_per_m: 1.0,
        currency: "USD".to_string(),
        exchange_rate: 7.2,
        ..RawPrice::default()
    })
    .unwrap();
    let cny = normalize(RawPrice {
        scheme: "flat".to_string(),
        input_per_m: 1.0,
        currency: "CNY".to_string(),
        ..RawPrice::default()
    })
    .unwrap();
    assert!(
        almost(usd.input_per_m, 7.2),
        "exchange rate not applied: {}",
        usd.input_per_m
    );
    assert!(
        almost(cny.input_per_m, 1.0),
        "rate 0 should mean no conversion, got {}",
        cny.input_per_m
    );
}

#[test]
fn test_currency_rate() {
    assert!(almost(currency_rate("USD", "CNY", 7.1), 7.1));
    assert!(almost(currency_rate("CNY", "USD", 7.1), 1.0 / 7.1));
    assert!(almost(currency_rate("CNY", "CNY", 7.1), 1.0));
    // A missing rate falls back to a sensible default instead of dividing by 0.
    assert!(almost(currency_rate("USD", "CNY", 0.0), 7.1));
}

/// End-to-end sanity check that a normalized price produces the expected cash
/// amount.
#[test]
fn test_normalize_then_price() {
    let pm = normalize(RawPrice {
        scheme: "multiplier".to_string(),
        base_price_per_m: 2.0,
        model_ratio: 1.0,
        group_ratio: 1.0,
        completion_ratio: 2.0,
        currency: "USD".to_string(),
        exchange_rate: 7.0,
        ..RawPrice::default()
    })
    .expect("normalize");
    // 14 CNY/M in, 28 CNY/M out.
    let q = price_with(&pm, Tokens::new(1_000_000, 1_000_000), noon(), utc());
    assert!(almost(q.cost, 42.0), "cost = {}, want 42", q.cost);
}

// ---------------------------------------------------------------------------
// Rust-side boundary tests
// ---------------------------------------------------------------------------

#[test]
fn unknown_promotion_kind_is_ignored() {
    let pm = PriceModel {
        promotions: vec![Promotion {
            kind: "lottery".to_string(),
            multiplier: 0.1,
            ..Promotion::default()
        }],
        ..price_model(100.0, 0.0, "")
    };
    let q = price_with(&pm, Tokens::new(1_000_000, 0), noon(), utc());
    assert!(almost(q.cost, 100.0), "cost = {}", q.cost);
    assert!(q.applied.is_empty(), "applied = {:?}", q.applied);
}

#[test]
fn full_day_window_applies_always() {
    let pm = PriceModel {
        promotions: vec![Promotion {
            kind: config::PROMO_OFF_PEAK.to_string(),
            multiplier: 0.5,
            start_minute: 90,
            end_minute: 90,
            ..Promotion::default()
        }],
        ..price_model(100.0, 0.0, "")
    };
    for at in [noon(), night()] {
        let q = price_with(&pm, Tokens::new(1_000_000, 0), at, utc());
        assert!(
            almost(q.cost, 50.0),
            "a zero-width window is a full day: {}",
            q.cost
        );
    }
}

#[test]
fn subscription_without_allowance_does_not_apply() {
    let pm = PriceModel {
        promotions: vec![Promotion {
            kind: config::PROMO_SUBSCRIBE.to_string(),
            monthly_fee: 50.0,
            included_tokens: 0,
            ..Promotion::default()
        }],
        ..price_model(100.0, 0.0, "")
    };
    let q = price_with(&pm, Tokens::new(1_000_000, 0), noon(), utc());
    assert!(almost(q.cost, 100.0), "cost = {}", q.cost);
    assert!(q.applied.is_empty());
}

#[test]
fn volume_threshold_of_zero_does_not_apply() {
    let pm = PriceModel {
        promotions: vec![Promotion {
            kind: config::PROMO_VOLUME.to_string(),
            multiplier: 0.1,
            min_tokens: 0,
            ..Promotion::default()
        }],
        ..price_model(100.0, 0.0, "")
    };
    let q = price_with(&pm, Tokens::new(1_000_000, 0), noon(), utc());
    assert!(almost(q.cost, 100.0), "cost = {}", q.cost);
}

#[test]
fn per_request_rejects_a_zero_sum_mix() {
    let err = normalize(RawPrice {
        scheme: "per_request".to_string(),
        per_request_price: 1.0,
        typical_input: 5,
        typical_output: -5,
        ..RawPrice::default()
    })
    .unwrap_err();
    assert_eq!(err, MeterError::PerRequestZeroTypicalMix);
    assert_eq!(
        err.to_string(),
        "meter: per_request scheme needs a non-zero token mix"
    );
}

#[test]
fn tiered_without_a_typical_mix_uses_one_million() {
    let pm = normalize(RawPrice {
        scheme: "tiered".to_string(),
        tiers: vec![Tier {
            up_to_tokens: 0,
            input_per_m: 3.0,
            ..Tier::default()
        }],
        ..RawPrice::default()
    })
    .expect("normalize");
    assert!(almost(pm.input_per_m, 3.0), "input = {}", pm.input_per_m);
}

#[test]
fn tiered_rows_past_the_mix_are_collapsed_to_one_row() {
    let pm = normalize(RawPrice {
        scheme: "tiered".to_string(),
        tiers: vec![
            Tier {
                up_to_tokens: 10_000_000,
                input_per_m: 10.0,
                ..Tier::default()
            },
            Tier {
                up_to_tokens: 1_000_000,
                input_per_m: 1.0,
                ..Tier::default()
            },
        ],
        typical_input: 1_000_000,
        ..RawPrice::default()
    })
    .expect("normalize");
    // The first tier swallows the whole mix, so the second one never weighs in.
    assert!(almost(pm.input_per_m, 10.0), "input = {}", pm.input_per_m);
}

#[test]
fn tokens_total_wraps_like_go() {
    let tokens = Tokens::new(i64::MAX, 1);
    assert_eq!(tokens.total(), i64::MIN);
}

#[test]
fn quote_json_matches_go_field_names() {
    let pm = PriceModel {
        promotions: vec![Promotion {
            kind: config::PROMO_OFF_PEAK.to_string(),
            multiplier: 0.5,
            start_minute: 0,
            end_minute: 24 * 60,
            note: "night".to_string(),
            ..Promotion::default()
        }],
        ..price_model(100.0, 0.0, "CNY")
    };
    let q = price_with(&pm, Tokens::new(1_000_000, 0), noon(), utc());
    let json = serde_json::to_string(&q).unwrap();
    for key in [
        "\"list_cost\":",
        "\"cost\":",
        "\"currency\":\"CNY\"",
        "\"discount_pct\":",
        "\"input_cost\":",
        "\"output_cost\":",
        "\"applied\":[{\"kind\":\"off_peak\",\"multiplier\":0.5,\"note\":\"night\"}]",
        "\"basis\":\"catalogue\"",
    ] {
        assert!(json.contains(key), "missing {key} in {json}");
    }

    // An empty `applied` is omitted, and an empty `note` disappears with it.
    let plain = price_with(&price_model(1.0, 2.0, ""), Tokens::default(), noon(), utc());
    let json = serde_json::to_string(&plain).unwrap();
    assert!(!json.contains("applied"), "{json}");
}

#[test]
fn timestamps_are_evaluated_in_the_pricer_timezone() {
    // 2026-03-01T18:00Z is 02:00 the next day in UTC+8, so an off-peak window
    // of 00:00-08:00 local time fires there; 02:00Z is 10:00 local and must not.
    let pm = PriceModel {
        promotions: vec![Promotion {
            kind: config::PROMO_OFF_PEAK.to_string(),
            multiplier: 0.5,
            start_minute: 0,
            end_minute: 8 * 60,
            ..Promotion::default()
        }],
        ..price_model(100.0, 0.0, "")
    };
    let p = Pricer::new(
        vec![PriceEntry {
            channel: "c".to_string(),
            model: "m".to_string(),
            price: pm,
        }],
        Some(FixedOffset::east_opt(8 * 3600).unwrap()),
    );
    let at_two_am_local = Utc.with_ymd_and_hms(2026, 3, 1, 18, 0, 0).unwrap();
    let at_ten_am_local = Utc.with_ymd_and_hms(2026, 3, 1, 2, 0, 0).unwrap();
    assert!(almost(
        p.quote("c", "m", Tokens::new(1_000_000, 0), at_two_am_local)
            .cost,
        50.0
    ));
    assert!(almost(
        p.quote("c", "m", Tokens::new(1_000_000, 0), at_ten_am_local)
            .cost,
        100.0
    ));
}

#[test]
fn pricer_models_lists_only_its_channel() {
    let p = Pricer::new(
        vec![
            PriceEntry {
                channel: "a".to_string(),
                model: "x".to_string(),
                price: price_model(1.0, 1.0, ""),
            },
            PriceEntry {
                channel: "ab".to_string(),
                model: "y".to_string(),
                price: price_model(1.0, 1.0, ""),
            },
        ],
        None,
    );
    assert_eq!(p.models("a"), vec!["x".to_string()]);
    assert_eq!(p.models("ab"), vec!["y".to_string()]);
    assert!(p.models("missing").is_empty());
    // A channel name that merely prefixes another must not leak that channel's
    // models: the separator decides, not the prefix.
    assert!(p.models("a").iter().all(|m| m != "y"));
}
