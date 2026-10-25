//! Quota accounting and the aggregation queries the management UI needs.
//!
//! This module is the Rust counterpart of the Go package `internal/store`
//! (`store.go`).
//!
//! The gateway reuses new-api for the user/token/quota front end, but the plan
//! requires its own independent accounting ledger. This module is that ledger's
//! in-memory index: it is fed by the audit ledger and reconstructable from it, so
//! it never becomes a second source of truth that can disagree with the
//! evidence.
//!
//! # Locking discipline
//!
//! The mutex protects the user map only. The change callback is invoked *after*
//! the lock is released, because the production callback persists the state file
//! and therefore takes the lock again - calling it while holding it would
//! deadlock the process.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Datelike, Duration as ChronoDuration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::audit;
use crate::config::{self, zero_time};

// ---------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------

/// The mutable runtime state of one user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserState {
    pub name: String,
    /// Mirrors the configured trust level for convenience.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub trust: String,
    /// The quota reset schedule.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub period: String,
    /// The allowance for the current period; 0 means unlimited.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub quota: i64,
    /// The quota consumed in the current period.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub consumed: i64,
    /// Never resets.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub lifetime_consumed: i64,
    /// Requests in the current period.
    #[serde(skip_serializing_if = "crate::json::is_zero_i64")]
    pub requests: i64,
    /// The monetary cost accumulated in the current period.
    #[serde(
        skip_serializing_if = "crate::json::is_zero_f64",
        serialize_with = "crate::json::go_f64"
    )]
    pub spend: f64,
    /// Currency of `spend`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub currency: String,
    /// When the current period began.
    ///
    /// Always serialised, like Go: `omitempty` has no effect on a struct, so an
    /// unset start appears as Go's zero time.
    #[serde(serialize_with = "crate::json::go_time")]
    pub period_start: DateTime<Utc>,
    /// The last request time.
    #[serde(serialize_with = "crate::json::go_time")]
    pub last_seen: DateTime<Utc>,
    /// Mirrors the configuration.
    pub enabled: bool,
    /// The model allowlist, empty meaning all.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

impl Default for UserState {
    /// The Go zero value, with Go's zero *time* (year 1) rather than the Unix
    /// epoch.
    fn default() -> Self {
        UserState {
            name: String::new(),
            trust: String::new(),
            period: String::new(),
            quota: 0,
            consumed: 0,
            lifetime_consumed: 0,
            requests: 0,
            spend: 0.0,
            currency: String::new(),
            period_start: zero_time(),
            last_seen: zero_time(),
            enabled: false,
            models: Vec::new(),
        }
    }
}

impl UserState {
    /// Returns the quota left in the current period, or -1 when unlimited.
    pub fn remaining(&self) -> i64 {
        if self.quota <= 0 {
            return -1;
        }
        let r = self.quota - self.consumed;
        if r < 0 {
            return 0;
        }
        r
    }

    /// Reports whether the user has run out of quota.
    pub fn exhausted(&self) -> bool {
        if self.quota <= 0 {
            return false;
        }
        self.consumed >= self.quota
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failures returned by the store.
///
/// Go uses three sentinel values (`ErrQuotaExceeded`, `ErrUserSuspended`,
/// `ErrModelNotAllowed`) wrapped with the subject, so the HTTP layer can map each
/// to its own status code; the variants below play that role and carry the same
/// text as the Go errors.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The user's allowance is used up.
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),
    /// The user is disabled.
    #[error("user is suspended: {0}")]
    UserSuspended(String),
    /// The user may not call this model.
    #[error("model not allowed for this user: {0}")]
    ModelNotAllowed(String),
    /// The store has no state for the named user.
    #[error("store: unknown user {name:?}")]
    UnknownUser { name: String },
    /// The requested reset schedule is not one of the four supported values.
    #[error("store: invalid period {period:?}")]
    InvalidPeriod { period: String },
    /// An I/O failure, wrapped with the operation that failed.
    #[error("store: {context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    /// A JSON failure, wrapped with the operation that failed.
    #[error("store: {context}: {source}")]
    Json {
        context: String,
        #[source]
        source: serde_json::Error,
    },
}

impl StoreError {
    fn unknown_user(name: &str) -> StoreError {
        StoreError::UnknownUser {
            name: name.to_string(),
        }
    }
}

/// The supported quota reset schedules.
pub const PERIOD_NONE: &str = "none";
pub const PERIOD_DAILY: &str = "daily";
pub const PERIOD_WEEKLY: &str = "weekly";
pub const PERIOD_MONTHLY: &str = "monthly";

