//! `customsctl audit ...`: verify, repair, list and aggregate the audit ledger.
//!
//! These commands are deliberately offline: verifying an evidence store must not
//! depend on the gateway being up, or an incident could not be investigated
//! while the service is down.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;

use clap::{Args, Subcommand};
use customapi_core::{audit, config};

use crate::commands::emit;
use crate::ui::TabWriter;
use crate::util::{dir_of, load_records, record_json, resolve};

/// `customsctl audit <command>`.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Verify the audit hash chain.
    Verify(VerifyArgs),
    /// Rebuild the chain in the current format (preserves a backup).
    Rehash(RehashArgs),
    /// Print the most recent audit records.
    Tail(TailArgs),
    /// Aggregate usage from the audit ledger.
    Stats(StatsArgs),
}

/// Flags accepted by `audit verify`.
#[derive(Args, Debug, Clone)]
pub struct VerifyArgs {
    /// configuration file
    #[arg(long, short = 'c', value_name = "FILE")]
    pub config: Option<String>,
    /// ledger path (overrides config)
    #[arg(long, value_name = "PATH")]
    pub ledger: Option<String>,
    /// anchor file path
    #[arg(long, value_name = "PATH")]
    pub anchors: Option<String>,
}

/// Flags accepted by `audit rehash`.
#[derive(Args, Debug, Clone)]
pub struct RehashArgs {
    /// configuration file
    #[arg(long, short = 'c', value_name = "FILE")]
    pub config: Option<String>,
    /// ledger path (overrides config)
    #[arg(long, value_name = "PATH")]
    pub ledger: Option<String>,
    /// anchor file path
    #[arg(long, value_name = "PATH")]
    pub anchors: Option<String>,
    /// confirm the rewrite
    #[arg(long)]
    pub yes: bool,
}

/// Flags accepted by `audit tail`.
#[derive(Args, Debug, Clone)]
pub struct TailArgs {
    /// configuration file
    #[arg(long, short = 'c', value_name = "FILE")]
    pub config: Option<String>,
    /// how many records to print
    #[arg(long = "n", short = 'n', value_name = "N", default_value_t = 20)]
    pub n: i64,
    /// filter by user
    #[arg(long, value_name = "NAME")]
    pub actor: Option<String>,
    /// filter by channel
    #[arg(long, value_name = "NAME")]
    pub channel: Option<String>,
    /// filter by usage verdict
    #[arg(long, value_name = "VERDICT")]
    pub verdict: Option<String>,
    /// emit raw JSON records
    #[arg(long)]
    pub json: bool,
}

/// Flags accepted by `audit stats`.
#[derive(Args, Debug, Clone)]
pub struct StatsArgs {
    /// configuration file
    #[arg(long, short = 'c', value_name = "FILE")]
    pub config: Option<String>,
}

impl VerifyArgs {
    fn split_paths(&self) -> Result<(String, String), Box<dyn Error>> {
        let ledger = match self.ledger.clone().filter(|p| !p.is_empty()) {
            Some(path) => path,
            None => ledger_from_config(self.config.as_deref())?,
        };
        let anchors = self
            .anchors
            .clone()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| default_anchor_path(&ledger));
        Ok((ledger, anchors))
    }
}

/// Resolves the ledger path from a configuration file, like the Go helper.
fn ledger_from_config(config_path: Option<&str>) -> Result<String, Box<dyn Error>> {
    let Some(path) = config_path.filter(|p| !p.is_empty()) else {
        return Err("either --config or --ledger is required".into());
    };
    let cfg = config::load(Path::new(path))?;
    Ok(resolve(&dir_of(path), &cfg.server.audit_path))
}

/// The anchor file beside the ledger, or an empty string when it does not exist.
///
/// Go concatenates rather than joining, so a ledger given as `audit.jsonl`
/// yields `./anchor.jsonl`; the printed path is compared verbatim.
fn default_anchor_path(ledger: &str) -> String {
    let candidate = format!("{}/anchor.jsonl", dir_of(ledger));
    if Path::new(&candidate).exists() {
        candidate
    } else {
        String::new()
    }
}

/// Runs `audit verify`.
pub fn verify(args: &VerifyArgs) -> Result<(), Box<dyn Error>> {
    let (ledger, anchors) = args.split_paths()?;
    let res = audit::verify(&ledger, &anchors)?;
    println!("ledger:      {ledger}");
    if !anchors.is_empty() {
        println!("anchors:     {anchors}");
    }
    println!("records:     {}", res.records);
    println!("head:        seq={} hash={}", res.head_seq, res.head_hash);
    if res.anchors_checked > 0 || res.anchors_ignored > 0 {
        println!(
            "anchors:     checked={} ignored={}",
            res.anchors_checked, res.anchors_ignored
        );
    }
    if res.ok {
        println!("result:      OK - hash chain intact");
        return Ok(());
    }
    println!(
        "result:      FAILED - {} violation(s)",
        res.violations.len()
    );
    print_violations(&res.violations);
    Err("audit chain verification failed".into())
}

