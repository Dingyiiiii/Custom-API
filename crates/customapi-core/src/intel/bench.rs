//! Capability scoring: the layered question suite, its grading run, and the
//! report shape that `benchmarks.json` holds.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::chat_request_body;
use super::grade::grade_case;
use super::truncate;
use crate::config::{upstream_url, zero_time};

// ---------------------------------------------------------------------------
// Capability scoring
// ---------------------------------------------------------------------------

/// One graded question.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BenchCase {
    /// Identifies the case.
    pub id: String,
    /// math/logic/code/instruction/fact/chinese/reasoning.
    pub category: String,
    /// E (easy), M (medium) or H (hard).
    pub difficulty: String,
    /// An optional system prompt.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub system: String,
    /// The question.
    pub prompt: String,
    /// The exact expected answer, compared after normalisation.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub expect: String,
    /// Substrings that must all appear.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expect_contains: Vec<String>,
    /// An optional regular expression the reply must match.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub expect_regex: String,
    /// Bounds the reply.
    pub max_tokens: i32,
    /// Scales this case's contribution.
    #[serde(
        default,
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub weight: f64,
}

/// Compact row constructor for the built-in suite.
///
/// Equivalent to the Go composite literals, which name only the fields a row
/// sets: every built-in case has an id, a category, a difficulty, a prompt, a
/// token bound and either an exact expectation or a required-substring list.
fn case(
    id: &str,
    category: &str,
    difficulty: &str,
    prompt: &str,
    max_tokens: i32,
    expect: Option<&str>,
    expect_contains: &[&str],
) -> BenchCase {
    BenchCase {
        id: id.to_string(),
        category: category.to_string(),
        difficulty: difficulty.to_string(),
        prompt: prompt.to_string(),
        expect: expect.map(str::to_string).unwrap_or_default(),
        expect_contains: expect_contains.iter().map(|s| s.to_string()).collect(),
        max_tokens,
        ..BenchCase::default()
    }
}

