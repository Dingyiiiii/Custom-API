//! Process-level tests for the operator CLI.
//!
//! They run the real binary and compare its stdout with the text the Go
//! implementation prints for the same fixtures; the deploy scripts parse that
//! text, so it is part of the interface. A real ledger is built with the core
//! audit writer, which also proves the CLI reads a ledger the gateway wrote.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use customapi_core::audit;

const BIN: &str = env!("CARGO_BIN_EXE_customsctl");

/// A throw-away directory that removes itself.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "customsctl-cli-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    fn path(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }

    fn join(&self, rel: &str) -> String {
        format!("{}/{}", self.path(), rel)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The configuration fixture: two channels, two users, two price rows.
fn write_config(dir: &TempDir) -> String {
    let path = dir.join("customs.json");
    fs::write(
        &path,
        r#"{
  "server": {
    "listen": "127.0.0.1:18080",
    "admin_listen": "127.0.0.1:18081",
    "audit_path": "var/audit.jsonl",
    "fail_mode": "closed",
    "route_strategy": "value",
    "log_level": "info"
  },
  "channels": [
    {"name": "alpha", "base_url": "http://127.0.0.1:19001/v1", "api_key": "sk-alpha",
     "protocol": "openai", "trust": "trusted", "models": {"mock-basic": "mock-basic"}},
    {"name": "beta", "base_url": "http://127.0.0.1:19001/v1",
     "protocol": "anthropic", "trust": "untrusted", "models": {"mock-basic": "mock-basic"}}
  ],
  "users": [
    {"name": "alice", "token": "alice-token", "quota": 0, "period": "monthly"},
    {"name": "bob", "token": "bob-token", "quota": 500000, "period": "daily"}
  ],
  "prices": [
    {"channel": "alpha", "model": "mock-basic",
     "price": {"input_per_m": 10, "output_per_m": 30, "currency": "CNY", "scheme": "flat"}},
    {"channel": "beta", "model": "mock-basic",
     "price": {"input_per_m": 8, "output_per_m": 24, "currency": "USD", "scheme": "flat"}}
  ]
}"#,
    )
    .expect("writes config");
    path
}

/// Builds a ledger with the four records the Go fixture produces.
fn write_ledger(dir: &TempDir, config_path: &str) -> String {
    let _ = config_path;
    let path = dir.join("var/audit.jsonl");
    let writer = audit::Writer::open(audit::WriterOptions {
        path: path.clone(),
        sync: false,
        truncate: true,
    })
    .expect("opens ledger");
    let base = Utc.with_ymd_and_hms(2026, 3, 4, 13, 5, 6).unwrap();
    let rows = [
        (
            "alice",
            "alpha",
            "mock-basic",
            "allow",
            "consistent",
            120,
            80,
            120,
            80,
            0.0036,
        ),
        (
            "bob",
            "beta",
            "mock-basic",
            "report",
            "over_billed",
            200,
            100,
            260,
            100,
            0.0041,
        ),
        (
            "alice",
            "",
            "gpt-x",
            "block_user",
            "not_reported",
            0,
            0,
            0,
            0,
            0.0,
        ),
        (
            "alice",
            "alpha",
            "gpt-x",
            "upstream_error",
            "not_reported",
            50,
            0,
            0,
            0,
            0.0,
        ),
    ];
    for (
        i,
        (
            actor,
            channel,
            model,
            decision,
            verdict,
            prompt,
            completion,
            up_prompt,
            up_completion,
            cost,
        ),
    ) in rows.iter().enumerate()
    {
        let started = base + chrono::Duration::seconds(i as i64 * 37);
        let mut record = audit::Record {
            id: format!("req-{:02}", i + 1),
            time: started,
            end_time: started + chrono::Duration::milliseconds(1500),
            actor: actor.to_string(),
            channel: channel.to_string(),
            model: model.to_string(),
            protocol: "openai".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            client_ip: "127.0.0.1".to_string(),
            decision: decision.to_string(),
            status_code: 200,
            ..audit::Record::default()
        };
        record.usage.prompt_tokens = *prompt;
        record.usage.completion_tokens = *completion;
        record.usage.total_tokens = prompt + completion;
        record.usage.upstream_prompt_tokens = *up_prompt;
        record.usage.upstream_completion_tokens = *up_completion;
        record.usage.upstream_total_tokens = up_prompt + up_completion;
        record.usage.upstream_reported = *up_prompt > 0;
        record.usage.method = "upstream".to_string();
        record.usage.verdict = verdict.to_string();
        record.cost.currency = "CNY".to_string();
        record.cost.total_cost = *cost;
        record.cost.list_cost = *cost;
        record.cost.basis = "catalogue".to_string();
        writer.append(&mut record).expect("appends");
    }
    let anchor = audit::Anchor {
        seq: writer.seq(),
        hash: writer.head(),
        time: base + chrono::Duration::hours(1),
        records: writer.seq(),
        component: "customsd".to_string(),
    };
    audit::append_anchor(&dir.join("var/anchor.jsonl"), &anchor).expect("anchor");
    writer.close().expect("closes");
    path
}

