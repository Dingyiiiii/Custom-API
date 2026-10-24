//! Process-level tests for the intelligence daemon.
//!
//! Each test runs the real binary against a fixture directory and checks the
//! evidence files it produces: those files are shared with the Go implementation,
//! so their shape (keys, ordering, number formatting) is part of the contract.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

use customapi_core::audit;

const BIN: &str = env!("CARGO_BIN_EXE_benchd");

/// A throw-away directory that removes itself.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "benchd-cli-{name}-{}-{}",
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

/// The configuration fixture used by the daemon tests.
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
     "protocol": "openai", "trust": "untrusted", "models": {"mock-basic": "mock-basic"}}
  ],
  "users": [
    {"name": "alice", "token": "alice-token", "quota": 0, "period": "monthly"},
    {"name": "probe", "token": "probe-token", "quota": 0, "trust": "trusted"}
  ],
  "prices": [
    {"channel": "alpha", "model": "mock-basic",
     "price": {"input_per_m": 10, "output_per_m": 30, "currency": "CNY", "scheme": "flat"}}
  ]
}"#,
    )
    .expect("writes config");
    path
}

/// Writes the evidence files a `score` run folds together.
fn write_evidence(dir: &TempDir) {
    fs::create_dir_all(dir.join("var")).expect("mkdir var");
    fs::create_dir_all(dir.join("etc")).expect("mkdir etc");
    fs::write(
        dir.join("var/benchmarks.json"),
        r#"[{"channel":"alpha","model":"mock-basic","total":10,"passed":8,"score":0.8,
             "by_category":{"code":1,"math":0.75},"by_level":{"E":1,"H":0.5},
             "started":"2026-03-04T13:00:00Z","finished":"2026-03-04T13:02:00Z"},
            {"channel":"beta","model":"mock-basic","total":10,"passed":6,"score":0.6,
             "by_category":{"code":0.5},"by_level":{"E":0.75},
             "started":"2026-03-04T13:00:00Z","finished":"2026-03-04T13:05:00Z"}]"#,
    )
    .expect("writes benchmarks");
    fs::write(
        dir.join("var/fingerprints.json"),
        r#"{"alpha\u0000mock-basic":{"channel":"alpha","model":"mock-basic","results":[],
             "lengths":[12,18,25,30],"collected_at":"2026-03-04T13:10:00Z"},
            "beta\u0000mock-basic":{"channel":"beta","model":"mock-basic","results":[],
             "lengths":[40,41,42,44],"collected_at":"2026-03-04T13:11:00Z"}}"#,
    )
    .expect("writes fingerprints");
    fs::write(
        dir.join("etc/baselines.json"),
        r#"{"version":1,"updated_at":"2026-03-01T00:00:00Z",
            "models":{"mock-basic":{"model":"mock-basic","source":"trusted-endpoint",
            "samples":[12,19,25,31],"distinct":4,"created_at":"2026-03-01T00:00:00Z"}}}"#,
    )
    .expect("writes baselines");
    // One allowed request and one upstream failure on alpha, so reliability is
    // 0.5 rather than the neutral default.
    let writer = audit::Writer::open(audit::WriterOptions {
        path: dir.join("var/audit.jsonl"),
        sync: false,
        truncate: true,
    })
    .expect("opens ledger");
    for (decision, verdict) in [
        (audit::DECISION_ALLOW, "consistent"),
        (audit::DECISION_UPSTREAM_ERROR, "not_reported"),
    ] {
        let mut record = audit::Record {
            actor: "probe".to_string(),
            channel: "alpha".to_string(),
            model: "mock-basic".to_string(),
            decision: decision.to_string(),
            ..audit::Record::default()
        };
        record.usage.upstream_reported = verdict == "consistent";
        record.usage.verdict = verdict.to_string();
        writer.append(&mut record).expect("appends");
    }
    writer.close().expect("closes");
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().expect("runs benchd")
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn err(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Serves one canned HTTP response and returns the base URL.
fn serve_once(body: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let addr = listener.local_addr().expect("addr");
    let body = body.to_string();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .ok();
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
fn version_prints_the_build_identity() {
    let out = run(&["--version"]);
    assert!(out.status.success());
    assert!(text(&out).starts_with("customapi "), "{}", text(&out));
}

#[test]
fn missing_config_is_reported_like_go() {
    let out = run(&[]);
    assert!(!out.status.success());
    let message = err(&out);
    // Go prints the usage first, then the error; only the error line is ours.
    assert!(message.contains("Usage: benchd"), "{message}");
    assert_eq!(message.lines().last(), Some("benchd: --config is required"));
}

#[test]
fn unknown_tasks_are_reported_like_go() {
    let dir = TempDir::new("unknown");
    let config = write_config(&dir);
    let out = run(&["--config", &config, "--token", "x", "nosuch"]);
    assert!(!out.status.success());
    let message = err(&out);
    assert!(message.contains("Usage: benchd"), "{message}");
    assert_eq!(
        message.lines().last(),
        Some("benchd: unknown task \"nosuch\"")
    );
}

#[test]
fn a_missing_probe_credential_is_reported() {
    let dir = TempDir::new("no-token");
    // A valid configuration with no probe identity: there is nothing to run
    // synthetic traffic as, so the daemon refuses to start.
    let path = dir.join("customs.json");
    fs::write(
        &path,
        r#"{"server":{"listen":"127.0.0.1:1","admin_listen":"127.0.0.1:2",
             "audit_path":"var/audit.jsonl"},
            "channels":[{"name":"alpha","base_url":"http://127.0.0.1:9000/v1"}],
            "users":[{"name":"alice","token":"alice-token","quota":0}]}"#,
    )
    .expect("writes config");
    let out = run(&["--config", &path, "score"]);
    assert!(!out.status.success());
    // This failure comes after flag parsing, so no usage block precedes it.
    assert_eq!(
        err(&out),
        "benchd: no probe credential: set --token, or configure a user named \"probe\" (or \"*-probe\") with unlimited quota\n"
    );
}

