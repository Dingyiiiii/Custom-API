//! Distribution comparison: Jensen-Shannon divergence over reply lengths.
//!
//! This is the metric behind the fingerprint verdict. It compares two sample
//! sets of assistant-reply lengths taken under a fixed probe.

/// Returns the Jensen-Shannon divergence between two sample sets, in bits,
/// using a shared histogram. It is symmetric and bounded by 1, which makes it a
/// usable threshold metric.
///
/// A small sample count is handled by using a small number of bins, so the
/// estimate does not become noise-dominated.
pub fn jensen_shannon(a: &[i32], b: &[i32]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 1.0;
    }
    const BINS: usize = 8;
    let mut lo = i32::MAX;
    let mut hi = i32::MIN;
    for &v in a.iter().chain(b.iter()) {
        if v < lo {
            lo = v;
        }
        if v > hi {
            hi = v;
        }
    }
    if hi == lo {
        // Both sample sets are the same single point.
        return 0.0;
    }
    // A distribution that is a point mass is handled exactly rather than by
    // binning: binning a constant against a spread distribution wastes almost
    // all bins and understates the divergence, which would let a canned
    // upstream (one that always returns the same length) slip under the
    // threshold.
    let a_const = is_constant(a);
    let b_const = is_constant(b);
    match (a_const, b_const) {
        (true, true) => {
            if a[0] == b[0] {
                0.0
            } else {
                1.0
            }
        }
        (true, false) => point_mass_divergence(a[0], b, lo, hi, BINS),
        (false, true) => point_mass_divergence(b[0], a, lo, hi, BINS),
        (false, false) => {
            let pa = histogram(a, lo, hi, BINS);
            let pb = histogram(b, lo, hi, BINS);
            let m: Vec<f64> = pa
                .iter()
                .zip(pb.iter())
                .map(|(x, y)| 0.5 * (x + y))
                .collect();
            0.5 * kl(&pa, &m) + 0.5 * kl(&pb, &m)
        }
    }
}

/// Reports whether every sample has the same value.
///
/// Go indexes `s[1:]` and would panic on an empty slice; every caller checks for
/// emptiness first, and a one-element slice is constant either way.
fn is_constant(s: &[i32]) -> bool {
    match s.first() {
        Some(&first) => s.iter().all(|&v| v == first),
        None => true,
    }
}

/// `sum p[i] * log2(p[i]/q[i])` over the bins where both sides carry mass.
fn kl(p: &[f64], q: &[f64]) -> f64 {
    let mut sum = 0.0;
    for (pi, qi) in p.iter().zip(q.iter()) {
        if *pi > 0.0 && *qi > 0.0 {
            sum += pi * (pi / qi).log2();
        }
    }
    sum
}

/// Maps a sample value onto a bin index of a histogram spanning `lo..=hi`.
///
/// Go computes `int(float64(v-lo)/width)` and clamps; the width is always
/// positive here because the caller guarantees `hi > lo`.
fn bin_index(v: i32, lo: i32, hi: i32, bins: usize) -> usize {
    let span = hi as f64 - lo as f64 + 1.0;
    let width = span / bins as f64;
    let idx = ((v as f64 - lo as f64) / width) as i64;
    if idx >= bins as i64 {
        bins - 1
    } else if idx < 0 {
        0
    } else {
        idx as usize
    }
}

/// The Jensen-Shannon divergence between a point mass at `point` and an
/// empirical distribution, estimated with the same smoothing the histogram uses
/// so that the result stays comparable with the general case.
fn point_mass_divergence(point: i32, samples: &[i32], lo: i32, hi: i32, bins: usize) -> f64 {
    // The point mass puts all its weight on one bucket; the empirical
    // distribution spreads over the observed buckets. With Laplace smoothing
    // over `bins` buckets, an observation in the point bucket carries
    // (mass+eps) and every other bucket carries eps alone.
    const EPS: f64 = 0.5;
    let pi = bin_index(point, lo, hi, bins);
    let mut p_obs = vec![0.0; bins];
    let mut q_obs = vec![0.0; bins];
    p_obs[pi] = 1.0;
    let total = samples.len() as f64 + EPS * bins as f64;
    // Where the point mass lands relative to the empirical support decides the
    // divergence, which is exactly the discrimination being asked for.
    for &v in samples {
        q_obs[bin_index(v, lo, hi, bins)] += 1.0;
    }
    let in_support = q_obs[pi] > 0.0;
    for q in q_obs.iter_mut() {
        *q = (*q + EPS) / total;
    }
    // Smooth the point mass the same way so neither side is degenerate.
    let p_mass = (1.0 + EPS) / (1.0 + EPS * bins as f64);
    let p_rest = EPS / (1.0 + EPS * bins as f64);
    for (i, p) in p_obs.iter_mut().enumerate() {
        *p = if i == pi { p_mass } else { p_rest };
    }
    let m: Vec<f64> = p_obs
        .iter()
        .zip(q_obs.iter())
        .map(|(p, q)| 0.5 * (p + q))
        .collect();
    let js = 0.5 * kl(&p_obs, &m) + 0.5 * kl(&q_obs, &m);
    if !in_support {
        // The constant reply is outside the baseline's observed range
        // entirely, which is unambiguous evidence of substitution.
        return js.max(0.75);
    }
    js
}

/// Laplace-smoothed histogram of `samples` over `bins` buckets spanning
/// `lo..=hi`.
fn histogram(samples: &[i32], lo: i32, hi: i32, bins: usize) -> Vec<f64> {
    let mut out = vec![0.0; bins];
    for &v in samples {
        out[bin_index(v, lo, hi, bins)] += 1.0;
    }
    let total = samples.len() as f64;
    for slot in out.iter_mut() {
        // Laplace smoothing keeps an empty bin from producing an infinite
        // divergence.
        *slot = (*slot + 0.5) / (total + 0.5 * bins as f64);
    }
    out
}
