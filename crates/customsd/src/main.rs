//! Command customsd runs the Custom-API customs gateway.
//!
//! It serves two listeners:
//!
//! - `--config/-c` the gateway configuration file
//! - `server.listen` the user-facing proxy (default 127.0.0.1:8080)
//! - `server.admin_listen` the management API (default 127.0.0.1:8081)
//!
//! The process is designed to run under an rc.d script on FreeBSD with no
//! privileges beyond the ability to read its configuration and write its
//! ledger.

mod args;
mod cli;

use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use clap::{error::ErrorKind, CommandFactory, Parser};
use customapi_core::{audit, config, gateway, store, version};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::cli::{check_summary, init_logging, Cli};

#[tokio::main]
async fn main() {
    let argv = args::normalize_argv(std::env::args());
    if let Err(err) = run(argv).await {
        eprintln!("customsd: {err}");
        std::process::exit(1);
    }
}

async fn run(argv: Vec<String>) -> Result<(), Box<dyn Error>> {
    let cli = match Cli::try_parse_from(&argv) {
        Ok(cli) => cli,
        Err(err) => return handle_parse_error(err),
    };

    if cli.version {
        println!("{}", version::identity());
        return Ok(());
    }
    let Some(config_path) = cli.config.clone().filter(|p| !p.is_empty()) else {
        // Go prints the usage before reporting the missing flag.
        eprint!("{}", Cli::command().render_help());
        return Err("--config is required".into());
    };

    let env_file = cli.env_file.clone().filter(|p| !p.is_empty());
    let mut cfg =
        config::load_with_env_file(Path::new(&config_path), env_file.as_deref().map(Path::new))?;
    // Command-line overrides win over the file, exactly like the Go flags.
    if let Some(listen) = cli.listen.clone().filter(|v| !v.is_empty()) {
        cfg.server.listen = listen;
    }
    if let Some(admin) = cli.admin_listen.clone().filter(|v| !v.is_empty()) {
        cfg.server.admin_listen = admin;
    }
    if let Some(level) = cli.log_level.clone().filter(|v| !v.is_empty()) {
        cfg.server.log_level = level;
    }
    if cli.check {
        println!(
            "{}",
            check_summary(cfg.all_channels().len(), cfg.users.len(), cfg.prices.len())
        );
        return Ok(());
    }

    init_logging(&cfg.server.log_level);
    log::info!("starting version={}", version::identity());

    let base = args::config_dir(&config_path);
    let server = gateway::Server::new(gateway::Options {
        config: Some(cfg.clone()),
        base_dir: base,
        scores: None,
        client: None,
    })?;

    let outcome = serve(server.clone(), &cfg, cli.anchor_interval).await;
    // Runs even when serving failed, like the Go `defer srv.Close()`.
    server.close().await;
    outcome
}

/// Binds both listeners, runs the background workers, and waits for a signal.
async fn serve(
    server: Arc<gateway::Server>,
    cfg: &config::Config,
    anchor_every: Duration,
) -> Result<(), Box<dyn Error>> {
    let proxy_router = server.handler();
    let admin_router = server.admin_handler();

    log::info!("proxy listening addr={}", cfg.server.listen);
    let proxy_listener = tokio::net::TcpListener::bind(&cfg.server.listen)
        .await
        .map_err(|e| format!("proxy listener: {e}"))?;
    log::info!("admin listening addr={}", cfg.server.admin_listen);
    let admin_listener = tokio::net::TcpListener::bind(&cfg.server.admin_listen)
        .await
        .map_err(|e| format!("admin listener: {e}"))?;

    // One shutdown signal stops every listener and worker. axum 0.7's `serve`
    // has no `Shutdown` method, so graceful shutdown is driven by a future that
    // completes when the watch channel flips.
    let (stop_tx, stop_rx) = watch::channel(false);

    let mut proxy_stop = stop_rx.clone();
    let mut proxy_handle = tokio::spawn(async move {
        // `ConnectInfo` carries the socket peer. The admin gate reads it to
        // decide whether a caller is on the loopback interface, and the audit
        // record needs it for `client_ip`, so a plain `axum::serve(router)` would
        // make every admin call look remote and be refused.
        axum::serve(
            proxy_listener,
            proxy_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = proxy_stop.changed().await;
        })
        .await
        .map_err(|e| format!("proxy listener: {e}"))
    });
    let mut admin_stop = stop_rx.clone();
    let mut admin_handle = tokio::spawn(async move {
        axum::serve(
            admin_listener,
            admin_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = admin_stop.changed().await;
        })
        .await
        .map_err(|e| format!("admin listener: {e}"))
    });

    // Periodically reset expired quota periods even if no request arrives.
    let roll_task = spawn_roll(server.store().clone(), stop_rx.clone());

    // Periodically pin the audit chain head so that a wholesale rewrite of the
    // ledger becomes detectable.
    let anchor_task = if anchor_every > Duration::ZERO {
        Some(spawn_anchor(server.clone(), anchor_every, stop_rx.clone()))
    } else {
        None
    };

    // Model discovery runs in the background. It is started after the listeners
    // so that a slow or unreachable upstream cannot delay serving traffic.
    server.start_discovery();
    server.mark_ready();

    let signal = tokio::select! {
        signal = wait_for_signal() => signal?,
        outcome = &mut proxy_handle => return listener_outcome(outcome),
        outcome = &mut admin_handle => return listener_outcome(outcome),
    };
    log::info!("shutting down signal={signal}");

    let _ = stop_tx.send(true);
    roll_task.abort();
    if let Some(task) = anchor_task {
        task.abort();
    }

    // Final anchor so the last records are covered.
    if anchor_every > Duration::ZERO {
        write_anchor(&server);
    }

    // Drain in-flight requests, bounded like Go's 15-second Shutdown context.
    // Single exchanges may legitimately take minutes, so the bound applies to
    // the shutdown, not to any one request.
    let drained = tokio::time::timeout(Duration::from_secs(15), async {
        let _ = proxy_handle.await;
        let _ = admin_handle.await;
    })
    .await;
    if drained.is_err() {
        log::warn!("graceful shutdown timed out after 15s");
    }
    Ok(())
}