/// The built-in layered capability suite.
///
/// Sizes are balanced across category x difficulty so that a partial run still
/// produces a comparable profile. Cases are objective (exact answer, required
/// substring, or a regular expression) precisely so scoring needs no judge model
/// and therefore costs nothing beyond the generation itself.
pub fn default_bench_suite() -> Vec<BenchCase> {
    vec![
        // ---------------- easy ----------------
        case(
            "math-e1",
            "math",
            "E",
            "What is 17 + 25? Reply with the number only.",
            16,
            Some("42"),
            &[],
        ),
        case(
            "math-e2",
            "math",
            "E",
            "What is 144 / 12? Reply with the number only.",
            16,
            Some("12"),
            &[],
        ),
        case(
            "math-e3",
            "math",
            "E",
            "What is 9 * 8? Reply with the number only.",
            16,
            Some("72"),
            &[],
        ),
        case(
            "logic-e1",
            "logic",
            "E",
            "All cats are mammals. Tom is a cat. Is Tom a mammal? Reply yes or no.",
            16,
            None,
            &["yes"],
        ),
        case(
            "logic-e2",
            "logic",
            "E",
            "If A > B and B > C, which is largest? Reply with one letter.",
            16,
            Some("A"),
            &[],
        ),
        case(
            "fact-e1",
            "fact",
            "E",
            "What is the chemical symbol for water? Reply with the formula only.",
            16,
            None,
            &["H2O"],
        ),
        case(
            "fact-e2",
            "fact",
            "E",
            "What planet is known as the Red Planet? Reply with one word.",
            16,
            None,
            &["Mars"],
        ),
        case(
            "instr-e1",
            "instruction",
            "E",
            "Reply with exactly the word PONG and nothing else.",
            16,
            Some("PONG"),
            &[],
        ),
        case(
            "chinese-e1",
            "chinese",
            "E",
            "“苹果”的英文是什么？只回答单词。",
            16,
            None,
            &["apple", "Apple"],
        ),
        case(
            "chinese-e2",
            "chinese",
            "E",
            "请只回答：中国的首都是哪里？",
            24,
            None,
            &["北京", "Beijing"],
        ),
        case(
            "code-e1",
            "code",
            "E",
            "In Python, what builtin returns the length of a list? Reply with the name only.",
            16,
            None,
            &["len"],
        ),
        case(
            "reason-e1",
            "reasoning",
            "E",
            "A bat and a ball cost $1.10 together. The bat costs $1.00 more than the ball. \
             How much does the ball cost in cents? Reply with the number only.",
            32,
            None,
            &["5"],
        ),
        // ---------------- medium ----------------
        case(
            "math-m1",
            "math",
            "M",
            "What is 2^10? Reply with the number only.",
            24,
            Some("1024"),
            &[],
        ),
        case(
            "math-m2",
            "math",
            "M",
            "What is the sum of the first 10 positive integers? Reply with the number only.",
            24,
            Some("55"),
            &[],
        ),
        case(
            "math-m3",
            "math",
            "M",
            "How many prime numbers are there between 1 and 20 inclusive? Reply with the number only.",
            24,
            Some("8"),
            &[],
        ),
        case(
            "logic-m1",
            "logic",
            "M",
            "Three friends A, B, C each have a different pet: cat, dog, fish. A does not have a cat. \
             B has a dog. Who has the fish? Reply with one letter.",
            32,
            Some("A"),
            &[],
        ),
        case(
            "logic-m2",
            "logic",
            "M",
            "If yesterday was Wednesday, what day is tomorrow? Reply with one word.",
            24,
            None,
            &["Friday"],
        ),
        case(
            "code-m1",
            "code",
            "M",
            "In Python, which exception is raised by int('abc')? Reply with the exception name only.",
            24,
            None,
            &["ValueError"],
        ),
        case(
            "code-m2",
            "code",
            "M",
            "What is the output of `print(len({1,2,2,3}))` in Python? Reply with the number only.",
            24,
            Some("3"),
            &[],
        ),
        case(
            "instr-m1",
            "instruction",
            "M",
            r#"Output exactly this JSON and nothing else: {"a":[1,2],"b":"x"}"#,
            48,
            None,
            &["\"a\"", "\"b\"", "1,2"],
        ),
        case(
            "instr-m2",
            "instruction",
            "M",
            "Write the word ALPHA in uppercase, then a hyphen, then the word beta in lowercase. \
             Output only that.",
            32,
            Some("ALPHA-beta"),
            &[],
        ),
        case(
            "chinese-m1",
            "chinese",
            "M",
            "把下面这句话翻译成英文，只输出译文：今天天气很好。",
            48,
            None,
            &["weather", "nice", "good", "today"],
        ),
        case(
            "chinese-m2",
            "chinese",
            "M",
            "“马”字的笔画数是多少？只回答数字。",
            24,
            Some("3"),
            &[],
        ),
        case(
            "reason-m1",
            "reasoning",
            "M",
            "I have 3 apples. I eat one and give away half of the rest. How many do I have left? \
             Reply with the number only.",
            32,
            Some("1"),
            &[],
        ),
        case(
            "fact-m1",
            "fact",
            "M",
            "In what year did the Berlin Wall fall? Reply with the year only.",
            24,
            Some("1989"),
            &[],
        ),
        case(
            "fact-m2",
            "fact",
            "M",
            "What is the longest river in Asia? Reply with the name only.",
            24,
            None,
            &["Yangtze", "Chang Jiang", "长江"],
        ),
        // ---------------- hard ----------------
        case(
            "math-h1",
            "math",
            "H",
            "What is 1234 * 5678? Reply with the number only.",
            40,
            Some("7006652"),
            &[],
        ),
        case(
            "math-h2",
            "math",
            "H",
            "How many trailing zeros are in 100! ? Reply with the number only.",
            40,
            Some("24"),
            &[],
        ),
        case(
            "logic-h1",
            "logic",
            "H",
            "There are 5 houses in a row. The red house is immediately to the left of the blue house. \
             The green house is at one end. If the blue house is in the middle, how many houses are \
             to the right of the green house? Reply with the number only.",
            64,
            None,
            &["4"],
        ),
        case(
            "code-h1",
            "code",
            "H",
            "What does this print? `print(sum(1 for i in range(100) if i % 7 == 0))`. \
             Reply with the number only.",
            48,
            Some("15"),
            &[],
        ),
        case(
            "code-h2",
            "code",
            "H",
            "In Python, what is the output of `print([x for x in range(5)][::-2])`? \
             Reply with the list only.",
            48,
            None,
            &["4", "2", "0"],
        ),
        case(
            "instr-h1",
            "instruction",
            "H",
            r#"Reply with exactly this JSON and nothing else: {"n": 1e3}"#,
            48,
            None,
            &["1e3"],
        ),
        case(
            "chinese-h1",
            "chinese",
            "H",
            "请用四个字概括“亡羊补牢”这个成语的含义，只输出四个字。",
            32,
            None,
            &["为时", "未晚", "及时", "补救"],
        ),
        case(
            "reason-h1",
            "reasoning",
            "H",
            "A farmer has 17 sheep. All but 9 run away. How many are left? Reply with the number only.",
            32,
            Some("9"),
            &[],
        ),
        case(
            "reason-h2",
            "reasoning",
            "H",
            "If it takes 5 machines 5 minutes to make 5 widgets, how long would it take 100 machines \
             to make 100 widgets, in minutes? Reply with the number only.",
            32,
            Some("5"),
            &[],
        ),
        case(
            "fact-h1",
            "fact",
            "H",
            "What is the atomic number of gold? Reply with the number only.",
            24,
            Some("79"),
            &[],
        ),
    ]
}

