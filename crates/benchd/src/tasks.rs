//! The intelligence tasks and the scheduler that drives them.
//!
//! Layer L3 is four pieces of work, and each is also a one-shot command:
//!
//! - `prices` collects and normalises upstream prices;
//! - `fingerprint` runs L1 behavioural probes against every channel;
//! - `bench` runs the L2 layered capability suite;
//! - `score` folds the evidence into the routing scorecard.
//!
//! `run` repeats them on a schedule, which is what the rc.d script starts.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use customapi_core::{audit, config, intel, route};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::io::{join_path, read_json, write_json};

/// The daemon's error type: it travels out of spawned tasks, so it has to be
/// `Send + Sync`.
pub type TaskError = Box<dyn Error + Send + Sync>;
/// Shorthand for task results.
pub type TaskResult = Result<(), TaskError>;

/// Everything the tasks need from the command line, mirroring Go's `options`.
#[derive(Debug, Clone)]
pub struct Options {
    /// Path of the configuration file.
    ///
    /// Go's `options` carries it too and never reads it; it stays so the two
    /// structs can be diffed field by field.
    #[allow(dead_code)]
    pub config_path: String,
    /// Gateway base URL to probe through.
    pub base_url: String,
    /// Gateway API key used for probes.
    pub token: String,
    /// Canonical model to probe/bench; empty means "all priced models".
    pub model: String,
    /// Restrict work to one channel.
    pub channel: String,
    /// Directory the evidence files live in.
    pub out_dir: String,
    /// Requested log level.
    pub log_level: String,
    /// new-api compatible pricing URL.
    pub prices_url: String,
    /// Group name for multiplier pricing.
    pub group: String,
    /// Whether to record per-case benchmark outcomes.
    pub include_outcomes: bool,
}

/// Selects the credential used for synthetic traffic.
///
/// Probe traffic is real traffic as far as accounting is concerned, so it needs
/// its own identity: an operator can then see exactly how much the intelligence
/// layer costs, and exclude it from user-facing aggregates. The reserved name is
/// "probe"; a "*-probe" name is also accepted so several probe identities can be
/// configured (one per upstream group, for example).
pub fn probe_token(cfg: &config::Config) -> String {
    let is_probe = |name: &str| name == "probe" || name.ends_with("-probe");
    // Prefer an unlimited probe identity, which is the intended setup.
    for user in &cfg.users {
        if user.quota == 0 && is_probe(&user.name) {
            return user.token.clone();
        }
    }
    // Fall back to any probe identity, so a quota-limited one at least starts
    // the service and shows up in the ledger as it drains.
    for user in &cfg.users {
        if is_probe(&user.name) {
            return user.token.clone();
        }
    }
    String::new()
}

/// Returns the canonical models to probe.
pub fn models(cfg: &config::Config, opts: &Options) -> Vec<String> {
    if !opts.model.is_empty() {
        return vec![opts.model.clone()];
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for channel in cfg.all_channels() {
        if !opts.channel.is_empty() && channel.name != opts.channel {
            continue;
        }
        // Go iterates a map here, so its order is unspecified. Sorting keeps two
        // runs of this daemon comparable, which is worth more than matching an
        // arbitrary order.
        let mut names: Vec<&String> = channel.models.keys().collect();
        names.sort();
        for name in names {
            if seen.insert(name.clone()) {
                out.push(name.clone());
            }
        }
    }
    for price in &cfg.prices {
        if seen.insert(price.model.clone()) {
            out.push(price.model.clone());
        }
    }
    out
}

/// Returns the channels to probe, honouring `--channel`.
pub fn channels(cfg: &config::Config, opts: &Options) -> Vec<config::Channel> {
    if opts.channel.is_empty() {
        return cfg.all_channels();
    }
    cfg.all_channels()
        .into_iter()
        .filter(|channel| channel.name == opts.channel)
        .collect()
}

/// Builds the upstream client used for probes, with the Go timeouts.
fn upstream_client(seconds: u64) -> Result<reqwest::Client, TaskError> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(seconds))
        .build()?)
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

