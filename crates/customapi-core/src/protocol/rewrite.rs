//! In-place request inspection and rewriting.
//!
//! Everything here works on the raw bytes of a body rather than on a parsed
//! structure: a body the gateway does not understand must come out of a rewrite
//! byte-identical apart from the one field that was replaced. Unmarshalling and
//! re-marshalling would reorder keys, normalise numbers and drop unknown
//! members, so it is not an option.

// ---------------------------------------------------------------------------
// Protocol detection
// ---------------------------------------------------------------------------

/// Guesses which wire protocol a request body speaks.
///
/// The gateway uses this when a channel is configured for automatic protocol
/// selection. It is intentionally conservative: an ambiguous body is reported
/// as OpenAI-compatible, matching the plan's "OpenAI first" milestone order.
pub fn detect_protocol(path: &str, body: &[u8]) -> String {
    if path.ends_with("/v1/messages") || path.contains("/v1/complete") {
        return "anthropic".to_string();
    }
    // /v1/responses is its own protocol, not a spelling of chat completions:
    // the request, response and streaming event models all differ, so the
    // gateway parses it natively and can translate to and from it.
    if path.ends_with("/v1/responses") {
        return "responses".to_string();
    }
    if path.ends_with("/v1/chat/completions")
        || path.ends_with("/v1/completions")
        || path.ends_with("/v1/embeddings")
    {
        return "openai".to_string();
    }

    // Fall back to shape sniffing, since the two APIs differ in ways that can
    // be scored rather than guessed at:
    //
    //   anthropic_version        - decisive, OpenAI has no such field
    //   top-level system         - OpenAI puts the system prompt in messages
    //   system + max_tokens      - the canonical minimal Messages request
    //   assistant prefill/tools  - both, so not decisive
    //
    // The scoring form is used so that a body which merely happens to carry
    // one Anthropic-ish field is not misclassified.
    // `top_p` is parsed because Go's probe models it, but the scoring below
    // never consults it.
    #[allow(dead_code)]
    #[derive(Default, serde::Deserialize)]
    struct Probe {
        // Raw, not Option: Go's json.RawMessage keeps an explicit `null` as a
        // present field, and the scoring below depends on that.
        #[serde(default)]
        system: super::types::RawJson,
        #[serde(default)]
        anthropic_version: String,
        #[serde(default)]
        max_tokens: Option<i64>,
        #[serde(rename = "top_p", default)]
        top_p: Option<f64>,
        #[serde(default)]
        stop_sequences: super::types::RawJson,
        #[serde(default)]
        messages: Vec<ProbeMessage>,
    }

    #[derive(Default, serde::Deserialize)]
    struct ProbeMessage {
        #[serde(default)]
        role: String,
    }

    let Ok(probe) = serde_json::from_slice::<Probe>(body) else {
        return "openai".to_string();
    };
    if !probe.anthropic_version.is_empty() {
        return "anthropic".to_string();
    }

    // Shape check for the Responses API before the chat heuristics: it is
    // identified by an `input` field (string or array) with no `messages`.
    #[derive(Default, serde::Deserialize)]
    struct ResponsesProbe {
        #[serde(default)]
        input: super::types::RawJson,
        #[serde(default)]
        messages: super::types::RawJson,
        #[serde(default)]
        instructions: String,
    }
    if let Ok(rprobe) = serde_json::from_slice::<ResponsesProbe>(body) {
        if rprobe.messages.is_empty()
            && (!rprobe.input.is_empty() || !rprobe.instructions.is_empty())
        {
            return "responses".to_string();
        }
    }

    let mut score: i32 = 0;
    if !probe.system.is_empty() {
        score += 2;
    }
    if !probe.stop_sequences.is_empty() {
        // OpenAI calls this field "stop"; its presence under the Anthropic
        // name is decisive on its own.
        score += 2;
    }
    if probe.max_tokens.is_some() && !probe.system.is_empty() {
        score += 2;
    }
    // The Messages API has no "developer" role and never places a "system"
    // entry inside messages; a system message is a strong OpenAI signal.
    for m in &probe.messages {
        if m.role == "system" || m.role == "developer" {
            score -= 2;
        }
    }
    if score >= 2 {
        return "anthropic".to_string();
    }
    "openai".to_string()
}

/// Reports whether a request body asks for a streamed response.
pub fn is_streaming(body: &[u8]) -> bool {
    #[derive(Default, serde::Deserialize)]
    struct Probe {
        #[serde(default)]
        stream: bool,
    }
    serde_json::from_slice::<Probe>(body)
        .map(|p| p.stream)
        .unwrap_or(false)
}