/// Prints one line per violation, aligned like the Go format string.
fn print_violations(violations: &[audit::Violation]) {
    for v in violations {
        println!(
            "  line {:<6} seq {:<6} {:<20} {}",
            v.line, v.seq, v.kind, v.detail
        );
    }
}

/// Runs `audit rehash`.
///
/// Two independent problems can make a ledger unverifiable, and they need
/// different treatment:
///
/// - `chain`: the record hashes were computed by an older format, so every
///   record reports a mismatch. Rebuilding recomputes them from the records'
///   own content.
/// - `anchors`: the anchors pin the previous chain's head, so after a rebuild
///   they disagree by construction. They are moved aside and preserved.
///
/// Treating them as one operation hid the second: the chain repair reported
/// "nothing to do", the anchors were never touched, and the gateway still
/// refused to start.
///
/// Deviation from the Go CLI worth knowing about: Go's `audit.Rehash` already
/// moves `anchor.jsonl` aside, and the Go command then tries to move the same
/// file again, so that path dies with `rename …: no such file or directory`
/// after a successful repair. Here the result of the rebuild says where the
/// anchors went, so the intended message is printed and the command succeeds.
pub fn rehash(args: &RehashArgs) -> Result<(), Box<dyn Error>> {
    let ledger = match args.ledger.clone().filter(|p| !p.is_empty()) {
        Some(path) => path,
        None => ledger_from_config(args.config.as_deref())?,
    };
    let anchors = args
        .anchors
        .clone()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| default_anchor_path(&ledger));

    // --- problem 1: the chain itself -------------------------------------
    let chain = audit::verify(&ledger, "")?;
    let chain_broken = !chain.ok;

    // --- problem 2: anchors that no longer match -------------------------
    let mut anchors_stale = false;
    if !anchors.is_empty() {
        let with_anchors = audit::verify(&ledger, &anchors)?;
        anchors_stale = !with_anchors.ok;
    }

    if !chain_broken && !anchors_stale {
        println!(
            "ledger {} verifies ({} records); nothing to do",
            ledger, chain.records
        );
        return Ok(());
    }

    if chain_broken {
        println!(
            "chain:   {} violation(s) across {} records",
            chain.violations.len(),
            chain.records
        );
        print_violations(&chain.violations);
    }
    if anchors_stale {
        println!("anchors: {anchors} no longer match this ledger");
    }
    println!();
    println!("Repairing recomputes record hashes from their own content and moves");
    println!("stale anchors aside. It cannot distinguish a format change from a");
    println!("genuine tampering: review the violations above before proceeding.");
    if !args.yes {
        println!();
        println!("Re-run with -yes to proceed.");
        return Err("rehash not confirmed".into());
    }

    let mut anchors_already_moved: Option<String> = None;
    if chain_broken {
        let out = audit::rehash(&ledger)?;
        println!(
            "\nrewrote {} record(s), {} hash(es) changed",
            out.records, out.changed
        );
        println!("original ledger preserved at {}", out.backup);
        println!("new head: seq={} hash={}", out.head_seq, out.head_hash);
        if !out.anchor_backup.is_empty() {
            anchors_already_moved = Some(out.anchor_backup);
        }
    } else {
        println!("\nchain already consistent; only the anchors need attention");
    }

    if anchors_stale && !anchors.is_empty() {
        let backup = match anchors_already_moved {
            // The rebuild moved the file itself (the usual case, since it is
            // the anchor file beside the ledger).
            Some(backup) if !Path::new(&anchors).exists() => backup,
            _ => {
                let backup = format!("{anchors}.pre-rehash");
                std::fs::rename(&anchors, &backup).map_err(|e| {
                    format!("move stale anchors aside: rename {anchors} {backup}: {e}")
                })?;
                backup
            }
        };
        println!("stale anchors moved to {backup}");
        println!("  (they pin the previous chain's head, so they cannot match after a rebuild;");
        println!("   keep them as evidence that the chain was rebuilt on this date)");
    }

    println!("\nVerify again with: customsctl audit verify");
    Ok(())
}

