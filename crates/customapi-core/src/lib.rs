//! Custom-API core library.
//!
//! The crate mirrors the Go implementation package by package, so that the
//! behaviour, the wire format and the audit semantics stay comparable:
//!
//! | Go package            | Rust module        | role                                        |
//! |-----------------------|--------------------|---------------------------------------------|
//! | internal/version      | version            | build identity                              |
//! | internal/config       | config             | configuration, env overlays, validation     |
//! | internal/protocol     | protocol           | wire types, SSE, in-place rewrite, translate |
//! | internal/policy       | policy             | outbound/inbound rule engine                |
//! | internal/metering     | metering           | token recount and usage verification        |
//! | internal/meter        | meter              | price normalisation and quoting             |
//! | internal/audit        | audit              | tamper-evident ledger                       |
//! | internal/store        | store              | quota accounting and cycle reset            |
//! | internal/intel        | intel              | price collection, probes, benchmarks, scores |
//! | internal/discovery    | discovery          | merged model catalogue                      |
//! | internal/route        | route              | channel choice, health, integrity weighting |
//! | internal/gateway      | gateway            | proxy pipeline, streaming, admin API        |

pub mod audit;
pub mod config;
pub mod discovery;
pub mod gateway;
pub mod intel;
pub mod meter;
pub mod metering;
pub mod policy;
pub mod protocol;
pub mod route;
pub mod store;
pub mod version;

/// Shared JSON helpers.
///
/// The Go implementation relies on `omitempty` semantics and on a duration type
/// that accepts either "30s" or a number of seconds. Both are reproduced here so
/// that configuration files and ledger records stay interoperable.
pub mod json {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration as StdDuration;

    /// Reports whether a numeric field is zero (Go's `omitempty` for numbers).
    pub fn is_zero_i64(v: &i64) -> bool {
        *v == 0
    }

    /// Reports whether a numeric field is zero.
    pub fn is_zero_i32(v: &i32) -> bool {
        *v == 0
    }

    /// Reports whether a numeric field is zero.
    pub fn is_zero_u32(v: &u32) -> bool {
        *v == 0
    }

    /// Reports whether a numeric field is zero.
    pub fn is_zero_f64(v: &f64) -> bool {
        *v == 0.0
    }

    /// Reports whether a boolean field is false.
    pub fn is_false(v: &bool) -> bool {
        !*v
    }

    /// Reports whether a duration field is zero (Go's `omitempty`).
    pub fn is_zero_duration(v: &Duration) -> bool {
        v.0.is_zero()
    }

    /// Serialises a `Duration` the way Go writes `time.Duration`: "30s", "2m0s",
    /// "1.5s", "300ms", "0s".
    pub fn format_duration(d: StdDuration) -> String {
        let total_ns = d.as_nanos();
        if total_ns == 0 {
            return "0s".to_string();
        }
        if total_ns < 1_000_000_000 {
            // Sub-second: Go picks ns, µs or ms and keeps the fraction.
            if total_ns < 1_000 {
                return format!("{total_ns}ns");
            }
            if total_ns < 1_000_000 {
                return format!("{}µs", format_fixed(total_ns, 1_000, 3));
            }
            return format!("{}ms", format_fixed(total_ns, 1_000_000, 6));
        }
        let secs = total_ns / 1_000_000_000;
        let nanos = total_ns % 1_000_000_000;
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        let seconds = secs % 60;
        let mut out = String::new();
        if hours > 0 {
            out.push_str(&format!("{}h", hours));
            out.push_str(&format!("{}m", minutes));
            out.push_str(&format!("{}s", seconds));
        } else if minutes > 0 {
            out.push_str(&format!("{}m", minutes));
            out.push_str(&format!("{}s", seconds));
        } else if nanos == 0 {
            out.push_str(&format!("{}s", seconds));
        } else {
            out.push_str(&format!("{}.{}s", seconds, trim_fraction(nanos, 9)));
        }
        out
    }

    /// Renders an integer with trailing fractional zeros removed, padding the
    /// value out to `width` digits first (so 5ms becomes "005").
    fn trim_fraction(value: u128, width: usize) -> String {
        let s = format!("{:0width$}", value, width = width);
        let trimmed = s.trim_end_matches('0');
        if trimmed.is_empty() {
            "0".to_string()
        } else {
            trimmed.to_string()
        }
    }

    /// Renders `value / divisor` as a decimal with at most `decimals` fractional
    /// digits and no trailing zeros ("300", "1.25").
    fn format_fixed(value: u128, divisor: u128, decimals: u32) -> String {
        let whole = value / divisor;
        let mut frac = value % divisor;
        if frac == 0 {
            return whole.to_string();
        }
        let digits = format!("{:0width$}", frac, width = divisor.ilog10() as usize);
        let mut digits = digits.as_str();
        while digits.len() > decimals as usize {
            digits = &digits[..digits.len() - 1];
        }
        let digits = digits.trim_end_matches('0');
        frac = 0;
        let _ = frac;
        if digits.is_empty() {
            whole.to_string()
        } else {
            format!("{whole}.{digits}")
        }
    }

