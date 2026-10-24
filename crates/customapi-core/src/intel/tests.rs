//! Tests for the intelligence layer.
//!
//! The Go package's test file (`intel_test.go`) is translated case for case;
//! each test keeps the Go test's name in snake_case so the two suites can be
//! diffed. Network tests run against a loopback stub server, never the internet.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::grade::{as_number, last_number, normalize};
use super::*;
use crate::config::{PriceEntry, PriceModel};

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

/// A unique temporary directory, removed when the test ends.
///
/// The crate has no `tempfile` dev-dependency, and adding one is a workspace
/// change the porting rules reserve for the maintainer, so the directory is
/// created by hand from the process id and a counter.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> TempDir {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "customapi-intel-{}-{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Returns a path inside the directory as the `String` the API takes.
    fn file(&self, name: &str) -> String {
        self.path.join(name).to_string_lossy().to_string()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A loopback HTTP server that answers every request with one fixed response.
///
/// The Go tests use `httptest.NewServer`; this is its tokio equivalent. It
/// records the raw requests so a test can assert the wire format.
struct StubServer {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl StubServer {
    async fn start(status: u16, body: &str) -> StubServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let address = listener.local_addr().expect("local address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&requests);
        let payload = body.to_string();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let sink = Arc::clone(&sink);
                let payload = payload.clone();
                tokio::spawn(async move {
                    let raw = read_request(&mut socket).await;
                    sink.lock()
                        .expect("request log")
                        .push(String::from_utf8_lossy(&raw).to_string());
                    let reason = match status {
                        200 => "OK",
                        401 => "Unauthorized",
                        500 => "Internal Server Error",
                        _ => "Status",
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes())
                        .await;
                    let _ = tokio::io::AsyncWriteExt::flush(&mut socket).await;
                });
            }
        });
        StubServer {
            base_url: format!("http://{address}"),
            requests,
            handle,
        }
    }

    /// Returns the raw requests seen so far.
    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("request log").clone()
    }

    /// Returns the body of the last request, without the headers.
    fn last_body(&self) -> String {
        let requests = self.requests();
        let last = requests.last().expect("at least one request");
        match last.split_once("\r\n\r\n") {
            Some((_, body)) => body.to_string(),
            None => String::new(),
        }
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Reads one HTTP request: headers plus `content-length` bytes of body.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    use tokio::io::AsyncReadExt;

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            let body_start = position + 4;
            let declared = content_length(&buffer[..position]);
            if buffer.len() >= body_start + declared {
                break;
            }
        }
    }
    buffer
}

/// Returns the index of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Parses the `content-length` header of a request head.
fn content_length(head: &[u8]) -> usize {
    let text = String::from_utf8_lossy(head).to_lowercase();
    for line in text.split("\r\n") {
        if let Some(value) = line.strip_prefix("content-length:") {
            return value.trim().parse().unwrap_or(0);
        }
    }
    0
}

/// A chat-completion response body for the stub server.
fn chat_reply(model: &str, content: &str) -> String {
    serde_json::json!({
        "model": model,
        "choices": [{"message": {"role": "assistant", "content": content}}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2},
    })
    .to_string()
}

/// A bench case with only an exact expectation, as the Go table writes it.
fn exact(expect: &str) -> BenchCase {
    BenchCase {
        expect: expect.to_string(),
        ..BenchCase::default()
    }
}

/// A probe with the defaults the runner tests care about.
fn probe(name: &str, prompt: &str, max_tokens: i32) -> Probe {
    Probe {
        name: name.to_string(),
        system: String::new(),
        prompt: prompt.to_string(),
        max_tokens,
        temperature: 0.0,
    }
}

/// A bench case with only required substrings.
fn containing(wants: &[&str]) -> BenchCase {
    BenchCase {
        expect_contains: wants.iter().map(|want| want.to_string()).collect(),
        ..BenchCase::default()
    }
}

// ---------------------------------------------------------------------------
// Distribution comparison
// ---------------------------------------------------------------------------

/// Identical distributions score zero.
#[test]
fn jensen_shannon_identical() {
    let a = [10, 20, 30, 40, 50];
    assert_eq!(
        jensen_shannon(&a, &a),
        0.0,
        "identical distributions should not diverge"
    );
}

/// A substituted model - one whose reply lengths differ materially - is
/// separated from the baseline.
#[test]
fn jensen_shannon_discriminates() {
    let baseline = [100, 110, 120, 105, 115, 108];
    let same_model = [102, 108, 118, 111, 113, 106];
    let substitute = [5, 6, 7, 5, 6, 8]; // terse, cheap substitute

    let near = jensen_shannon(&baseline, &same_model);
    let far = jensen_shannon(&baseline, &substitute);
    assert!(
        near < far,
        "divergence did not separate the cases: same={near} different={far}"
    );
    assert!(
        near <= 0.35,
        "a matching model scored above the default threshold: {near}"
    );
    assert!(
        far > 0.35,
        "a substituted model scored below the default threshold: {far}"
    );
}

#[test]
fn jensen_shannon_bounded() {
    let got = jensen_shannon(&[1, 1, 1], &[1000, 1000, 1000]);
    assert!(
        (0.0..=1.001).contains(&got),
        "divergence out of range: {got}"
    );
}

#[test]
fn jensen_shannon_empty() {
    assert_eq!(
        jensen_shannon(&[], &[1]),
        1.0,
        "an empty input should be maximally divergent"
    );
    assert_eq!(
        jensen_shannon(&[1], &[]),
        1.0,
        "an empty input should be maximally divergent"
    );
    assert_eq!(jensen_shannon(&[], &[]), 1.0);
}

#[test]
fn jensen_shannon_constant_distributions() {
    assert_eq!(
        jensen_shannon(&[5, 5, 5], &[5, 5, 5]),
        0.0,
        "identical constants diverged"
    );
    assert_eq!(
        jensen_shannon(&[5, 5], &[9, 9]),
        1.0,
        "different constants should be maximally divergent"
    );
}

/// A constant reply that lands on a bin the baseline populated is a real but
/// modest divergence; one that lands on an empty bin is raised to at least 0.75,
/// because a canned upstream answering with a length the genuine model never
/// produced is unambiguous evidence of substitution.
#[test]
fn jensen_shannon_point_mass_inside_support() {
    // 100..500 spread across 8 bins of 50 leaves gaps at 250 and 350, which is
    // the only way a clamped point mass can fall outside the support.
    let baseline = [100, 200, 300, 400, 500];
    let inside = jensen_shannon(&baseline, &[300, 300]);
    let outside = jensen_shannon(&baseline, &[250, 250]);
    assert!(
        inside < 0.75,
        "a point mass inside the support should stay below the floor: {inside}"
    );
    assert!(
        outside >= 0.75,
        "a point mass outside the support should hit the floor: {outside}"
    );
    assert!(inside < outside);
}

// ---------------------------------------------------------------------------
// Baseline comparison
// ---------------------------------------------------------------------------

