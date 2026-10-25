//! Tests translated from `internal/route/route_test.go`.

use super::*;
use crate::config::{Channel, PriceEntry, PriceEntry as Entry, PriceModel};
use crate::meter::{Pricer, Tokens};
use chrono::{TimeDelta, TimeZone, Utc};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

/// Locks a test mutex, ignoring poisoning (a failed assertion in another test
/// must not cascade).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn yes() -> Option<bool> {
    Some(true)
}

fn no() -> Option<bool> {
    Some(false)
}

fn channel(name: &str, base_url: &str, model: &str) -> Channel {
    Channel {
        name: name.to_string(),
        base_url: base_url.to_string(),
        protocol: "openai".to_string(),
        weight: 1.0,
        enabled: yes(),
        models: HashMap::from([(model.to_string(), model.to_string())]),
        ..Channel::default()
    }
}

fn chans() -> Vec<Channel> {
    vec![
        channel("cheap-degraded", "http://a", "gpt-4o"),
        channel("expensive-genuine", "http://b", "gpt-4o"),
        Channel {
            enabled: no(),
            ..channel("disabled", "http://c", "gpt-4o")
        },
    ]
}

fn prices() -> Vec<PriceEntry> {
    vec![
        Entry {
            channel: "cheap-degraded".to_string(),
            model: "gpt-4o".to_string(),
            price: PriceModel {
                input_per_m: 1.0,
                output_per_m: 2.0,
                currency: "CNY".to_string(),
                ..PriceModel::default()
            },
        },
        Entry {
            channel: "expensive-genuine".to_string(),
            model: "gpt-4o".to_string(),
            price: PriceModel {
                input_per_m: 10.0,
                output_per_m: 20.0,
                currency: "CNY".to_string(),
                ..PriceModel::default()
            },
        },
    ]
}

fn score_book() -> Arc<MapScoreBook> {
    Arc::new(book([
        (
            "cheap-degraded",
            "gpt-4o",
            // Cheap, but measured as a substituted/degraded model.
            Scores {
                capability: 0.35,
                fingerprint_match: 0.2,
                reliability: 0.9,
                usage_integrity: 1.0,
                ..Scores::default()
            },
        ),
        (
            "expensive-genuine",
            "gpt-4o",
            Scores {
                capability: 0.95,
                fingerprint_match: 1.0,
                reliability: 0.99,
                usage_integrity: 1.0,
                ..Scores::default()
            },
        ),
    ]))
}

/// Builds a score book from (channel, model, score) triples.
fn book<const N: usize>(entries: [(&str, &str, Scores); N]) -> MapScoreBook {
    let mut by_channel: HashMap<String, HashMap<String, Scores>> = HashMap::new();
    for (channel, model, score) in entries {
        by_channel
            .entry(channel.to_string())
            .or_default()
            .insert(model.to_string(), score);
    }
    MapScoreBook::new(by_channel)
}

fn new_selector(strategy: &str, scores: Option<Arc<dyn ScoreBook>>) -> Selector {
    Selector::new(Options {
        pricer: Some(Arc::new(Pricer::new(prices(), Some(meter_utc())))),
        scores,
        strategy: strategy.to_string(),
        seed: 42,
    })
}

fn meter_utc() -> chrono::FixedOffset {
    crate::meter::utc_offset()
}

fn request(model: &str) -> Request {
    Request {
        model: model.to_string(),
        ..Request::default()
    }
}

fn tokens(input: i64) -> Tokens {
    Tokens::new(input, 0)
}

fn names(cands: &[Candidate]) -> Vec<String> {
    cands.iter().map(|c| c.channel.name.clone()).collect()
}

// ---------------------------------------------------------------------------
// Candidate construction
// ---------------------------------------------------------------------------

#[test]
fn test_candidates_exclude_disabled() {
    let s = new_selector("value", Some(score_book()));
    let got = s
        .candidates(&chans(), &request("gpt-4o"))
        .expect("candidates");
    for c in &got {
        assert_ne!(c.channel.name, "disabled", "a disabled channel was offered");
    }
}