    /// Parses a Go duration literal: "300ms", "1.5h", "2h45m", "-30s", "0".
    pub fn parse_duration(input: &str) -> Option<StdDuration> {
        let s = input.trim();
        if s.is_empty() {
            return None;
        }
        if s == "0" {
            return Some(StdDuration::ZERO);
        }
        let mut rest = s;
        let mut negative = false;
        if let Some(stripped) = rest.strip_prefix('-') {
            negative = true;
            rest = stripped;
        } else if let Some(stripped) = rest.strip_prefix('+') {
            rest = stripped;
        }
        if rest.is_empty() {
            return None;
        }
        let mut total = StdDuration::ZERO;
        let bytes = rest.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            if start == i {
                return None;
            }
            let number: f64 = rest[start..i].parse().ok()?;
            let unit_start = i;
            while i < bytes.len() && !bytes[i].is_ascii_digit() && bytes[i] != b'.' {
                i += 1;
            }
            let unit = &rest[unit_start..i];
            let factor = match unit {
                "ns" => 1e-9,
                "us" | "µs" | "μs" => 1e-6,
                "ms" => 1e-3,
                "s" => 1.0,
                "m" => 60.0,
                "h" => 3600.0,
                _ => return None,
            };
            let secs = number * factor;
            if !secs.is_finite() || secs < 0.0 {
                return None;
            }
            total = total.checked_add(StdDuration::from_secs_f64(secs))?;
        }
        if negative {
            // Negative durations are rejected by the configuration layer, so
            // they never reach this parser from a valid file.
            return None;
        }
        Some(total)
    }

    /// `Duration` is a JSON-friendly `time.Duration`: it accepts a Go duration
    /// string or a number of seconds, and serialises as a Go duration string.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
    pub struct Duration(pub StdDuration);

    impl Duration {
        /// Wraps a standard duration.
        pub const fn new(d: StdDuration) -> Self {
            Duration(d)
        }

        /// Returns the standard duration.
        pub const fn std(self) -> StdDuration {
            self.0
        }

        /// Returns the duration in seconds as a float.
        pub fn as_secs_f64(self) -> f64 {
            self.0.as_secs_f64()
        }

        /// Builds a duration from whole seconds.
        pub const fn from_secs(secs: u64) -> Self {
            Duration(StdDuration::from_secs(secs))
        }
    }

    impl From<StdDuration> for Duration {
        fn from(value: StdDuration) -> Self {
            Duration(value)
        }
    }

    impl Serialize for Duration {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_str(&format_duration(self.0))
        }
    }

    impl<'de> Deserialize<'de> for Duration {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl serde::de::Visitor<'_> for Visitor {
                type Value = Duration;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("a duration string like \"30s\" or a number of seconds")
                }
                fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Duration, E> {
                    parse_duration(v)
                        .map(Duration)
                        .ok_or_else(|| E::custom(format!("invalid duration {:?}", v)))
                }
                fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Duration, E> {
                    self.visit_str(&v)
                }
                fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Duration, E> {
                    self.visit_f64(v as f64)
                }
                fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Duration, E> {
                    self.visit_f64(v as f64)
                }
                fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Duration, E> {
                    if !v.is_finite() || v < 0.0 {
                        return Err(E::custom("duration seconds must be finite and non-negative"));
                    }
                    Ok(Duration(StdDuration::from_secs_f64(v)))
                }
                fn visit_unit<E: serde::de::Error>(self) -> Result<Duration, E> {
                    Ok(Duration(StdDuration::ZERO))
                }
                fn visit_none<E: serde::de::Error>(self) -> Result<Duration, E> {
                    Ok(Duration(StdDuration::ZERO))
                }
            }
            deserializer.deserialize_any(Visitor)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn duration_round_trips_go_literals() {
            for (text, expected) in [
                ("30s", 30.0),
                ("2m", 120.0),
                ("1h30m", 5400.0),
                ("300ms", 0.3),
                ("1.5s", 1.5),
                ("0", 0.0),
            ] {
                let parsed = parse_duration(text).unwrap();
                assert!((parsed.as_secs_f64() - expected).abs() < 1e-9, "{text}");
            }
        }

        #[test]
        fn duration_formats_like_go() {
            assert_eq!(format_duration(StdDuration::ZERO), "0s");
            assert_eq!(format_duration(StdDuration::from_secs(30)), "30s");
            assert_eq!(format_duration(StdDuration::from_secs(120)), "2m0s");
            assert_eq!(format_duration(StdDuration::from_secs(5400)), "1h30m0s");
            assert_eq!(
                format_duration(StdDuration::from_millis(300)),
                "300ms"
            );
            assert_eq!(
                format_duration(StdDuration::from_millis(1500)),
                "1.5s"
            );
        }
    }
}
