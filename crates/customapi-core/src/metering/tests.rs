//! Tests for the metering module.
//!
//! Every case in `internal/metering/metering_test.go` is translated one for one
//! and keeps its Go name in snake_case; the additional tests at the end cover
//! Rust-side boundaries (empty input, invalid UTF-8, huge input, JSON shape).

use super::*;
use crate::protocol::responses::{
    ResponsesContentPart, ResponsesOutputItem, ResponsesRequest, ResponsesResponse, ResponsesTool,
};
use crate::protocol::types::{
    AnthropicBlock, AnthropicMessage, AnthropicRequest, AnthropicResponse, ChatMessage,
    ChatRequest, ChatResponse, Choice, Content, ContentPart, FunctionCall, RawJson, Tool, ToolCall,
    ToolFunction, Usage as WireUsage,
};

// ---------------------------------------------------------------------------
// Tokenizer calibration
// ---------------------------------------------------------------------------

/// Translated from `TestEstimatorAccuracy`.
///
/// The estimator is checked against token counts produced by real tokenizers
/// (recorded offline with tiktoken o200k_base / cl100k_base and the Anthropic
/// tokenizer). The tolerance is deliberately generous because the estimator is
/// an approximation; what matters for the customs check is that a deviation
/// large enough to be worth an upstream's while to fake is far larger than the
/// estimator's error.
#[test]
fn estimator_accuracy() {
    let cases: [(&str, &str, &str, i64); 6] = [
        (
            "english prose",
            "gpt-4o",
            "The quick brown fox jumps over the lazy dog. This sentence is used to test tokenizer behaviour across many implementations.",
            26,
        ),
        (
            "chinese prose",
            "gpt-4o",
            "哈希函数是一种把任意长度的输入映射为固定长度输出的函数。它在密码学和数据结构中有广泛的应用。",
            40,
        ),
        (
            "python code",
            "gpt-4o",
            "def reverse(s: str) -> str:\n    return s[::-1]\n\nif __name__ == '__main__':\n    print(reverse('hello'))\n",
            34,
        ),
        (
            "json payload",
            "gpt-4o",
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"temperature":0.7}"#,
            22,
        ),
        (
            "numbers",
            "gpt-4o",
            "1234567890 9876543210 3.14159265358979 2.718281828459045",
            22,
        ),
        (
            "cl100k english",
            "gpt-4",
            "The quick brown fox jumps over the lazy dog. This sentence is used to test tokenizer behaviour across many implementations.",
            27,
        ),
    ];

    for (name, model, text, actual) in cases {
        let est = Estimator::for_model(model);
        let got = est.estimate(text);
        let err = (got - actual).abs() as f64 / actual as f64;
        // 50% is the acceptance bound. The worst measured case is a digit-dense
        // string at ~46%; ordinary prose stays under 20%. The bound is
        // deliberately loose because the estimator is vocabulary-free by design -
        // what matters is that it is far tighter than the deviation the metering
        // check must catch.
        assert!(
            err <= 0.50,
            "{name}: estimate {got} too far from actual {actual} ({:.1}%)",
            err * 100.0
        );
    }
}

/// Pins the exact estimate of each family for a battery of strings.
///
/// The expected numbers were produced by the Go implementation (a probe test
/// compiled against the unmodified `internal/metering` package), so this test
/// fails if a ratio, the rounding, the word-boundary blend or the character
/// classification drifts away from the behaviour baseline.
#[test]
fn estimator_calibration_parity() {
    // (text, o200k, claude, cl100k, generic)
    let cases: [(&str, i64, i64, i64, i64); 17] = [
        (
            "The quick brown fox jumps over the lazy dog. This sentence is used to test tokenizer behaviour across many implementations.",
            29, 28, 29, 29,
        ),
        (
            "哈希函数是一种把任意长度的输入映射为固定长度输出的函数。它在密码学和数据结构中有广泛的应用。",
            46, 53, 62, 48,
        ),
        (
            "def reverse(s: str) -> str:\n    return s[::-1]\n\nif __name__ == '__main__':\n    print(reverse('hello'))\n",
            24, 24, 25, 24,
        ),
        (
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"temperature":0.7}"#,
            22, 23, 25, 24,
        ),
        ("1234567890 9876543210 3.14159265358979 2.718281828459045", 12, 12, 13, 12),
        ("word ", 1, 1, 2, 1),
        ("hello world", 3, 3, 3, 3),
        ("a", 1, 1, 1, 1),
        ("中", 1, 1, 1, 1),
        ("中文文本", 4, 5, 5, 4),
        ("hello\tworld\n\nline", 4, 4, 4, 4),
        ("café naïve", 3, 3, 3, 3),
        // Decomposed combining marks: Go classifies them as letters, not
        // punctuation, and the estimate follows.
        ("cafe\u{0301} nai\u{0308}ve", 3, 3, 4, 4),
        ("🎉 emoji 👍 ok", 6, 5, 5, 5),
        ("a b c", 2, 2, 2, 2),
        ("a b", 1, 1, 1, 1),
        ("AbC 123 xyz", 3, 3, 3, 3),
    ];
    for (text, o200k, claude, cl100k, generic) in cases {
        assert_eq!(
            Estimator::new(FAMILY_O200K).estimate(text),
            o200k,
            "o200k {text:?}"
        );
        assert_eq!(
            Estimator::new(FAMILY_CLAUDE).estimate(text),
            claude,
            "claude {text:?}"
        );
        assert_eq!(
            Estimator::new(FAMILY_CL100K).estimate(text),
            cl100k,
            "cl100k {text:?}"
        );
        assert_eq!(
            Estimator::new(FAMILY_GENERIC).estimate(text),
            generic,
            "generic {text:?}"
        );
    }
    // Go charges an invalid byte as three bytes of U+FFFD classified as
    // punctuation, i.e. 2 tokens for two stray bytes.
    assert_eq!(
        Estimator::new(FAMILY_O200K).estimate_bytes(&[0xFF, 0xFE]),
        2
    );
}