#[test]
fn the_probe_user_supplies_the_credential() {
    let dir = TempDir::new("probe-user");
    let config = write_config(&dir);
    write_evidence(&dir);
    // No --token: the "probe" user from the configuration is used, and the run
    // succeeds without network access because `score` is offline.
    let out = run(&["--config", &config, "score"]);
    assert!(out.status.success(), "{}", err(&out));
    assert!(text(&out).contains("wrote scorecard for 2 channel/model pairs"));
}

#[test]
fn score_writes_the_shared_scorecard_shape() {
    let dir = TempDir::new("score");
    let config = write_config(&dir);
    write_evidence(&dir);
    let out = run(&[
        "--config",
        &config,
        "--token",
        "probe-token",
        "--out",
        &dir.path(),
        "score",
    ]);
    assert!(out.status.success(), "{}", err(&out));
    let expected_path = dir.join("var/scores.json");
    assert_eq!(
        text(&out),
        format!("wrote scorecard for 2 channel/model pairs to {expected_path}\n")
    );

    let raw = fs::read_to_string(&expected_path).expect("reads scorecard");
    let scores: serde_json::Value = serde_json::from_str(&raw).expect("parses scorecard");
    // Channel -> model -> Scores, with Go's number formatting.
    assert_eq!(scores["alpha"]["mock-basic"]["capability"], 0.8);
    assert_eq!(scores["alpha"]["mock-basic"]["reliability"], 0.5);
    assert_eq!(scores["alpha"]["mock-basic"]["fingerprint_match"], 1);
    assert_eq!(scores["alpha"]["mock-basic"]["usage_integrity"], 1);
    assert_eq!(scores["alpha"]["mock-basic"]["benchmarks"]["level_H"], 0.5);
    assert_eq!(scores["beta"]["mock-basic"]["capability"], 0.6);
    // An integral float must be written without a fraction, exactly as Go does.
    assert!(raw.contains(r#""fingerprint_match": 1,"#), "{raw}");
    assert!(!raw.contains("1.0"), "{raw}");
}

#[test]
fn score_defaults_the_output_directory_to_the_config_directory() {
    let dir = TempDir::new("score-default-out");
    let _config = write_config(&dir);
    write_evidence(&dir);
    let out = Command::new(BIN)
        .current_dir(&dir.0)
        .args([
            "--config",
            "customs.json",
            "--token",
            "probe-token",
            "score",
        ])
        .output()
        .expect("runs benchd");
    assert!(out.status.success(), "{}", err(&out));
    // filepath.Join cleans the "." away, exactly like Go.
    assert_eq!(
        text(&out),
        "wrote scorecard for 2 channel/model pairs to var/scores.json\n"
    );
    assert!(PathBuf::from(dir.join("var/scores.json")).exists());
}

#[test]
fn prices_writes_the_catalogue() {
    let dir = TempDir::new("prices");
    let config = write_config(&dir);
    let pricing = r#"{"success":true,
        "data":[{"model_name":"mock-basic","quota_type":1,"model_price":20,"completion_ratio":2}],
        "group_ratio":{"default":1},"usd_exchange_rate":7.1}"#;
    let base = serve_once(pricing);
    let out = run(&[
        "--config",
        &config,
        "--token",
        "probe-token",
        "--out",
        &dir.path(),
        "--prices-url",
        &format!("{base}/api/pricing"),
        "prices",
    ]);
    assert!(out.status.success(), "{}", err(&out));
    let path = dir.join("var/catalogue.json");
    assert_eq!(text(&out), format!("collected 1 price rows into {path}\n"));

    let raw = fs::read_to_string(&path).expect("reads catalogue");
    let catalogue: serde_json::Value = serde_json::from_str(&raw).expect("parses catalogue");
    let price = &catalogue["prices"]["alpha\u{0}mock-basic"];
    assert!(price.is_object(), "{raw}");
    // quota_type 1 is a flat per-million rate converted at the payload's FX rate.
    let input = price["input_per_m"].as_f64().expect("input_per_m");
    assert!((input - 142.0).abs() < 1e-6, "{input}");
    assert_eq!(price["scheme"], "manual");
}

#[test]
fn an_invalid_configuration_is_rejected_before_any_work() {
    let dir = TempDir::new("invalid-config");
    // No channels: the configuration layer rejects it, so the daemon never
    // reaches the price collector.
    let path = dir.join("customs.json");
    fs::write(
        &path,
        r#"{"server":{"listen":"127.0.0.1:1","audit_path":"var/audit.jsonl"},
            "users":[{"name":"probe","token":"probe-token","quota":0}]}"#,
    )
    .expect("writes config");
    let out = run(&["--config", &path, "prices"]);
    assert!(!out.status.success());
    assert_eq!(
        err(&out),
        "benchd: config: at least one channel or provider is required\n"
    );
}

#[test]
fn single_dash_options_are_accepted() {
    let dir = TempDir::new("single-dash");
    let config = write_config(&dir);
    write_evidence(&dir);
    let out = run(&[
        "-config",
        &config,
        "-token",
        "probe-token",
        "-out",
        &dir.path(),
        "score",
    ]);
    assert!(out.status.success(), "{}", err(&out));
    assert!(text(&out).contains("wrote scorecard"));
}
