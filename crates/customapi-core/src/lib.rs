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
    use serde::ser::SerializeMap;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::value::RawValue;
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

    /// Serialises an empty collection as `null`, the way Go's `encoding/json`
    /// renders a nil slice.
    ///
    /// The management API mirrors Go's structs field for field, and a Go handler
    /// that builds its payload from a nil `[]string` emits `null` rather than
    /// `[]`. Clients accept both, but the wire format is part of the contract
    /// the port promises.
    pub fn vec_or_null<T, S>(items: &[T], serializer: S) -> Result<S::Ok, S::Error>
    where
        T: Serialize,
        S: Serializer,
    {
        if items.is_empty() {
            serializer.serialize_none()
        } else {
            items.serialize(serializer)
        }
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
        let frac = value % divisor;
        if frac == 0 {
            return whole.to_string();
        }
        let digits = format!("{:0width$}", frac, width = divisor.ilog10() as usize);
        let digits = &digits[..digits.len().min(decimals as usize)];
        let digits = digits.trim_end_matches('0');
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
                    let trimmed = v.trim();
                    // Go treats an empty string as zero, and accepts a bare
                    // number of seconds as well as a unit-suffixed literal.
                    if trimmed.is_empty() {
                        return Ok(Duration(StdDuration::ZERO));
                    }
                    if let Some(d) = parse_duration(trimmed) {
                        return Ok(Duration(d));
                    }
                    if let Ok(secs) = trimmed.parse::<f64>() {
                        if secs.is_finite() && secs >= 0.0 {
                            return Ok(Duration(StdDuration::from_secs_f64(secs)));
                        }
                    }
                    Err(E::custom(format!("invalid duration {:?}", v)))
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
                        return Err(E::custom(
                            "duration seconds must be finite and non-negative",
                        ));
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

    // -----------------------------------------------------------------------
    // Go-compatible numbers and timestamps
    //
    // The two implementations exchange JSON files, ledger lines and admin
    // responses, so the encoders have to agree on every byte. serde_json and
    // `encoding/json` disagree on two things and only two things: how a float64
    // is written (`40.0` against Go's `40`, and different scientific-notation
    // cutoffs) and how a `time.Time` is written (chrono pads the fractional
    // second to 3/6/9 digits, Go trims trailing zeros). Everything below exists
    // to close exactly that gap; structure, field names and field order were
    // already aligned.
    // -----------------------------------------------------------------------

    /// Formats an `f64` exactly the way Go's `encoding/json` does.
    ///
    /// Go takes the shortest representation that round-trips and picks the
    /// notation by magnitude: `abs(v) < 1e-6` or `abs(v) >= 1e21` is written in
    /// `e` notation (`1e-7`, `2.5e-7`, `1.5e+21`), everything else in plain
    /// decimal with no trailing `.0` (`40`, `0.00035`, `100000000000000000000`).
    /// Rust's `Display` and `LowerExp` produce the same digits Go's `strconv`
    /// does (both are shortest-round-trip), so only the cutoffs and the exponent
    /// spelling have to be restored.
    ///
    /// Go refuses to encode NaN and ±Inf (`json: unsupported value: NaN`). A
    /// formatting helper cannot return an error the way the encoder does, so this
    /// returns `null` instead: the written document stays valid JSON, and callers
    /// that must not lose the value have to check [`f64::is_finite`] first.
    pub fn format_go_f64(v: f64) -> String {
        if !v.is_finite() {
            return "null".to_string();
        }
        let abs = v.abs();
        if abs != 0.0 && (abs < 1e-6 || abs >= 1e21) {
            let text = format!("{:e}", v);
            return match text.split_once('e') {
                Some((mantissa, exponent)) => go_exponent(mantissa, exponent),
                None => text,
            };
        }
        format!("{v}")
    }

    /// Spells the exponent half of `strconv.FormatFloat(v, 'e', -1, 64)`.
    ///
    /// `strconv` always writes a sign and at least two digits (`1e-07`,
    /// `1.5e+21`); `encoding/json` then drops the leading zero of a negative
    /// single-digit exponent (`1e-07` becomes `1e-7`) and leaves everything else
    /// alone. The cutoffs in [`format_go_f64`] make a positive exponent below 21
    /// unreachable, so "sign, then digits without padding" is the same output.
    fn go_exponent(mantissa: &str, exponent: &str) -> String {
        let (sign, digits) = match exponent.strip_prefix('-') {
            Some(rest) => ('-', rest),
            None => ('+', exponent.strip_prefix('+').unwrap_or(exponent)),
        };
        let digits = digits.trim_start_matches('0');
        let digits = if digits.is_empty() { "0" } else { digits };
        format!("{mantissa}e{sign}{digits}")
    }

    /// An `f64` that serialises with [`format_go_f64`].
    ///
    /// The text is spliced in through [`RawValue`], which is the only way to make
    /// serde_json write digits it would not choose itself. Two consequences worth
    /// knowing:
    ///
    /// * It must reach a JSON *formatter* (`to_string`, `to_vec`, `json!`). A
    ///   non-JSON serializer rejects `RawValue`, which is what Go does with a
    ///   value its own encoder cannot represent: it fails rather than guessing.
    /// * `json!` and `to_value` reparse the text into a number, so the value
    ///   stays a JSON number there as well. Those paths keep the digits but not
    ///   the spelling: `1e+21` becomes `1e21`, because a `Value` holds the float
    ///   and no longer the text. Magnitudes that large or that small do not occur
    ///   in prices, scores or weights, so the affected surfaces are unaffected in
    ///   practice; the file and ledger writers, which hash what they write, go
    ///   through a formatter and are exact.
    #[derive(Debug, Clone, Copy, PartialEq, Default)]
    pub struct GoFloat(pub f64);

    impl From<f64> for GoFloat {
        fn from(value: f64) -> Self {
            GoFloat(value)
        }
    }

    impl Serialize for GoFloat {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let text = format_go_f64(self.0);
            match RawValue::from_string(text) {
                Ok(raw) => raw.serialize(serializer),
                // Unreachable: the text is either a decimal number Go's own
                // encoder produced or the literal `null`.
                Err(error) => Err(serde::ser::Error::custom(error)),
            }
        }
    }

    impl<'de> Deserialize<'de> for GoFloat {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            f64::deserialize(deserializer).map(GoFloat)
        }
    }

    /// Serialises a float with Go's number formatting.
    ///
    /// Use it as `#[serde(serialize_with = "crate::json::go_f64")]`.
    pub fn go_f64<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
        GoFloat(*value).serialize(serializer)
    }

    /// Serialises an optional float with Go's number formatting.
    pub fn go_f64_opt<S: Serializer>(
        value: &Option<f64>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => GoFloat(*value).serialize(serializer),
            None => serializer.serialize_none(),
        }
    }

    /// Serialises a `map[string]float64` with Go's number formatting.
    ///
    /// A `BTreeMap` keeps the keys sorted, which is the order `encoding/json`
    /// writes a Go map in, so both implementations emit the same bytes.
    pub fn go_f64_map<S: Serializer>(
        values: &std::collections::BTreeMap<String, f64>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(values.len()))?;
        for (key, value) in values {
            map.serialize_entry(key, &GoFloat(*value))?;
        }
        map.end()
    }

    /// Formats a UTC instant the way Go's `time.RFC3339Nano` does.
    ///
    /// The fractional second is written only when it is non-zero and its trailing
    /// zeros are removed: `2026-10-24T14:08:29Z`, `.5Z`, `.123456Z`,
    /// `.000000001Z`. chrono's `AutoSi` keeps the digit group it picked (`.500Z`),
    /// which is why this formats the fraction itself.
    pub fn format_go_time(t: &chrono::DateTime<chrono::Utc>) -> String {
        // %Y zero-pads to four digits, so Go's zero time reads 0001-01-01.
        let mut out = t.format("%Y-%m-%dT%H:%M:%S").to_string();
        let nanos = t.timestamp_subsec_nanos();
        if nanos != 0 {
            let mut digits = format!("{nanos:09}");
            while digits.ends_with('0') {
                digits.pop();
            }
            out.push('.');
            out.push_str(&digits);
        }
        out.push('Z');
        out
    }

    /// A timestamp that serialises with [`format_go_time`].
    ///
    /// Deserialisation is the same flexible RFC 3339 parse a plain
    /// `DateTime<Utc>` does, so a file written by either implementation is
    /// readable by both: a padded fraction (`.500Z`, which an older Rust build
    /// wrote) and a trimmed one (`.5Z`, which Go writes) both parse.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct GoTime(pub chrono::DateTime<chrono::Utc>);

    impl From<chrono::DateTime<chrono::Utc>> for GoTime {
        fn from(value: chrono::DateTime<chrono::Utc>) -> Self {
            GoTime(value)
        }
    }

    impl Serialize for GoTime {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_str(&format_go_time(&self.0))
        }
    }

    impl<'de> Deserialize<'de> for GoTime {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            chrono::DateTime::<chrono::Utc>::deserialize(deserializer).map(GoTime)
        }
    }

    /// Serialises a timestamp with Go's `RFC3339Nano` formatting.
    ///
    /// Use it as `#[serde(serialize_with = "crate::json::go_time")]`.
    pub fn go_time<S: Serializer>(
        value: &chrono::DateTime<chrono::Utc>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format_go_time(value))
    }

    /// Serialises an optional timestamp with Go's `RFC3339Nano` formatting.
    pub fn go_time_opt<S: Serializer>(
        value: &Option<chrono::DateTime<chrono::Utc>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.serialize_str(&format_go_time(value)),
            None => serializer.serialize_none(),
        }
    }

    /// Serialises a slice the way Go writes a nil slice: `null` when empty.
    ///
    /// Go's `var rows []T` is nil until something is appended, and
    /// `encoding/json` writes a nil slice as `null` (a `make([]T, 0, n)` slice,
    /// by contrast, is `[]`). The admin API mirrors that, so the empty case needs
    /// `null` rather than an empty array.
    pub fn go_slice_or_null<T, S>(values: &[T], serializer: S) -> Result<S::Ok, S::Error>
    where
        T: Serialize,
        S: Serializer,
    {
        if values.is_empty() {
            serializer.serialize_none()
        } else {
            values.serialize(serializer)
        }
    }

    /// Deserialises a `DateTime<Utc>` for a field serialised by [`go_time`].
    ///
    /// chrono already accepts any number of fractional digits, so this exists to
    /// pair with the `serialize_with` helper and to document that a padded
    /// fraction (`.500Z`, written by an older Rust build) and a trimmed one
    /// (`.5Z`, written by Go) both parse.
    pub fn go_time_deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<chrono::DateTime<chrono::Utc>, D::Error> {
        chrono::DateTime::<chrono::Utc>::deserialize(deserializer)
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
            assert_eq!(format_duration(StdDuration::from_millis(300)), "300ms");
            assert_eq!(format_duration(StdDuration::from_millis(1500)), "1.5s");
        }

        /// The expected strings are the output of `encoding/json` for the same
        /// values, measured with the Go reference implementation (Go 1.26,
        /// `json.Marshal(struct{ V float64 }{v})`), not derived from the spec.
        #[test]
        fn floats_render_exactly_like_go() {
            let cases: &[(f64, &str)] = &[
                (40.0, "40"),
                (0.00035, "0.00035"),
                (1e-7, "1e-7"),
                (1e21, "1e+21"),
                (-0.0, "-0"),
                (0.0, "0"),
                (0.5, "0.5"),
                (1.5e-6, "0.0000015"),
                (1e-6, "0.000001"),
                (9.999999e-7, "9.999999e-7"),
                (1e-5, "0.00001"),
                (123456789.0, "123456789"),
                (2.5e-7, "2.5e-7"),
                (1.2345678901234568e21, "1.2345678901234568e+21"),
                (1e100, "1e+100"),
                (1e-100, "1e-100"),
                (5e-324, "5e-324"),
                (f64::MAX, "1.7976931348623157e+308"),
                (100.0, "100"),
                (40.5, "40.5"),
                (1e20, "100000000000000000000"),
                (-1e-7, "-1e-7"),
                (-1e21, "-1e+21"),
                (1.2345678901234568e18, "1234567890123456800"),
                (0.001, "0.001"),
                (1e-9, "1e-9"),
                (1.234e-7, "1.234e-7"),
                (4577.26, "4577.26"),
                (12345678901234567890.0, "12345678901234567000"),
                (3.0 / 7.0, "0.42857142857142855"),
            ];
            for (value, want) in cases {
                assert_eq!(format_go_f64(*value), *want, "format_go_f64({value:?})");
            }
            // Go rejects both, and the helper degrades to a JSON null.
            assert_eq!(format_go_f64(f64::NAN), "null");
            assert_eq!(format_go_f64(f64::INFINITY), "null");
            assert_eq!(format_go_f64(f64::NEG_INFINITY), "null");
        }

        #[test]
        fn go_float_serialises_with_go_digits() {
            #[derive(Serialize)]
            struct Row {
                #[serde(serialize_with = "go_f64")]
                cost: f64,
                #[serde(serialize_with = "go_f64_opt", skip_serializing_if = "Option::is_none")]
                weight: Option<f64>,
                #[serde(serialize_with = "go_f64_map")]
                detail: std::collections::BTreeMap<String, f64>,
            }
            let mut detail = std::collections::BTreeMap::new();
            detail.insert("pass".to_string(), 1.0);
            detail.insert("rate".to_string(), 0.5);
            let row = Row {
                cost: 40.0,
                weight: Some(2.0),
                detail,
            };
            assert_eq!(
                serde_json::to_string(&row).unwrap(),
                r#"{"cost":40,"weight":2,"detail":{"pass":1,"rate":0.5}}"#
            );
            let row = Row {
                cost: 1e-7,
                weight: None,
                detail: std::collections::BTreeMap::new(),
            };
            assert_eq!(
                serde_json::to_string(&row).unwrap(),
                r#"{"cost":1e-7,"detail":{}}"#
            );
            // `json!` reparses the spliced text, so the JSON type stays a
            // number there too.
            let value = serde_json::json!({ "cost": GoFloat(40.0), "pick": GoFloat(0.5) });
            assert!(value["cost"].is_number() && value["pick"].is_number());
            assert_eq!(
                serde_json::to_string(&value).unwrap(),
                r#"{"cost":40,"pick":0.5}"#
            );
        }

        /// Expected strings measured the same way: `json.Marshal` of a
        /// `time.Time` built with `time.Date(..., nsec, time.UTC)`.
        #[test]
        fn times_render_exactly_like_go() {
            use chrono::{TimeZone, Utc};
            let base = Utc.with_ymd_and_hms(2026, 10, 24, 14, 8, 29).unwrap();
            let cases = [
                (base, "2026-10-24T14:08:29Z"),
                (
                    base + chrono::Duration::milliseconds(500),
                    "2026-10-24T14:08:29.5Z",
                ),
                (
                    base + chrono::Duration::milliseconds(50),
                    "2026-10-24T14:08:29.05Z",
                ),
                (
                    base + chrono::Duration::milliseconds(100),
                    "2026-10-24T14:08:29.1Z",
                ),
                (
                    base + chrono::Duration::milliseconds(10),
                    "2026-10-24T14:08:29.01Z",
                ),
                (
                    base + chrono::Duration::microseconds(123_456),
                    "2026-10-24T14:08:29.123456Z",
                ),
                (
                    base + chrono::Duration::nanoseconds(123_456_789),
                    "2026-10-24T14:08:29.123456789Z",
                ),
                (
                    base + chrono::Duration::nanoseconds(1),
                    "2026-10-24T14:08:29.000000001Z",
                ),
                (Utc.timestamp_opt(0, 0).unwrap(), "1970-01-01T00:00:00Z"),
                (crate::config::zero_time(), "0001-01-01T00:00:00Z"),
                (
                    Utc.timestamp_opt(-1, 500_000_000).unwrap(),
                    "1969-12-31T23:59:59.5Z",
                ),
                (
                    Utc.with_ymd_and_hms(9999, 12, 31, 23, 59, 59).unwrap()
                        + chrono::Duration::nanoseconds(999_999_999),
                    "9999-12-31T23:59:59.999999999Z",
                ),
            ];
            for (at, want) in cases {
                assert_eq!(format_go_time(&at), want, "{want}");
            }
        }

        #[test]
        fn go_time_serialises_and_reads_both_fraction_styles() {
            use chrono::TimeZone;
            let at = chrono::Utc
                .with_ymd_and_hms(2026, 10, 24, 14, 8, 29)
                .unwrap()
                + chrono::Duration::milliseconds(500);
            #[derive(Debug, Serialize, Deserialize, PartialEq)]
            struct Row {
                #[serde(serialize_with = "go_time", deserialize_with = "go_time_deserialize")]
                time: chrono::DateTime<chrono::Utc>,
            }
            // Round trips through the trimmed form.
            let text = serde_json::to_string(&Row { time: at }).unwrap();
            assert_eq!(text, r#"{"time":"2026-10-24T14:08:29.5Z"}"#);
            let back: Row = serde_json::from_str(&text).unwrap();
            assert_eq!(back, Row { time: at });
            // A padded fraction from an older writer still parses, and so does
            // the zero time.
            let padded: Row =
                serde_json::from_str(r#"{"time":"2026-10-24T14:08:29.500Z"}"#).unwrap();
            assert_eq!(padded, Row { time: at });
            let zero: Row = serde_json::from_str(r#"{"time":"0001-01-01T00:00:00Z"}"#).unwrap();
            assert_eq!(zero.time, crate::config::zero_time());
        }
    }
}