#[test]
fn test_candidates_respect_exclusions() {
    let s = new_selector("value", Some(score_book()));
    let req = Request {
        exclude: HashSet::from(["cheap-degraded".to_string()]),
        ..request("gpt-4o")
    };
    let got = s.candidates(&chans(), &req).expect("candidates");
    assert_eq!(
        names(&got),
        vec!["expensive-genuine".to_string()],
        "exclusion not honoured"
    );
}

#[test]
fn test_candidates_respect_unhealthy() {
    let s = new_selector("value", Some(score_book()));
    let req = Request {
        unhealthy: HashSet::from(["cheap-degraded".to_string()]),
        ..request("gpt-4o")
    };
    let got = s.candidates(&chans(), &req).expect("candidates");
    assert_eq!(
        names(&got),
        vec!["expensive-genuine".to_string()],
        "unhealthy channel not skipped"
    );
}

#[test]
fn test_candidates_no_match() {
    let s = new_selector("value", Some(score_book()));
    let err = s
        .candidates(&chans(), &request("no-such-model"))
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "route: no channel can serve model \"no-such-model\""
    );
}

/// The canonical name is translated to the channel's own model name.
#[test]
fn test_model_aliasing() {
    let cs = vec![Channel {
        models: HashMap::from([("gpt-4o".to_string(), "openai/gpt-4o-2024-11-20".to_string())]),
        ..channel("aliased", "http://a", "gpt-4o")
    }];
    let s = new_selector("value", None);
    let got = s.candidates(&cs, &request("gpt-4o")).expect("candidates");
    assert_eq!(got[0].upstream_model, "openai/gpt-4o-2024-11-20");
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// The pure price strategy.
#[test]
fn test_cheapest_strategy() {
    let s = new_selector("cheapest", Some(score_book()));
    let d = s
        .select(
            &chans(),
            &Request {
                tokens: tokens(1000),
                ..request("gpt-4o")
            },
        )
        .expect("select");
    assert_eq!(d.chosen.channel.name, "cheap-degraded");
}

/// The pure capability strategy.
#[test]
fn test_best_score_strategy() {
    let s = new_selector("best_score", Some(score_book()));
    let d = s
        .select(
            &chans(),
            &Request {
                tokens: tokens(1000),
                ..request("gpt-4o")
            },
        )
        .expect("select");
    assert_eq!(d.chosen.channel.name, "expensive-genuine");
}

/// The central routing test: the design's whole point is that a cheap channel
/// with a bad fingerprint must not win. Here the genuine channel is 10x the price
/// but ~13x better on capability*fingerprint, so value must choose it.
#[test]
fn test_value_strategy_prefers_genuine_over_cheap() {
    let s = new_selector("value", Some(score_book()));
    let d = s
        .select(
            &chans(),
            &Request {
                tokens: Tokens::new(1_000_000, 1_000_000),
                ..request("gpt-4o")
            },
        )
        .expect("select");
    assert_eq!(
        d.chosen.channel.name, "expensive-genuine",
        "value strategy should pick the genuine channel"
    );
}

/// The metric is not simply "always pick the most expensive channel".
#[test]
fn test_value_strategy_still_prefers_cheap_when_quality_is_equal() {
    let sb: Arc<dyn ScoreBook> = Arc::new(book([
        (
            "cheap-degraded",
            "gpt-4o",
            Scores {
                capability: 0.9,
                fingerprint_match: 1.0,
                reliability: 1.0,
                usage_integrity: 1.0,
                ..Scores::default()
            },
        ),
        (
            "expensive-genuine",
            "gpt-4o",
            Scores {
                capability: 0.9,
                fingerprint_match: 1.0,
                reliability: 1.0,
                usage_integrity: 1.0,
                ..Scores::default()
            },
        ),
    ]));
    let s = new_selector("value", Some(sb));
    let d = s
        .select(
            &chans(),
            &Request {
                tokens: tokens(1_000_000),
                ..request("gpt-4o")
            },
        )
        .expect("select");
    assert_eq!(
        d.chosen.channel.name, "cheap-degraded",
        "equal quality should favour the cheaper channel"
    );
}

/// A channel caught inflating usage loses to an honest competitor even at a much
/// lower price.
#[test]
fn test_value_penalises_usage_inflation() {
    let sb: Arc<dyn ScoreBook> = Arc::new(book([
        (
            "cheap-degraded",
            "gpt-4o",
            // Very cheap and apparently capable, but measured as dishonest.
            Scores {
                capability: 1.0,
                fingerprint_match: 1.0,
                reliability: 1.0,
                usage_integrity: 0.2,
                ..Scores::default()
            },
        ),
        (
            "expensive-genuine",
            "gpt-4o",
            Scores {
                capability: 1.0,
                fingerprint_match: 1.0,
                reliability: 1.0,
                usage_integrity: 1.0,
                ..Scores::default()
            },
        ),
    ]));
    let s = new_selector("value", Some(sb));
    let d = s
        .select(
            &chans(),
            &Request {
                tokens: Tokens::new(1_000_000, 1_000_000),
                ..request("gpt-4o")
            },
        )
        .expect("select");
    assert_eq!(
        d.chosen.channel.name, "expensive-genuine",
        "a dishonest channel won routing"
    );
}

/// A channel without a price is still usable, just not preferred on cost.
#[test]
fn test_unknown_price_is_not_excluded() {
    let cs = vec![Channel {
        enabled: None,
        ..channel("unpriced", "http://a", "gpt-4o")
    }];
    let s = new_selector("value", None);
    let d = s.select(&cs, &request("gpt-4o")).expect("select");
    assert!(
        !d.chosen.has_price,
        "expected has_price=false for an unpriced channel"
    );
}

#[test]
fn test_pin_overrides_strategy() {
    let s = new_selector("cheapest", Some(score_book()));
    let d = s
        .select(
            &chans(),
            &Request {
                pin: "expensive-genuine".to_string(),
                ..request("gpt-4o")
            },
        )
        .expect("select");
    assert_eq!(d.chosen.channel.name, "expensive-genuine", "pin ignored");
    assert_eq!(d.strategy, "pinned");
}

#[test]
fn test_pin_to_unavailable_channel_fails() {
    let s = new_selector("value", Some(score_book()));
    let err = s
        .select(
            &chans(),
            &Request {
                pin: "nope".to_string(),
                ..request("gpt-4o")
            },
        )
        .unwrap_err();
    assert_eq!(err, RouteError::PinUnavailable("nope".to_string()));
    assert_eq!(
        err.to_string(),
        "route: pinned channel \"nope\" is not available"
    );
}

#[test]
fn test_fixed_strategy_preserves_config_order() {
    let s = new_selector("fixed", Some(score_book()));
    let d = s.select(&chans(), &request("gpt-4o")).expect("select");
    assert_eq!(
        d.chosen.channel.name, "cheap-degraded",
        "fixed strategy reordered channels"
    );
}

/// The full ranking is returned so the caller can retry the next candidate.
#[test]
fn test_ranked_list_supports_failover() {
    let s = new_selector("value", Some(score_book()));
    let d = s
        .select(
            &chans(),
            &Request {
                tokens: tokens(1_000_000),
                ..request("gpt-4o")
            },
        )
        .expect("select");
    assert_eq!(d.ranked.len(), 2, "expected 2 ranked candidates");
    assert_eq!(
        d.ranked[0].channel.name, d.chosen.channel.name,
        "the chosen channel must head the ranked list"
    );
}

/// A volume promotion that the current request unlocks is reflected in routing.
#[test]
fn test_request_price_uses_this_requests_mix() {
    let pm = PriceModel {
        input_per_m: 100.0,
        output_per_m: 100.0,
        promotions: vec![crate::config::Promotion {
            kind: crate::config::PROMO_VOLUME.to_string(),
            multiplier: 0.1,
            min_tokens: 1_000_000,
            ..crate::config::Promotion::default()
        }],
        ..PriceModel::default()
    };
    let pricer = Arc::new(Pricer::new(
        vec![PriceEntry {
            channel: "vol".to_string(),
            model: "m".to_string(),
            price: pm,
        }],
        Some(meter_utc()),
    ));
    let cs = vec![Channel {
        name: "vol".to_string(),
        base_url: "http://a".to_string(),
        models: HashMap::from([("m".to_string(), "m".to_string())]),
        ..Channel::default()
    }];
    let s = Selector::new(Options {
        pricer: Some(pricer),
        strategy: "cheapest".to_string(),
        ..Options::default()
    });

    let small = s
        .candidates(
            &cs,
            &Request {
                tokens: tokens(1000),
                ..request("m")
            },
        )
        .expect("candidates");
    let big = s
        .candidates(
            &cs,
            &Request {
                tokens: tokens(2_000_000),
                ..request("m")
            },
        )
        .expect("candidates");
    let small_unit = small[0].quote.cost / 0.001;
    let big_unit = big[0].quote.cost / 2.0;
    assert!(
        big_unit < small_unit,
        "volume promotion not reflected in the unit price: big={big_unit} small={small_unit}"
    );
}

// ---------------------------------------------------------------------------
// Degradation feedback
// ---------------------------------------------------------------------------

/// The feedback edge from metering back into routing.
#[test]
fn test_degrade_channel_lowers_integrity() {
    let sb = score_book();
    let s = Selector::new(Options {
        pricer: Some(Arc::new(Pricer::new(prices(), Some(meter_utc())))),
        scores: Some(sb.clone()),
        strategy: "value".to_string(),
        seed: 0,
    });

    let before = sb.score("expensive-genuine", "gpt-4o").unwrap();
    assert!(
        before.usage_integrity >= 0.99,
        "expected a clean score to start, got {}",
        before.usage_integrity
    );

    // The upstream claims 100% more than the gateway counted.
    s.degrade_channel(
        "expensive-genuine",
        &Deviation {
            worst: 1.0,
            over_threshold: true,
            direction: "over".to_string(),
        },
    );

    let after = sb.score("expensive-genuine", "gpt-4o").unwrap();
    assert!(
        after.usage_integrity < before.usage_integrity,
        "integrity did not decrease: {} -> {}",
        before.usage_integrity,
        after.usage_integrity
    );
    assert_eq!(
        after.samples,
        before.samples + 1,
        "sample count not updated"
    );
}

/// The penalty is scoped.
#[test]
fn test_degrade_channel_does_not_affect_other_channels() {
    let sb = score_book();
    let s = Selector::new(Options {
        scores: Some(sb.clone()),
        strategy: "value".to_string(),
        ..Options::default()
    });
    s.degrade_channel(
        "cheap-degraded",
        &Deviation {
            worst: 1.0,
            over_threshold: true,
            direction: "over".to_string(),
        },
    );
    let other = sb.score("expensive-genuine", "gpt-4o").unwrap();
    assert_eq!(
        other.usage_integrity, 1.0,
        "degrading one channel changed another"
    );
}

// ---------------------------------------------------------------------------
// Health tracking
// ---------------------------------------------------------------------------

#[test]
fn test_health_tracker_opens_and_recovers() {
    let h = HealthTracker::new(2, std::time::Duration::from_secs(60));
    let now = Arc::new(Mutex::new(Utc::now()));
    let clock = now.clone();
    h.set_clock(move || *lock(&clock));

    assert!(
        !h.failure("a"),
        "a single failure should not trip the breaker"
    );
    assert!(h.failure("a"), "the breaker should trip at the threshold");
    assert!(h.unhealthy().contains("a"), "channel should be unhealthy");

    // After the cooldown it becomes eligible again (half-open).
    let later = *lock(&now) + TimeDelta::minutes(2);
    *lock(&now) = later;
    assert!(
        !h.unhealthy().contains("a"),
        "channel should recover after the cooldown"
    );

    // A success clears the state entirely.
    h.failure("a");
    h.success("a");
    assert_eq!(h.failure_count("a"), 0);
}

#[test]
fn test_health_tracker_isolates_channels() {
    let h = HealthTracker::new(1, std::time::Duration::from_secs(60));
    h.failure("a");
    let u = h.unhealthy();
    assert!(
        u.contains("a") && !u.contains("b"),
        "health state leaked across channels: {u:?}"
    );
}

// ---------------------------------------------------------------------------
// Explanations
// ---------------------------------------------------------------------------

/// Every routing decision is explainable, which the audit ledger records.
#[test]
fn test_decision_includes_reason() {
    let s = new_selector("value", Some(score_book()));
    let d = s
        .select(
            &chans(),
            &Request {
                tokens: tokens(1000),
                ..request("gpt-4o")
            },
        )
        .expect("select");
    assert!(
        !d.reason.is_empty(),
        "a routing decision must carry an explanation"
    );
}

/// The wording is part of the audit record, so it is pinned literally.
#[test]
fn test_reason_wording_is_stable() {
    let s = new_selector("value", Some(score_book()));
    let d = s
        .select(
            &chans(),
            &Request {
                tokens: Tokens::new(1_000_000, 1_000_000),
                ..request("gpt-4o")
            },
        )
        .expect("select");
    assert_eq!(
        d.reason,
        "strategy=value picked \"expensive-genuine\" of 2 candidate(s): protocol=openai \
         unit_price=15.0000 CNY/M capability=0.950 usage_integrity=1.000 value=0.0627"
    );
}

/// Same-protocol preference is a partition, not a tie-break: no price advantage
/// justifies a lossy conversion. The flag used to be accepted and never read, so
/// this asserts the ordering directly rather than through the gateway, where a
/// candidate can also disappear for unrelated reasons.
///
/// The converting candidate is an anthropic-native channel that has no chat
/// surface, so serving a chat request on it means converting the payload. A
/// channel that declared chat as well would serve it natively instead, which is a
/// different (and separately tested) case.
#[test]
fn test_prefer_same_protocol_partitions_candidates() {
    let channels = vec![
        Channel {
            protocol: "anthropic".to_string(),
            metadata: HashMap::from([("protocols".to_string(), "anthropic".to_string())]),
            ..channel("converted", "http://x", "m")
        },
        channel("native", "http://x", "m"),
    ];

    let sel = Selector::new(Options {
        strategy: "fixed".to_string(),
        ..Options::default()
    });
    let dec = sel
        .select(
            &channels,
            &Request {
                client_protocol: "openai".to_string(),
                allow_translation: true,
                prefer_same_protocol: true,
                ..request("m")
            },
        )
        .expect("select");
    assert_eq!(
        dec.ranked.len(),
        2,
        "the point is the order, not the membership"
    );
    assert_eq!(
        (
            dec.ranked[0].channel.name.as_str(),
            dec.ranked[0].translated
        ),
        ("native", false),
        "want the native channel first"
    );
    assert_eq!(
        (
            dec.ranked[1].channel.name.as_str(),
            dec.ranked[1].translated
        ),
        ("converted", true),
        "want the channel needing conversion second"
    );
}

/// With the preference off, configuration order decides - which is what makes the
/// flag meaningful rather than decorative.
#[test]
fn test_prefer_same_protocol_off_lets_the_strategy_decide() {
    let channels = vec![
        Channel {
            protocol: "anthropic".to_string(),
            metadata: HashMap::from([("protocols".to_string(), "anthropic".to_string())]),
            ..channel("converted", "http://x", "m")
        },
        channel("native", "http://x", "m"),
    ];
    let sel = Selector::new(Options {
        strategy: "fixed".to_string(),
        ..Options::default()
    });
    let dec = sel
        .select(
            &channels,
            &Request {
                client_protocol: "openai".to_string(),
                allow_translation: true,
                ..request("m")
            },
        )
        .expect("select");
    assert_eq!(
        dec.ranked[0].channel.name, "converted",
        "want the first configured channel"
    );
    assert!(
        dec.ranked[0].translated,
        "the converting candidate is not marked as translated"
    );
}

// ---------------------------------------------------------------------------
// Rust-side boundary tests
// ---------------------------------------------------------------------------

#[test]
fn weighted_strategy_leads_with_the_drawn_channel() {
    let s = new_selector("weighted", Some(score_book()));
    let mut seen = HashSet::new();
    for _ in 0..64 {
        let d = s
            .select(
                &chans(),
                &Request {
                    tokens: tokens(1000),
                    ..request("gpt-4o")
                },
            )
            .expect("select");
        assert_eq!(d.ranked.len(), 2);
        assert_eq!(d.chosen.channel.name, d.ranked[0].channel.name);
        seen.insert(d.chosen.channel.name);
    }
    assert_eq!(
        seen.len(),
        2,
        "a weighted draw over equal weights should reach both channels, saw {seen:?}"
    );
}

#[test]
fn default_strategy_is_value() {
    let s = Selector::new(Options::default());
    assert_eq!(s.strategy(), "value");
}

#[test]
fn translation_is_refused_without_permission() {
    let channels = vec![Channel {
        protocol: "anthropic".to_string(),
        metadata: HashMap::from([("protocols".to_string(), "anthropic".to_string())]),
        ..channel("converted", "http://x", "m")
    }];
    let sel = Selector::new(Options::default());
    let err = sel
        .select(
            &channels,
            &Request {
                client_protocol: "openai".to_string(),
                allow_translation: false,
                ..request("m")
            },
        )
        .unwrap_err();
    assert_eq!(err, RouteError::NoChannel("m".to_string()));
}

#[test]
fn a_declared_surface_is_served_without_translation() {
    let channels = vec![Channel {
        protocol: "anthropic".to_string(),
        metadata: HashMap::from([("protocols".to_string(), "anthropic,openai".to_string())]),
        ..channel("both", "http://x", "m")
    }];
    let sel = Selector::new(Options::default());
    let cands = sel
        .candidates(
            &channels,
            &Request {
                client_protocol: "openai".to_string(),
                ..request("m")
            },
        )
        .expect("candidates");
    assert!(
        !cands[0].translated,
        "a declared surface must not be translated"
    );
}

#[test]
fn a_channel_with_no_metadata_is_trusted_for_any_protocol() {
    let channels = vec![channel("plain", "http://x", "m")];
    let sel = Selector::new(Options::default());
    let cands = sel
        .candidates(
            &channels,
            &Request {
                client_protocol: "responses".to_string(),
                ..request("m")
            },
        )
        .expect("candidates");
    assert!(!cands[0].translated);
}

#[test]
fn groups_restrict_candidates() {
    let channels = vec![Channel {
        metadata: HashMap::from([("groups".to_string(), "fast, cheap".to_string())]),
        ..channel("grouped", "http://x", "m")
    }];
    let sel = Selector::new(Options::default());
    let ok = sel.candidates(
        &channels,
        &Request {
            groups: vec!["cheap".to_string()],
            ..request("m")
        },
    );
    assert!(ok.is_ok(), "a matching group must keep the channel: {ok:?}");
    let err = sel
        .candidates(
            &channels,
            &Request {
                groups: vec!["slow".to_string()],
                ..request("m")
            },
        )
        .unwrap_err();
    assert_eq!(err, RouteError::NoChannel("m".to_string()));
}

#[test]
fn excluded_providers_are_skipped_by_name_and_prefix() {
    let channels = vec![
        Channel {
            metadata: HashMap::from([("provider".to_string(), "vendor".to_string())]),
            ..channel("vendor/k", "http://x", "m")
        },
        channel("other", "http://x", "m"),
    ];
    let sel = Selector::new(Options::default());
    let cands = sel
        .candidates(
            &channels,
            &Request {
                exclude_providers: vec!["vendor".to_string()],
                ..request("m")
            },
        )
        .expect("candidates");
    assert_eq!(names(&cands), vec!["other".to_string()]);
}

#[test]
fn health_tracker_defaults_and_stale_entries() {
    let h = HealthTracker::new(0, std::time::Duration::ZERO);
    assert_eq!(h.threshold, 3);
    assert_eq!(h.cooldown, std::time::Duration::from_secs(30));

    // A channel that never failed is not unhealthy, and the map is pruned once
    // the cooldown elapses so memory does not grow without bound.
    assert!(!h.unhealthy().contains("a"));
    let now = Arc::new(Mutex::new(Utc::now()));
    let clock = now.clone();
    h.set_clock(move || *lock(&clock));
    h.failure("a");
    h.failure("a");
    h.failure("a");
    assert!(h.unhealthy().contains("a"));
    let later = *lock(&now) + TimeDelta::seconds(31);
    *lock(&now) = later;
    assert!(h.unhealthy().is_empty());
    assert_eq!(
        h.failure_count("a"),
        0,
        "a half-open channel restarts from zero"
    );
}

#[test]
fn scores_json_matches_go_field_names_and_omits_empty_values() {
    let scores = Scores {
        capability: 0.5,
        samples: 3,
        updated_at: Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap(),
        ..Scores::default()
    };
    let json = serde_json::to_string(&scores).unwrap();
    assert!(json.contains("\"capability\":0.5"), "{json}");
    assert!(json.contains("\"samples\":3"), "{json}");
    assert!(
        json.contains("\"updated_at\":\"2026-03-01T12:00:00Z\""),
        "{json}"
    );
    assert!(!json.contains("fingerprint_match"), "{json}");
    assert!(!json.contains("benchmarks"), "{json}");

    // Go's omitempty does not apply to a time.Time, so a zero timestamp is still
    // written; that is what makes the scorecard file self-describing.
    let json = serde_json::to_string(&Scores::default()).unwrap();
    assert!(
        json.contains("\"updated_at\":\"0001-01-01T00:00:00Z\""),
        "{json}"
    );
}

#[test]
fn scores_with_null_benchmarks_parse() {
    // Go writes `null` for a nil map, and the intelligence layer saves a
    // scorecard before every field is filled in.
    let json = r#"{"capability":0.5,"benchmarks":null,"updated_at":"2026-03-01T12:00:00Z"}"#;
    let scores: Scores = serde_json::from_str(json).expect("null map parses");
    assert_eq!(scores.capability, 0.5);
    assert!(scores.benchmarks.is_empty());
    assert_eq!(scores.samples, 0);
}

#[test]
fn degraded_integrity_blends_with_previous_samples() {
    let sb = MapScoreBook::default();
    sb.set(
        "ch",
        "m",
        Scores {
            usage_integrity: 1.0,
            samples: 1,
            ..Scores::default()
        },
    );
    // A single 100% deviation halves the estimate rather than collapsing it.
    sb.degrade(
        "ch",
        &Deviation {
            worst: 1.0,
            over_threshold: true,
            direction: "over".to_string(),
        },
    );
    let after = sb.score("ch", "m").unwrap();
    assert!(
        (after.usage_integrity - 0.5).abs() < 1e-12,
        "{}",
        after.usage_integrity
    );
    assert_eq!(after.samples, 2);
}

#[test]
fn unmeasured_scores_leave_the_value_neutral() {
    let sb: Arc<dyn ScoreBook> = Arc::new(MapScoreBook::default());
    let s = new_selector("value", Some(sb));
    let d = s.select(&chans(), &request("gpt-4o")).expect("select");
    // Quality stays at 1 and the cheaper channel wins on price alone.
    assert_eq!(d.chosen.channel.name, "cheap-degraded");
    assert_eq!(d.ranked[0].value, 1.0 / 1.5);
}

#[test]
fn fallback_score_applies_when_enabled() {
    let mut sb = MapScoreBook::default();
    sb.fallback = Scores {
        capability: 0.1,
        ..Scores::default()
    };
    assert!(sb.score("a", "m").is_none());
    sb.use_fallback = true;
    assert_eq!(sb.score("a", "m").unwrap().capability, 0.1);
}

#[test]
fn selector_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Selector>();
    assert_send_sync::<HealthTracker>();
    assert_send_sync::<MapScoreBook>();
}

