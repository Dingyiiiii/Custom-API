//! Grading: deciding whether a reply satisfies a bench case.
//!
//! The grading is deliberately objective - exact match, required substrings, or
//! a regular expression - so that scoring a channel costs nothing beyond the
//! generation itself and cannot drift the way an LLM judge would.

use regex::Regex;

use super::bench::BenchCase;

/// Decides whether a reply satisfies a bench case.
pub fn grade_case(tc: &BenchCase, reply: &str) -> bool {
    let norm = normalize(reply);
    if norm.is_empty() {
        return false;
    }
    let match_any = |wants: &[String]| -> bool {
        for want in wants {
            if want.is_empty() {
                continue;
            }
            if norm.contains(&normalize(want)) {
                return true;
            }
        }
        false
    };
    if !tc.expect.is_empty() {
        // An exact expectation is satisfied either by a clean exact match or,
        // when the expectation is numeric, by the final number in the reply -
        // models that show their working should not be marked wrong.
        if norm == normalize(&tc.expect) {
            return true;
        }
        if let Some(want) = as_number(&tc.expect) {
            if let Some(got) = last_number(&norm) {
                if nearly_equal(got, want) {
                    return true;
                }
            }
        }
        // A short prose answer may legitimately be wrapped in a sentence. Go
        // measures this in bytes (`len`), which is what the threshold was tuned
        // against, so the byte length is used here too.
        if tc.expect.len() > 3 && norm.contains(&normalize(&tc.expect)) {
            return true;
        }
        return false;
    }
    if !tc.expect_contains.is_empty() {
        if !match_any(&tc.expect_contains) {
            return false;
        }
        // When a numeric expectation is present in the alternatives, prefer the
        // numeric check so that "5" does not match "15".
        for want in &tc.expect_contains {
            if let Some(want) = as_number(want) {
                if let Some(got) = last_number(&norm) {
                    return nearly_equal(got, want);
                }
            }
        }
        return true;
    }
    if !tc.expect_regex.is_empty() {
        // Go compiles per call as well; a case with a broken expression fails
        // instead of panicking so one bad question cannot abort a whole run.
        let Ok(re) = Regex::new(&tc.expect_regex) else {
            return false;
        };
        return re.is_match(reply);
    }
    // A case with no expectation is a smoke test: any non-empty reply passes.
    true
}

/// Prefixes stripped before comparison so that "the answer is 42" matches "42".
const ANSWER_PREFIXES: [&str; 6] = [
    "the answer is",
    "answer:",
    "final answer:",
    "答案是",
    "答案：",
    "答：",
];

/// Punctuation trimmed from both ends of an answer.
const TRIM_CUTSET: &str = " \t\r\n`\"'*.。,，!！?？:：;；";

/// Collapses whitespace, strips answer prefixes, trims punctuation and
/// lowercases.
pub(crate) fn normalize(s: &str) -> String {
    let mut s = s.trim().to_lowercase();
    for prefix in ANSWER_PREFIXES {
        if let Some(index) = s.find(prefix) {
            s = s[index + prefix.len()..].to_string();
        }
    }
    let trimmed = s.trim_matches(|c| TRIM_CUTSET.contains(c));
    trimmed.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Reports whether `c` may appear in a Go `%g` rendering of a finite number.
fn is_number_char(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E')
}

/// Parses a string that is entirely a number.
///
/// Go's version accepts a value when re-rendering it with `%g` reproduces the
/// input, *or* when every character of the input is numeric punctuation. The
/// second clause subsumes the first - a `%g` rendering of a finite value only
/// ever contains `0-9 . + - e E` - so only the character test is kept, with a
/// prefix scan for the value itself. The two therefore agree on oddities like
/// `"1.2.3"`, which both read as `1.2`.
pub(crate) fn as_number(s: &str) -> Option<f64> {
    let trimmed = s.trim().trim_matches(|c| c == '`' || c == '"' || c == '\'');
    let value = scan_float(trimmed)?;
    if !trimmed.is_empty() && trimmed.chars().all(is_number_char) {
        Some(value)
    } else {
        None
    }
}

/// Parses the longest valid float prefix of `s`, the way Go's
/// `fmt.Sscanf(s, "%g", &v)` does: `"1.2.3"` reads as `1.2`, `"."` fails.
fn scan_float(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let mut has_digit = false;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
        has_digit = true;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            has_digit = true;
        }
    }
    if !has_digit {
        return None;
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let before_exponent = i;
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let exponent_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == exponent_start {
            // "1e" is just 1: Go's scanner backtracks over the dangling
            // exponent marker.
            i = before_exponent;
        }
    }
    s[..i].parse::<f64>().ok()
}

/// Returns the final numeric literal in a string.
pub(crate) fn last_number(s: &str) -> Option<f64> {
    // Go's `strings.FieldsFunc` splits on every character that is not part of a
    // number, so the runs it yields are maximal `[0-9.+-]` sequences.
    let mut fields: Vec<&str> = Vec::new();
    let mut start: Option<usize> = None;
    for (index, c) in s.char_indices() {
        let keep = c.is_ascii_digit() || matches!(c, '.' | '-' | '+');
        match (keep, start) {
            (true, None) => start = Some(index),
            (false, Some(from)) => {
                fields.push(&s[from..index]);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        fields.push(&s[from..]);
    }
    for field in fields.iter().rev() {
        let candidate = field.trim_matches(|c| c == '+' || c == '-');
        if candidate.is_empty() || candidate == "." {
            continue;
        }
        if let Some(value) = scan_float(candidate) {
            return Some(value);
        }
    }
    None
}

/// Compares numbers with a tolerance so that `5` and `5.0` agree.
pub(crate) fn nearly_equal(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}