/// Runs the binary with the given arguments.
fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("runs customsctl")
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn err(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Serves one canned HTTP response and returns the base URL.
///
/// The CLI talks to the gateway's admin API over HTTP; a single-response server
/// is all a test needs, and it keeps the suite off the network.
fn serve_once(body: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let addr = listener.local_addr().expect("addr");
    let body = body.to_string();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .ok();
            // Drain the request (headers and, for POST, the body) before
            // answering; the client is waiting for the response.
            let mut buffer = [0u8; 8192];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{addr}")
}

#[test]
fn no_arguments_prints_the_usage() {
    let out = run(&[]);
    assert!(out.status.success());
    let text = text(&out);
    assert!(text.starts_with("customsctl - operator CLI"), "{text}");
    assert!(text.contains("audit verify  -config FILE"), "{text}");
    assert!(text.contains("fingerprint baseline"), "{text}");
}

#[test]
fn config_check_matches_the_go_summary() {
    let dir = TempDir::new("config-check");
    let config = write_config(&dir);
    let out = run(&["config", "check", "--config", &config]);
    assert!(out.status.success(), "{}", err(&out));
    // A raw string keeps the four-space indent: `\` line continuations would
    // swallow it, and the indent is part of Go's output.
    let expected = r#"OK: 2 channel(s), 2 user(s), 2 price row(s)
    listen=127.0.0.1:18080 admin=127.0.0.1:18081 strategy=value fail_mode=closed
    channel alpha        http://127.0.0.1:19001/v1    protocol=openai    trust=trusted   key=true
    channel beta         http://127.0.0.1:19001/v1    protocol=anthropic trust=untrusted key=false
"#;
    assert_eq!(text(&out), expected);
}

#[test]
fn single_dash_long_options_are_accepted() {
    let dir = TempDir::new("single-dash");
    let config = write_config(&dir);
    // Go's flag package accepts both spellings; operators keep using this one.
    let out = run(&["config", "check", "-config", &config]);
    assert!(out.status.success(), "{}", err(&out));
    assert!(text(&out).starts_with("OK: 2 channel(s)"));
}

#[test]
fn audit_verify_reports_ok_and_the_head() {
    let dir = TempDir::new("verify-ok");
    let config = write_config(&dir);
    let ledger = write_ledger(&dir, &config);
    let anchor = dir.join("var/anchor.jsonl");
    let out = run(&["audit", "verify", "--config", &config]);
    assert!(out.status.success(), "{}", err(&out));
    let text = text(&out);
    let head = audit::read_all(&ledger)
        .expect("reads")
        .last()
        .map(|r| r.entry_hash.clone())
        .unwrap_or_default();
    let expected = format!(
        "ledger:      {ledger}\n\
anchors:     {anchor}\n\
records:     4\n\
head:        seq=4 hash={head}\n\
anchors:     checked=1 ignored=0\n\
result:      OK - hash chain intact\n"
    );
    assert_eq!(text, expected);
}

#[test]
fn audit_verify_detects_a_tampered_record() {
    let dir = TempDir::new("verify-tampered");
    let config = write_config(&dir);
    let ledger = write_ledger(&dir, &config);
    let content = fs::read_to_string(&ledger).expect("reads ledger");
    let tampered = content.replacen(r#""total_tokens":200"#, r#""total_tokens":999999"#, 1);
    fs::write(&ledger, tampered).expect("writes tampered ledger");

    let out = run(&["audit", "verify", "--ledger", &ledger]);
    assert!(!out.status.success());
    let text = text(&out);
    assert!(
        text.contains("result:      FAILED - 1 violation(s)"),
        "{text}"
    );
    assert!(text.contains("entry_hash_mismatch"), "{text}");
    assert_eq!(err(&out), "customsctl: audit chain verification failed\n");
}

#[test]
fn audit_tail_prints_the_go_table() {
    let dir = TempDir::new("tail");
    let config = write_config(&dir);
    write_ledger(&dir, &config);
    let out = run(&["audit", "tail", "--config", &config]);
    assert!(out.status.success(), "{}", err(&out));
    let expected = "\
SEQ  TIME      ACTOR  CHANNEL     MODEL       DECISION        TOKENS  UPSTREAM  VERDICT       COST\n\
1    13:05:06  alice  alpha       mock-basic  allow           200     200       consistent    0.003600\n\
2    13:05:43  bob    beta        mock-basic  report          300     360       over_billed   0.004100\n\
3    13:06:20  alice  (unrouted)  gpt-x       block_user      0       0         not_reported  0.000000\n\
4    13:06:57  alice  alpha       gpt-x       upstream_error  50      0         not_reported  0.000000\n";
    assert_eq!(text(&out), expected);
}

#[test]
fn audit_tail_filters_and_limits() {
    let dir = TempDir::new("tail-filter");
    let config = write_config(&dir);
    write_ledger(&dir, &config);
    let out = run(&[
        "audit", "tail", "--config", &config, "-n", "2", "--actor", "alice",
    ]);
    assert!(out.status.success(), "{}", err(&out));
    let text = text(&out);
    // Header plus two rows, and the newest records win.
    assert_eq!(text.lines().count(), 3, "{text}");
    assert!(text.contains("(unrouted)"), "{text}");
    assert!(text.contains("upstream_error"), "{text}");
    assert!(!text.contains("mock-basic"), "{text}");
}

#[test]
fn audit_tail_json_uses_go_timestamp_text() {
    let dir = TempDir::new("tail-json");
    let config = write_config(&dir);
    write_ledger(&dir, &config);
    let out = run(&["audit", "tail", "--config", &config, "--json", "-n", "1"]);
    assert!(out.status.success(), "{}", err(&out));
    let text = text(&out);
    // Go writes RFC3339Nano: trailing zeros in the fraction are trimmed, so the
    // 1.5s end time is `.5Z`, not `.500Z`.
    assert!(
        text.contains(r#""end_time": "2026-03-04T13:06:58.5Z""#),
        "{text}"
    );
    assert!(text.contains(r#""decision": "upstream_error""#), "{text}");
}

#[test]
fn audit_stats_matches_the_go_aggregate() {
    let dir = TempDir::new("stats");
    let config = write_config(&dir);
    write_ledger(&dir, &config);
    let out = run(&["audit", "stats", "--config", &config]);
    assert!(out.status.success(), "{}", err(&out));
    // Raw string so the two-space indent of the count lines survives.
    let expected = r#"records:          4
gateway tokens:   550
upstream tokens:  560
token inflation:  +1.82%
total cost:       0.007700
by channel:
  (unrouted)               1
  alpha                    2
  beta                     1
by model:
  gpt-x                    2
  mock-basic               2
by verdict:
  consistent               1
  not_reported             2
  over_billed              1
"#;
    assert_eq!(text(&out), expected);
}

#[test]
fn prices_compare_prints_the_go_table_cheapest_first() {
    let dir = TempDir::new("prices-compare");
    let config = write_config(&dir);
    let out = run(&[
        "prices",
        "compare",
        "--config",
        &config,
        "--model",
        "mock-basic",
    ]);
    assert!(out.status.success(), "{}", err(&out));
    let expected = "\
MODEL    mock-basic  (1000 in / 500 out tokens)\n\
CHANNEL  TRUST       IN/M     OUT/M    CUR  LIST      COST      DISC%  PROMOTIONS\n\
beta     untrusted   8.0000   24.0000  USD  0.020000  0.020000  0.00   \n\
alpha    trusted     10.0000  30.0000  CNY  0.025000  0.025000  0.00   \n";
    assert_eq!(text(&out), expected);
}

#[test]
fn prices_normalize_prints_go_shaped_json() {
    let out = run(&["prices", "normalize"]);
    assert!(out.status.success(), "{}", err(&out));
    let printed = text(&out);
    // Integral floats lose the `.0`, and an unstamped price carries Go's zero
    // time, not the Unix epoch.
    assert!(printed.contains(r#""completion_ratio": 1,"#), "{printed}");
    assert!(
        printed.contains(r#""updated_at": "0001-01-01T00:00:00Z""#),
        "{printed}"
    );
    assert!(
        printed.contains("effective unit price: 14.200000 USD per 1M tokens"),
        "{printed}"
    );

    let out = run(&[
        "prices",
        "normalize",
        "--scheme",
        "flat",
        "--input-per-m",
        "12.5",
        "--completion-ratio",
        "3",
    ]);
    let flat = text(&out);
    assert!(flat.contains(r#""input_per_m": 88.75"#), "{flat}");
    assert!(flat.contains(r#""output_per_m": 266.25"#), "{flat}");
}

#[test]
fn users_list_uses_the_admin_api() {
    let body = r#"{"count":2,"users":[
        {"name":"alice","trust":"trusted","period":"monthly","quota":0,"consumed":0,
         "lifetime_consumed":0,"requests":5,"spend":0.1234,
         "last_seen":"2026-03-04T13:05:06Z","enabled":true},
        {"name":"bob","trust":"untrusted","period":"daily","quota":500000,"consumed":1200,
         "requests":2,"spend":0,"last_seen":"0001-01-01T00:00:00Z","enabled":false}]}"#;
    let base = serve_once(body);
    let out = run(&["users", "list", "--admin", &base]);
    assert!(out.status.success(), "{}", err(&out));
    let expected = "\
USER   TRUST      PERIOD   QUOTA   CONSUMED  REMAINING  REQUESTS  SPEND     LAST SEEN\n\
alice  trusted    monthly  0       0         unlimited  5         0.123400  03-04 13:05\n\
bob    untrusted  daily    500000  1200      498800     2         0.000000  -\n";
    assert_eq!(text(&out), expected);
}

#[test]
fn users_grant_prints_the_sorted_user_object() {
    let body = r#"{"action":"grant","ok":true,"user":{"name":"bob","trust":"untrusted",
        "period":"daily","quota":900,"consumed":0,"lifetime_consumed":0,"requests":0,
        "spend":0,"currency":"CNY","period_start":"0001-01-01T00:00:00Z",
        "last_seen":"0001-01-01T00:00:00Z","enabled":true}}"#;
    let base = serve_once(body);
    let out = run(&[
        "users", "grant", "--admin", &base, "--user", "bob", "--quota", "900",
    ]);
    assert!(out.status.success(), "{}", err(&out));
    // Go re-marshals `map[string]any`, so keys are sorted and floats that are
    // whole numbers print without a fraction.
    assert_eq!(
        text(&out),
        "grant: {\"consumed\":0,\"currency\":\"CNY\",\"enabled\":true,\"last_seen\":\"0001-01-01T00:00:00Z\",\"lifetime_consumed\":0,\"name\":\"bob\",\"period\":\"daily\",\"period_start\":\"0001-01-01T00:00:00Z\",\"quota\":900,\"requests\":0,\"spend\":0,\"trust\":\"untrusted\"}\n"
    );
}

#[test]
fn users_add_posts_the_add_quota_action() {
    let body = r#"{"action":"add_quota","ok":true,"user":{"name":"bob"}}"#;
    let base = serve_once(body);
    let out = run(&[
        "users", "add", "--admin", &base, "--user", "bob", "--quota", "100",
    ]);
    assert!(out.status.success(), "{}", err(&out));
    assert_eq!(text(&out), "add_quota: {\"name\":\"bob\"}\n");
}

#[test]
fn users_requires_a_user_name() {
    let out = run(&["users", "grant"]);
    assert!(!out.status.success());
    assert_eq!(err(&out), "customsctl: --user is required\n");
}

#[test]
fn unknown_commands_print_the_usage_and_go_error() {
    let out = run(&["foo", "bar"]);
    assert!(!out.status.success());
    assert!(text(&out).starts_with("customsctl - operator CLI"));
    assert_eq!(err(&out), "customsctl: unknown command \"foo bar\"\n");
}

#[test]
fn missing_config_is_reported_like_go() {
    let out = run(&["audit", "tail"]);
    assert!(!out.status.success());
    assert_eq!(err(&out), "customsctl: --config is required\n");

    let out = run(&["audit", "verify"]);
    assert!(!out.status.success());
    assert_eq!(
        err(&out),
        "customsctl: either --config or --ledger is required\n"
    );
}

#[test]
fn an_unreadable_config_reports_the_core_error() {
    let dir = TempDir::new("bad-config");
    let path = dir.join("missing.json");
    let out = run(&["config", "check", "--config", &path]);
    assert!(!out.status.success());
    assert!(
        err(&out).starts_with("customsctl: config: file not found"),
        "{}",
        err(&out)
    );
}

#[test]
fn a_ledger_without_anchors_omits_the_anchor_lines() {
    let dir = TempDir::new("no-anchor");
    let config = write_config(&dir);
    let ledger = write_ledger(&dir, &config);
    fs::remove_file(dir.join("var/anchor.jsonl")).expect("removes anchor");
    let out = run(&["audit", "verify", "--ledger", &ledger]);
    assert!(out.status.success(), "{}", err(&out));
    let text = text(&out);
    assert!(!text.contains("anchors:"), "{text}");
    assert!(
        text.contains("result:      OK - hash chain intact"),
        "{text}"
    );
}