/// Collects and normalises upstream prices into the catalogue.
pub async fn prices(cfg: &config::Config, opts: &Options) -> TaskResult {
    let mut url = opts.prices_url.clone();
    if url.is_empty() {
        // Fall back to the first channel that looks like a new-api gateway.
        if let Some(channel) = cfg.all_channels().first() {
            url = config::upstream_url(&channel.base_url, "/api/pricing");
        }
    }
    if url.is_empty() {
        return Err("no price source: set --prices-url".into());
    }
    log::info!("collecting prices url={} group={}", url, opts.group);
    let source = intel::PriceSource {
        channel: cfg
            .all_channels()
            .first()
            .map(|channel| channel.name.clone())
            .unwrap_or_default(),
        kind: "newapi".to_string(),
        url: url.clone(),
        ..intel::PriceSource::default()
    };
    let entries = intel::collect_prices(None, &source, &opts.group).await?;
    let catalogue = intel::Catalogue::new();
    let path = join_path(&[&opts.out_dir, "var", "catalogue.json"]);
    // A missing catalogue is expected on the first run; `merge` then seeds it.
    let _ = catalogue.load(&path);
    catalogue.merge(&entries);
    // `Catalogue::save` renders with plain serde, which writes a whole number as
    // `4.0` where Go writes `4`. The catalogue is a file an operator diffs
    // against the Go build and the other implementation reads, so it is written
    // through this crate's Go-compatible writer instead. (The core helper is the
    // right long-term home for the number formatting; it is flagged in the
    // porting report rather than changed here.)
    write_json(&path, &catalogue)?;
    log::info!(
        "price catalogue updated entries={} path={}",
        entries.len(),
        path
    );
    println!("collected {} price rows into {}", entries.len(), path);
    Ok(())
}

/// Runs the L1 probes and compares them with the baselines.
pub async fn fingerprint(cfg: &config::Config, opts: &Options) -> TaskResult {
    let baselines_path = join_path(&[&opts.out_dir, "etc", "baselines.json"]);
    let baselines = intel::load_baselines(&baselines_path)?;
    let probes = intel::default_probes();
    let mut results: BTreeMap<String, intel::ChannelResult> = BTreeMap::new();
    for model in models(cfg, opts) {
        for channel in channels(cfg, opts) {
            if channel.upstream_model(&model).is_none() {
                continue;
            }
            let runner = intel::Runner {
                base_url: opts.base_url.clone(),
                token: opts.token.clone(),
                model: model.clone(),
                channel: channel.name.clone(),
                client: Some(upstream_client(90)?),
                sleep: Duration::from_millis(250),
            };
            let result = runner.run(&probes).await;
            let comparison = baselines.compare(
                &model,
                &result.lengths,
                cfg.policy.metering.fingerprint_threshold,
            );
            log::info!(
                "fingerprint channel={} model={} probes={} score={} verdict={}",
                channel.name,
                model,
                result.lengths.len(),
                comparison.score,
                comparison.verdict
            );
            println!(
                "{:<14} {:<24} probes={} js={:.4} verdict={}",
                channel.name,
                model,
                result.lengths.len(),
                comparison.score,
                comparison.verdict
            );
            results.insert(format!("{}\u{0}{}", channel.name, model), result);
        }
    }
    let path = join_path(&[&opts.out_dir, "var", "fingerprints.json"]);
    write_json(&path, &results)?;
    log::info!("fingerprint run complete path={}", path);
    Ok(())
}