/// One graded case.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BenchOutcome {
    /// The question that was asked.
    pub case: BenchCase,
    /// What the channel answered.
    pub reply: String,
    /// Whether the reply satisfied the case.
    pub passed: bool,
    /// Transport or protocol failure, when the case did not run.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// Round-trip time.
    #[serde(default, skip_serializing_if = "crate::json::is_zero_i64")]
    pub latency_ms: i64,
}

/// A full suite run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchReport {
    /// Channel the suite ran against.
    pub channel: String,
    /// Canonical model that was asked for.
    pub model: String,
    /// Cases attempted.
    pub total: i32,
    /// Cases that passed.
    pub passed: i32,
    /// `passed / total`, in [0,1].
    #[serde(default, serialize_with = "crate::json::go_f64")]
    pub score: f64,
    /// Pass rate per category.
    #[serde(
        default,
        deserialize_with = "super::null_map",
        serialize_with = "crate::json::go_f64_map"
    )]
    pub by_category: BTreeMap<String, f64>,
    /// Pass rate per difficulty level.
    #[serde(
        default,
        deserialize_with = "super::null_map",
        serialize_with = "crate::json::go_f64_map"
    )]
    pub by_level: BTreeMap<String, f64>,
    /// When the run started.
    #[serde(default = "zero_time", serialize_with = "crate::json::go_time")]
    pub started: DateTime<Utc>,
    /// When the run finished.
    #[serde(default = "zero_time", serialize_with = "crate::json::go_time")]
    pub finished: DateTime<Utc>,
    /// Per-case detail; verbose, so it is only populated on request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outcomes: Vec<BenchOutcome>,
}