#[test]
fn baseline_compare_no_baseline() {
    let baselines = FingerprintBaselines::new();
    let result = baselines.compare("gpt-4o", &[1, 2, 3], 0.35);
    assert_eq!(result.verdict, VERDICT_NO_BASELINE, "{result:?}");
    assert_eq!(result.observed_samples, 3);
    assert_eq!(result.baseline_samples, 0);
    assert_eq!(result.score, 0.0);
    assert!(result.sample_hash.is_empty());
}

#[test]
fn baseline_compare_match() {
    let mut baselines = FingerprintBaselines::new();
    baselines.models.insert(
        "gpt-4o".to_string(),
        Baseline {
            model: "gpt-4o".to_string(),
            source: "official".to_string(),
            samples: vec![100, 105, 110, 108, 102, 106],
            ..Baseline::default()
        },
    );
    let result = baselines.compare("gpt-4o", &[101, 107, 109, 104, 103, 108], 0.35);
    assert_eq!(result.verdict, VERDICT_MATCH, "{result:?}");
    assert_eq!(result.baseline_source, "official");
    assert!(
        !result.sample_hash.is_empty(),
        "a comparison should record a sample hash for reproducibility"
    );
}

#[test]
fn baseline_compare_mismatch() {
    let mut baselines = FingerprintBaselines::new();
    baselines.models.insert(
        "gpt-4o".to_string(),
        Baseline {
            model: "gpt-4o".to_string(),
            source: "official".to_string(),
            samples: vec![200, 210, 220, 205, 215],
            ..Baseline::default()
        },
    );
    let result = baselines.compare("gpt-4o", &[3, 4, 3, 5, 4], 0.35);
    assert_eq!(result.verdict, VERDICT_MISMATCH, "{result:?}");
}

/// A non-positive threshold falls back to the built-in 0.35, which is what the
/// configuration layer passes when the operator left the field unset.
#[test]
fn baseline_compare_default_threshold() {
    let baselines = FingerprintBaselines::new();
    let result = baselines.compare("gpt-4o", &[1], 0.0);
    assert_eq!(result.threshold, 0.35);
    let result = baselines.compare("gpt-4o", &[1], -1.0);
    assert_eq!(result.threshold, 0.35);
}

/// A baseline entry with no samples is the same as no baseline at all: Go
/// checks `len(bl.Samples) == 0` and never divides by zero.
#[test]
fn baseline_compare_empty_samples_is_no_baseline() {
    let mut baselines = FingerprintBaselines::new();
    baselines.models.insert(
        "gpt-4o".to_string(),
        Baseline {
            model: "gpt-4o".to_string(),
            source: "official".to_string(),
            ..Baseline::default()
        },
    );
    let result = baselines.compare("gpt-4o", &[100, 101], 0.35);
    assert_eq!(result.verdict, VERDICT_NO_BASELINE);
}

// ---------------------------------------------------------------------------
// Baseline persistence
// ---------------------------------------------------------------------------

#[test]
fn baseline_save_load() {
    let dir = TempDir::new();
    let path = dir.file("baselines.json");

    let mut baselines = FingerprintBaselines::new();
    baselines.models.insert(
        "gpt-4o".to_string(),
        Baseline {
            model: "gpt-4o".to_string(),
            source: "official".to_string(),
            samples: vec![1, 2, 3],
            distinct: 3,
            fingerprint: "abc".to_string(),
            ..Baseline::default()
        },
    );
    baselines.save(&path).expect("save baselines");

    let loaded = load_baselines(&path).expect("load baselines");
    let got = loaded.models.get("gpt-4o").expect("baseline lost");
    assert_eq!(got.samples, vec![1, 2, 3]);
    assert_eq!(got.source, "official");
    assert_eq!(got.distinct, 3);
    assert_eq!(got.fingerprint, "abc");
    assert_eq!(loaded.version, 1);
    assert!(
        loaded.updated_at > crate::config::zero_time(),
        "saving should stamp the file"
    );
}

#[test]
fn load_baselines_missing_is_empty() {
    let dir = TempDir::new();
    let baselines = load_baselines(&format!("{}/absent.json", dir.path().display()))
        .expect("a missing baseline file is not an error");
    assert!(baselines.models.is_empty());
    assert_eq!(baselines.version, 1);
}

#[test]
fn load_baselines_corrupt_fails() {
    let dir = TempDir::new();
    let path = dir.file("bad.json");
    fs::write(&path, "{not json").expect("write corrupt file");
    let err = load_baselines(&path).expect_err("a corrupt baseline file must fail");
    assert!(
        err.to_string().contains("intel: parse baselines"),
        "unexpected error text: {err}"
    );
    assert!(err.to_string().contains(&path));
}