/// Runs the L2 capability suite.
pub async fn bench(cfg: &config::Config, opts: &Options) -> TaskResult {
    let suite = intel::default_bench_suite();
    let mut reports: Vec<intel::BenchReport> = Vec::new();
    for model in models(cfg, opts) {
        for channel in channels(cfg, opts) {
            if channel.upstream_model(&model).is_none() {
                continue;
            }
            let runner = intel::BenchRunner {
                base_url: opts.base_url.clone(),
                token: opts.token.clone(),
                model: model.clone(),
                channel: channel.name.clone(),
                client: Some(upstream_client(180)?),
                include_outcomes: opts.include_outcomes,
            };
            let report = runner.run(&suite).await;
            log::info!(
                "bench channel={} model={} passed={} total={} score={}",
                channel.name,
                model,
                report.passed,
                report.total,
                report.score
            );
            println!(
                "{:<14} {:<24} {}/{} ({:.3})",
                channel.name, model, report.passed, report.total, report.score
            );
            reports.push(report);
        }
    }
    let path = join_path(&[&opts.out_dir, "var", "benchmarks.json"]);
    // Go marshals a nil slice as `null`; an empty run therefore leaves `null`
    // behind rather than `[]`. Reproducing that keeps the evidence files
    // interchangeable with the Go daemon's.
    if reports.is_empty() {
        write_json(&path, &serde_json::Value::Null)?;
    } else {
        write_json(&path, &reports)?;
    }
    log::info!("bench run complete path={}", path);
    Ok(())
}

/// Writes the routing scorecard.
pub fn score(cfg: &config::Config, opts: &Options) -> TaskResult {
    let scores = build_scores(cfg, opts)?;
    let path = join_path(&[&opts.out_dir, "var", "scores.json"]);
    write_json(&path, &scores)?;
    log::info!("scorecard written path={}", path);
    println!(
        "wrote scorecard for {} channel/model pairs to {}",
        count_scores(&scores),
        path
    );
    Ok(())
}

/// Folds the evidence files into the routing scorecard.
pub fn build_scores(
    cfg: &config::Config,
    opts: &Options,
) -> Result<BTreeMap<String, BTreeMap<String, route::Scores>>, TaskError> {
    let reports: Vec<intel::BenchReport> =
        read_json(&join_path(&[&opts.out_dir, "var", "benchmarks.json"]))?;
    let fp_results: BTreeMap<String, intel::ChannelResult> =
        read_json(&join_path(&[&opts.out_dir, "var", "fingerprints.json"]))?;
    let baselines_path = join_path(&[&opts.out_dir, "etc", "baselines.json"]);
    let baselines = intel::load_baselines(&baselines_path)?;

    // Group evidence by channel.
    let mut by_channel: BTreeMap<String, Vec<intel::BenchReport>> = BTreeMap::new();
    for report in &reports {
        by_channel
            .entry(report.channel.clone())
            .or_default()
            .push(report.clone());
    }
    let mut fingerprints: BTreeMap<String, Vec<intel::CompareResult>> = BTreeMap::new();
    for (key, result) in &fp_results {
        let Some((channel, model)) = key.split_once('\u{0}') else {
            continue;
        };
        let comparison = baselines.compare(
            model,
            &result.lengths,
            cfg.policy.metering.fingerprint_threshold,
        );
        fingerprints
            .entry(channel.to_string())
            .or_default()
            .push(comparison);
    }

    // Reliability and usage integrity come from the live audit ledger, which is
    // the only place that knows what actually happened in production.
    let (integrity, reliability) = live_integrity(cfg, opts);

    let mut out: BTreeMap<String, BTreeMap<String, route::Scores>> = BTreeMap::new();
    for channel in cfg.all_channels() {
        let empty: Vec<intel::BenchReport> = Vec::new();
        let empty_fp: Vec<intel::CompareResult> = Vec::new();
        let scores = intel::assemble_scores(
            by_channel.get(&channel.name).unwrap_or(&empty),
            fingerprints.get(&channel.name).unwrap_or(&empty_fp),
            integrity.get(&channel.name).copied().unwrap_or(0.0),
            reliability.get(&channel.name).copied().unwrap_or(0.0),
        );
        if !scores.is_empty() {
            out.insert(channel.name.clone(), scores);
        }
    }
    // Carry over channels that only have fingerprint evidence.
    for (channel, comparisons) in &fingerprints {
        if out.contains_key(channel) {
            continue;
        }
        let scores = intel::assemble_scores(
            &[],
            comparisons,
            integrity.get(channel).copied().unwrap_or(0.0),
            reliability.get(channel).copied().unwrap_or(0.0),
        );
        if !scores.is_empty() {
            out.insert(channel.clone(), scores);
        }
    }
    Ok(out)
}