/// Translated from `TestEstimatorMonotonic`.
#[test]
fn estimator_monotonic() {
    let est = Estimator::for_model("gpt-4o");
    let mut prev = 0;
    for n in [1usize, 10, 100, 1000, 10000] {
        let got = est.estimate(&"word ".repeat(n));
        assert!(
            got >= prev,
            "estimate decreased with more input: {got} < {prev} at n={n}"
        );
        prev = got;
    }
}

/// Translated from `TestEstimatorEmptyAndSmall`.
#[test]
fn estimator_empty_and_small() {
    let est = Estimator::for_model("gpt-4o");
    assert_eq!(est.estimate(""), 0, "empty text should be 0 tokens");
    assert!(
        est.estimate("a") >= 1,
        "single character should be at least 1 token"
    );
    assert!(
        est.estimate("中") >= 1,
        "single CJK character should be at least 1 token"
    );
}

/// Translated from `TestFamilyForModel`.
#[test]
fn family_for_model() {
    let cases: [(&str, &str); 13] = [
        ("gpt-4o", FAMILY_O200K),
        ("gpt-4o-mini", FAMILY_O200K),
        ("gpt-4.1", FAMILY_O200K),
        ("o3-mini", FAMILY_O200K),
        ("gpt-4", FAMILY_CL100K),
        ("gpt-3.5-turbo", FAMILY_CL100K),
        ("text-embedding-3-small", FAMILY_CL100K),
        ("claude-3-5-sonnet", FAMILY_CLAUDE),
        ("claude-opus-4", FAMILY_CLAUDE),
        ("gemini-2.0-flash", FAMILY_GEMINI),
        ("deepseek-chat", FAMILY_GENERIC),
        ("qwen-max", FAMILY_GENERIC),
        ("some-unknown-model", FAMILY_GENERIC),
    ];
    for (model, want) in cases {
        assert_eq!(family_for(model), want, "FamilyFor({model:?})");
    }
}

/// Translated from `TestCJKCostsMoreThanLatin`, which documents the property the
/// metering check relies on: the estimator must not treat byte length as a proxy
/// for token count.
#[test]
fn cjk_costs_more_than_latin() {
    let est = Estimator::for_model("gpt-4o");
    let latin = est.estimate(&"a".repeat(30)); // 30 bytes
    let cjk = est.estimate(&"中".repeat(10)); // 30 bytes, 10 chars
    assert!(
        cjk > latin,
        "CJK text (30 bytes/10 chars) should cost more tokens than Latin (30 bytes): {cjk} vs {latin}"
    );
}

// ---------------------------------------------------------------------------
// Request counting
// ---------------------------------------------------------------------------

fn text_content(text: &str) -> Content {
    Content::from_parts(vec![ContentPart {
        r#type: "text".to_string(),
        text: text.to_string(),
        ..ContentPart::default()
    }])
}

/// Translated from `TestCountRequestBasic`.
#[test]
fn count_request_basic() {
    let req = ChatRequest {
        model: "gpt-4o".to_string(),
        messages: vec![
            ChatMessage {
                role: "system".to_string(),
                content: text_content("You are a helpful assistant."),
                ..ChatMessage::default()
            },
            ChatMessage {
                role: "user".to_string(),
                content: text_content("What is 2+2?"),
                ..ChatMessage::default()
            },
        ],
        ..ChatRequest::default()
    };
    let (n, fam) = count_request("gpt-4o", Some(&req));
    assert!(n > 0, "expected a positive prompt count, got {n}");
    assert_eq!(fam, FAMILY_O200K);
    // Two messages plus system text should be well under 40 tokens.
    assert!(n <= 40, "prompt count suspiciously high: {n}");
}

/// Translated from `TestCountRequestNil`.
#[test]
fn count_request_nil() {
    assert_eq!(count_request("gpt-4o", None).0, 0);
    assert_eq!(count_request_anthropic("claude-3", None).0, 0);
    assert_eq!(count_request_responses("gpt-4o", None).0, 0);
    assert_eq!(count_response("gpt-4o", None), 0);
    assert_eq!(count_response_anthropic("claude-3", None), 0);
    assert_eq!(count_response_responses("gpt-4o", None), 0);
}