/// Reports whether a string is one of the four supported schedules.
pub fn period_valid(period: &str) -> bool {
    matches!(
        period,
        PERIOD_NONE | PERIOD_DAILY | PERIOD_WEEKLY | PERIOD_MONTHLY
    )
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

/// Owns per-user state and the aggregate views.
pub struct Manager {
    path: String,
    inner: Mutex<Inner>,
}

/// The clock used for period accounting; injectable for tests.
type Clock = Box<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// The change callback, invoked after every mutation so the state file can be
/// persisted.
///
/// It runs outside the lock, so it may call back into the manager (for example
/// [`Manager::save`]).
pub type ChangeHook = Arc<dyn Fn() + Send + Sync>;

struct Inner {
    users: HashMap<String, UserState>,
    now: Clock,
    on_change: Option<ChangeHook>,
    /// Tracks which currencies have been recorded, so an aggregate view can
    /// report mixing. Go writes this and never reads it;
    /// [`Manager::currencies_seen`] exposes it rather than leaving dead state
    /// behind.
    currencies_seen: HashSet<String>,
}

impl Manager {
    /// Creates a manager for the configured users.
    ///
    /// Each user's accounting period starts at zero time and is set to "now" the
    /// first time the clock is consulted, rather than at construction. That keeps
    /// the manager independent of construction order, which matters because the
    /// clock is injectable (see [`Manager::set_clock`]) and a caller must be able
    /// to install its clock after building the manager.
    pub fn new(users: &[config::User], state_path: impl Into<String>) -> Manager {
        let mut map = HashMap::with_capacity(users.len());
        for u in users {
            map.insert(
                u.name.clone(),
                UserState {
                    name: u.name.clone(),
                    trust: u.trust_level().to_string(),
                    period: u.period.clone(),
                    quota: u.quota,
                    enabled: u.is_enabled(),
                    models: u.models.clone(),
                    ..Default::default()
                },
            );
        }
        Manager {
            path: state_path.into(),
            inner: Mutex::new(Inner {
                users: map,
                now: Box::new(Utc::now),
                on_change: None,
                currencies_seen: HashSet::new(),
            }),
        }
    }

    /// Overrides the clock, for tests.
    ///
    /// Periods are anchored lazily on first use, so a clock installed right after
    /// construction behaves exactly as if it had been passed to the constructor.
    pub fn set_clock<F>(&self, f: F)
    where
        F: Fn() -> DateTime<Utc> + Send + Sync + 'static,
    {
        let mut inner = self.lock();
        inner.now = Box::new(f);
    }

    /// Registers a callback invoked after every state mutation, used to persist
    /// the state file.
    pub fn set_on_change(&self, f: Option<ChangeHook>) {
        let mut inner = self.lock();
        inner.on_change = f;
    }

    /// Returns a copy of a user's state.
    pub fn get(&self, name: &str) -> Option<UserState> {
        let inner = self.lock();
        inner.users.get(name).cloned()
    }

    /// Returns every user's state, sorted by name.
    pub fn list(&self) -> Vec<UserState> {
        let inner = self.lock();
        let mut out: Vec<UserState> = inner.users.values().cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Returns the currencies recorded so far, sorted, for the aggregate view.
    ///
    /// This is a Rust-side accessor for what Go keeps as an internal field.
    pub fn currencies_seen(&self) -> Vec<String> {
        let inner = self.lock();
        let mut out: Vec<String> = inner.currencies_seen.iter().cloned().collect();
        out.sort();
        out
    }

    /// Adds or replaces a user's configuration.
    pub fn upsert(&self, u: &config::User) {
        {
            let mut inner = self.lock();
            let now = (inner.now)();
            let st = inner
                .users
                .entry(u.name.clone())
                .or_insert_with(|| UserState {
                    name: u.name.clone(),
                    period_start: now,
                    ..Default::default()
                });
            st.trust = u.trust_level().to_string();
            st.period = u.period.clone();
            st.quota = u.quota;
            st.enabled = u.is_enabled();
            st.models = u.models.clone();
        }
        self.notify();
    }

    /// Grants or adjusts a user's allowance ("管理员发配额").
    ///
    /// Returns the resulting state. A negative `quota` clears the allowance,
    /// making the user unlimited.
    pub fn set_quota(&self, name: &str, quota: i64) -> Result<UserState, StoreError> {
        let out = {
            let mut inner = self.lock();
            let st = inner
                .users
                .get_mut(name)
                .ok_or_else(|| StoreError::unknown_user(name))?;
            if quota < 0 {
                st.quota = 0;
                st.consumed = 0;
            } else {
                st.quota = quota;
            }
            st.clone()
        };
        self.notify();
        Ok(out)
    }

    /// Increases a user's allowance by `delta`.
    pub fn add_quota(&self, name: &str, delta: i64) -> Result<UserState, StoreError> {
        let out = {
            let mut inner = self.lock();
            let st = inner
                .users
                .get_mut(name)
                .ok_or_else(|| StoreError::unknown_user(name))?;
            st.quota += delta;
            if st.quota < 0 {
                st.quota = 0;
            }
            st.clone()
        };
        self.notify();
        Ok(out)
    }

    /// Suspends or resumes a user.
    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<UserState, StoreError> {
        let out = {
            let mut inner = self.lock();
            let st = inner
                .users
                .get_mut(name)
                .ok_or_else(|| StoreError::unknown_user(name))?;
            st.enabled = enabled;
            st.clone()
        };
        self.notify();
        Ok(out)
    }

    /// Changes the reset schedule and restarts the current period.
    pub fn set_period(&self, name: &str, period: &str) -> Result<UserState, StoreError> {
        if !period_valid(period) {
            return Err(StoreError::InvalidPeriod {
                period: period.to_string(),
            });
        }
        let out = {
            let mut inner = self.lock();
            let now = (inner.now)();
            let st = inner
                .users
                .get_mut(name)
                .ok_or_else(|| StoreError::unknown_user(name))?;
            st.period = period.to_string();
            st.period_start = now;
            st.clone()
        };
        self.notify();
        Ok(out)
    }

    /// Verifies that a request may proceed, resetting the period first.
    pub fn check(&self, cfg: &config::User, model: &str) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let now = (inner.now)();
        let st = inner
            .users
            .get_mut(&cfg.name)
            .ok_or_else(|| StoreError::unknown_user(&cfg.name))?;
        roll_period(st, now);
        if !st.enabled {
            return Err(StoreError::UserSuspended(cfg.name.clone()));
        }
        if !cfg.models.is_empty() && !cfg.allows_model(model) {
            return Err(StoreError::ModelNotAllowed(model.to_string()));
        }
        if st.exhausted() {
            return Err(StoreError::QuotaExceeded(format!(
                "{} has {}/{} quota left",
                cfg.name,
                st.remaining(),
                st.quota
            )));
        }
        Ok(())
    }

    /// Records the outcome of a request.
    ///
    /// This is called for every exchange that reached accounting, including ones
    /// that were blocked: the plan's "记账永不跳过" rule means a blocked request
    /// still consumes quota and still appears in the ledger.
    pub fn commit(
        &self,
        name: &str,
        quota_units: i64,
        cost: f64,
        currency: &str,
        at: DateTime<Utc>,
    ) -> Result<UserState, StoreError> {
        let out = {
            let mut inner = self.lock();
            let now = (inner.now)();
            let Inner {
                users,
                currencies_seen,
                ..
            } = &mut *inner;
            let st = users
                .get_mut(name)
                .ok_or_else(|| StoreError::unknown_user(name))?;
            roll_period(st, now);
            st.consumed += quota_units;
            st.lifetime_consumed += quota_units;
            st.requests += 1;
            st.spend += cost;
            if !currency.is_empty() {
                st.currency = currency.to_string();
                currencies_seen.insert(currency.to_string());
            }
            st.last_seen = at;
            st.clone()
        };
        self.notify();
        Ok(out)
    }

    /// Resets any user whose period has elapsed. It is safe to call
    /// periodically.
    pub fn roll(&self) {
        let changed = {
            let mut inner = self.lock();
            let now = (inner.now)();
            let mut changed = false;
            for st in inner.users.values_mut() {
                let before = st.consumed;
                roll_period(st, now);
                if st.consumed != before {
                    changed = true;
                }
            }
            changed
        };
        if changed {
            // Only persist when a period actually rolled, so a periodic tick does
            // not rewrite the state file every minute.
            self.notify();
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Go has no poisoned-mutex notion; keep working after a panicked thread
        // instead of failing every later call.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Fires the change callback outside the lock.
    fn notify(&self) {
        let hook = {
            let inner = self.lock();
            inner.on_change.clone()
        };
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// Resets a user's consumption when the period has elapsed.
fn roll_period(st: &mut UserState, now: DateTime<Utc>) {
    if is_zero_time(st.period_start) {
        // First observation of this user: anchor the period to now.
        st.period_start = now;
        return;
    }
    let Some(mut next) = advance_period(&st.period, st.period_start) else {
        return;
    };
    if now < next {
        return;
    }
    // Advance in whole periods so a long outage does not leave the counter
    // permanently behind, and reset consumption for the new period.
    while now >= next {
        st.period_start = next;
        let Some(after) = advance_period(&st.period, st.period_start) else {
            return;
        };
        next = after;
    }
    st.consumed = 0;
    st.requests = 0;
    st.spend = 0.0;
}

/// Returns the end of the period that starts at `start`, or `None` for a schedule
/// that never resets.
fn advance_period(period: &str, start: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match period {
        PERIOD_DAILY => Some(add_date(start, 0, 0, 1)),
        PERIOD_WEEKLY => Some(add_date(start, 0, 0, 7)),
        PERIOD_MONTHLY => Some(add_date(start, 0, 1, 0)),
        _ => None,
    }
}

/// Reports whether a timestamp is Go's zero time.
fn is_zero_time(t: DateTime<Utc>) -> bool {
    t == zero_time()
}

/// Adds years, months and days the way Go's `time.Time.AddDate` does.
///
/// Go normalises its result the same way `time.Date` does, so adding one month to
/// January 31 yields March 3 (the normalised form of February 31) rather than
/// clamping to February 28. `chrono`'s `checked_add_months` clamps instead, which
/// would change which day a monthly allowance resets on.
fn add_date(t: DateTime<Utc>, years: i32, months: i32, days: i64) -> DateTime<Utc> {
    let date = t.date_naive();
    let time = t.time();
    let mut year = date.year() + years;
    let mut month0 = date.month0() as i32 + months;
    year += month0.div_euclid(12);
    month0 = month0.rem_euclid(12);
    let Some(first) = NaiveDate::from_ymd_opt(year, month0 as u32 + 1, 1) else {
        return t;
    };
    let offset = (date.day() as i64 - 1) + days;
    let Some(normalised) = first.checked_add_signed(ChronoDuration::days(offset)) else {
        return t;
    };
    normalised.and_time(time).and_utc()
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// The on-disk representation of the manager.
///
/// The user map is a `BTreeMap` so the file is written with sorted keys, exactly
/// as Go's `encoding/json` sorts map keys; that keeps the state file diffable and
/// lets tests pin its bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct StateFile {
    version: i32,
    saved_at: DateTime<Utc>,
    users: BTreeMap<String, Option<UserState>>,
}

impl Default for StateFile {
    fn default() -> Self {
        StateFile {
            version: 0,
            saved_at: zero_time(),
            users: BTreeMap::new(),
        }
    }
}

impl Manager {
    /// Writes the state to disk atomically.
    pub fn save(&self) -> Result<(), StoreError> {
        if self.path.is_empty() {
            return Ok(());
        }
        let snap = {
            let inner = self.lock();
            let mut users = BTreeMap::new();
            for (name, st) in &inner.users {
                users.insert(name.clone(), Some(st.clone()));
            }
            StateFile {
                version: 1,
                saved_at: (inner.now)(),
                users,
            }
        };
        let b = serde_json::to_string_pretty(&snap).map_err(|e| StoreError::Json {
            context: "marshal state".to_string(),
            source: e,
        })?;
        if let Some(dir) = parent_dir(&self.path) {
            mkdir_all(&dir).map_err(|e| StoreError::Io {
                context: "create dir".to_string(),
                source: e,
            })?;
        }
        let tmp = format!("{}.tmp", self.path);
        write_file(&tmp, b.as_bytes()).map_err(|e| StoreError::Io {
            context: "write state".to_string(),
            source: e,
        })?;
        fs::rename(&tmp, &self.path).map_err(|e| StoreError::Io {
            context: "replace state".to_string(),
            source: e,
        })?;
        Ok(())
    }

    /// Restores state from disk, applying it over the configured users.
    ///
    /// Users present in the file but absent from the configuration are retained
    /// so that a temporarily removed user does not lose their accounting history.
    pub fn load(&self) -> Result<(), StoreError> {
        if self.path.is_empty() {
            return Ok(());
        }
        let raw = match fs::read(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(StoreError::Io {
                    context: "read state".to_string(),
                    source: e,
                })
            }
        };
        let snap: StateFile = serde_json::from_slice(&raw).map_err(|e| StoreError::Json {
            context: "parse state".to_string(),
            source: e,
        })?;
        let mut inner = self.lock();
        let Inner { users, .. } = &mut *inner;
        for (name, saved) in snap.users {
            let Some(mut saved) = saved else { continue };
            if let Some(cur) = users.get(&name) {
                // Keep the configured identity fields, restore the counters.
                saved.trust = cur.trust.clone();
                saved.enabled = cur.enabled;
                saved.models = cur.models.clone();
                if saved.period.is_empty() {
                    saved.period = cur.period.clone();
                }
                if saved.quota == 0 && cur.quota != 0 {
                    saved.quota = cur.quota;
                }
            }
            users.insert(name, saved);
        }
        Ok(())
    }
}

/// Returns the parent directory of a path, or `None` for `"."` and empty parents,
/// mirroring Go's `filepath.Dir` guard.
fn parent_dir(path: &str) -> Option<std::path::PathBuf> {
    let dir = std::path::Path::new(path).parent()?;
    if dir.as_os_str().is_empty() || dir == std::path::Path::new(".") {
        return None;
    }
    Some(dir.to_path_buf())
}

/// Creates a directory and its parents with the deployment's 0750 mode.
#[cfg(unix)]
fn mkdir_all(dir: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o750)
        .create(dir)
}

/// Creates a directory and its parents.
#[cfg(not(unix))]
fn mkdir_all(dir: &std::path::Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

/// Writes a file with the state file's 0640 mode.
#[cfg(unix)]
fn write_file(path: &str, body: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o640)
        .open(path)?;
    f.write_all(body)
}

/// Writes a file.
#[cfg(not(unix))]
fn write_file(path: &str, body: &[u8]) -> io::Result<()> {
    fs::write(path, body)
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

/// Aggregates usage for one model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelStat {
    pub model: String,
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    #[serde(serialize_with = "crate::json::go_f64")]
    pub cost: f64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub currency: String,
    pub blocked: i64,
    pub over_billed: i64,
}

/// Aggregates usage for one upstream channel.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChannelStat {
    pub channel: String,
    pub requests: i64,
    pub failures: i64,
    pub blocked: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    #[serde(serialize_with = "crate::json::go_f64")]
    pub cost: f64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub currency: String,
    /// The mean of per-request integrity (1 - clamped deviation) over verifiable
    /// requests; 1.0 means "always consistent".
    #[serde(serialize_with = "crate::json::go_f64")]
    pub usage_integrity: f64,
    /// Requests where a comparison was possible.
    pub verifiable: i64,
    /// The mean total latency.
    #[serde(serialize_with = "crate::json::go_f64")]
    pub mean_latency_ms: f64,
}

/// Builds per-model and per-channel statistics from audit records.
///
/// The audit ledger is the source of truth: the aggregate views are always
/// recomputable from it, which is what lets the management UI be trusted.
pub fn aggregate(records: &[audit::Record]) -> (Vec<ModelStat>, Vec<ChannelStat>) {
    let mut model_idx: HashMap<String, ModelStat> = HashMap::new();
    let mut chan_idx: HashMap<String, ChannelStat> = HashMap::new();
    let mut lat_sum: HashMap<String, i64> = HashMap::new();

    for r in records {
        let ms = model_idx
            .entry(r.model.clone())
            .or_insert_with(|| ModelStat {
                model: r.model.clone(),
                currency: r.cost.currency.clone(),
                ..Default::default()
            });
        let cs = chan_idx
            .entry(r.channel.clone())
            .or_insert_with(|| ChannelStat {
                channel: r.channel.clone(),
                currency: r.cost.currency.clone(),
                ..Default::default()
            });

        ms.requests += 1;
        cs.requests += 1;
        ms.prompt_tokens += r.usage.prompt_tokens;
        ms.completion_tokens += r.usage.completion_tokens;
        ms.total_tokens += r.usage.total_tokens;
        ms.cost += r.cost.total_cost;
        cs.prompt_tokens += r.usage.prompt_tokens;
        cs.completion_tokens += r.usage.completion_tokens;
        cs.total_tokens += r.usage.total_tokens;
        cs.cost += r.cost.total_cost;
        *lat_sum.entry(r.channel.clone()).or_insert(0) += r.latency_ms.total_ms;

        match r.decision.as_str() {
            audit::DECISION_BLOCK_USER | audit::DECISION_BLOCK_UPSTREAM => {
                ms.blocked += 1;
                cs.blocked += 1;
            }
            audit::DECISION_UPSTREAM_ERROR => {
                cs.failures += 1;
            }
            _ => {}
        }
        if r.usage.verdict == "over_billed" {
            ms.over_billed += 1;
        }
        if r.usage.upstream_reported && r.usage.verdict != "not_reported" {
            cs.verifiable += 1;
            // Integrity is derived from the verdict the gateway recorded, not
            // from a re-derivation of the raw deltas: the verdict already
            // accounts for the absolute floor below which a deviation is
            // estimator noise, and re-deriving would discard that.
            let mut integrity = 1.0;
            if r.usage.verdict == "over_billed" || r.usage.verdict == "under_billed" {
                let mut d = r.usage.prompt_delta;
                let v = r.usage.completion_delta;
                if v.abs() > d.abs() {
                    d = v;
                }
                integrity = 1.0 - d.abs();
                if integrity < 0.0 {
                    integrity = 0.0;
                }
            }
            // Running mean.
            cs.usage_integrity += (integrity - cs.usage_integrity) / cs.verifiable as f64;
        }
    }

    let mut models: Vec<ModelStat> = model_idx.into_values().collect();
    models.sort_by(|a, b| a.model.cmp(&b.model));

    let mut channels: Vec<ChannelStat> = Vec::with_capacity(chan_idx.len());
    for (name, mut v) in chan_idx {
        if v.requests > 0 {
            let total = lat_sum.get(&name).copied().unwrap_or(0);
            v.mean_latency_ms = total as f64 / v.requests as f64;
        }
        if v.verifiable == 0 {
            v.usage_integrity = 1.0;
        }
        channels.push(v);
    }
    channels.sort_by(|a, b| a.channel.cmp(&b.channel));
    (models, channels)
}

// ---------------------------------------------------------------------------
// Tests (translated from store_test.go)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::testutil::TempDir;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn users() -> Vec<config::User> {
        vec![
            config::User {
                name: "alice".to_string(),
                token: "t1".to_string(),
                quota: 1000,
                period: "daily".to_string(),
                ..Default::default()
            },
            // unlimited
            config::User {
                name: "bob".to_string(),
                token: "t2".to_string(),
                ..Default::default()
            },
            config::User {
                name: "carol".to_string(),
                token: "t3".to_string(),
                quota: 500,
                period: "monthly".to_string(),
                models: vec!["gpt-4o".to_string()],
                ..Default::default()
            },
        ]
    }

    fn new_test_manager() -> (Manager, TempDir) {
        let dir = TempDir::new();
        let m = Manager::new(&users(), dir.join("state.json").to_string_lossy());
        (m, dir)
    }

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, y, mo, d, h, mi, s).unwrap()
    }

    /// A settable clock, the Rust stand-in for Go's captured `now` variable.
    #[derive(Clone)]
    struct Clock(Arc<Mutex<DateTime<Utc>>>);

    impl Clock {
        fn new(at: DateTime<Utc>) -> Clock {
            Clock(Arc::new(Mutex::new(at)))
        }

        fn set(&self, at: DateTime<Utc>) {
            *self.0.lock().unwrap() = at;
        }

        fn get(&self) -> DateTime<Utc> {
            *self.0.lock().unwrap()
        }

        fn callback(&self) -> impl Fn() -> DateTime<Utc> + Send + Sync + 'static {
            let inner = Arc::clone(&self.0);
            move || *inner.lock().unwrap()
        }
    }

    // -----------------------------------------------------------------------
    // Quota
    // -----------------------------------------------------------------------

    #[test]
    fn check_and_commit() {
        let (m, _dir) = new_test_manager();
        let alice = users()[0].clone();

        m.check(&alice, "gpt-4o").expect("initial check failed");
        let st = m
            .commit("alice", 400, 0.4, "CNY", Utc::now())
            .expect("commit");
        assert_eq!(st.consumed, 400);
        assert_eq!(st.remaining(), 600, "unexpected state: {st:?}");
        assert_eq!(st.requests, 1);
        assert_eq!(st.lifetime_consumed, 400);
    }

    #[test]
    fn quota_exhaustion() {
        let (m, _dir) = new_test_manager();
        let alice = users()[0].clone();
        m.commit("alice", 1000, 1.0, "CNY", Utc::now())
            .expect("commit");

        let err = m.check(&alice, "gpt-4o").unwrap_err();
        assert!(
            matches!(err, StoreError::QuotaExceeded(_)),
            "expected QuotaExceeded, got {err:?}"
        );
        assert_eq!(
            err.to_string(),
            "quota exceeded: alice has 0/1000 quota left"
        );
    }

    #[test]
    fn unlimited_quota() {
        let (m, _dir) = new_test_manager();
        let bob = users()[1].clone();
        m.commit("bob", 10_000_000, 10.0, "CNY", Utc::now())
            .expect("commit");
        m.check(&bob, "gpt-4o")
            .expect("an unlimited user should never be exhausted");
        let st = m.get("bob").expect("bob");
        assert_eq!(st.remaining(), -1, "unlimited remaining should be -1");
    }

    #[test]
    fn model_restriction() {
        let (m, _dir) = new_test_manager();
        let carol = users()[2].clone();
        m.check(&carol, "gpt-4o").expect("allowed model rejected");
        let err = m.check(&carol, "claude-3").unwrap_err();
        assert!(
            matches!(err, StoreError::ModelNotAllowed(_)),
            "expected ModelNotAllowed, got {err:?}"
        );
        assert_eq!(err.to_string(), "model not allowed for this user: claude-3");
    }

    #[test]
    fn suspended_user() {
        let (m, _dir) = new_test_manager();
        let alice = users()[0].clone();
        m.set_enabled("alice", false).expect("set enabled");
        let err = m.check(&alice, "gpt-4o").unwrap_err();
        assert!(
            matches!(err, StoreError::UserSuspended(_)),
            "expected UserSuspended, got {err:?}"
        );
        assert_eq!(err.to_string(), "user is suspended: alice");
        m.set_enabled("alice", true).expect("set enabled");
        m.check(&alice, "gpt-4o").expect("resumed user rejected");
    }

    /// The "管理员发配额" workflow.
    #[test]
    fn quota_grant() {
        let (m, _dir) = new_test_manager();

        let st = m.set_quota("alice", 5000).expect("set quota");
        assert_eq!(st.quota, 5000);
        // Granting preserves consumption so an audit can reconstruct history.
        m.commit("alice", 100, 0.1, "CNY", Utc::now())
            .expect("commit");
        let st = m.add_quota("alice", 1000).expect("add quota");
        assert_eq!((st.quota, st.consumed), (6000, 100), "AddQuota: {st:?}");
        // A negative grant clears the allowance.
        let st = m.set_quota("alice", -1).expect("set quota");
        assert_eq!((st.quota, st.consumed), (0, 0), "clearing quota: {st:?}");
        assert!(
            m.set_quota("nobody", 1).is_err(),
            "expected an error for an unknown user"
        );
    }

    // -----------------------------------------------------------------------
    // Periods
    // -----------------------------------------------------------------------

    /// A daily quota resets when the day rolls over.
    #[test]
    fn daily_reset() {
        let (m, _dir) = new_test_manager();
        let clock = Clock::new(dt(2026, 3, 1, 10, 0, 0));
        m.set_clock(clock.callback());

        // Roll sets each user's period start to the current clock.
        m.roll();
        m.commit("alice", 900, 0.9, "CNY", clock.get())
            .expect("commit");
        let st = m.get("alice").expect("alice");
        assert_eq!(st.consumed, 900);

        // Same day: no reset.
        clock.set(clock.get() + ChronoDuration::hours(2));
        m.roll();
        assert_eq!(m.get("alice").expect("alice").consumed, 900);

        // Next day: reset, but lifetime persists.
        clock.set(clock.get() + ChronoDuration::hours(24));
        m.roll();
        let st = m.get("alice").expect("alice");
        assert_eq!(st.consumed, 0, "daily quota did not reset");
        assert_eq!(
            st.lifetime_consumed, 900,
            "lifetime consumption must survive a period reset"
        );
        assert_eq!(st.requests, 0, "period request count did not reset");
    }

    /// A long gap does not leave the counter permanently behind.
    #[test]
    fn period_catch_up_after_outage() {
        let (m, _dir) = new_test_manager();
        let start = dt(2026, 3, 1, 10, 0, 0);
        let clock = Clock::new(start);
        m.set_clock(clock.callback());
        m.roll();
        m.commit("alice", 500, 0.5, "CNY", clock.get())
            .expect("commit");

        // The gateway is down for a week.
        clock.set(add_date(start, 0, 0, 7));
        m.roll();
        let st = m.get("alice").expect("alice");
        assert_eq!(st.consumed, 0, "expected a reset after a week");
        // The period start must have advanced to the current period, not lagged.
        assert!(
            st.period_start >= clock.get() - ChronoDuration::hours(25),
            "period start lagged behind: {} vs now {}",
            st.period_start,
            clock.get()
        );
    }

    #[test]
    fn monthly_reset() {
        let (m, _dir) = new_test_manager();
        let clock = Clock::new(dt(2026, 1, 15, 0, 0, 0));
        m.set_clock(clock.callback());
        m.roll();
        m.commit("carol", 100, 0.1, "CNY", clock.get())
            .expect("commit");

        clock.set(dt(2026, 2, 15, 0, 0, 0));
        m.roll();
        assert_eq!(m.get("carol").expect("carol").consumed, 0);
    }

    #[test]
    fn set_period_changes_the_schedule() {
        let (m, _dir) = new_test_manager();
        m.set_period("bob", "weekly").expect("set period");
        assert_eq!(m.get("bob").expect("bob").period, "weekly");
        let err = m.set_period("bob", "hourly").unwrap_err();
        assert!(matches!(err, StoreError::InvalidPeriod { .. }));
        assert_eq!(err.to_string(), "store: invalid period \"hourly\"");
    }

    /// Period "none" is truly permanent.
    #[test]
    fn no_period_never_resets() {
        let (m, _dir) = new_test_manager();
        let clock = Clock::new(dt(2026, 1, 1, 0, 0, 0));
        m.set_clock(clock.callback());
        m.commit("bob", 42, 0.1, "CNY", clock.get())
            .expect("commit");

        clock.set(add_date(clock.get(), 2, 0, 0));
        m.roll();
        assert_eq!(
            m.get("bob").expect("bob").consumed,
            42,
            "a permanent allowance was reset"
        );
    }

    // -----------------------------------------------------------------------
    // Persistence
    // -----------------------------------------------------------------------

    #[test]
    fn state_persistence() {
        let dir = TempDir::new();
        let path = dir.join("state.json").to_string_lossy().into_owned();

        let m1 = Manager::new(&users(), path.clone());
        m1.load().expect("load");
        m1.commit("alice", 750, 0.75, "CNY", Utc::now())
            .expect("commit");
        m1.save().expect("save");

        let m2 = Manager::new(&users(), path);
        m2.load().expect("load");
        let st = m2.get("alice").expect("user missing after reload");
        assert_eq!(
            (st.consumed, st.lifetime_consumed),
            (750, 750),
            "state not restored: {st:?}"
        );
    }

    /// A first run works.
    #[test]
    fn load_missing_state_is_not_an_error() {
        let dir = TempDir::new();
        let m = Manager::new(&users(), dir.join("absent.json").to_string_lossy());
        m.load().expect("load");
    }

    #[test]
    fn load_corrupt_state_fails() {
        let dir = TempDir::new();
        let path = dir.join("state.json");
        fs::write(&path, "{not json").expect("write");
        let m = Manager::new(&users(), path.to_string_lossy());
        assert!(
            m.load().is_err(),
            "expected an error for a corrupt state file"
        );
    }

    /// The in-memory mode used by tests.
    #[test]
    fn save_disabled_when_no_path() {
        let m = Manager::new(&users(), "");
        m.save().expect("Save with no path should be a no-op");
    }

    #[test]
    fn on_change_callback() {
        let (m, _dir) = new_test_manager();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        m.set_on_change(Some(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })));
        m.commit("alice", 1, 0.0, "CNY", Utc::now())
            .expect("commit");
        m.set_quota("alice", 10).expect("set quota");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "expected 2 change notifications"
        );
    }

    // -----------------------------------------------------------------------
    // Aggregation
    // -----------------------------------------------------------------------

    #[test]
    fn aggregate_stats() {
        use crate::audit::{Cost, Latency, UsageSummary};
        let records = vec![
            audit::Record {
                channel: "a".to_string(),
                model: "gpt-4o".to_string(),
                decision: audit::DECISION_ALLOW.to_string(),
                usage: UsageSummary {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    upstream_reported: true,
                    verdict: "consistent".to_string(),
                    ..Default::default()
                },
                cost: Cost {
                    total_cost: 0.5,
                    currency: "CNY".to_string(),
                    ..Default::default()
                },
                latency_ms: Latency {
                    total_ms: 100,
                    ..Default::default()
                },
                ..Default::default()
            },
            audit::Record {
                channel: "a".to_string(),
                model: "gpt-4o".to_string(),
                decision: audit::DECISION_ALLOW.to_string(),
                // Upstream billed 50% more than the recount.
                usage: UsageSummary {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    upstream_reported: true,
                    verdict: "over_billed".to_string(),
                    completion_delta: 0.5,
                    ..Default::default()
                },
                cost: Cost {
                    total_cost: 0.5,
                    currency: "CNY".to_string(),
                    ..Default::default()
                },
                latency_ms: Latency {
                    total_ms: 200,
                    ..Default::default()
                },
                ..Default::default()
            },
            audit::Record {
                channel: "b".to_string(),
                model: "claude-3".to_string(),
                decision: audit::DECISION_BLOCK_USER.to_string(),
                usage: UsageSummary {
                    verdict: "not_reported".to_string(),
                    ..Default::default()
                },
                cost: Cost {
                    total_cost: 0.0,
                    currency: "CNY".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
            audit::Record {
                channel: "b".to_string(),
                model: "claude-3".to_string(),
                decision: audit::DECISION_UPSTREAM_ERROR.to_string(),
                usage: UsageSummary {
                    verdict: "not_reported".to_string(),
                    ..Default::default()
                },
                cost: Cost {
                    total_cost: 0.0,
                    currency: "CNY".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
        ];
        let (models, channels) = aggregate(&records);

        assert_eq!(models.len(), 2, "expected 2 model stats");
        let gpt = models
            .iter()
            .find(|m| m.model == "gpt-4o")
            .expect("gpt-4o stats");
        assert_eq!(
            (gpt.requests, gpt.total_tokens),
            (2, 300),
            "model aggregation wrong: {gpt:?}"
        );
        assert_eq!(gpt.over_billed, 1);
        assert!(
            (0.99..=1.01).contains(&gpt.cost),
            "cost aggregation wrong: {}",
            gpt.cost
        );

        assert_eq!(channels.len(), 2, "expected 2 channel stats");
        let ch_a = channels
            .iter()
            .find(|c| c.channel == "a")
            .expect("channel a");
        let ch_b = channels
            .iter()
            .find(|c| c.channel == "b")
            .expect("channel b");
        assert_eq!(ch_a.requests, 2);
        // Mean latency over two requests: (100+200)/2 = 150.
        assert_eq!(ch_a.mean_latency_ms, 150.0);
        // Usage integrity is the mean of (1-0) and (1-0.5) = 0.75.
        assert!(
            (0.74..=0.76).contains(&ch_a.usage_integrity),
            "usage integrity = {}, want ~0.75",
            ch_a.usage_integrity
        );
        assert_eq!(ch_a.verifiable, 2);
        assert_eq!(
            (ch_b.blocked, ch_b.failures),
            (1, 1),
            "channel b outcomes wrong: {ch_b:?}"
        );
        // A channel with nothing verifiable reports full integrity rather than 0,
        // so it is not punished for an absence of evidence.
        assert_eq!(ch_b.usage_integrity, 1.0);
    }

    #[test]
    fn aggregate_empty() {
        let (models, channels) = aggregate(&[]);
        assert!(
            models.is_empty() && channels.is_empty(),
            "aggregating nothing should produce nothing"
        );
    }

    #[test]
    fn list_is_sorted() {
        let (m, _dir) = new_test_manager();
        let list = m.list();
        assert_eq!(list.len(), 3, "expected 3 users");
        for pair in list.windows(2) {
            assert!(
                pair[0].name <= pair[1].name,
                "users not sorted: {:?}",
                list.iter().map(|u| &u.name).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn upsert_adds_and_updates() {
        let (m, _dir) = new_test_manager();
        m.upsert(&config::User {
            name: "dave".to_string(),
            token: "t4".to_string(),
            quota: 77,
            period: "weekly".to_string(),
            ..Default::default()
        });
        let st = m.get("dave").expect("upsert did not create the user");
        assert_eq!((st.quota, st.period.as_str()), (77, "weekly"), "{st:?}");
        // Upserting again updates rather than resetting consumption.
        m.commit("dave", 10, 0.1, "CNY", Utc::now())
            .expect("commit");
        m.upsert(&config::User {
            name: "dave".to_string(),
            token: "t4".to_string(),
            quota: 100,
            period: "weekly".to_string(),
            ..Default::default()
        });
        let st = m.get("dave").expect("dave");
        assert_eq!(
            (st.quota, st.consumed),
            (100, 10),
            "re-upsert produced {st:?}"
        );
    }

    #[test]
    fn unknown_user_operations() {
        let (m, _dir) = new_test_manager();
        assert!(
            m.commit("ghost", 1, 0.0, "CNY", Utc::now()).is_err(),
            "expected an error committing for an unknown user"
        );
        assert!(
            m.add_quota("ghost", 1).is_err(),
            "expected an error adding quota for an unknown user"
        );
        assert!(
            m.set_enabled("ghost", true).is_err(),
            "expected an error enabling an unknown user"
        );
        let ghost = config::User {
            name: "ghost".to_string(),
            ..Default::default()
        };
        assert!(
            m.check(&ghost, "m").is_err(),
            "expected an error checking an unknown user"
        );
        assert!(m.get("ghost").is_none());
    }

    // -----------------------------------------------------------------------
    // Rust-side boundary tests
    // -----------------------------------------------------------------------

    /// Pins the on-disk state format against the Go implementation. The expected
    /// text is Go's `MarshalIndent` output for the same two users after the same
    /// operations.
    #[test]
    fn state_file_matches_the_go_layout() {
        let dir = TempDir::new();
        let path = dir.join("state.json").to_string_lossy().into_owned();
        let m = Manager::new(
            &[
                config::User {
                    name: "alice".to_string(),
                    token: "t1".to_string(),
                    quota: 1000,
                    period: "daily".to_string(),
                    trust: "trusted".to_string(),
                    ..Default::default()
                },
                config::User {
                    name: "bob".to_string(),
                    token: "t2".to_string(),
                    ..Default::default()
                },
            ],
            path.clone(),
        );
        let clock = Clock::new(dt(2026, 3, 1, 10, 0, 0));
        m.set_clock(clock.callback());
        m.roll();
        m.commit("alice", 400, 0.4, "CNY", clock.get())
            .expect("commit");
        m.commit("bob", 1, 0.01, "", clock.get() + ChronoDuration::minutes(1))
            .expect("commit");
        m.save().expect("save");

        let want = r#"{
  "version": 1,
  "saved_at": "2026-03-01T10:00:00Z",
  "users": {
    "alice": {
      "name": "alice",
      "trust": "trusted",
      "period": "daily",
      "quota": 1000,
      "consumed": 400,
      "lifetime_consumed": 400,
      "requests": 1,
      "spend": 0.4,
      "currency": "CNY",
      "period_start": "2026-03-01T10:00:00Z",
      "last_seen": "2026-03-01T10:00:00Z",
      "enabled": true
    },
    "bob": {
      "name": "bob",
      "trust": "untrusted",
      "consumed": 1,
      "lifetime_consumed": 1,
      "requests": 1,
      "spend": 0.01,
      "period_start": "2026-03-01T10:00:00Z",
      "last_seen": "2026-03-01T10:01:00Z",
      "enabled": true
    }
  }
}"#;
        assert_eq!(fs::read_to_string(&path).expect("read state"), want);
    }

    /// Go's `AddDate` normalises overflow instead of clamping, which decides which
    /// day a monthly allowance resets on.
    #[test]
    fn monthly_add_date_normalises_like_go() {
        assert_eq!(
            add_date(dt(2026, 1, 31, 10, 0, 0), 0, 1, 0),
            dt(2026, 3, 3, 10, 0, 0)
        );
        assert_eq!(
            add_date(dt(2026, 2, 28, 10, 0, 0), 0, 1, 0),
            dt(2026, 3, 28, 10, 0, 0)
        );
        assert_eq!(
            add_date(dt(2024, 2, 29, 10, 0, 0), 0, 12, 0),
            dt(2025, 3, 1, 10, 0, 0)
        );
        assert_eq!(
            add_date(dt(2026, 3, 1, 10, 0, 0), 0, 0, 7),
            dt(2026, 3, 8, 10, 0, 0)
        );
    }

    /// A monthly period anchored on the 31st resets on Go's normalised date, and
    /// catching up over years lands on the current period (values captured from
    /// the Go implementation).
    #[test]
    fn monthly_period_anchor_and_catch_up() {
        let (m, _dir) = new_test_manager();
        let clock = Clock::new(dt(2026, 1, 31, 10, 0, 0));
        m.set_clock(clock.callback());
        m.roll();
        m.commit("carol", 100, 0.1, "CNY", clock.get())
            .expect("commit");

        // February 15 is before January 31 + 1 month (March 3): no reset.
        clock.set(dt(2026, 2, 15, 0, 0, 0));
        m.roll();
        let st = m.get("carol").expect("carol");
        assert_eq!(st.consumed, 100);
        assert_eq!(st.period_start, dt(2026, 1, 31, 10, 0, 0));

        // March 3 resets.
        clock.set(dt(2026, 3, 3, 10, 0, 0));
        m.roll();
        let st = m.get("carol").expect("carol");
        assert_eq!(st.consumed, 0);
        assert_eq!(st.period_start, dt(2026, 3, 3, 10, 0, 0));

        // Two years later the catch-up loop lands on the current period.
        clock.set(dt(2028, 5, 20, 0, 0, 0));
        m.roll();
        let st = m.get("carol").expect("carol");
        assert_eq!(st.consumed, 0);
        assert_eq!(st.period_start, dt(2028, 5, 3, 10, 0, 0));
    }

    /// A loaded state file keeps the configured identity fields but restores the
    /// counters, and a user that disappeared from the configuration keeps its
    /// history.
    #[test]
    fn load_merges_config_over_saved_state() {
        let dir = TempDir::new();
        let path = dir.join("state.json");
        fs::write(
            &path,
            r#"{
  "version": 1,
  "saved_at": "2026-03-01T10:00:00Z",
  "users": {
    "alice": {
      "name": "alice",
      "trust": "untrusted",
      "period": "",
      "consumed": 700,
      "lifetime_consumed": 900,
      "requests": 3,
      "enabled": false,
      "models": ["stale"]
    },
    "gone": {"name": "gone", "consumed": 5, "lifetime_consumed": 5}
  }
}"#,
        )
        .expect("write");

        let m = Manager::new(
            &[config::User {
                name: "alice".to_string(),
                token: "t1".to_string(),
                quota: 1000,
                period: "daily".to_string(),
                trust: "trusted".to_string(),
                ..Default::default()
            }],
            path.to_string_lossy(),
        );
        m.load().expect("load");

        let st = m.get("alice").expect("alice");
        assert_eq!(st.trust, "trusted", "configured trust must win");
        assert!(st.enabled, "configured enabled must win");
        assert!(st.models.is_empty(), "configured models must win");
        assert_eq!(st.period, "daily", "empty saved period falls back");
        assert_eq!(st.quota, 1000, "zero saved quota falls back");
        assert_eq!((st.consumed, st.lifetime_consumed), (700, 900));

        let gone = m.get("gone").expect("users removed from config are kept");
        assert_eq!(gone.consumed, 5);
    }

    /// Negative grants clamp to zero rather than producing a negative allowance.
    #[test]
    fn add_quota_clamps_at_zero() {
        let (m, _dir) = new_test_manager();
        let st = m.add_quota("alice", -5000).expect("add quota");
        assert_eq!(st.quota, 0);
        assert_eq!(st.remaining(), -1, "a cleared quota means unlimited");
    }

    /// The change callback runs outside the lock, so it can persist the state
    /// file - the reason the Go implementation releases the mutex first.
    #[test]
    fn on_change_callback_can_save() {
        let dir = TempDir::new();
        let path = dir.join("state.json").to_string_lossy().into_owned();
        let m = Arc::new(Manager::new(&users(), path.clone()));
        {
            let weak = Arc::downgrade(&m);
            m.set_on_change(Some(Arc::new(move || {
                if let Some(m) = weak.upgrade() {
                    m.save().expect("save from the callback");
                }
            })));
        }
        m.commit("alice", 5, 0.5, "CNY", Utc::now())
            .expect("commit");
        let text = fs::read_to_string(&path).expect("state written by the callback");
        assert!(text.contains("\"consumed\": 5"), "state: {text}");
    }

    /// A commit during a period roll only resets what the roll resets.
    #[test]
    fn commit_and_roll_interact() {
        let (m, _dir) = new_test_manager();
        let clock = Clock::new(dt(2026, 3, 1, 10, 0, 0));
        m.set_clock(clock.callback());
        m.roll();
        m.commit("alice", 100, 0.1, "CNY", clock.get())
            .expect("commit");
        clock.set(dt(2026, 3, 2, 11, 0, 0));
        // Commit itself rolls the period before adding the new units.
        let st = m
            .commit("alice", 7, 0.07, "CNY", clock.get())
            .expect("commit");
        assert_eq!(st.consumed, 7);
        assert_eq!(st.requests, 1);
        assert_eq!(st.lifetime_consumed, 107);
    }

    /// Eight threads charging the same account must not lose a single unit: the
    /// counter is the quota authority, so a lost update would let a user spend
    /// more than their allowance.
    #[test]
    fn concurrent_commits_are_atomic() {
        let (m, _dir) = new_test_manager();
        let at = dt(2026, 3, 4, 12, 0, 0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..50 {
                        m.commit("alice", 1, 0.0, "", at).expect("commit");
                    }
                });
            }
        });

        let st = m.get("alice").expect("state");
        assert_eq!(st.consumed, 400);
        assert_eq!(st.requests, 400);
        assert_eq!(st.lifetime_consumed, 400);
        assert_eq!(st.remaining(), 600, "quota 1000 - 400 consumed");
    }

    /// Spend accumulates across currencies while the last currency used is kept
    /// on the state, which is what the aggregate views read.
    #[test]
    fn spend_accumulates_and_currencies_are_tracked() {
        let (m, _dir) = new_test_manager();
        let at = dt(2026, 3, 4, 12, 0, 0);
        m.commit("bob", 1, 0.5, "CNY", at).expect("commit");
        m.commit("bob", 1, 0.25, "USD", at).expect("commit");

        let st = m.get("bob").expect("state");
        assert!(almost(st.spend, 0.75), "spend = {}", st.spend);
        assert_eq!(st.currency, "USD");
        assert_eq!(
            m.currencies_seen(),
            vec!["CNY".to_string(), "USD".to_string()]
        );
    }

    /// Weekly periods advance in seven-day steps from the period anchor, the way
    /// the Go manager's `advancePeriod` does.
    #[test]
    fn weekly_reset_uses_seven_day_periods() {
        let users = vec![config::User {
            name: "alice".to_string(),
            token: "t1".to_string(),
            quota: 100,
            period: "weekly".to_string(),
            ..config::User::default()
        }];
        let dir = TempDir::new();
        let m = Manager::new(&users, dir.join("state.json").to_string_lossy());
        let clock = Clock::new(dt(2026, 3, 2, 10, 0, 0));
        m.set_clock(clock.callback());

        m.roll();
        m.commit("alice", 40, 0.0, "", clock.get()).expect("commit");

        // Six days later the period is still the same one.
        clock.set(clock.get() + ChronoDuration::days(6));
        m.roll();
        assert_eq!(m.get("alice").expect("alice").consumed, 40);

        // Past the seven-day mark the counter resets; lifetime does not.
        clock.set(clock.get() + ChronoDuration::days(2));
        m.roll();
        let st = m.get("alice").expect("alice");
        assert_eq!(st.consumed, 0, "weekly quota did not reset");
        assert_eq!(st.lifetime_consumed, 40);
        assert_eq!(st.requests, 0);
    }

    /// `commit` is deliberately unconditional, as it is in the Go manager: the
    /// gateway charges one unit and writes an audit record even for a request it
    /// refused, so that the ledger covers what it was asked to do. The suspension
    /// gate lives in `check`, which runs before any upstream call.
    #[test]
    fn commit_records_refusals_but_check_is_the_gate() {
        let (m, _dir) = new_test_manager();
        m.set_enabled("alice", false).expect("suspend");

        let err = m
            .check(&users()[0], "gpt-4o")
            .expect_err("check must refuse a suspended key");
        assert!(err.to_string().contains("suspended"), "{err}");

        let st = m
            .commit("alice", 1, 0.0, "", dt(2026, 3, 4, 12, 0, 0))
            .expect("a refusal is still recorded");
        assert_eq!(st.consumed, 1);
        assert!(!st.enabled);
    }

    #[track_caller]
    fn almost(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }
}