/// A file written by hand (or by an older build) may carry `null` where the Go
/// decoder accepts a nil map; that must still load as "no baselines".
#[test]
fn load_baselines_null_models_is_empty() {
    let dir = TempDir::new();
    let path = dir.file("nulls.json");
    fs::write(&path, r#"{"version":1,"updated_at":"2026-01-01T00:00:00Z","models":null}"#)
        .expect("write file");
    let baselines = load_baselines(&path).expect("null models must load");
    assert!(baselines.models.is_empty());
}

/// Saving creates the parent directories the way Go's `MkdirAll` does, so a
/// fresh deployment can point at `var/etc/baselines.json` directly.
#[test]
fn baseline_save_creates_directories() {
    let dir = TempDir::new();
    let path = format!("{}/nested/deeper/baselines.json", dir.path().display());
    let mut baselines = FingerprintBaselines::new();
    baselines
        .save(&path)
        .expect("save into a directory that does not exist yet");
    assert!(Path::new(&path).exists());
    assert!(
        !Path::new(&format!("{path}.tmp")).exists(),
        "the temporary file must be renamed away"
    );
}

/// The baseline file is an operator-facing artefact, so its field names and the
/// `omitempty` behaviour have to match the Go tags exactly.
#[test]
fn baseline_file_json_shape_matches_go() {
    let dir = TempDir::new();
    let path = dir.file("baselines.json");
    let mut baselines = FingerprintBaselines::new();
    baselines.models.insert(
        "gpt-4o".to_string(),
        Baseline {
            model: "gpt-4o".to_string(),
            source: "official".to_string(),
            samples: vec![1, 2, 3],
            ..Baseline::default()
        },
    );
    baselines.save(&path).expect("save baselines");
    let text = fs::read_to_string(&path).expect("read back");
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");

    assert_eq!(value["version"], 1);
    assert!(value.get("updated_at").is_some());
    let entry = &value["models"]["gpt-4o"];
    assert_eq!(entry["model"], "gpt-4o");
    assert_eq!(entry["source"], "official");
    assert_eq!(entry["samples"][2], 3);
    assert!(entry.get("created_at").is_some());
    // `omitempty` fields stay out of the file when they are zero/empty.
    assert!(entry.get("distinct").is_none());
    assert!(entry.get("fingerprint").is_none());
    // Go's MarshalIndent uses two spaces and no trailing newline.
    assert!(text.contains("\n  \"version\""));
    assert!(!text.ends_with('\n'));
}

// ---------------------------------------------------------------------------
// Grading
// ---------------------------------------------------------------------------

/// The objective graders (Go's `TestGradeCase`). Grading must be exact enough
/// that a channel cannot pass by accident, and forgiving enough that showing
/// working is not punished.
///
/// The test is suffixed because a function named `grade_case` in this module
/// would shadow the imported grader.
#[test]
fn grade_case_table() {
    let cases: [(&str, BenchCase, &str, bool); 18] = [
        ("exact", exact("42"), "42", true),
        ("exact with whitespace", exact("42"), "  42  ", true),
        ("exact with trailing punctuation", exact("42"), "42.", true),
        ("answer prefix", exact("42"), "The answer is 42", true),
        ("chinese answer prefix", exact("北京"), "答案是北京", true),
        ("wrong number", exact("42"), "43", false),
        ("number with working", exact("42"), "17 + 25 = 42", true),
        ("wrong but contains number", exact("42"), "17 + 25 = 43", false),
        (
            "contains",
            containing(&["yes"]),
            "Yes, Tom is a mammal.",
            true,
        ),
        (
            "contains case insensitive",
            containing(&["Mars"]),
            "the red planet is mars",
            true,
        ),
        ("contains missing", containing(&["Mars"]), "Venus", false),
        (
            "contains numeric precision",
            containing(&["5"]),
            "15",
            false,
        ),
        ("contains numeric exact", containing(&["5"]), "5 cents", true),
        ("empty reply", exact("42"), "", false),
        ("whitespace reply", exact("42"), "   \n  ", false),
        (
            "smoke test no expectation",
            BenchCase::default(),
            "anything at all",
            true,
        ),
        ("smoke test empty", BenchCase::default(), "", false),
        (
            "uppercase instruction",
            exact("ALPHA-beta"),
            "ALPHA-beta",
            true,
        ),
    ];
    for (name, case, reply, want) in cases {
        assert_eq!(
            grade_case(&case, reply),
            want,
            "grade_case({case:?}, {reply:?}) should be {want} ({name})"
        );
    }
}

#[test]
fn default_bench_suite_is_well_formed() {
    let suite = default_bench_suite();
    assert!(
        suite.len() >= 30,
        "suite has only {} cases; the plan calls for a layered profile",
        suite.len()
    );
    let mut ids = std::collections::HashSet::new();
    let mut categories: BTreeMap<&str, usize> = BTreeMap::new();
    let mut levels: BTreeMap<&str, usize> = BTreeMap::new();
    for case in &suite {
        assert!(!case.id.is_empty(), "a case has no id");
        assert!(ids.insert(case.id.clone()), "duplicate case id {:?}", case.id);
        assert!(!case.prompt.is_empty(), "case {} has no prompt", case.id);
        assert!(
            case.max_tokens > 0,
            "case {} has no token bound, which would let a channel run away",
            case.id
        );
        assert!(
            !case.expect.is_empty()
                || !case.expect_contains.is_empty()
                || !case.expect_regex.is_empty(),
            "case {} has no expectation, so it cannot discriminate",
            case.id
        );
        *categories.entry(case.category.as_str()).or_insert(0) += 1;
        *levels.entry(case.difficulty.as_str()).or_insert(0) += 1;
    }
    for want in [
        "math",
        "logic",
        "code",
        "instruction",
        "fact",
        "chinese",
        "reasoning",
    ] {
        assert!(
            categories.get(want).copied().unwrap_or(0) > 0,
            "no cases in category {want:?}"
        );
    }
    for want in ["E", "M", "H"] {
        assert!(
            levels.get(want).copied().unwrap_or(0) > 0,
            "no cases at difficulty {want:?}"
        );
    }
}

/// The suite's structural table: the ids, categories, levels and token bounds
/// are part of the measurement, so a transcription slip in any of them would
/// make a Rust profile incomparable with a Go one.
#[test]
fn default_bench_suite_table_matches_go() {
    let expected: [(&str, &str, &str, i32); 36] = [
        ("math-e1", "math", "E", 16),
        ("math-e2", "math", "E", 16),
        ("math-e3", "math", "E", 16),
        ("logic-e1", "logic", "E", 16),
        ("logic-e2", "logic", "E", 16),
        ("fact-e1", "fact", "E", 16),
        ("fact-e2", "fact", "E", 16),
        ("instr-e1", "instruction", "E", 16),
        ("chinese-e1", "chinese", "E", 16),
        ("chinese-e2", "chinese", "E", 24),
        ("code-e1", "code", "E", 16),
        ("reason-e1", "reasoning", "E", 32),
        ("math-m1", "math", "M", 24),
        ("math-m2", "math", "M", 24),
        ("math-m3", "math", "M", 24),
        ("logic-m1", "logic", "M", 32),
        ("logic-m2", "logic", "M", 24),
        ("code-m1", "code", "M", 24),
        ("code-m2", "code", "M", 24),
        ("instr-m1", "instruction", "M", 48),
        ("instr-m2", "instruction", "M", 32),
        ("chinese-m1", "chinese", "M", 48),
        ("chinese-m2", "chinese", "M", 24),
        ("reason-m1", "reasoning", "M", 32),
        ("fact-m1", "fact", "M", 24),
        ("fact-m2", "fact", "M", 24),
        ("math-h1", "math", "H", 40),
        ("math-h2", "math", "H", 40),
        ("logic-h1", "logic", "H", 64),
        ("code-h1", "code", "H", 48),
        ("code-h2", "code", "H", 48),
        ("instr-h1", "instruction", "H", 48),
        ("chinese-h1", "chinese", "H", 32),
        ("reason-h1", "reasoning", "H", 32),
        ("reason-h2", "reasoning", "H", 32),
        ("fact-h1", "fact", "H", 24),
    ];
    let suite = default_bench_suite();
    assert_eq!(suite.len(), expected.len());
    for (case, (id, category, level, max_tokens)) in suite.iter().zip(expected) {
        assert_eq!(case.id, id);
        assert_eq!(case.category, category);
        assert_eq!(case.difficulty, level);
        assert_eq!(case.max_tokens, max_tokens);
    }
}

/// Prompts that span several lines in the Rust source must still be the single
/// Go string: the continuation-joined rows are the likeliest place for a
/// whitespace slip.
#[test]
fn default_bench_suite_prompts_join_correctly() {
    let suite = default_bench_suite();
    for (id, prompt) in [
        (
            "reason-e1",
            "A bat and a ball cost $1.10 together. The bat costs $1.00 more than the ball. How much does the ball cost in cents? Reply with the number only.",
        ),
        (
            "logic-m1",
            "Three friends A, B, C each have a different pet: cat, dog, fish. A does not have a cat. B has a dog. Who has the fish? Reply with one letter.",
        ),
        (
            "instr-m2",
            "Write the word ALPHA in uppercase, then a hyphen, then the word beta in lowercase. Output only that.",
        ),
        (
            "reason-m1",
            "I have 3 apples. I eat one and give away half of the rest. How many do I have left? Reply with the number only.",
        ),
        (
            "logic-h1",
            "There are 5 houses in a row. The red house is immediately to the left of the blue house. The green house is at one end. If the blue house is in the middle, how many houses are to the right of the green house? Reply with the number only.",
        ),
        (
            "code-h1",
            "What does this print? `print(sum(1 for i in range(100) if i % 7 == 0))`. Reply with the number only.",
        ),
        (
            "code-h2",
            "In Python, what is the output of `print([x for x in range(5)][::-2])`? Reply with the list only.",
        ),
        (
            "reason-h2",
            "If it takes 5 machines 5 minutes to make 5 widgets, how long would it take 100 machines to make 100 widgets, in minutes? Reply with the number only.",
        ),
        (
            "instr-m1",
            r#"Output exactly this JSON and nothing else: {"a":[1,2],"b":"x"}"#,
        ),
        (
            "instr-h1",
            r#"Reply with exactly this JSON and nothing else: {"n": 1e3}"#,
        ),
        ("chinese-h1", "请用四个字概括“亡羊补牢”这个成语的含义，只输出四个字。"),
    ] {
        let case = suite.iter().find(|case| case.id == id).expect("case exists");
        assert_eq!(case.prompt, prompt, "prompt of {id} drifted");
    }
}

/// Scoring needs no judge model.
#[test]
fn grading_is_deterministic() {
    for case in default_bench_suite() {
        assert_eq!(
            grade_case(&case, "some reply"),
            grade_case(&case, "some reply"),
            "grading of {} is not deterministic",
            case.id
        );
    }
}

/// The normaliser and the numeric readers are the load-bearing helpers under the
/// graders, so they get their own boundary checks.
#[test]
fn normalize_and_number_readers() {
    assert_eq!(normalize("  The ANSWER is 42. "), "42");
    assert_eq!(normalize("答：北京"), "北京");
    assert_eq!(normalize("   \n\t "), "");
    assert_eq!(normalize("a   b"), "a b");

    assert_eq!(as_number("42"), Some(42.0));
    assert_eq!(as_number("`-1.5`"), Some(-1.5));
    assert_eq!(as_number("1e3"), Some(1000.0));
    // Go's `%g` scan reads a prefix, so a junk-suffixed number still parses as
    // long as every character is numeric punctuation.
    assert_eq!(as_number("1.2.3"), Some(1.2));
    assert_eq!(as_number("1,2"), None);
    assert_eq!(as_number("."), None);
    assert_eq!(as_number(""), None);

    assert_eq!(last_number("17 + 25 = 42"), Some(42.0));
    assert_eq!(last_number("3.5 kg"), Some(3.5));
    // Go trims `+`/`-` from both ends of the field before parsing, so a negative
    // literal loses its sign. That quirk is preserved deliberately: the router
    // and the grader have to agree with the Go implementation, including here.
    assert_eq!(last_number("-3.5 kg"), Some(3.5));
    assert_eq!(last_number("no digits here"), None);
    assert_eq!(last_number("ends with - sign -"), None);

    // Multi-byte truncation must land on a character boundary.
    assert_eq!(truncate("abcdef", 3), "abc...");
    assert_eq!(truncate("abc", 3), "abc");
    assert_eq!(truncate("中文测试", 4), "中...");
}

// ---------------------------------------------------------------------------
// Scoring assembly
// ---------------------------------------------------------------------------

#[test]
fn assemble_scores_merges_evidence() {
    let reports = vec![BenchReport {
        model: "gpt-4o".to_string(),
        score: 0.8,
        by_category: BTreeMap::from([
            ("math".to_string(), 1.0),
            ("code".to_string(), 0.6),
        ]),
        by_level: BTreeMap::from([("E".to_string(), 1.0), ("H".to_string(), 0.5)]),
        ..BenchReport::default()
    }];
    let fingerprints = vec![CompareResult {
        model: "gpt-4o".to_string(),
        verdict: VERDICT_MATCH.to_string(),
        score: 0.05,
        threshold: 0.35,
        ..CompareResult::default()
    }];

    let got = assemble_scores(&reports, &fingerprints, 0.9, 0.95);
    let scores = got.get("gpt-4o").expect("no score produced");
    assert_eq!(scores.capability, 0.8);
    assert_eq!(
        scores.fingerprint_match, 1.0,
        "a match verdict should score 1"
    );
    assert_eq!(scores.usage_integrity, 0.9);
    assert_eq!(scores.reliability, 0.95);
    assert_eq!(scores.benchmarks.get("math"), Some(&1.0));
    assert_eq!(scores.benchmarks.get("level_E"), Some(&1.0));
    assert_eq!(scores.benchmarks.get("code"), Some(&0.6));
    assert_eq!(scores.samples, 1);
}

/// A fingerprint mismatch reduces the score proportionally to how far the
/// divergence exceeded the threshold.
#[test]
fn assemble_scores_mismatch_scales_down() {
    let fingerprints = vec![CompareResult {
        model: "gpt-4o".to_string(),
        verdict: VERDICT_MISMATCH.to_string(),
        score: 0.5,
        threshold: 0.35,
        ..CompareResult::default()
    }];
    let got = assemble_scores(&[], &fingerprints, 1.0, 1.0);
    let scores = got.get("gpt-4o").expect("no score produced");
    assert!(
        scores.fingerprint_match > 0.0 && scores.fingerprint_match < 1.0,
        "mismatch should scale the score below 1, got {}",
        scores.fingerprint_match
    );
    // 0.5 - 0.35 = 0.15 over the threshold, so 0.85 of the credit remains.
    assert!((scores.fingerprint_match - 0.85).abs() < 1e-9);
}

/// A mismatch that exceeds the threshold by more than 1 saturates at zero rather
/// than going negative.
#[test]
fn assemble_scores_mismatch_saturates() {
    let fingerprints = vec![CompareResult {
        model: "gpt-4o".to_string(),
        verdict: VERDICT_MISMATCH.to_string(),
        score: 9.0,
        threshold: 0.35,
        ..CompareResult::default()
    }];
    let got = assemble_scores(&[], &fingerprints, 1.0, 1.0);
    assert_eq!(got["gpt-4o"].fingerprint_match, 0.0);
}

/// An unmeasured channel is neither rewarded nor punished.
#[test]
fn assemble_scores_no_baseline_is_neutral() {
    let fingerprints = vec![CompareResult {
        model: "gpt-4o".to_string(),
        verdict: VERDICT_NO_BASELINE.to_string(),
        ..CompareResult::default()
    }];
    let got = assemble_scores(&[], &fingerprints, 1.0, 1.0);
    assert_eq!(got["gpt-4o"].fingerprint_match, 0.0);
    assert_eq!(got["gpt-4o"].usage_integrity, 1.0);
    assert_eq!(got["gpt-4o"].reliability, 1.0);
}

/// A fingerprint result with no model name cannot be attributed to a channel and
/// is dropped, as in Go.
#[test]
fn assemble_scores_skips_unnamed_fingerprint() {
    let fingerprints = vec![CompareResult {
        verdict: VERDICT_MATCH.to_string(),
        ..CompareResult::default()
    }];
    let got = assemble_scores(&[], &fingerprints, 1.0, 1.0);
    assert!(got.is_empty());
}

// ---------------------------------------------------------------------------
// Price collection
// ---------------------------------------------------------------------------

/// Normalization of a new-api pricing payload, including the multiplier and flat
/// schemes.
#[tokio::test]
async fn collect_prices_from_new_api() {
    let payload = serde_json::json!({
        "success": true,
        "data": [
            {
                "model_name": "gpt-4o", "quota_type": 0,
                "model_ratio": 1.25, "completion_ratio": 4.0,
            },
            {
                "model_name": "flat-model", "quota_type": 1,
                "model_price": 3.0, "completion_ratio": 2.0,
            },
        ],
        "group_ratio": {"default": 1.0, "vip": 0.8},
        "usd_exchange_rate": 7.0,
    })
    .to_string();
    let server = StubServer::start(200, &payload).await;

    let entries = collect_prices(
        None,
        &PriceSource {
            channel: "upstream-a".to_string(),
            kind: "newapi".to_string(),
            url: server.base_url.clone(),
            currency: "USD".to_string(),
            ..PriceSource::default()
        },
        "default",
    )
    .await
    .expect("collect prices");
    assert_eq!(entries.len(), 2);

    let mut by_model: BTreeMap<String, f64> = BTreeMap::new();
    for entry in &entries {
        assert_eq!(entry.channel, "upstream-a");
        assert_eq!(entry.price.currency, "USD");
        assert_eq!(entry.price.source, server.base_url);
        assert!(entry.price.updated_at > crate::config::zero_time());
        by_model.insert(entry.model.clone(), entry.price.input_per_m);
    }
    // multiplier: 2.0 base * 1.25 ratio * 1.0 group * 7.0 fx = 17.5
    assert_eq!(by_model["gpt-4o"], 17.5, "multiplier normalization");
    // flat: 3.0 * 7.0 = 21
    assert_eq!(by_model["flat-model"], 21.0, "flat normalization");
    // the entries come back sorted by model name, like Go's sort.Slice
    assert_eq!(entries[0].model, "flat-model");
    assert_eq!(entries[1].model, "gpt-4o");
    // the completion ratio survives into the normalized output
    assert_eq!(entries[1].price.output_per_m, 17.5 * 4.0);

    assert_eq!(server.requests().len(), 1);
    assert!(server.requests()[0].starts_with("GET / HTTP/1.1"));
}

/// A non-default group changes the normalized price.
#[tokio::test]
async fn collect_prices_group_ratio() {
    let payload = serde_json::json!({
        "success": true,
        "data": [{
            "model_name": "m", "quota_type": 0, "model_ratio": 1.0, "completion_ratio": 1.0,
        }],
        "group_ratio": {"default": 1.0, "vip": 0.5},
    })
    .to_string();
    let server = StubServer::start(200, &payload).await;

    let base = collect_prices(
        None,
        &PriceSource {
            channel: "c".to_string(),
            kind: "newapi".to_string(),
            url: server.base_url.clone(),
            ..PriceSource::default()
        },
        "default",
    )
    .await
    .expect("collect base prices");
    let vip = collect_prices(
        None,
        &PriceSource {
            channel: "c".to_string(),
            kind: "newapi".to_string(),
            url: server.base_url.clone(),
            ..PriceSource::default()
        },
        "vip",
    )
    .await
    .expect("collect vip prices");
    assert!(
        vip[0].price.input_per_m < base[0].price.input_per_m,
        "vip group should be cheaper: {} vs {}",
        vip[0].price.input_per_m,
        base[0].price.input_per_m
    );
    // No usd_exchange_rate in the payload, so the 7.1 fallback applies.
    assert_eq!(base[0].price.input_per_m, 2.0 * 7.1);
    assert_eq!(vip[0].price.input_per_m, 2.0 * 0.5 * 7.1);

    // Without an explicit group the payload's "default" entry is used.
    let implicit = collect_prices(
        None,
        &PriceSource {
            channel: "c".to_string(),
            kind: "newapi".to_string(),
            url: server.base_url.clone(),
            ..PriceSource::default()
        },
        "",
    )
    .await
    .expect("collect default prices");
    assert_eq!(implicit[0].price.input_per_m, base[0].price.input_per_m);
}

/// The rate in the payload is used when the source does not name one, and the
/// source's own rate wins when both are present.
#[tokio::test]
async fn collect_prices_exchange_rate_precedence() {
    let payload = serde_json::json!({
        "data": [{"model_name": "m", "quota_type": 1, "model_price": 1.0}],
        "usd_exchange_rate": 7.0,
    })
    .to_string();
    let server = StubServer::start(200, &payload).await;
    let entries = collect_prices(
        None,
        &PriceSource {
            channel: "c".to_string(),
            kind: "newapi".to_string(),
            url: server.base_url.clone(),
            usd_cny: 6.0,
            ..PriceSource::default()
        },
        "",
    )
    .await
    .expect("collect prices");
    assert_eq!(entries[0].price.input_per_m, 6.0);
    assert_eq!(entries[0].price.currency, "USD");
}

#[tokio::test]
async fn collect_prices_http_error() {
    let server = StubServer::start(401, "").await;
    let err = collect_prices(
        None,
        &PriceSource {
            channel: "c".to_string(),
            kind: "newapi".to_string(),
            url: server.base_url.clone(),
            ..PriceSource::default()
        },
        "",
    )
    .await
    .expect_err("an HTTP failure must be an error");
    assert_eq!(
        err.to_string(),
        format!("intel: fetch {}: HTTP 401", server.base_url)
    );
}

#[tokio::test]
async fn collect_prices_unknown_kind() {
    let err = collect_prices(
        None,
        &PriceSource {
            channel: "c".to_string(),
            kind: "wat".to_string(),
            url: "http://127.0.0.1:1".to_string(),
            ..PriceSource::default()
        },
        "",
    )
    .await
    .expect_err("an unknown source kind must be an error");
    assert_eq!(err.to_string(), "intel: unknown price source kind \"wat\"");
}

/// An unrecognised scheme is skipped rather than guessed at: a wrong price would
/// corrupt routing. `manual` sources collect nothing at all, by design.
#[tokio::test]
async fn collect_prices_skips_unknown_quota_type() {
    let payload = serde_json::json!({
        "success": true,
        "data": [
            {"model_name": "known", "quota_type": 0, "model_ratio": 1.0, "completion_ratio": 1.0},
            {"model_name": "weird", "quota_type": 99, "model_ratio": 1.0},
            {"model_name": "", "quota_type": 0, "model_ratio": 1.0},
        ],
    })
    .to_string();
    let server = StubServer::start(200, &payload).await;

    let entries = collect_prices(
        None,
        &PriceSource {
            channel: "c".to_string(),
            kind: "newapi".to_string(),
            url: server.base_url.clone(),
            ..PriceSource::default()
        },
        "",
    )
    .await
    .expect("collect prices");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].model, "known");

    let manual = collect_prices(
        None,
        &PriceSource {
            channel: "c".to_string(),
            kind: "manual".to_string(),
            ..PriceSource::default()
        },
        "",
    )
    .await
    .expect("a manual source is not an error");
    assert!(manual.is_empty());
}