/// Spawns the once-a-minute quota roll.
fn spawn_roll(store: Arc<store::Manager>, stop: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut stop = stop;
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // Go's `time.Ticker` fires after one interval; tokio's interval fires
        // immediately, so the first tick is consumed here.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => store.roll(),
                _ = stop.changed() => return,
            }
        }
    })
}

/// Spawns the periodic audit-chain anchor.
fn spawn_anchor(
    server: Arc<gateway::Server>,
    every: Duration,
    stop: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut stop = stop;
        let mut ticker = tokio::time::interval(every);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => write_anchor(&server),
                _ = stop.changed() => return,
            }
        }
    })
}

/// Pins the current chain head.
fn write_anchor(server: &gateway::Server) {
    let ledger = server.ledger();
    let anchor = audit::Anchor {
        seq: ledger.seq(),
        hash: ledger.head(),
        time: Utc::now(),
        // Go leaves `Records` at its zero value: the anchor pins the head, and
        // the record count is recoverable from the ledger itself.
        records: 0,
        component: "customsd".to_string(),
    };
    let path = server.anchor_path();
    if let Err(err) = audit::append_anchor(&path, &anchor) {
        log::error!("anchor write failed err={err}");
        return;
    }
    log::debug!("anchored audit chain seq={} path={}", anchor.seq, path);
}

/// Waits for SIGINT or SIGTERM, returning Go's `Signal.String()` spelling.
async fn wait_for_signal() -> Result<&'static str, Box<dyn Error>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let name = tokio::select! {
        _ = interrupt.recv() => "interrupt",
        _ = terminate.recv() => "terminated",
    };
    Ok(name)
}

/// Converts a listener task's result into the daemon's error type.
fn listener_outcome(
    outcome: Result<Result<(), String>, tokio::task::JoinError>,
) -> Result<(), Box<dyn Error>> {
    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(message)) => Err(message.into()),
        Err(err) => Err(format!("listener task failed: {err}").into()),
    }
}

/// Prints help quietly and turns every other clap failure into one error line.
fn handle_parse_error(err: clap::Error) -> Result<(), Box<dyn Error>> {
    if matches!(
        err.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        err.print()?;
        return Ok(());
    }
    eprint!("{}", Cli::command().render_help());
    let text = err.to_string();
    let first = text.lines().next().unwrap_or("invalid arguments").trim();
    let message = first.strip_prefix("error: ").unwrap_or(first).to_string();
    Err(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_pins_the_head_with_a_zero_record_count() {
        // The JSON written by the daemon must match Go's: `records` is present
        // and zero, not the ledger's record count.
        let anchor = audit::Anchor {
            seq: 4,
            hash: "abc".to_string(),
            time: Utc::now(),
            records: 0,
            component: "customsd".to_string(),
        };
        let json = serde_json::to_string(&anchor).unwrap();
        assert!(json.contains(r#""records":0"#), "{json}");
        assert!(json.contains(r#""component":"customsd""#), "{json}");
    }
}