/// Computes per-channel usage integrity and reliability from the audit ledger.
pub fn live_integrity(
    cfg: &config::Config,
    opts: &Options,
) -> (BTreeMap<String, f64>, BTreeMap<String, f64>) {
    let mut integrity: BTreeMap<String, f64> = BTreeMap::new();
    let mut reliability: BTreeMap<String, f64> = BTreeMap::new();
    let ledger_path = config::resolve(Path::new(&opts.out_dir), &cfg.server.audit_path);
    let records = match audit::read_all(&ledger_path) {
        Ok(records) => records,
        Err(_) => return (integrity, reliability),
    };
    if records.is_empty() {
        return (integrity, reliability);
    }

    #[derive(Default)]
    struct Accumulator {
        verifiable: i64,
        sum: f64,
        total: i64,
        failures: i64,
    }

    let mut aggregated: BTreeMap<String, Accumulator> = BTreeMap::new();
    for record in &records {
        let acc = aggregated.entry(record.channel.clone()).or_default();
        acc.total += 1;
        if record.decision == audit::DECISION_UPSTREAM_ERROR {
            acc.failures += 1;
        }
        // Integrity uses the *verdict* the gateway recorded, not a re-derivation
        // from the deltas. The gateway applies both a relative tolerance and an
        // absolute floor, and re-deriving here would silently drop the floor and
        // resurrect every false positive it exists to suppress.
        if record.usage.upstream_reported && record.usage.verdict != "not_reported" {
            let mut value = 1.0;
            if record.usage.verdict == "over_billed" || record.usage.verdict == "under_billed" {
                let mut delta = record.usage.prompt_delta;
                if record.usage.completion_delta.abs() > delta.abs() {
                    delta = record.usage.completion_delta;
                }
                value = 1.0 - delta.abs();
                if value < 0.0 {
                    value = 0.0;
                }
            }
            acc.verifiable += 1;
            acc.sum += value;
        }
    }
    for (channel, acc) in aggregated {
        integrity.insert(
            channel.clone(),
            if acc.verifiable > 0 {
                acc.sum / acc.verifiable as f64
            } else {
                1.0
            },
        );
        reliability.insert(
            channel,
            if acc.total > 0 {
                1.0 - acc.failures as f64 / acc.total as f64
            } else {
                1.0
            },
        );
    }
    (integrity, reliability)
}

/// Counts the channel/model entries in a scorecard.
fn count_scores(scores: &BTreeMap<String, BTreeMap<String, route::Scores>>) -> usize {
    scores.values().map(|models| models.len()).sum()
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

/// One of the scheduled pieces of work.
#[derive(Clone, Copy, Debug)]
enum Scheduled {
    Prices,
    Fingerprint,
    Bench,
}

impl Scheduled {
    /// Name used in log lines.
    fn name(self) -> &'static str {
        match self {
            Scheduled::Prices => "prices",
            Scheduled::Fingerprint => "fingerprint",
            Scheduled::Bench => "bench",
        }
    }

    /// Runs the task once.
    async fn run(self, cfg: &config::Config, opts: &Options) -> TaskResult {
        match self {
            Scheduled::Prices => prices(cfg, opts).await,
            Scheduled::Fingerprint => fingerprint(cfg, opts).await,
            Scheduled::Bench => bench(cfg, opts).await,
        }
    }
}