/// A payload that is not a price table is a parse error naming the endpoint.
#[tokio::test]
async fn collect_prices_malformed_payload() {
    let server = StubServer::start(200, "{not json").await;
    let err = collect_prices(
        None,
        &PriceSource {
            channel: "c".to_string(),
            kind: "newapi".to_string(),
            url: server.base_url.clone(),
            ..PriceSource::default()
        },
        "",
    )
    .await
    .expect_err("a malformed payload must be an error");
    assert!(
        err.to_string().starts_with("intel: parse "),
        "unexpected error text: {err}"
    );
}

/// An API key, when configured, is sent as a bearer token.
#[tokio::test]
async fn collect_prices_sends_api_key() {
    let payload = serde_json::json!({"data": []}).to_string();
    let server = StubServer::start(200, &payload).await;
    collect_prices(
        None,
        &PriceSource {
            channel: "c".to_string(),
            kind: "newapi".to_string(),
            url: server.base_url.clone(),
            api_key: "sk-test".to_string(),
            ..PriceSource::default()
        },
        "",
    )
    .await
    .expect("collect prices");
    let request = server.requests().remove(0).to_lowercase();
    assert!(
        request.contains("authorization: bearer sk-test"),
        "missing credential header: {request}"
    );
}

// ---------------------------------------------------------------------------
// Catalogue
// ---------------------------------------------------------------------------