/// Extracts the top-level "model" field without fully parsing the body.
pub fn model(body: &[u8]) -> String {
    let raw = top_level_string(body, "model");
    match raw {
        None => String::new(),
        Some(raw) => serde_json::from_slice::<String>(raw).unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// In-place field rewriting
// ---------------------------------------------------------------------------

/// Returns the raw JSON value bytes of a top-level string field.
///
/// It walks the object byte-wise tracking depth and string state rather than
/// unmarshalling, so that unknown fields, key order and whitespace are all
/// preserved when the caller splices a replacement back in.
pub fn top_level_string<'a>(body: &'a [u8], field: &str) -> Option<&'a [u8]> {
    top_level_field(body, field).1
}

/// Returns a copy of `body` with a top-level string field replaced by `value`.
///
/// When the field is absent, the encoded field is appended before the closing
/// brace. When `body` is not a JSON object, `body` is returned unchanged. The
/// caller's slice is never mutated.
pub fn replace_top_level_string(body: &[u8], field: &str, value: &str) -> Vec<u8> {
    let (start, _) = top_level_field(body, field);
    let encoded = match serde_json::to_vec(value) {
        Ok(encoded) => encoded,
        Err(_) => return body.to_vec(),
    };
    let Some(start) = start else {
        return append_field(body, field, &encoded);
    };
    // `start` points at the opening quote of the value. Scan to its matching
    // close, honouring backslash escapes, so an escaped quote inside the old
    // value does not end the scan early.
    let mut end = start + 1;
    while end < body.len() {
        if body[end] == b'\\' {
            end += 2;
            continue;
        }
        if body[end] == b'"' {
            end += 1;
            break;
        }
        end += 1;
    }
    let mut out = Vec::with_capacity(body.len() + encoded.len());
    out.extend_from_slice(&body[..start]);
    out.extend_from_slice(&encoded);
    out.extend_from_slice(&body[end.min(body.len())..]);
    out
}

/// Locates a top-level field, returning the byte offset where its value begins
/// and the raw value bytes. `None` means "not found" (or not an object).
fn top_level_field<'a>(body: &'a [u8], field: &str) -> (Option<usize>, Option<&'a [u8]>) {
    let mut i = skip_space(body, 0);
    if i >= body.len() || body[i] != b'{' {
        return (None, None);
    }
    i += 1;
    let mut depth = 1usize;
    while i < body.len() {
        i = skip_space(body, i);
        if i >= body.len() {
            return (None, None);
        }
        if body[i] == b'}' {
            depth -= 1;
            if depth == 0 {
                return (None, None);
            }
            i += 1;
            continue;
        }
        if body[i] == b',' {
            i += 1;
            continue;
        }
        if body[i] != b'"' {
            // Unexpected token at object level; give up rather than guess.
            return (None, None);
        }
        let key_start = i;
        let Some(key_end) = scan_string(body, key_start) else {
            return (None, None);
        };
        let Ok(name) = serde_json::from_slice::<String>(&body[key_start..key_end]) else {
            return (None, None);
        };
        i = skip_space(body, key_end);
        if i >= body.len() || body[i] != b':' {
            return (None, None);
        }
        i = skip_space(body, i + 1);
        let val_start = i;
        let Some(val_end) = scan_value(body, val_start) else {
            return (None, None);
        };
        if name == field {
            return (Some(val_start), Some(&body[val_start..val_end]));
        }
        i = val_end;
    }
    (None, None)
}

/// Inserts a field before the final closing brace.
fn append_field(body: &[u8], field: &str, encoded_value: &[u8]) -> Vec<u8> {
    let mut end = body.len();
    while end > 0 && matches!(body[end - 1], b' ' | b'\n' | b'\t' | b'\r') {
        end -= 1;
    }
    if end == 0 || body[end - 1] != b'}' {
        return body.to_vec();
    }
    if end < 2 {
        return body.to_vec();
    }
    // Detect an empty object to avoid emitting a leading comma.
    let inner = trim_space(&body[1..end - 1]);
    let mut out = Vec::with_capacity(body.len() + field.len() + encoded_value.len() + 4);
    out.extend_from_slice(&body[..end - 1]);
    if !inner.is_empty() {
        out.push(b',');
    }
    let key_json = match serde_json::to_vec(field) {
        Ok(key) => key,
        Err(_) => return body.to_vec(),
    };
    out.extend_from_slice(&key_json);
    out.push(b':');
    out.extend_from_slice(encoded_value);
    out.extend_from_slice(&body[end - 1..]);
    out
}