/// Runs the daemon's schedule until SIGINT or SIGTERM.
pub async fn scheduler(cfg: config::Config, opts: Options) -> TaskResult {
    let cfg = Arc::new(cfg);
    let (stop_tx, stop_rx) = watch::channel(false);
    let schedule = [
        (Scheduled::Prices, Duration::from_secs(30 * 60), true),
        (Scheduled::Fingerprint, Duration::from_secs(20 * 60), true),
        (Scheduled::Bench, Duration::from_secs(6 * 60 * 60), false),
    ];

    let mut handles: Vec<JoinHandle<()>> = Vec::new();
    for (task, every, run_first) in schedule {
        let cfg = cfg.clone();
        let opts = opts.clone();
        let mut stop = stop_rx.clone();
        handles.push(tokio::spawn(async move {
            if run_first {
                if let Err(err) = task.run(&cfg, &opts).await {
                    log::error!("task failed task={} err={err}", task.name());
                }
            }
            let mut ticker = tokio::time::interval(every);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            // Go's ticker waits one full interval before the first tick.
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if let Err(err) = task.run(&cfg, &opts).await {
                            log::error!("task failed task={} err={err}", task.name());
                        }
                        if let Err(err) = score(&cfg, &opts) {
                            log::error!("score refresh failed err={err}");
                        }
                    }
                    _ = stop.changed() => return,
                }
            }
        }));
    }

    // Refresh the scorecard on its own cadence so a gateway picks up new
    // evidence between the slower task runs.
    {
        let cfg = cfg.clone();
        let opts = opts.clone();
        let mut stop = stop_rx.clone();
        handles.push(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(2 * 60));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if let Err(err) = score(&cfg, &opts) {
                            log::error!("score refresh failed err={err}");
                        }
                    }
                    _ = stop.changed() => return,
                }
            }
        }));
    }

    let signal = wait_for_signal().await?;
    log::info!("benchd stopping signal={signal}");
    let _ = stop_tx.send(true);
    // Give in-flight probes a moment to finish, like the Go daemon.
    tokio::time::sleep(Duration::from_millis(500)).await;
    for handle in handles {
        handle.abort();
    }
    Ok(())
}