impl Default for BenchReport {
    fn default() -> Self {
        BenchReport {
            channel: String::new(),
            model: String::new(),
            total: 0,
            passed: 0,
            score: 0.0,
            by_category: BTreeMap::new(),
            by_level: BTreeMap::new(),
            started: zero_time(),
            finished: zero_time(),
            outcomes: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Bench runner
// ---------------------------------------------------------------------------

/// Executes the capability suite through the gateway.
#[derive(Debug, Clone, Default)]
pub struct BenchRunner {
    /// Gateway's user-facing address.
    pub base_url: String,
    /// Gateway API key used for the run.
    pub token: String,
    /// Canonical model to ask for.
    pub model: String,
    /// Pins the run to one channel using the customs pin header.
    pub channel: String,
    /// Performs the requests; `None` builds a client with a 120s timeout.
    pub client: Option<reqwest::Client>,
    /// Stores per-case detail (verbose; off by default).
    pub include_outcomes: bool,
}

impl BenchRunner {
    /// Executes the suite and grades it.
    ///
    /// Go takes a `context.Context` and stops between cases when it is
    /// cancelled; here cancellation is tokio's: dropping the future aborts the
    /// in-flight request and stops the run, which leaves the partial report
    /// unreachable, so a caller that needs the partial result should bound the
    /// suite before calling.
    pub async fn run(&self, suite: &[BenchCase]) -> BenchReport {
        let client = match &self.client {
            Some(client) => client.clone(),
            None => reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        };
        let mut report = BenchReport {
            channel: self.channel.clone(),
            model: self.model.clone(),
            started: Utc::now(),
            ..BenchReport::default()
        };
        // Keyed by (category, level) so the two averages below can be derived in
        // one pass, exactly as Go's `key` struct does.
        let mut totals: BTreeMap<(String, String), i32> = BTreeMap::new();
        let mut passed_by_key: BTreeMap<(String, String), i32> = BTreeMap::new();

        for tc in suite {
            let (reply, error, latency) = self.run_case(&client, tc).await;
            let passed = error.is_empty() && grade_case(tc, &reply);
            report.total += 1;
            if passed {
                report.passed += 1;
            }
            let key = (tc.category.clone(), tc.difficulty.clone());
            *totals.entry(key.clone()).or_insert(0) += 1;
            if passed {
                *passed_by_key.entry(key).or_insert(0) += 1;
            }
            if self.include_outcomes {
                report.outcomes.push(BenchOutcome {
                    case: tc.clone(),
                    reply: truncate(&reply, 400),
                    passed,
                    error,
                    latency_ms: latency,
                });
            }
        }
        if report.total > 0 {
            report.score = report.passed as f64 / report.total as f64;
        }
        for (key, total) in &totals {
            if *total == 0 {
                continue;
            }
            let value = *passed_by_key.get(key).unwrap_or(&0) as f64 / *total as f64;
            *report.by_category.entry(key.0.clone()).or_insert(0.0) += value;
            *report.by_level.entry(key.1.clone()).or_insert(0.0) += value;
        }
        // Turn the sums into means over the levels each category spans.
        let mut category_count: BTreeMap<String, i32> = BTreeMap::new();
        let mut level_count: BTreeMap<String, i32> = BTreeMap::new();
        for (category, level) in totals.keys() {
            *category_count.entry(category.clone()).or_insert(0) += 1;
            *level_count.entry(level.clone()).or_insert(0) += 1;
        }
        for (category, count) in category_count {
            if count > 0 {
                if let Some(slot) = report.by_category.get_mut(&category) {
                    *slot /= count as f64;
                }
            }
        }
        for (level, count) in level_count {
            if count > 0 {
                if let Some(slot) = report.by_level.get_mut(&level) {
                    *slot /= count as f64;
                }
            }
        }
        report.finished = Utc::now();
        report
    }

    /// Runs one case and returns `(reply, error, latency_ms)`.
    async fn run_case(&self, client: &reqwest::Client, tc: &BenchCase) -> (String, String, i64) {
        let system = if tc.system.is_empty() {
            None
        } else {
            Some(tc.system.as_str())
        };
        let body: Value = chat_request_body(&self.model, tc.max_tokens, 0.0, system, &tc.prompt);
        // Serialising a `Value` cannot fail, so unlike Go there is no error arm.
        let raw = serde_json::to_vec(&body).unwrap_or_default();
        let url = upstream_url(&self.base_url, "/v1/chat/completions");
        let start = Instant::now();
        let mut request = client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .body(raw);
        if !self.channel.is_empty() {
            request = request.header("X-Customs-Channel", &self.channel);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(err) => return (String::new(), err.to_string(), elapsed_ms(start)),
        };
        let status = response.status();
        // Go ignores a read error (`b, _ := io.ReadAll`) and caps the body at
        // 1 MiB so a runaway upstream cannot exhaust memory.
        let bytes = response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .unwrap_or_default();
        let latency = elapsed_ms(start);
        let bytes = limit_body(bytes, 1 << 20);
        // Go compares against `http.StatusOK` exactly, so a 2xx that is not 200
        // is still a failure here.
        if status.as_u16() != 200 {
            return (String::new(), format!("HTTP {}", status.as_u16()), latency);
        }
        let parsed: ChatCompletion = match serde_json::from_slice(&bytes) {
            Ok(parsed) => parsed,
            Err(_) => return (String::new(), "malformed response".to_string(), latency),
        };
        let choices = parsed.choices.unwrap_or_default();
        if choices.is_empty() {
            return (String::new(), "empty choices".to_string(), latency);
        }
        let reply = choices[0].content();
        (reply, String::new(), latency)
    }
}

/// Milliseconds elapsed since `start`, saturating instead of panicking on a
/// clock that somehow ran backwards.
fn elapsed_ms(start: Instant) -> i64 {
    i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX)
}

/// Truncates a response body to the bytes Go's `io.LimitReader` would hand to
/// the JSON decoder.
pub(crate) fn limit_body(mut bytes: Vec<u8>, limit: usize) -> Vec<u8> {
    if bytes.len() > limit {
        bytes.truncate(limit);
    }
    bytes
}

// ---------------------------------------------------------------------------
// Chat-completion response shape
// ---------------------------------------------------------------------------

/// The parts of a chat-completion response the intelligence layer reads.
///
/// Every field tolerates `null` because Go's decoder does: a JSON null leaves a
/// Go string/slice/struct at its zero value without an error, while serde would
/// reject it outright.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ChatCompletion {
    /// The model the upstream claimed.
    #[serde(default)]
    pub(crate) model: Option<String>,
    /// The generated choices.
    #[serde(default)]
    pub(crate) choices: Option<Vec<ChatChoice>>,
    /// The upstream usage block, when present.
    #[serde(default)]
    pub(crate) usage: Option<ChatUsage>,
}

/// One generated choice.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ChatChoice {
    /// The assistant message.
    #[serde(default)]
    pub(crate) message: Option<ChatMessage>,
}

impl ChatChoice {
    /// Returns the assistant text, empty when the upstream sent none.
    pub(crate) fn content(&self) -> String {
        self.message
            .as_ref()
            .and_then(|message| message.content.clone())
            .unwrap_or_default()
    }
}

/// The assistant message of a choice.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ChatMessage {
    /// The assistant text.
    #[serde(default)]
    pub(crate) content: Option<String>,
}

/// The usage block of a chat completion.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ChatUsage {
    /// Prompt tokens the upstream counted.
    #[serde(default)]
    pub(crate) prompt_tokens: Option<i32>,
    /// Completion tokens the upstream counted.
    #[serde(default)]
    pub(crate) completion_tokens: Option<i32>,
}
