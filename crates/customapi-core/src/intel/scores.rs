//! Score assembly: folding benchmark, fingerprint and integrity evidence into
//! the per-channel scorecard the router consumes.

use chrono::Utc;
use std::collections::BTreeMap;

use super::baseline::{CompareResult, VERDICT_MATCH, VERDICT_MISMATCH};
use super::bench::BenchReport;
use crate::route;

/// Merges benchmark, fingerprint and integrity evidence into the per-channel
/// scorecard the router consumes.
///
/// Capability and fingerprint evidence are keyed by model; integrity and
/// reliability are properties of the channel and are applied to every model it
/// serves.
///
/// Go returns `map[string]route.Scores`; the `BTreeMap` here keeps iteration -
/// and therefore anything serialised from it - deterministic.
pub fn assemble_scores(
    reports: &[BenchReport],
    fingerprints: &[CompareResult],
    integrity: f64,
    reliability: f64,
) -> BTreeMap<String, route::Scores> {
    let mut out: BTreeMap<String, route::Scores> = BTreeMap::new();
    // Channel-level properties apply to every model entry, so they are re-stamped
    // on both passes, exactly as Go's `ensure` closure does.
    let ensure = |out: &mut BTreeMap<String, route::Scores>, model: &str| -> route::Scores {
        let mut scores = out.get(model).cloned().unwrap_or_default();
        scores.usage_integrity = integrity;
        scores.reliability = reliability;
        scores
    };

    for report in reports {
        let mut scores = ensure(&mut out, &report.model);
        scores.capability = report.score;
        for (category, value) in &report.by_category {
            scores.benchmarks.insert(category.clone(), *value);
        }
        for (level, value) in &report.by_level {
            scores.benchmarks.insert(format!("level_{level}"), *value);
        }
        scores.updated_at = report.finished;
        scores.samples += 1;
        out.insert(report.model.clone(), scores);
    }

    for fingerprint in fingerprints {
        if fingerprint.model.is_empty() {
            continue;
        }
        let mut scores = ensure(&mut out, &fingerprint.model);
        // FingerprintMatch is the probability that a probe matches the
        // baseline. A "match" verdict is full credit; a mismatch scales down
        // with how far the divergence exceeded the threshold.
        match fingerprint.verdict.as_str() {
            VERDICT_MATCH => scores.fingerprint_match = 1.0,
            VERDICT_MISMATCH => {
                let over = fingerprint.score - fingerprint.threshold;
                let mut match_score = 1.0 - over.min(1.0);
                if match_score < 0.0 {
                    match_score = 0.0;
                }
                scores.fingerprint_match = match_score;
            }
            // No baseline: leave unmeasured, which is neutral in the router.
            _ => scores.fingerprint_match = 0.0,
        }
        scores.updated_at = Utc::now();
        out.insert(fingerprint.model.clone(), scores);
    }
    out
}