/// Runs `audit tail`.
pub fn tail(args: &TailArgs) -> Result<(), Box<dyn Error>> {
    let records = load_records(args.config.as_deref().unwrap_or(""))?;
    let actor = args.actor.as_deref().unwrap_or("");
    let channel = args.channel.as_deref().unwrap_or("");
    let verdict = args.verdict.as_deref().unwrap_or("");
    let mut filtered: Vec<&audit::Record> = records
        .iter()
        .filter(|r| {
            (actor.is_empty() || r.actor == actor)
                && (channel.is_empty() || r.channel == channel)
                && (verdict.is_empty() || r.usage.verdict == verdict)
        })
        .collect();
    if args.n >= 0 && filtered.len() > args.n as usize {
        let keep = args.n as usize;
        filtered = filtered[filtered.len() - keep..].to_vec();
    }

    if args.json {
        let mut out = String::new();
        for record in &filtered {
            out.push_str(&serde_json::to_string_pretty(&record_json(record))?);
            out.push('\n');
        }
        emit(&out);
        return Ok(());
    }

    let mut table = TabWriter::new();
    table.line("SEQ\tTIME\tACTOR\tCHANNEL\tMODEL\tDECISION\tTOKENS\tUPSTREAM\tVERDICT\tCOST");
    for record in &filtered {
        // A request can fail before any channel is chosen (no channel serves
        // the model, or the user is out of quota). Printing a bare empty column
        // would read as a display bug, so make the reason explicit.
        let channel = if record.channel.is_empty() {
            "(unrouted)"
        } else {
            record.channel.as_str()
        };
        table.line(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.6}",
            record.seq,
            record.time.format("%H:%M:%S"),
            record.actor,
            channel,
            record.model,
            record.decision,
            record.usage.total_tokens,
            record.usage.upstream_total_tokens,
            record.usage.verdict,
            record.cost.total_cost
        ));
    }
    emit(&table.finish());
    Ok(())
}

/// Runs `audit stats`.
pub fn stats(args: &StatsArgs) -> Result<(), Box<dyn Error>> {
    let records = load_records(args.config.as_deref().unwrap_or(""))?;
    let mut models: BTreeMap<String, i64> = BTreeMap::new();
    let mut channels: BTreeMap<String, i64> = BTreeMap::new();
    let mut verdicts: BTreeMap<String, i64> = BTreeMap::new();
    let mut total_tokens: i64 = 0;
    let mut upstream_tokens: i64 = 0;
    let mut cost = 0.0f64;
    for record in &records {
        *models.entry(record.model.clone()).or_insert(0) += 1;
        // A request that failed before routing has no channel. Grouping those
        // under an explicit label keeps the breakdown readable and makes it
        // obvious that they are a distinct outcome rather than missing data.
        let channel = if record.channel.is_empty() {
            "(unrouted)".to_string()
        } else {
            record.channel.clone()
        };
        *channels.entry(channel).or_insert(0) += 1;
        *verdicts.entry(record.usage.verdict.clone()).or_insert(0) += 1;
        total_tokens += record.usage.total_tokens;
        upstream_tokens += record.usage.upstream_total_tokens;
        cost += record.cost.total_cost;
    }
    println!("records:          {}", records.len());
    println!("gateway tokens:   {total_tokens}");
    println!("upstream tokens:  {upstream_tokens}");
    if total_tokens > 0 {
        println!(
            "token inflation:  {:+.2}%",
            (upstream_tokens - total_tokens) as f64 / total_tokens as f64 * 100.0
        );
    }
    println!("total cost:       {cost:.6}");
    print_counts("by channel", &channels);
    print_counts("by model", &models);
    print_counts("by verdict", &verdicts);
    Ok(())
}

/// Renders one sorted `key: count` block.
fn render_counts(title: &str, counts: &BTreeMap<String, i64>) -> String {
    let mut out = format!("{title}:\n");
    for (key, value) in counts {
        out.push_str(&format!("  {key:<24} {value}\n"));
    }
    out
}

/// Prints one sorted `key: count` block.
fn print_counts(title: &str, counts: &BTreeMap<String, i64>) {
    emit(&render_counts(title, counts));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_anchor_path_follows_go_string_concatenation() {
        // No anchor file exists for this made-up ledger, so the result is empty.
        assert_eq!(default_anchor_path("does/not/exist/audit.jsonl"), "");
    }

    #[test]
    fn count_blocks_are_sorted_and_padded_to_24_columns() {
        // The aggregate's breakdown is grepped by the deploy scripts, so both the
        // sorted order and the two-space indent plus 24-column name are fixed.
        let mut counts: BTreeMap<String, i64> = BTreeMap::new();
        counts.insert("(unrouted)".to_string(), 1);
        counts.insert("alpha".to_string(), 2);
        assert_eq!(
            render_counts("by channel", &counts),
            "by channel:\n  (unrouted)               1\n  alpha                    2\n"
        );
    }

    #[test]
    fn inflation_percent_uses_an_explicit_sign() {
        assert_eq!(format!("{:+.2}%", 1.8181), "+1.82%");
        assert_eq!(format!("{:+.2}%", -1.8181), "-1.82%");
        assert_eq!(format!("{:+.2}%", 0.0), "+0.00%");
    }
}