/// Translated from `TestCountRequestToolsAddTokens`.
#[test]
fn count_request_tools_add_tokens() {
    let base = ChatRequest {
        model: "gpt-4o".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: text_content("hi"),
            ..ChatMessage::default()
        }],
        ..ChatRequest::default()
    };
    let with_tools = ChatRequest {
        model: base.model.clone(),
        messages: base.messages.clone(),
        tools: vec![Tool {
            r#type: "function".to_string(),
            function: ToolFunction {
                name: "get_weather".to_string(),
                description: "Get the current weather for a city, returning temperature, humidity and conditions.".to_string(),
                parameters: Some(RawJson::new(
                    r#"{"type":"object","properties":{"city":{"type":"string","description":"The city name"}},"required":["city"]}"#,
                )),
                strict: None,
            },
        }],
        ..ChatRequest::default()
    };
    let n1 = count_request("gpt-4o", Some(&base)).0;
    let n2 = count_request("gpt-4o", Some(&with_tools)).0;
    assert!(
        n2 > n1,
        "tool schema should increase the prompt count: {n1} -> {n2}"
    );
}

/// Translated from `TestCountRequestAnthropic`.
#[test]
fn count_request_anthropic_basic() {
    let req = AnthropicRequest {
        model: "claude-3-5-sonnet".to_string(),
        max_tokens: 100,
        system: Some(text_content("You are terse.")),
        messages: vec![AnthropicMessage {
            role: "user".to_string(),
            content: Content::from_raw_str("\"Explain hashing.\""),
        }],
        ..AnthropicRequest::default()
    };
    let (n, fam) = count_request_anthropic("claude-3-5-sonnet", Some(&req));
    assert!(n > 0, "expected a positive count, got {n}");
    assert_eq!(fam, FAMILY_CLAUDE);
}

/// Translated from `TestCountResponse`.
#[test]
fn count_response_basic() {
    let resp = ChatResponse {
        choices: vec![Choice {
            message: ChatMessage {
                content: text_content("Hello world, this is a test response."),
                ..ChatMessage::default()
            },
            ..Choice::default()
        }],
        ..ChatResponse::default()
    };
    let n = count_response("gpt-4o", Some(&resp));
    assert!(n > 0, "expected a positive count, got {n}");
    assert!(n <= 30, "count suspiciously high: {n}");
    assert_eq!(count_response("gpt-4o", None), 0);
}

/// Translated from `TestCountResponseCountsReasoningAndToolCalls`.
#[test]
fn count_response_counts_reasoning_and_tool_calls() {
    let plain = ChatResponse {
        choices: vec![Choice {
            message: ChatMessage {
                content: text_content("answer"),
                ..ChatMessage::default()
            },
            ..Choice::default()
        }],
        ..ChatResponse::default()
    };
    let rich = ChatResponse {
        choices: vec![Choice {
            message: ChatMessage {
                content: text_content("answer"),
                reasoning_content: "thinking about the problem ".repeat(20),
                tool_calls: vec![ToolCall {
                    function: FunctionCall {
                        name: "search".to_string(),
                        arguments: r#"{"query":"a long query string here"}"#.to_string(),
                    },
                    ..ToolCall::default()
                }],
                ..ChatMessage::default()
            },
            ..Choice::default()
        }],
        ..ChatResponse::default()
    };
    let n1 = count_response("gpt-4o", Some(&plain));
    let n2 = count_response("gpt-4o", Some(&rich));
    assert!(
        n2 > n1,
        "reasoning and tool calls should add tokens: {n1} -> {n2}"
    );
}