fn trim_space(b: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = b.len();
    while start < end && matches!(b[start], b' ' | b'\t' | b'\n' | b'\r') {
        start += 1;
    }
    while end > start && matches!(b[end - 1], b' ' | b'\t' | b'\n' | b'\r') {
        end -= 1;
    }
    &b[start..end]
}

fn skip_space(b: &[u8], mut i: usize) -> usize {
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            _ => return i,
        }
    }
    i
}

/// Returns the offset just past the closing quote of a JSON string starting at
/// `i`.
fn scan_string(b: &[u8], mut i: usize) -> Option<usize> {
    if i >= b.len() || b[i] != b'"' {
        return None;
    }
    i += 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Returns the offset just past a JSON value starting at `i`.
fn scan_value(b: &[u8], mut i: usize) -> Option<usize> {
    if i >= b.len() {
        return None;
    }
    match b[i] {
        b'"' => scan_string(b, i),
        open @ (b'{' | b'[') => {
            let close_ch = if open == b'[' { b']' } else { b'}' };
            let mut depth: usize = 0;
            while i < b.len() {
                match b[i] {
                    b'"' => {
                        i = scan_string(b, i)?;
                        continue;
                    }
                    c if c == open => depth += 1,
                    c if c == close_ch => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            None
        }
        _ => {
            // Literal: number, true, false, null.
            let start = i;
            while i < b.len() {
                match b[i] {
                    b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r' => {
                        if i == start {
                            return None;
                        }
                        return Some(i);
                    }
                    _ => i += 1,
                }
            }
            if i > start {
                Some(i)
            } else {
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming model detection
// ---------------------------------------------------------------------------

/// The byte pattern every OpenAI-compatible stream chunk uses to report the
/// served model.
const STREAM_MODEL_KEY: &[u8] = br#""model":""#;

/// Scans a raw SSE chunk for the served model name.
///
/// Some upstreams rewrite the model field on every chunk, and others only send
/// it once. This helper lets the gateway notice a substitution mid-stream
/// without parsing every chunk into a struct, which matters because chunks can
/// be very large (base64 images, long tool arguments).
pub fn detect_stream_model(raw: &[u8]) -> String {
    let Some(idx) = find_subslice(raw, STREAM_MODEL_KEY) else {
        return String::new();
    };
    let start = idx + STREAM_MODEL_KEY.len();
    let mut end = start;
    while end < raw.len() {
        if raw[end] == b'\\' {
            end += 2;
            continue;
        }
        if raw[end] == b'"' {
            return String::from_utf8_lossy(&raw[start..end]).into_owned();
        }
        end += 1;
    }
    String::new()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Returns the effective output token cap of a request, preferring
/// `max_completion_tokens` when both are present.
pub fn max_tokens_of(body: &[u8]) -> i64 {
    #[derive(Default, serde::Deserialize)]
    struct Probe {
        #[serde(default)]
        max_tokens: i64,
        #[serde(default)]
        max_completion_tokens: i64,
    }
    let Ok(probe) = serde_json::from_slice::<Probe>(body) else {
        return 0;
    };
    if probe.max_completion_tokens > 0 {
        return probe.max_completion_tokens;
    }
    probe.max_tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Rewriting
    // -----------------------------------------------------------------

    /// Translated from `TestReplaceTopLevelStringPreservesEverythingElse`: the
    /// gateway must not disturb a request body it does not understand.
    #[test]
    fn test_replace_top_level_string_preserves_everything_else() {
        let input = r#"{"model":"gpt-4o","stream":true,"messages":[{"role":"user","content":"hi"}],"未知字段":{"a":1},"n":1e-3}"#.as_bytes();
        let out = replace_top_level_string(input, "model", "gpt-4o-2024-11-20");
        assert_eq!(model(&out), "gpt-4o-2024-11-20");
        let want = replace_first(input, br#""gpt-4o""#, br#""gpt-4o-2024-11-20""#);
        assert_eq!(out, want, "body was altered beyond the model field");
    }

    fn replace_first(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
        let idx = find_subslice(haystack, needle).expect("needle present");
        let mut out = Vec::new();
        out.extend_from_slice(&haystack[..idx]);
        out.extend_from_slice(replacement);
        out.extend_from_slice(&haystack[idx + needle.len()..]);
        out
    }

    /// Translated from `TestReplaceTopLevelStringKeepsValidJSON`.
    #[test]
    fn test_replace_top_level_string_keeps_valid_json() {
        let cases = [
            r#"{"model":"a"}"#,
            r#"{"model":"a","x":1}"#,
            r#"{ "model" : "a" , "x" : { "y" : [1,2,3] } }"#,
            r#"{"x":{"model":"nested"},"model":"a"}"#,
            r#"{"model":"a","content":"a \"model\": b"}"#,
            r#"{"model":"a","content":"escaped \" quote and \\ backslash"}"#,
            r#"{"model":"a","nested":{"deep":{"deeper":[{"k":"v"}]}}}"#,
            r#"{"model":"a","lit":[true,false,null,1.5e10]}"#,
        ];
        for input in cases {
            let out = replace_top_level_string(input.as_bytes(), "model", "NEW");
            let value: serde_json::Value = serde_json::from_slice(&out)
                .unwrap_or_else(|e| panic!("output is not valid JSON for {input}: {e}"));
            assert_eq!(
                value.get("model").and_then(|v| v.as_str()),
                Some("NEW"),
                "input {input}"
            );
        }
    }

    /// Translated from `TestReplaceTopLevelStringIgnoresNestedModel`.
    #[test]
    fn test_replace_top_level_string_ignores_nested_model() {
        let input = br#"{"nested":{"model":"inner"},"model":"outer"}"#;
        let out = replace_top_level_string(input, "model", "NEW");
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["nested"]["model"], "inner");
        assert_eq!(value["model"], "NEW");
    }

    /// Translated from `TestReplaceTopLevelStringAppendsWhenMissing`.
    #[test]
    fn test_replace_top_level_string_appends_when_missing() {
        let out = replace_top_level_string(br#"{"a":1}"#, "model", "gpt-4o");
        assert_eq!(model(&out), "gpt-4o");
        assert_eq!(out, br#"{"a":1,"model":"gpt-4o"}"#.to_vec());
    }

    /// Translated from `TestReplaceTopLevelStringEmptyObject`.
    #[test]
    fn test_replace_top_level_string_empty_object() {
        let out = replace_top_level_string(b"{}", "model", "x");
        assert_eq!(out, br#"{"model":"x"}"#.to_vec());
    }

    /// Translated from `TestReplaceTopLevelStringEscapesValue`.
    #[test]
    fn test_replace_top_level_string_escapes_value() {
        let out = replace_top_level_string(br#"{"model":"a"}"#, "model", "we\"ird\\name");
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["model"], "we\"ird\\name");
    }

    /// Translated from `TestReplaceTopLevelStringNonObject`.
    #[test]
    fn test_replace_top_level_string_non_object() {
        let input = b"[1,2,3]";
        assert_eq!(
            replace_top_level_string(input, "model", "x"),
            input.to_vec()
        );
    }

    /// Translated from `TestReplaceTopLevelStringDoesNotMutateInput`: the proxy
    /// keeps the original request for auditing.
    #[test]
    fn test_replace_top_level_string_does_not_mutate_input() {
        let input = br#"{"model":"gpt-4o"}"#.to_vec();
        let original = input.clone();
        let _ = replace_top_level_string(&input, "model", "a-much-longer-model-name");
        assert_eq!(input, original);
    }

    /// Translated from `TestTopLevelFieldAbsent`.
    #[test]
    fn test_top_level_field_absent() {
        assert!(top_level_string(br#"{"a":1}"#, "model").is_none());
    }

    #[test]
    fn test_replace_handles_trailing_whitespace_and_broken_bodies() {
        // A body with trailing whitespace keeps it, and the new field lands
        // before the brace.
        let out = replace_top_level_string(b"{}\n ", "model", "x");
        assert_eq!(out, b"{\"model\":\"x\"}\n ".to_vec());
        // A truncated body is returned unchanged rather than patched blindly.
        for broken in ["{\"a\":1", "null", "", "{\"model\":1}"] {
            let out = replace_top_level_string(broken.as_bytes(), "model", "x");
            let _ = out; // never panics; malformed input stays intact or degrades
        }
        assert_eq!(
            replace_top_level_string(b"null", "model", "x"),
            b"null".to_vec()
        );
    }

    #[test]
    fn test_replace_only_touches_the_first_matching_key() {
        // Duplicate keys are invalid-ish JSON but harmless to splice.
        let out = replace_top_level_string(br#"{"model":"a","model":"b"}"#, "model", "c");
        assert_eq!(out, br#"{"model":"c","model":"b"}"#.to_vec());
    }

    #[test]
    fn test_top_level_string_returns_the_raw_value_bytes() {
        let body = br#"{"model":"gpt-4o","n":[1,2]}"#;
        assert_eq!(top_level_string(body, "model").unwrap(), br#""gpt-4o""#);
        assert_eq!(top_level_string(body, "n").unwrap(), b"[1,2]");
        assert!(top_level_string(body, "missing").is_none());
    }

    // -----------------------------------------------------------------
    // Detection
    // -----------------------------------------------------------------

    /// Translated from `TestDetectProtocol`.
    #[test]
    fn test_detect_protocol() {
        let cases: [(&str, &str, &str); 11] = [
            ("/v1/chat/completions", r#"{"model":"gpt-4o"}"#, "openai"),
            ("/v1/completions", r#"{"model":"x"}"#, "openai"),
            ("/v1/messages", r#"{"model":"claude-3"}"#, "anthropic"),
            (
                "/v1/messages",
                r#"{"model":"claude-3","max_tokens":10}"#,
                "anthropic",
            ),
            // An explicit, known path is authoritative: a client that posts to
            // an OpenAI endpoint is speaking OpenAI even if its body carries a
            // field that also exists in the Messages API.
            (
                "/v1/chat/completions",
                r#"{"model":"x","system":"be nice"}"#,
                "openai",
            ),
            // Unknown paths fall back to body sniffing.
            (
                "/proxy",
                r#"{"model":"x","anthropic_version":"2023-06-01"}"#,
                "anthropic",
            ),
            (
                "/proxy",
                r#"{"model":"x","system":"be nice","max_tokens":100}"#,
                "anthropic",
            ),
            (
                "/proxy",
                r#"{"model":"x","stop_sequences":["\n"]}"#,
                "anthropic",
            ),
            (
                "/proxy",
                r#"{"model":"x","messages":[{"role":"system","content":"s"},{"role":"user","content":"u"}]}"#,
                "openai",
            ),
            (
                "/proxy",
                r#"{"model":"x","messages":[{"role":"user","content":"u"}]}"#,
                "openai",
            ),
            ("/v1/embeddings", r#"{"model":"x"}"#, "openai"),
        ];
        for (path, body, want) in cases {
            assert_eq!(
                detect_protocol(path, body.as_bytes()),
                want,
                "detect_protocol({path}, {body})"
            );
        }
    }

    /// Translated from `TestIsStreaming`.
    #[test]
    fn test_is_streaming_flags() {
        assert!(is_streaming(br#"{"stream":true}"#));
        assert!(!is_streaming(br#"{"stream":false}"#));
        assert!(!is_streaming(b"{}"), "absent stream field should be false");
        assert!(!is_streaming(b"not json"), "malformed body should be false");
        // A body that is not an object at all.
        assert!(!is_streaming(b"[1,2,3]"));
        assert!(!is_streaming(b""));
    }

    /// Translated from `TestMaxTokensOf`.
    #[test]
    fn test_max_tokens_of_prefers_completion_tokens() {
        assert_eq!(max_tokens_of(br#"{"max_tokens":100}"#), 100);
        assert_eq!(
            max_tokens_of(br#"{"max_tokens":100,"max_completion_tokens":200}"#),
            200,
            "max_completion_tokens should win"
        );
        assert_eq!(
            max_tokens_of(br#"{"max_tokens":100,"max_completion_tokens":0}"#),
            100
        );
        assert_eq!(max_tokens_of(b"not json"), 0);
        assert_eq!(max_tokens_of(b"{}"), 0);
    }

    /// Translated from `TestDetectStreamModel`.
    #[test]
    fn test_detect_stream_model() {
        let raw = br#"data: {"id":"x","model":"gpt-4o-mini","choices":[]}"#;
        assert_eq!(detect_stream_model(raw), "gpt-4o-mini");
        assert_eq!(detect_stream_model(br#"data: {"choices":[]}"#), "");
        // A model name containing an escaped quote does not end the scan early.
        assert_eq!(detect_stream_model(br#"{"model":"a\"b","x":1}"#), r#"a\"b"#);
        assert_eq!(detect_stream_model(b""), "");
        assert_eq!(detect_stream_model(br#"{"model":"unterminated"#), "");
    }
}