// ---------------------------------------------------------------------------
// Go-compatible `%.4g`
// ---------------------------------------------------------------------------

/// Expected strings produced by Go 1.26 (`fmt.Sprintf("%.4g", v)`).
#[test]
fn format_g4_matches_go() {
    let cases: [(f64, &str); 37] = [
        (1.5, "1.5"),
        (1.0, "1"),
        (100.0, "100"),
        (45.3590519, "45.36"),
        (4577.26, "4577"),
        (45772.6, "4.577e+04"),
        (9999.9, "1e+04"),
        (0.0001234, "0.0001234"),
        (1.234e-05, "1.234e-05"),
        (0.5, "0.5"),
        (1e6, "1e+06"),
        (123456789.0, "1.235e+08"),
        (0.00099999, "0.001"),
        (0.0, "0"),
        (-1.5, "-1.5"),
        (-0.0001234, "-0.0001234"),
        (std::f64::consts::PI, "3.142"),
        (2.5e-5, "2.5e-05"),
        (0.0001, "0.0001"),
        (0.001, "0.001"),
        (9.995, "9.995"),
        (999.95, "1000"),
        (9999.5, "1e+04"),
        (0.1 + 0.2, "0.3"),
        (1234567.89, "1.235e+06"),
        (1e21, "1e+21"),
        (1e-7, "1e-07"),
        (250.0, "250"),
        (45.0, "45"),
        (33.3333333333, "33.33"),
        (0.12345, "0.1235"),
        (6.02e23, "6.02e+23"),
        (1.0 / 3.0, "0.3333"),
        (12345678901234567890.0, "1.235e+19"),
        (-0.5, "-0.5"),
        (-4577.26, "-4577"),
        (5e-324, "4.941e-324"),
    ];
    for (value, want) in cases {
        assert_eq!(format_g4(value), want, "format_g4({value:?})");
    }
    assert_eq!(format_g(f64::INFINITY, 4), "+Inf");
    assert_eq!(format_g(f64::NEG_INFINITY, 4), "-Inf");
    assert_eq!(format_g(f64::NAN, 4), "NaN");
    assert_eq!(format_g(-0.0, 4), "-0");
}

/// The weighted strategy draws from a seeded generator, so two selectors built
/// with the same seed must make the same sequence of choices. Without that, a
/// request's routing could not be reproduced from its audit record.
#[test]
fn weighted_strategy_is_reproducible_for_a_fixed_seed() {
    let first = new_selector("weighted", Some(score_book()));
    let second = new_selector("weighted", Some(score_book()));
    let chans = chans();
    let req = Request {
        tokens: tokens(1000),
        ..request("gpt-4o")
    };
    let draw = |selector: &Selector| -> Vec<String> {
        (0..32)
            .map(|_| {
                selector
                    .select(&chans, &req)
                    .expect("select")
                    .chosen
                    .channel
                    .name
            })
            .collect()
    };
    let a = draw(&first);
    let b = draw(&second);
    assert_eq!(a, b, "same seed must produce the same sequence");
    assert!(
        a.windows(2).any(|w| w[0] != w[1]) || a.len() == 1,
        "a weighted draw over two channels should not be constant: {a:?}"
    );
}