/// Translated from `TestCountResponseAnthropic`.
#[test]
fn count_response_anthropic_basic() {
    let resp = AnthropicResponse {
        content: vec![
            AnthropicBlock {
                r#type: "thinking".to_string(),
                thinking: "let me consider".to_string(),
                ..AnthropicBlock::default()
            },
            AnthropicBlock {
                r#type: "text".to_string(),
                text: "The answer is 42.".to_string(),
                ..AnthropicBlock::default()
            },
            AnthropicBlock {
                r#type: "tool_use".to_string(),
                name: "calc".to_string(),
                input: Some(RawJson::new(r#"{"expr":"6*7"}"#)),
                ..AnthropicBlock::default()
            },
        ],
        ..AnthropicResponse::default()
    };
    let n = count_response_anthropic("claude-3-5-sonnet", Some(&resp));
    assert!(n > 0, "expected a positive count, got {n}");
    assert_eq!(count_response_anthropic("claude-3", None), 0);
}

// ---------------------------------------------------------------------------
// Usage comparison — the anti-inflation check
// ---------------------------------------------------------------------------

/// Translated from `TestCompareConsistent`.
#[test]
fn compare_consistent() {
    let u = Usage {
        prompt_tokens: 100,
        completion_tokens: 50,
        total_tokens: 150,
        upstream_prompt_tokens: 103,
        upstream_completion_tokens: 51,
        upstream_reported: true,
        ..Usage::default()
    };
    let d = u.compare(0.05);
    assert!(d.verifiable, "expected a verifiable comparison");
    assert!(
        !d.over_threshold,
        "a 3% deviation should be within a 5% tolerance: {d:?}"
    );
}

/// Translated from `TestCompareDetectsInflation`: the core "防虚报" test.
#[test]
fn compare_detects_inflation() {
    let u = Usage {
        prompt_tokens: 1000,
        completion_tokens: 500,
        total_tokens: 1500,
        // The upstream claims to have processed 50% more than it did.
        upstream_prompt_tokens: 1500,
        upstream_completion_tokens: 750,
        upstream_reported: true,
        ..Usage::default()
    };
    let d = u.compare(0.05);
    assert!(
        d.verifiable && d.over_threshold,
        "inflation not detected: {d:?}"
    );
    assert_eq!(d.direction, "over");
    assert!(
        (d.prompt_delta - 0.5).abs() <= 1e-9,
        "prompt delta = {}, want 0.5",
        d.prompt_delta
    );
}

/// Translated from `TestCompareDetectsUnderBilling`.
#[test]
fn compare_detects_under_billing() {
    let u = Usage {
        prompt_tokens: 1000,
        completion_tokens: 1000,
        upstream_prompt_tokens: 100,
        upstream_completion_tokens: 100,
        upstream_reported: true,
        ..Usage::default()
    };
    let d = u.compare(0.05);
    assert!(
        d.over_threshold && d.direction == "under",
        "under-billing not classified correctly: {d:?}"
    );
}

/// Translated from `TestCompareNotReported`.
#[test]
fn compare_not_reported() {
    let u = Usage {
        prompt_tokens: 100,
        completion_tokens: 50,
        upstream_reported: false,
        ..Usage::default()
    };
    let d = u.compare(0.05);
    assert!(
        !d.verifiable,
        "an upstream that reported nothing cannot be verified"
    );
    assert!(
        !d.over_threshold,
        "an unverifiable request must never be flagged"
    );
    // The floor is not even recorded, so the field stays out of the ledger.
    assert_eq!(d.min_absolute, 0);
    assert!(d.direction.is_empty());
}

/// Translated from `TestCompareZeroActual`: guards the division-by-zero path. An
/// upstream billing for tokens when the gateway counted nothing is maximally
/// suspicious.
#[test]
fn compare_zero_actual() {
    let u = Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        upstream_prompt_tokens: 500,
        upstream_completion_tokens: 500,
        upstream_reported: true,
        ..Usage::default()
    };
    let d = u.compare(0.05);
    assert!(
        d.over_threshold,
        "billing for content the gateway never saw must be flagged: {d:?}"
    );
    assert!(
        d.worst.is_finite() && !d.worst.is_nan(),
        "deviation must stay finite for JSON: {d:?}"
    );
    assert_eq!(d.worst, MAX_FINITE_DELTA);
}

/// Translated from `TestCompareIsJSONSafe`, which verifies that a deviation
/// always round-trips since it is persisted in the audit ledger.
#[test]
fn compare_is_json_safe() {
    let u = Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        upstream_prompt_tokens: 1,
        upstream_completion_tokens: 1,
        upstream_reported: true,
        ..Usage::default()
    };
    let d = u.compare(0.05);
    let encoded = serde_json::to_string(&d).expect("deviation is not JSON serialisable");
    let decoded: Deviation = serde_json::from_str(&encoded).expect("deviation does not decode");
    assert_eq!(decoded.prompt_delta, d.prompt_delta);
    assert_eq!(decoded.worst, d.worst);
}

/// Translated from `TestToleranceIsRelative`, which verifies the check is
/// proportional, not absolute.
#[test]
fn tolerance_is_relative() {
    let small = Usage {
        prompt_tokens: 10,
        upstream_prompt_tokens: 11,
        upstream_reported: true,
        ..Usage::default()
    };
    let large = Usage {
        prompt_tokens: 10000,
        upstream_prompt_tokens: 11000,
        upstream_reported: true,
        ..Usage::default()
    };
    assert!(
        !small.compare(0.2).over_threshold,
        "a 10% deviation on a small request should pass a 20% tolerance"
    );
    assert!(
        large.compare(0.05).over_threshold,
        "a 10% deviation on a large request should fail a 5% tolerance"
    );
}

// ---------------------------------------------------------------------------
// Counter
// ---------------------------------------------------------------------------

/// Translated from `TestCounterUsagePrefersLocalRecount`.
#[test]
fn counter_usage_prefers_local_recount() {
    let mut c = Counter::new("gpt-4o");
    c.add_prompt_text("hello world, this is a test");
    c.add_completion_text("a reply");
    c.set_upstream(
        WireUsage {
            prompt_tokens: 999,
            completion_tokens: 888,
            total_tokens: 1887,
            ..WireUsage::default()
        },
        true,
    );
    let u = c.usage();
    assert_ne!(
        u.prompt_tokens, 999,
        "the counter must not adopt the upstream's claim as its own count"
    );
    assert_eq!(
        u.upstream_prompt_tokens, 999,
        "upstream claim not retained: {u:?}"
    );
    assert_eq!(u.method, "upstream");
}

/// Translated from `TestCounterFallsBackToUpstreamWhenNothingCountable`, which
/// documents the deliberate anti-false-positive behaviour.
#[test]
fn counter_falls_back_to_upstream_when_nothing_countable() {
    let mut c = Counter::new("gpt-4o");
    c.add_prompt_text("question");
    c.set_upstream(
        WireUsage {
            prompt_tokens: 10,
            completion_tokens: 77,
            total_tokens: 87,
            ..WireUsage::default()
        },
        true,
    );
    let u = c.usage();
    assert_eq!(
        u.completion_tokens, 77,
        "expected the upstream completion count to be used as a floor, got {u:?}"
    );
}