/// Waits for SIGINT or SIGTERM, returning Go's `Signal.String()` spelling.
async fn wait_for_signal() -> Result<&'static str, TaskError> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let name = tokio::select! {
        _ = interrupt.recv() => "interrupt",
        _ = terminate.recv() => "terminated",
    };
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use customapi_core::config::{Channel, User};

    /// A throw-away directory that removes itself.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "benchd-task-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }

        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn options(dir: &str) -> Options {
        Options {
            config_path: format!("{dir}/customs.json"),
            base_url: "http://127.0.0.1:8080".to_string(),
            token: "probe-token".to_string(),
            model: String::new(),
            channel: String::new(),
            out_dir: dir.to_string(),
            log_level: "info".to_string(),
            prices_url: String::new(),
            group: "default".to_string(),
            include_outcomes: false,
        }
    }

    fn config_with(users: Vec<User>, channels: Vec<Channel>) -> config::Config {
        config::Config {
            users,
            channels,
            ..config::Config::default()
        }
    }

    fn user(name: &str, token: &str, quota: i64) -> User {
        User {
            name: name.to_string(),
            token: token.to_string(),
            quota,
            period: "none".to_string(),
            ..User::default()
        }
    }

    fn channel(name: &str, models: &[(&str, &str)]) -> Channel {
        Channel {
            name: name.to_string(),
            base_url: "http://127.0.0.1:9000/v1".to_string(),
            protocol: "openai".to_string(),
            models: models
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Channel::default()
        }
    }

    #[test]
    fn probe_token_prefers_an_unlimited_probe_user() {
        let cfg = config_with(
            vec![
                user("alice", "alice-token", 0),
                user("probe-cny", "limited", 100),
                user("probe", "probe-token", 0),
            ],
            vec![channel("alpha", &[("mock-basic", "mock-basic")])],
        );
        assert_eq!(probe_token(&cfg), "probe-token");
    }

    #[test]
    fn probe_token_falls_back_to_a_quota_limited_probe_user() {
        let cfg = config_with(
            vec![
                user("alice", "alice-token", 0),
                user("vip-probe", "vip", 100),
            ],
            vec![channel("alpha", &[("mock-basic", "mock-basic")])],
        );
        assert_eq!(probe_token(&cfg), "vip");
        let cfg = config_with(
            vec![user("alice", "alice-token", 0)],
            vec![channel("alpha", &[("mock-basic", "mock-basic")])],
        );
        assert_eq!(probe_token(&cfg), "");
    }

    #[test]
    fn models_come_from_channels_then_prices() {
        let mut cfg = config_with(
            vec![],
            vec![
                channel("alpha", &[("mock-basic", "mock-basic"), ("gpt-x", "up")]),
                channel("beta", &[("mock-basic", "mock-basic")]),
            ],
        );
        cfg.prices.push(config::PriceEntry {
            channel: "alpha".to_string(),
            model: "priced-only".to_string(),
            price: config::PriceModel::default(),
        });
        let opts = options("/tmp/x");
        assert_eq!(
            models(&cfg, &opts),
            vec!["gpt-x", "mock-basic", "priced-only"]
        );
    }

    #[test]
    fn models_honour_the_model_and_channel_filters() {
        let cfg = config_with(
            vec![],
            vec![
                channel("alpha", &[("mock-basic", "mock-basic")]),
                channel("beta", &[("mock-basic", "mock-basic")]),
            ],
        );
        let mut opts = options("/tmp/x");
        opts.model = "mock-basic".to_string();
        assert_eq!(models(&cfg, &opts), vec!["mock-basic"]);
        let mut opts = options("/tmp/x");
        opts.channel = "beta".to_string();
        assert_eq!(channels(&cfg, &opts).len(), 1);
        assert_eq!(channels(&cfg, &opts)[0].name, "beta");
    }

    #[test]
    fn live_integrity_aggregates_the_ledger() {
        let dir = TempDir::new("integrity");
        let cfg = config::Config {
            server: config::Server {
                audit_path: "var/audit.jsonl".to_string(),
                ..config::Server::default()
            },
            ..config::Config::default()
        };
        let opts = options(&dir.path());
        let ledger_path = format!("{}/var/audit.jsonl", dir.path());
        let writer = audit::Writer::open(audit::WriterOptions {
            path: ledger_path,
            sync: false,
            truncate: true,
        })
        .expect("opens ledger");
        for (channel, decision, verdict, delta) in [
            ("alpha", audit::DECISION_ALLOW, "consistent", 0.0),
            ("alpha", audit::DECISION_UPSTREAM_ERROR, "not_reported", 0.0),
            ("beta", audit::DECISION_REPORT, "over_billed", 0.4),
        ] {
            let mut record = audit::Record {
                actor: "probe".to_string(),
                channel: channel.to_string(),
                model: "mock-basic".to_string(),
                decision: decision.to_string(),
                ..audit::Record::default()
            };
            record.usage.upstream_reported = verdict != "not_reported";
            record.usage.verdict = verdict.to_string();
            record.usage.prompt_delta = delta;
            writer.append(&mut record).expect("appends");
        }
        writer.close().expect("closes");

        let (integrity, reliability) = live_integrity(&cfg, &opts);
        // alpha: one verifiable consistent record => 1.0; one of two failed.
        assert_eq!(integrity.get("alpha"), Some(&1.0));
        assert_eq!(reliability.get("alpha"), Some(&0.5));
        // beta: one over-billed record with a 0.4 delta => 0.6.
        assert!((integrity.get("beta").copied().unwrap_or(0.0) - 0.6).abs() < 1e-9);
        assert_eq!(reliability.get("beta"), Some(&1.0));
    }

    #[test]
    fn build_scores_folds_benchmarks_and_fingerprints() {
        let dir = TempDir::new("scores");
        let base = dir.path();
        let cfg = config_with(
            vec![],
            vec![
                channel("alpha", &[("mock-basic", "mock-basic")]),
                channel("beta", &[("mock-basic", "mock-basic")]),
            ],
        );
        let opts = options(&base);
        std::fs::create_dir_all(format!("{base}/var")).expect("mkdir var");
        std::fs::create_dir_all(format!("{base}/etc")).expect("mkdir etc");

        // evidence files, in the same shape the tasks write
        let benchmarks = serde_json::json!([{
            "channel": "alpha",
            "model": "mock-basic",
            "total": 10,
            "passed": 8,
            "score": 0.8,
            "by_category": {"code": 1.0, "math": 0.75},
            "by_level": {"E": 1.0},
            "started": "2026-03-04T13:00:00Z",
            "finished": "2026-03-04T13:02:00Z"
        }]);
        std::fs::write(
            format!("{base}/var/benchmarks.json"),
            serde_json::to_vec(&benchmarks).unwrap(),
        )
        .expect("write benchmarks");
        let fingerprints = serde_json::json!({
            "beta\u{0}mock-basic": {
                "channel": "beta",
                "model": "mock-basic",
                "results": [],
                "lengths": [40, 41, 42],
                "collected_at": "2026-03-04T13:10:00Z"
            }
        });
        std::fs::write(
            format!("{base}/var/fingerprints.json"),
            serde_json::to_vec(&fingerprints).unwrap(),
        )
        .expect("write fingerprints");
        let baselines = serde_json::json!({
            "version": 1,
            "updated_at": "2026-03-01T00:00:00Z",
            "models": {}
        });
        std::fs::write(
            format!("{base}/etc/baselines.json"),
            serde_json::to_vec(&baselines).unwrap(),
        )
        .expect("write baselines");

        let scores = build_scores(&cfg, &opts).expect("scores");
        assert_eq!(scores.len(), 2, "{scores:?}");
        let alpha = &scores["alpha"]["mock-basic"];
        assert!((alpha.capability - 0.8).abs() < 1e-9);
        assert_eq!(alpha.benchmarks.get("level_E"), Some(&1.0));
        // beta has fingerprint evidence only, and no baseline to compare against,
        // which is the "no_baseline" verdict: unmeasured rather than penalised.
        assert!(scores["beta"].contains_key("mock-basic"));
    }

    #[test]
    fn score_writes_the_go_file_shape() {
        let dir = TempDir::new("scorefile");
        let base = dir.path();
        let cfg = config_with(
            vec![],
            vec![channel("alpha", &[("mock-basic", "mock-basic")])],
        );
        let opts = options(&base);
        std::fs::create_dir_all(format!("{base}/var")).expect("mkdir var");
        std::fs::create_dir_all(format!("{base}/etc")).expect("mkdir etc");
        let benchmarks = serde_json::json!([{
            "channel": "alpha", "model": "mock-basic", "total": 4, "passed": 4, "score": 1.0,
            "by_category": {"math": 1.0}, "by_level": {"E": 1.0},
            "started": "2026-03-04T13:00:00Z", "finished": "2026-03-04T13:02:00Z"
        }]);
        std::fs::write(
            format!("{base}/var/benchmarks.json"),
            serde_json::to_vec(&benchmarks).unwrap(),
        )
        .expect("write benchmarks");
        let baselines =
            serde_json::json!({"version": 1, "updated_at": "2026-03-01T00:00:00Z", "models": {}});
        std::fs::write(
            format!("{base}/etc/baselines.json"),
            serde_json::to_vec(&baselines).unwrap(),
        )
        .expect("write baselines");

        score(&cfg, &opts).expect("writes scorecard");
        let text = std::fs::read_to_string(format!("{base}/var/scores.json")).expect("reads");
        // Go writes an integral score as `1`, not `1.0`.
        assert!(text.contains(r#""capability": 1"#), "{text}");
        assert!(text.contains(r#""mock-basic""#), "{text}");
        assert!(!text.contains("1.0"), "{text}");
    }

    #[test]
    fn a_missing_price_source_is_reported() {
        // `prices` with no channel and no --prices-url: no URL can be derived.
        let cfg = config::Config::default();
        let opts = options("/tmp/x");
        let error = futures_lite_block_on(prices(&cfg, &opts)).expect_err("no source");
        assert_eq!(error.to_string(), "no price source: set --prices-url");
    }

    /// Minimal blocking executor so the test above can drive one future without
    /// pulling in a dependency for it.
    fn futures_lite_block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    }
}