#[test]
fn catalogue_merge_and_save() {
    let dir = TempDir::new();
    let path = dir.file("catalogue.json");

    let catalogue = Catalogue::new();
    catalogue.load(&path).expect("load missing catalogue");
    catalogue.merge(&[PriceEntry {
        channel: "a".to_string(),
        model: "m".to_string(),
        price: PriceModel {
            input_per_m: 5.0,
            currency: "CNY".to_string(),
            ..PriceModel::default()
        },
    }]);
    assert_eq!(catalogue.entries().len(), 1);
    catalogue.save(&path).expect("save catalogue");

    let reopened = Catalogue::new();
    reopened.load(&path).expect("load catalogue");
    let entries = reopened.entries();
    assert_eq!(entries.len(), 1, "round-trip lost data: {entries:?}");
    assert_eq!(entries[0].price.input_per_m, 5.0);
    assert_eq!(entries[0].channel, "a");
    assert_eq!(entries[0].model, "m");
}

/// Entries are ordered by model then channel, matching Go's comparator, and the
/// key separator survives a channel or model that contains unusual characters.
#[test]
fn catalogue_entries_are_sorted_and_split() {
    let catalogue = Catalogue::new();
    catalogue.merge(&[
        PriceEntry {
            channel: "z".to_string(),
            model: "b".to_string(),
            price: PriceModel::default(),
        },
        PriceEntry {
            channel: "z".to_string(),
            model: "a".to_string(),
            price: PriceModel::default(),
        },
        PriceEntry {
            channel: "a".to_string(),
            model: "a".to_string(),
            price: PriceModel::default(),
        },
    ]);
    let entries = catalogue.entries();
    let keys: Vec<(&str, &str)> = entries
        .iter()
        .map(|entry| (entry.model.as_str(), entry.channel.as_str()))
        .collect();
    assert_eq!(keys, vec![("a", "a"), ("a", "z"), ("b", "z")]);
}

