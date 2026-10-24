//! Process-level tests for the gateway daemon.
//!
//! The interesting behaviour is startup and shutdown: which flags override the
//! file, what `--check` prints, and that a terminated daemon writes its final
//! audit anchor before exiting.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_customsd");

/// A throw-away directory that removes itself.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "customsd-cli-{name}-{}-{}",
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

/// A configuration with the secrets left out, so the env file has work to do.
fn write_config(dir: &TempDir, listen: &str, admin: &str) -> String {
    let path = dir.join("customs.json");
    fs::write(
        &path,
        format!(
            r#"{{
  "server": {{
    "listen": "{listen}",
    "admin_listen": "{admin}",
    "audit_path": "var/audit.jsonl",
    "state_path": "var/state.json",
    "fail_mode": "closed",
    "route_strategy": "value",
    "log_level": "info"
  }},
  "channels": [
    {{"name": "alpha", "base_url": "http://127.0.0.1:19001/v1",
     "protocol": "openai", "trust": "trusted", "models": {{"mock-basic": "mock-basic"}}}}
  ],
  "users": [
    {{"name": "alice", "token": "", "quota": 0, "period": "monthly"}}
  ],
  "prices": []
}}"#
        ),
    )
    .expect("writes config");
    path
}

/// Writes the secrets file that fills in alice's token.
fn write_env_file(dir: &TempDir) -> String {
    let path = dir.join("customsd.env");
    fs::write(&path, "CUSTOMS_USER_ALICE_TOKEN=alice-token\n").expect("writes env file");
    path
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("runs customsd")
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn err(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Asks the kernel for a free port by binding and immediately releasing it.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    port
}

/// Waits until the admin listener answers `GET /healthz`.
fn wait_for_healthz(port: u16, child: &mut Child) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("customsd exited early with {status}"));
        }
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .ok();
            let request = "GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
            if stream.write_all(request.as_bytes()).is_ok() {
                let mut response = String::new();
                if stream.read_to_string(&mut response).is_ok()
                    && response.starts_with("HTTP/1.1 200")
                {
                    return Ok(());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("customsd did not answer /healthz".to_string())
}

#[test]
fn version_prints_the_build_identity() {
    let out = run(&["--version"]);
    assert!(out.status.success());
    assert!(text(&out).starts_with("customapi "), "{}", text(&out));
}

#[test]
fn check_prints_the_go_summary_and_exits() {
    let dir = TempDir::new("check");
    let config = write_config(&dir, "127.0.0.1:0", "127.0.0.1:0");
    let env = write_env_file(&dir);
    let out = run(&["--config", &config, "--env-file", &env, "--check"]);
    assert!(out.status.success(), "{}", err(&out));
    assert_eq!(
        text(&out),
        "configuration OK: 1 channel(s), 1 user(s), 0 price row(s)\n"
    );
    // --check must not touch the ledger.
    assert!(!PathBuf::from(dir.join("var/audit.jsonl")).exists());
}

#[test]
fn listen_and_log_level_override_the_file() {
    let dir = TempDir::new("overrides");
    let config = write_config(&dir, "127.0.0.1:1080", "127.0.0.1:1081");
    let env = write_env_file(&dir);
    let out = run(&[
        "--config",
        &config,
        "--env-file",
        &env,
        "--check",
        "--listen",
        "0.0.0.0:9",
        "--admin-listen",
        "0.0.0.0:10",
        "--log-level",
        "debug",
    ]);
    assert!(out.status.success(), "{}", err(&out));
    assert!(text(&out).starts_with("configuration OK: 1 channel(s)"));
}

#[test]
fn single_dash_long_options_are_accepted() {
    let dir = TempDir::new("single-dash");
    let config = write_config(&dir, "127.0.0.1:0", "127.0.0.1:0");
    let env = write_env_file(&dir);
    // rc.d scripts and cron entries use Go's single-dash spelling.
    let out = run(&[
        "-config",
        &config,
        "-env-file",
        &env,
        "-check",
        "-anchor-interval",
        "0",
    ]);
    assert!(out.status.success(), "{}", err(&out));
    assert!(text(&out).starts_with("configuration OK:"));
}

#[test]
fn a_missing_config_is_reported_like_go() {
    let out = run(&[]);
    assert!(!out.status.success());
    let message = err(&out);
    // Go prints the usage to stderr before returning the error.
    assert!(message.contains("Usage: customsd"), "{message}");
    assert_eq!(
        message.lines().last(),
        Some("customsd: --config is required")
    );
}

#[test]
fn an_invalid_anchor_interval_names_the_flag() {
    let dir = TempDir::new("bad-interval");
    let config = write_config(&dir, "127.0.0.1:0", "127.0.0.1:0");
    let out = run(&["--config", &config, "--anchor-interval", "soon"]);
    assert!(!out.status.success());
    let message = err(&out);
    assert!(message.contains("--anchor-interval"), "{message}");
    assert!(
        message.contains(r#"time: invalid duration "soon""#),
        "{message}"
    );
}

#[test]
fn help_exits_zero() {
    let out = run(&["--help"]);
    assert!(out.status.success());
    assert!(text(&out).contains("--config"), "{}", text(&out));
}

#[test]
fn a_taken_proxy_port_is_reported() {
    let dir = TempDir::new("port-taken");
    let blocker = TcpListener::bind("127.0.0.1:0").expect("binds");
    let taken = blocker.local_addr().expect("addr").port();
    let config = write_config(
        &dir,
        &format!("127.0.0.1:{taken}"),
        &format!("127.0.0.1:{}", free_port()),
    );
    let env = write_env_file(&dir);
    let out = run(&["--config", &config, "--env-file", &env]);
    assert!(!out.status.success());
    assert!(err(&out).contains("proxy listener:"), "{}", err(&out));
}

#[test]
fn sigterm_shuts_down_cleanly_and_writes_the_final_anchor() {
    let dir = TempDir::new("sigterm");
    let listen = free_port();
    let admin = free_port();
    let config = write_config(
        &dir,
        &format!("127.0.0.1:{listen}"),
        &format!("127.0.0.1:{admin}"),
    );
    let env = write_env_file(&dir);

    let mut child = Command::new(BIN)
        .args([
            "--config",
            &config,
            "--env-file",
            &env,
            "--anchor-interval",
            "1h",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawns customsd");

    if let Err(message) = wait_for_healthz(admin, &mut child) {
        let _ = child.kill();
        panic!("{message}");
    }

    // SIGTERM is what `service customsd stop` sends.
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("sends SIGTERM");
    assert!(status.success(), "kill failed");

    let deadline = Instant::now() + Duration::from_secs(20);
    let exit = loop {
        match child.try_wait().expect("waits for child") {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("customsd did not exit after SIGTERM");
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    assert!(exit.success(), "customsd exited with {exit}");

    // The final anchor covers the last records even when the interval has not
    // elapsed, so a wholesale ledger rewrite stays detectable.
    let anchor = dir.join("var/anchor.jsonl");
    let content = fs::read_to_string(&anchor).expect("reads anchor");
    assert!(content.contains(r#""component":"customsd""#), "{content}");
    assert!(content.contains(r#""records":0"#), "{content}");
    assert_eq!(content.lines().count(), 1, "{content}");
}