/// Translated from `TestCounterStreamMethod`.
#[test]
fn counter_stream_method() {
    let mut c = Counter::new("gpt-4o");
    c.mark_stream_accumulated();
    assert_eq!(c.usage().method, "stream_chunks");
    // A reported upstream usage outranks the stream marker.
    c.set_upstream(WireUsage::default(), true);
    c.mark_stream_accumulated();
    assert_eq!(c.usage().method, "upstream");
}

/// Translated from `TestUsageTotalConsistency`.
#[test]
fn usage_total_consistency() {
    let mut c = Counter::new("gpt-4o");
    c.add_prompt_text("a b c d e f g h i j");
    c.add_completion_text("x y z");
    let u = c.usage();
    assert_eq!(
        u.total_tokens,
        u.prompt_tokens + u.completion_tokens,
        "total mismatch: {u:?}"
    );
}

// ---------------------------------------------------------------------------
// Absolute floor
// ---------------------------------------------------------------------------

/// Translated from `TestShortResponseNoiseIsNotFlagged`, the regression test for
/// a real false positive: a five-token reply claimed as eight is +60% in
/// relative terms, which a purely relative check calls fraud.
#[test]
fn short_response_noise_is_not_flagged() {
    let u = Usage {
        prompt_tokens: 5,
        completion_tokens: 5,
        total_tokens: 10,
        upstream_prompt_tokens: 8,
        upstream_completion_tokens: 8,
        upstream_reported: true,
        ..Usage::default()
    };
    let d = u.compare(0.25);
    assert!(
        d.worst > 0.25,
        "this case is meant to be relatively large: {d:?}"
    );
    assert_eq!(d.worst_abs, 3);
    assert!(
        !d.over_threshold,
        "a 3-token difference on a 5-token reply must not be flagged: {d:?}"
    );
}

/// Translated from `TestLargeDeviationIsStillFlagged`, which verifies the floor
/// does not blunt the check.
#[test]
fn large_deviation_is_still_flagged() {
    let u = Usage {
        prompt_tokens: 500,
        completion_tokens: 500,
        upstream_prompt_tokens: 1500,
        upstream_completion_tokens: 1500,
        upstream_reported: true,
        ..Usage::default()
    };
    let d = u.compare(0.25);
    assert!(d.over_threshold, "a 200% deviation must be flagged: {d:?}");
    assert_eq!(d.worst_abs, 1000);
}

/// Translated from `TestFloorBoundaryIsInclusive`, which verifies a deviation
/// exactly at the floor and exactly at the tolerance is reported.
#[test]
fn floor_boundary_is_inclusive() {
    let u = Usage {
        prompt_tokens: 100,
        completion_tokens: 100,
        upstream_prompt_tokens: 108,
        upstream_completion_tokens: 100,
        upstream_reported: true,
        ..Usage::default()
    };
    // 8 tokens absolute was the floor before it became configurable; with an
    // explicit floor of 8 and a tolerance of 0.05 this is exactly on both.
    let d = u.compare_with(0.05, 8);
    assert!(
        d.over_threshold,
        "a deviation exactly at both bounds should be reported: {d:?}"
    );
    let d = u.compare_with(0.05, 9);
    assert!(
        !d.over_threshold,
        "one token under the floor must not be reported: {d:?}"
    );
    assert_eq!(d.min_absolute, 9);
}

/// The relative bound is strict and the absolute floor is inclusive: a deviation
/// exactly at the tolerance is noise, one just above it is evidence, and neither
/// counts unless the floor is met.
#[test]
fn relative_tolerance_boundary_is_exclusive_above_the_floor() {
    let usage = |upstream_prompt: i64| Usage {
        prompt_tokens: 100,
        completion_tokens: 100,
        upstream_prompt_tokens: upstream_prompt,
        upstream_completion_tokens: 100,
        upstream_reported: true,
        ..Usage::default()
    };

    // 110 vs 100 is a relative deviation of exactly 0.1; 10 tokens clears the
    // floor of 8, so only the relative comparison decides.
    let exactly = usage(110).compare_with(0.1, 8);
    assert!(
        !exactly.over_threshold,
        "equal to the tolerance is not over: {exactly:?}"
    );
    let just_over = usage(110).compare_with(0.0999, 8);
    assert!(
        just_over.over_threshold,
        "just above the tolerance is over: {just_over:?}"
    );
    assert_eq!(just_over.direction, "over");

    // 4 tokens is 4%: above a zero tolerance, but below the absolute floor.
    let small = usage(104).compare_with(0.0, 8);
    assert!(
        !small.over_threshold,
        "the absolute floor must still apply: {small:?}"
    );
    assert_eq!(small.worst_abs, 4);
}

/// Translated from `TestZeroActualWithFloor`, which verifies an upstream billing
/// for a request the gateway counted as empty is still caught.
#[test]
fn zero_actual_with_floor() {
    let u = Usage {
        upstream_prompt_tokens: 500,
        upstream_completion_tokens: 500,
        upstream_reported: true,
        ..Usage::default()
    };
    let d = u.compare(0.25);
    assert!(
        d.over_threshold,
        "billing 1000 tokens for an empty exchange must be flagged: {d:?}"
    );
}

// ---------------------------------------------------------------------------
// Rust-side additions
// ---------------------------------------------------------------------------