/// A catalogue file that names only some fields updates only those fields:
/// Go's `Load` copies a map when it is non-nil and skips it otherwise, which is
/// what makes a partial file (an older writer, a hand edit) non-destructive.
#[test]
fn catalogue_load_preserves_unmentioned_fields() {
    let dir = TempDir::new();
    let path = dir.file("partial.json");
    let catalogue = Catalogue::new();
    catalogue.merge(&[PriceEntry {
        channel: "a".to_string(),
        model: "m".to_string(),
        price: PriceModel {
            input_per_m: 5.0,
            source: "https://prices.example/api/pricing".to_string(),
            ..PriceModel::default()
        },
    }]);
    assert_eq!(catalogue.entries().len(), 1);

    // The file names `prices` (empty) but not `sources` or `updated_at`.
    fs::write(&path, r#"{"prices":{}}"#).expect("write partial file");
    catalogue.load(&path).expect("load partial catalogue");
    assert!(catalogue.entries().is_empty(), "prices came from the file");
    let value = serde_json::to_value(&catalogue).expect("serialize");
    assert!(
        value.get("sources").is_some(),
        "sources were not named by the file, so they stay in memory"
    );
    assert_eq!(
        value["updated_at"], serde_json::json!("0001-01-01T00:00:00Z"),
        "an unnamed timestamp resets, as Go's unconditional assignment does"
    );
}

/// The catalogue file is read by the comparison page and by `customsctl`, so its
/// shape is pinned: `prices` and `updated_at` always appear, `sources` only when
/// something was recorded.
#[test]
fn catalogue_json_shape_matches_go() {
    let dir = TempDir::new();
    let empty_path = dir.file("empty.json");
    let empty = Catalogue::new();
    empty.save(&empty_path).expect("save empty catalogue");
    let empty_text = fs::read_to_string(&empty_path).expect("read back");
    let empty_value: serde_json::Value = serde_json::from_str(&empty_text).expect("valid json");
    assert_eq!(empty_value["prices"], serde_json::json!({}));
    assert!(empty_value.get("updated_at").is_some());
    assert!(
        empty_value.get("sources").is_none(),
        "an empty source map is omitted"
    );

    let path = dir.file("full.json");
    let catalogue = Catalogue::new();
    catalogue.merge(&[PriceEntry {
        channel: "a".to_string(),
        model: "m".to_string(),
        price: PriceModel {
            input_per_m: 5.0,
            source: "https://prices.example/api/pricing".to_string(),
            updated_at: crate::config::zero_time(),
            ..PriceModel::default()
        },
    }]);
    catalogue.save(&path).expect("save catalogue");
    let text = fs::read_to_string(&path).expect("read back");
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    assert_eq!(value["prices"]["a\u{0}m"]["input_per_m"], 5.0);
    assert_eq!(
        value["sources"]["a\u{0}m"],
        "https://prices.example/api/pricing"
    );
}

// ---------------------------------------------------------------------------
// Probe runner
// ---------------------------------------------------------------------------

/// The L1 runner records per-probe observations, keeps the lengths of the
/// successful ones, and pins the channel through the customs header.
#[tokio::test]
async fn runner_collects_probe_results() {
    let server = StubServer::start(200, &chat_reply("gpt-4o-2024-08-06", "hello world")).await;
    let runner = Runner {
        base_url: server.base_url.clone(),
        token: "probe-token".to_string(),
        model: "gpt-4o".to_string(),
        channel: "upstream-a".to_string(),
        ..Runner::default()
    };
    let probes = vec![probe("first", "say hello", 16), probe("second", "say it again", 16)];
    let result = runner.run(&probes).await;

    assert_eq!(result.channel, "upstream-a");
    assert_eq!(result.model, "gpt-4o");
    assert_eq!(result.results.len(), 2);
    assert_eq!(result.lengths, vec![11, 11]);
    assert!(result.collected_at > crate::config::zero_time());
    let first = &result.results[0];
    assert_eq!(first.probe, "first");
    assert_eq!(first.reply, "hello world");
    assert_eq!(first.length, 11);
    assert_eq!(first.prompt_tokens, 5);
    assert_eq!(first.completion_tokens, 2);
    assert_eq!(first.model, "gpt-4o-2024-08-06");
    assert!(first.error.is_empty());
    assert_eq!(server.requests().len(), 2);

    let request = server.requests().remove(0).to_lowercase();
    assert!(request.contains("x-customs-channel: upstream-a"), "{request}");
    assert!(request.contains("authorization: bearer probe-token"), "{request}");
}

/// The probe request body is audited by the gateway, so its byte layout follows
/// the Go implementation: sorted keys and an integral temperature.
#[tokio::test]
async fn runner_request_body_matches_go_layout() {
    let server = StubServer::start(200, &chat_reply("m", "ok")).await;
    let runner = Runner {
        base_url: server.base_url.clone(),
        token: "t".to_string(),
        model: "m".to_string(),
        ..Runner::default()
    };
    let mut count = probe(
        "count",
        "Count from 1 to 20, separated by spaces. Output only the numbers.",
        80,
    );
    count.system = "be terse".to_string();
    runner.run(&[count]).await;
    assert_eq!(
        server.last_body(),
        r#"{"max_tokens":80,"messages":[{"content":"be terse","role":"system"},{"content":"Count from 1 to 20, separated by spaces. Output only the numbers.","role":"user"}],"model":"m","temperature":0}"#
    );
    assert!(server.requests()[0].starts_with("POST /v1/chat/completions HTTP/1.1"));
}

/// A failed probe records the error and contributes no length, so the baseline
/// comparison never sees a zero sample.
#[tokio::test]
async fn runner_records_probe_failures() {
    let server = StubServer::start(500, "boom").await;
    let runner = Runner {
        base_url: server.base_url.clone(),
        token: String::new(),
        model: "m".to_string(),
        ..Runner::default()
    };
    let result = runner.run(&[probe("only", "hi", 8)]).await;
    assert_eq!(result.results.len(), 1);
    assert!(result.lengths.is_empty());
    assert_eq!(result.results[0].error, "HTTP 500: boom");
}

/// Go accepts exactly 200, so a 2xx that is not 200 is a failure even when the
/// body is a perfectly good completion: the gateway's own success contract is
/// 200 and a proxy changing it is worth flagging.
#[tokio::test]
async fn runner_rejects_non_200_success_codes() {
    let server = StubServer::start(201, &chat_reply("m", "hi")).await;
    let runner = Runner {
        base_url: server.base_url.clone(),
        model: "m".to_string(),
        ..Runner::default()
    };
    let result = runner.run(&[probe("only", "hi", 8)]).await;
    assert!(
        result.results[0].error.starts_with("HTTP 201: "),
        "unexpected error text: {}",
        result.results[0].error
    );
    assert!(result.lengths.is_empty());
}

/// A 200 response that is not a chat completion is a malformed response rather
/// than an empty observation.
#[tokio::test]
async fn runner_reports_malformed_response() {
    let server = StubServer::start(200, "not json").await;
    let runner = Runner {
        base_url: server.base_url.clone(),
        model: "m".to_string(),
        ..Runner::default()
    };
    let result = runner.run(&[probe("only", "hi", 8)]).await;
    assert!(
        result.results[0].error.starts_with("malformed response: "),
        "unexpected error text: {}",
        result.results[0].error
    );
    assert!(result.lengths.is_empty());
}

/// The default probe set is short and cheap: six named probes, all at
/// temperature 0 so that two runs are comparable.
#[test]
fn default_probes_are_well_formed() {
    let probes = default_probes();
    let names: Vec<&str> = probes.iter().map(|probe| probe.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["count", "explain", "chinese", "code", "refuse", "json"]
    );
    for probe in &probes {
        assert!(!probe.prompt.is_empty());
        assert!(probe.max_tokens > 0);
        assert_eq!(probe.temperature, 0.0);
        assert!(probe.system.is_empty());
    }
    assert_eq!(
        probes[5].prompt,
        r#"Reply with only this JSON object and nothing else: {"ok":true,"n":7}"#
    );
}

// ---------------------------------------------------------------------------
// Bench runner
// ---------------------------------------------------------------------------

/// The L2 runner grades every case, reports the score and builds the per
/// category/level profile from the same answers.
#[tokio::test]
async fn bench_runner_grades_and_profiles() {
    let server = StubServer::start(200, &chat_reply("m", "42")).await;
    let runner = BenchRunner {
        base_url: server.base_url.clone(),
        token: "bench-token".to_string(),
        model: "gpt-4o".to_string(),
        channel: "upstream-a".to_string(),
        include_outcomes: true,
        ..BenchRunner::default()
    };
    let suite = vec![
        BenchCase {
            id: "math-e1".to_string(),
            category: "math".to_string(),
            difficulty: "E".to_string(),
            prompt: "17 + 25?".to_string(),
            expect: "42".to_string(),
            max_tokens: 16,
            ..BenchCase::default()
        },
        BenchCase {
            id: "math-m1".to_string(),
            category: "math".to_string(),
            difficulty: "M".to_string(),
            prompt: "2^10?".to_string(),
            expect: "1024".to_string(),
            max_tokens: 16,
            ..BenchCase::default()
        },
    ];
    let report = runner.run(&suite).await;

    assert_eq!(report.channel, "upstream-a");
    assert_eq!(report.model, "gpt-4o");
    assert_eq!(report.total, 2);
    assert_eq!(report.passed, 1);
    assert_eq!(report.score, 0.5);
    assert_eq!(report.by_category.get("math"), Some(&0.5));
    assert_eq!(report.by_level.get("E"), Some(&1.0));
    assert_eq!(report.by_level.get("M"), Some(&0.0));
    assert!(report.started <= report.finished);
    assert_eq!(report.outcomes.len(), 2);
    assert_eq!(report.outcomes[0].case.id, "math-e1");
    assert!(report.outcomes[0].passed);
    assert_eq!(report.outcomes[0].reply, "42");
    assert!(!report.outcomes[1].passed);
}

/// Outcomes are verbose and therefore opt-in; without the flag the report still
/// carries the aggregate numbers.
#[tokio::test]
async fn bench_runner_omits_outcomes_by_default() {
    let server = StubServer::start(200, &chat_reply("m", "42")).await;
    let runner = BenchRunner {
        base_url: server.base_url.clone(),
        model: "m".to_string(),
        ..BenchRunner::default()
    };
    let report = runner.run(&[exact("42")]).await;
    assert_eq!(report.total, 1);
    assert_eq!(report.passed, 1);
    assert!(report.outcomes.is_empty());
}

/// A transport failure marks the case failed but never aborts the run: one dead
/// upstream must not stop the profile for the rest of the suite.
#[tokio::test]
async fn bench_runner_survives_case_failures() {
    let server = StubServer::start(500, "").await;
    let runner = BenchRunner {
        base_url: server.base_url.clone(),
        model: "m".to_string(),
        include_outcomes: true,
        ..BenchRunner::default()
    };
    let report = runner.run(&[exact("42"), exact("7")]).await;
    assert_eq!(report.total, 2);
    assert_eq!(report.passed, 0);
    assert_eq!(report.score, 0.0);
    assert!(report.outcomes.iter().all(|outcome| !outcome.passed));
    assert_eq!(report.outcomes[0].error, "HTTP 500");
}

/// An empty suite is not an error and does not divide by zero.
#[tokio::test]
async fn bench_runner_empty_suite() {
    let runner = BenchRunner::default();
    let report = runner.run(&[]).await;
    assert_eq!(report.total, 0);
    assert_eq!(report.score, 0.0);
    assert!(report.by_category.is_empty());
    assert!(report.by_level.is_empty());
}

/// The bench report is written to `benchmarks.json` and read back by the scoring
/// pass, so its tags are pinned.
#[test]
fn bench_report_json_shape_matches_go() {
    let report = BenchReport {
        channel: "c".to_string(),
        model: "m".to_string(),
        total: 3,
        passed: 2,
        score: 2.0 / 3.0,
        by_category: BTreeMap::from([("math".to_string(), 1.0)]),
        by_level: BTreeMap::from([("E".to_string(), 1.0)]),
        ..BenchReport::default()
    };
    let value = serde_json::to_value(&report).expect("serialize report");
    assert_eq!(value["channel"], "c");
    assert_eq!(value["total"], 3);
    assert_eq!(value["passed"], 2);
    assert_eq!(value["by_category"]["math"], 1.0);
    assert_eq!(value["by_level"]["E"], 1.0);
    assert!(value.get("started").is_some());
    assert!(value.get("finished").is_some());
    assert!(value.get("outcomes").is_none(), "outcomes are omitempty");

    // The maps come back as maps, and a `null` map is tolerated on read.
    let round_tripped: BenchReport = serde_json::from_value(value).expect("round trip");
    assert_eq!(round_tripped.by_category.get("math"), Some(&1.0));
    let nulled: BenchReport =
        serde_json::from_str(r#"{"channel":"c","model":"m","total":0,"passed":0,"score":0,"by_category":null,"by_level":null}"#)
            .expect("null maps are accepted");
    assert!(nulled.by_category.is_empty());
}

/// A comparison result is embedded in `fingerprints.json`; its tags and its
/// `omitempty` fields are part of that file's contract.
#[test]
fn compare_result_json_shape_matches_go() {
    let result = CompareResult {
        model: "gpt-4o".to_string(),
        score: 0.05,
        threshold: 0.35,
        verdict: VERDICT_MATCH.to_string(),
        baseline_source: "official".to_string(),
        baseline_samples: 6,
        observed_samples: 6,
        sample_hash: "deadbeefdeadbeef".to_string(),
    };
    let value = serde_json::to_value(&result).expect("serialize");
    assert_eq!(value["model"], "gpt-4o");
    assert_eq!(value["verdict"], "match");
    assert_eq!(value["score"], 0.05);
    assert_eq!(value["threshold"], 0.35);
    assert_eq!(value["baseline_source"], "official");
    assert_eq!(value["baseline_samples"], 6);
    assert_eq!(value["observed_samples"], 6);
    assert_eq!(value["sample_hash"], "deadbeefdeadbeef");

    let bare = CompareResult {
        verdict: VERDICT_NO_BASELINE.to_string(),
        threshold: 0.35,
        observed_samples: 3,
        ..CompareResult::default()
    };
    let value = serde_json::to_value(&bare).expect("serialize");
    assert!(value.get("model").is_none());
    assert!(value.get("baseline_source").is_none());
    assert!(value.get("baseline_samples").is_none());
    assert!(value.get("sample_hash").is_none());
    assert_eq!(value["verdict"], "no_baseline");
    assert_eq!(value["observed_samples"], 3);

    // Any verdict string survives a round trip: old files must stay readable.
    let custom: CompareResult =
        serde_json::from_str(r#"{"verdict":"quarantined","score":2,"threshold":1}"#)
            .expect("unknown verdict strings are accepted");
    assert_eq!(custom.verdict, "quarantined");
}

/// A fingerprints file is a map of `"channel\0model"` to a channel result; the
/// Go spelling of the key is what `benchd` groups by.
#[test]
fn channel_result_json_shape_matches_go() {
    let result = ChannelResult {
        channel: "c".to_string(),
        model: "m".to_string(),
        results: vec![ProbeResult {
            probe: "count".to_string(),
            reply: "1 2 3".to_string(),
            length: 5,
            prompt_tokens: 4,
            completion_tokens: 3,
            model: "m-2024".to_string(),
            latency_ms: 12,
            error: String::new(),
        }],
        lengths: vec![5],
        collected_at: crate::config::zero_time(),
    };
    let value = serde_json::to_value(&result).expect("serialize");
    assert_eq!(value["channel"], "c");
    assert_eq!(value["model"], "m");
    assert_eq!(value["lengths"][0], 5);
    assert!(value.get("collected_at").is_some());
    let probe = &value["results"][0];
    assert_eq!(probe["probe"], "count");
    assert_eq!(probe["reply"], "1 2 3");
    assert_eq!(probe["length"], 5);
    assert_eq!(probe["prompt_tokens"], 4);
    assert_eq!(probe["completion_tokens"], 3);
    assert_eq!(probe["model"], "m-2024");
    assert_eq!(probe["latency_ms"], 12);
    assert!(probe.get("error").is_none(), "an empty error is omitempty");

    let map = serde_json::json!({ "c\u{0}m": value });
    let parsed: BTreeMap<String, ChannelResult> =
        serde_json::from_value(map).expect("channel results read back");
    assert_eq!(parsed["c\u{0}m"].lengths, vec![5]);
}

/// A price source is configuration-shaped; it has to round-trip through JSON as
/// well, because a deployment lists its sources in the config file.
#[test]
fn price_source_json_round_trip() {
    let source = PriceSource {
        channel: "a".to_string(),
        kind: "newapi".to_string(),
        url: "https://prices.example/api/pricing".to_string(),
        api_key: String::new(),
        currency: "USD".to_string(),
        usd_cny: 7.2,
    };
    let value = serde_json::to_value(&source).expect("serialize");
    assert_eq!(value["channel"], "a");
    assert_eq!(value["kind"], "newapi");
    assert_eq!(value["url"], "https://prices.example/api/pricing");
    assert_eq!(value["currency"], "USD");
    assert_eq!(value["usd_cny"], 7.2);
    assert!(value.get("api_key").is_none(), "an empty key is omitempty");
    let parsed: PriceSource = serde_json::from_value(value).expect("round trip");
    assert_eq!(parsed, source);
}