/// The calibration table must stay byte-identical to the Go one; a silent edit
/// here would change the measured error characteristics.
#[test]
fn family_ratios_are_copied_verbatim() {
    let cases: [(&str, f64, f64, f64, f64, f64); 5] = [
        (FAMILY_CL100K, 0.255, 0.36, 0.40, 0.5, 1.35),
        (FAMILY_O200K, 0.215, 0.33, 0.36, 0.5, 1.00),
        (FAMILY_CLAUDE, 0.220, 0.34, 0.38, 0.5, 1.15),
        (FAMILY_GEMINI, 0.240, 0.35, 0.39, 0.5, 1.05),
        (FAMILY_GENERIC, 0.240, 0.35, 0.38, 0.5, 1.05),
    ];
    for (family, ascii, digits, punct, newline, cjk_scale) in cases {
        let r = ratios_for(family);
        assert_eq!(r.ascii, ascii, "{family} ascii");
        assert_eq!(r.digits, digits, "{family} digits");
        assert_eq!(r.punct, punct, "{family} punct");
        assert_eq!(r.newline, newline, "{family} newline");
        assert_eq!(r.cjk_scale, cjk_scale, "{family} cjk_scale");
    }
    // Unknown families fall through to the generic table.
    assert_eq!(ratios_for("something-new"), ratios_for(FAMILY_GENERIC));
    assert_eq!(CJK_RATIO, 1.0);
}

#[test]
fn words_per_token_per_family() {
    assert_eq!(words_per_token(FAMILY_O200K), 1.30);
    assert_eq!(words_per_token(FAMILY_CL100K), 1.10);
    assert_eq!(words_per_token(FAMILY_CLAUDE), 1.20);
    assert_eq!(words_per_token(FAMILY_GEMINI), 1.15);
    assert_eq!(words_per_token(FAMILY_GENERIC), 1.15);
    assert_eq!(words_per_token("unknown"), 1.15);
}

#[test]
fn only_gemini_uses_sentence_piece_delimiters() {
    assert!(sentence_piece_delimiter(FAMILY_GEMINI));
    for family in [FAMILY_O200K, FAMILY_CL100K, FAMILY_CLAUDE, FAMILY_GENERIC] {
        assert!(!sentence_piece_delimiter(family), "{family}");
    }
}

#[test]
fn count_words_treats_punctuation_only_runs_as_words() {
    // Go's countWords counts any non-space run, including "..." - the doc
    // comment says otherwise but the behaviour is what callers rely on.
    assert_eq!(count_words(""), 0);
    assert_eq!(count_words("   "), 0);
    assert_eq!(count_words("one"), 1);
    assert_eq!(count_words("one two three"), 3);
    assert_eq!(count_words("  padded  words  "), 2);
    assert_eq!(count_words("line\nbreak\tand\rspace"), 4);
}

/// `estimate_bytes` must not panic or lose tokens on invalid UTF-8: SSE deltas
/// can split a multi-byte rune, and Go's rune loop charges each replacement
/// character three bytes.
#[test]
fn estimate_bytes_handles_invalid_utf8() {
    let est = Estimator::for_model("gpt-4o");
    assert_eq!(est.estimate_bytes(b""), 0);
    let split = "中文".as_bytes();
    // Half of a three-byte rune, exactly what a chunk boundary can produce.
    let truncated = &split[..4];
    assert!(est.estimate_bytes(truncated) >= 1);
    // Lossy decoding turns each stray byte into U+FFFD, i.e. three bytes.
    assert_eq!(
        est.estimate_bytes(&[0xFF, 0xFE]),
        est.estimate("\u{FFFD}\u{FFFD}")
    );
    assert_eq!(
        est.estimate_bytes("hello".as_bytes()),
        est.estimate("hello")
    );
}

/// A large body must stay linear and not overflow the run accumulator.
#[test]
fn estimate_handles_megabyte_input() {
    let est = Estimator::for_model("gpt-4o");
    let text = "The quick brown fox jumps over the lazy dog. ".repeat(20_000);
    let n = est.estimate(&text);
    assert!(
        n > 100_000,
        "1 MiB of prose should be > 100k tokens, got {n}"
    );
    // Monotonic in the number of repetitions.
    let smaller = est.estimate(&"The quick brown fox jumps over the lazy dog. ".repeat(10_000));
    assert!(n > smaller);
}

/// An unknown family must still produce a usable estimate rather than zero.
#[test]
fn unknown_family_estimates_like_generic() {
    let unknown = Estimator::new("vendor-x");
    let generic = Estimator::new(FAMILY_GENERIC);
    for text in ["hello world", "中文文本", "{\"a\":1}", "1234567890"] {
        assert_eq!(unknown.estimate(text), generic.estimate(text), "{text}");
    }
}

/// The JSON field names and order must match the Go struct tags, because the
/// record lands in the audit ledger.
#[test]
fn usage_json_matches_go_field_names() {
    let u = Usage {
        prompt_tokens: 1,
        completion_tokens: 2,
        total_tokens: 3,
        upstream_prompt_tokens: 4,
        upstream_completion_tokens: 5,
        upstream_total_tokens: 6,
        upstream_reported: true,
        method: "upstream".to_string(),
        family: FAMILY_O200K.to_string(),
    };
    let encoded = serde_json::to_string(&u).unwrap();
    assert_eq!(
        encoded,
        r#"{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3,"upstream_prompt_tokens":4,"upstream_completion_tokens":5,"upstream_total_tokens":6,"upstream_reported":true,"method":"upstream","family":"o200k"}"#
    );
}

/// `omitempty` parity for `min_absolute` and `direction`.
///
/// `compare` always applies the default floor, so a verifiable result carries
/// `min_absolute`; only an unverifiable one (the early return) leaves both
/// omitted, which is what the ledger reader expects.
#[test]
fn deviation_json_omits_zero_floor_and_direction() {
    let u = Usage {
        prompt_tokens: 100,
        upstream_prompt_tokens: 101,
        upstream_reported: true,
        ..Usage::default()
    };
    let encoded = serde_json::to_string(&u.compare(0.05)).unwrap();
    assert!(encoded.contains(r#""min_absolute":8"#), "{encoded}");
    assert!(!encoded.contains("direction"), "{encoded}");

    // Not reported: the zero Deviation has no floor and no direction.
    let unverifiable = Usage::default().compare(0.05);
    let encoded = serde_json::to_string(&unverifiable).unwrap();
    assert!(!encoded.contains("min_absolute"), "{encoded}");
    assert!(!encoded.contains("direction"), "{encoded}");

    let d = u.compare_with(0.05, 8);
    let encoded = serde_json::to_string(&d).unwrap();
    assert!(encoded.contains(r#""min_absolute":8"#), "{encoded}");
    assert!(!encoded.contains("direction"), "{encoded}");
}

/// The Responses API helpers are not covered by the Go metering tests; these
/// cases pin the accounting rules the gateway depends on.
#[test]
fn count_request_responses_counts_instructions_and_tools() {
    let plain = ResponsesRequest {
        model: "gpt-4o".to_string(),
        instructions: "be brief".to_string(),
        input: Some(RawJson::new(r#""hello there""#)),
        ..ResponsesRequest::default()
    };
    let with_tools = ResponsesRequest {
        tools: vec![ResponsesTool {
            r#type: "function".to_string(),
            name: "lookup".to_string(),
            description: "Look something up in the catalogue of things.".to_string(),
            parameters: Some(RawJson::new(
                r#"{"type":"object","properties":{"q":{"type":"string"}}}"#,
            )),
            ..ResponsesTool::default()
        }],
        ..plain.clone()
    };
    let (n1, fam) = count_request_responses("gpt-4o", Some(&plain));
    let (n2, _) = count_request_responses("gpt-4o", Some(&with_tools));
    assert_eq!(fam, FAMILY_O200K);
    assert!(n1 > 0, "expected a positive count, got {n1}");
    assert!(n2 > n1, "tools should add tokens: {n1} -> {n2}");
}

#[test]
fn count_response_responses_counts_message_reasoning_and_function_call() {
    let base = ResponsesResponse {
        output: vec![ResponsesOutputItem {
            r#type: "message".to_string(),
            content: vec![ResponsesContentPart {
                r#type: "output_text".to_string(),
                text: "answer".to_string(),
                annotations: None,
            }],
            ..ResponsesOutputItem::default()
        }],
        ..ResponsesResponse::default()
    };
    let rich = ResponsesResponse {
        output: vec![
            ResponsesOutputItem {
                r#type: "message".to_string(),
                content: vec![ResponsesContentPart {
                    r#type: "output_text".to_string(),
                    text: "answer".to_string(),
                    annotations: None,
                }],
                ..ResponsesOutputItem::default()
            },
            ResponsesOutputItem {
                r#type: "reasoning".to_string(),
                summary: vec![ResponsesContentPart {
                    r#type: "summary_text".to_string(),
                    text: "thinking it through at length".to_string(),
                    annotations: None,
                }],
                ..ResponsesOutputItem::default()
            },
            ResponsesOutputItem {
                r#type: "function_call".to_string(),
                name: "search".to_string(),
                arguments: r#"{"query":"a long query string here"}"#.to_string(),
                ..ResponsesOutputItem::default()
            },
        ],
        ..ResponsesResponse::default()
    };
    let n1 = count_response_responses("gpt-4o", Some(&base));
    let n2 = count_response_responses("gpt-4o", Some(&rich));
    assert!(n1 > 0);
    assert!(
        n2 > n1,
        "reasoning and calls should add tokens: {n1} -> {n2}"
    );
}

#[test]
fn counter_add_prompt_tokens_and_overhead() {
    let mut c = Counter::new("claude-3-5-sonnet");
    c.add_prompt_tokens(50);
    c.add_overhead(3);
    let u = c.usage();
    assert_eq!(u.prompt_tokens, 53);
    assert_eq!(u.completion_tokens, 0);
    assert_eq!(u.method, "local_only");
    assert_eq!(u.family, FAMILY_CLAUDE);
    assert!(!u.upstream_reported);
    assert_eq!(u.upstream_total_tokens, 0);
}

#[test]
fn counter_upstream_total_falls_back_to_sum() {
    let mut c = Counter::new("gpt-4o");
    c.set_upstream(
        WireUsage {
            prompt_tokens: 7,
            completion_tokens: 11,
            total_tokens: 0,
            ..WireUsage::default()
        },
        true,
    );
    let u = c.usage();
    assert_eq!(u.upstream_total_tokens, 18);
}

/// Go's decoder zero-fills missing fields, so a ledger record written by an older
/// build (or by hand) must still load.
#[test]
fn usage_and_deviation_decode_partial_json() {
    let u: Usage = serde_json::from_str(r#"{"prompt_tokens":5}"#).unwrap();
    assert_eq!(u.prompt_tokens, 5);
    assert_eq!(u.completion_tokens, 0);
    assert!(!u.upstream_reported);
    assert!(u.family.is_empty());
    let u: Usage = serde_json::from_str("{}").unwrap();
    assert_eq!(u, Usage::default());

    let d: Deviation = serde_json::from_str(r#"{"worst":0.5}"#).unwrap();
    assert_eq!(d.worst, 0.5);
    assert_eq!(d.worst_abs, 0);
    assert!(!d.over_threshold);
    assert!(d.direction.is_empty());
}

/// The four counting helpers must reproduce Go's totals exactly for the same
/// bodies.
///
/// The expected numbers come from a probe compiled against the unmodified Go
/// `internal/metering` package; a drift here means the request/response
/// accounting changed, which would move every prompt count by a few tokens.
#[test]
fn counting_helpers_match_go() {
    let req: ChatRequest = serde_json::from_str(
        r#"{
            "model":"gpt-4o",
            "messages":[
                {"role":"system","content":[{"type":"text","text":"You are a helpful assistant."}]},
                {"role":"user","content":[{"type":"text","text":"What is 2+2?"}]},
                {"role":"assistant","name":"bot","content":[{"type":"text","text":"4"}],
                 "tool_calls":[{"function":{"name":"search","arguments":"{\"q\":\"x\"}"}}]}
            ],
            "tools":[{"type":"function","function":{"name":"get_weather","description":"Get the weather for a city.",
                      "parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}],
            "response_format":{"type":"json_schema","json_schema":{"type":"object"}}
        }"#,
    )
    .unwrap();
    let (n, fam) = count_request("gpt-4o", Some(&req));
    assert_eq!((n, fam.as_str()), (68, "o200k"));

    let resp: ChatResponse = serde_json::from_str(
        r#"{"choices":[{"message":{"content":[{"type":"text","text":"The answer is four."}],
            "reasoning_content":"let me think",
            "tool_calls":[{"function":{"name":"calc","arguments":"{\"x\":1}"}}]}}]}"#,
    )
    .unwrap();
    assert_eq!(count_response("gpt-4o", Some(&resp)), 11);

    let anth_req: AnthropicRequest = serde_json::from_str(
        r#"{"model":"claude-3-5-sonnet","max_tokens":100,
            "system":[{"type":"text","text":"You are terse."}],
            "messages":[{"role":"user","content":"Explain hashing."}],
            "tools":[{"name":"calc","input_schema":{"type":"object"}}]}"#,
    )
    .unwrap();
    let (n, fam) = count_request_anthropic("claude-3-5-sonnet", Some(&anth_req));
    assert_eq!((n, fam.as_str()), (30, "claude"));

    let anth_resp: AnthropicResponse = serde_json::from_str(
        r#"{"content":[
            {"type":"thinking","thinking":"let me consider"},
            {"type":"text","text":"The answer is 42."},
            {"type":"tool_use","name":"calc","input":{"expr":"6*7"}}]}"#,
    )
    .unwrap();
    assert_eq!(
        count_response_anthropic("claude-3-5-sonnet", Some(&anth_resp)),
        15
    );

    let resp_req: ResponsesRequest = serde_json::from_str(
        r#"{"model":"gpt-4o","instructions":"be brief",
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello there"}]},
                     {"type":"function_call_output","call_id":"c1","output":"42"}],
            "tools":[{"type":"function","name":"lookup","description":"Look something up.",
                      "parameters":{"type":"object","properties":{"q":{"type":"string"}}}}]}"#,
    )
    .unwrap();
    let (n, fam) = count_request_responses("gpt-4o", Some(&resp_req));
    assert_eq!((n, fam.as_str()), (30, "o200k"));

    let resp_resp: ResponsesResponse = serde_json::from_str(
        r#"{"output":[
            {"type":"message","content":[{"type":"output_text","text":"answer"}]},
            {"type":"reasoning","summary":[{"type":"summary_text","text":"thinking it through"}]},
            {"type":"function_call","name":"search","arguments":"{\"query\":\"x\"}"}]}"#,
    )
    .unwrap();
    assert_eq!(count_response_responses("gpt-4o", Some(&resp_resp)), 10);
}

/// `relative_delta` and `clamp_finite` are the division guards behind
/// `compare`; check them directly so a refactor cannot silently reintroduce an
/// infinity into the ledger.
#[test]
fn relative_delta_and_clamp_finite_guards() {
    assert_eq!(relative_delta(110, 100), 0.1);
    assert_eq!(relative_delta(0, 0), 0.0);
    assert_eq!(relative_delta(-5, 0), 0.0);
    assert!(relative_delta(5, 0).is_infinite());
    assert_eq!(clamp_finite(f64::INFINITY), MAX_FINITE_DELTA);
    assert_eq!(clamp_finite(f64::NEG_INFINITY), -MAX_FINITE_DELTA);
    assert_eq!(clamp_finite(f64::NAN), 0.0);
    assert_eq!(clamp_finite(0.25), 0.25);
}
