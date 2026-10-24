//! Catalogue: the merged price + source view the intelligence layer maintains
//! and writes to `catalogue.json`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::RwLock;

use super::{write_file_atomic, IntelError};
use crate::config::{self, zero_time};

/// Separator between channel and model in the catalogue key.
///
/// Go uses `"\x00"` because neither a channel nor a model name may contain it,
/// so the composite key can be split unambiguously.
const KEY_SEPARATOR: char = '\u{0}';

/// The merged price + score view the intelligence layer maintains.
///
/// Go keeps a `sync.RWMutex` beside the three maps; Rust cannot hold a guard
/// across the JSON serialisation without a wrapper, so the state lives behind
/// one `RwLock` and the type is `Serialize` by locking on demand. The JSON shape
/// is still `{"prices":…,"updated_at":…,"sources":…}`.
pub struct Catalogue {
    inner: RwLock<CatalogueState>,
}

/// The serialised half of [`Catalogue`].
#[derive(Debug, Clone, Serialize)]
struct CatalogueState {
    /// Maps `"channel\0model"` to a normalized price.
    prices: BTreeMap<String, config::PriceModel>,
    /// Last refresh time.
    updated_at: DateTime<Utc>,
    /// Records where each price came from.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    sources: BTreeMap<String, String>,
}

/// The on-disk shape read by [`Catalogue::load`].
///
/// Every field is optional because Go's `Load` copies a map only when it is
/// non-nil: a file that names just `prices` leaves the recorded sources alone,
/// and a `null` map keeps whatever is in memory. `updated_at` is copied
/// unconditionally, so a file without it resets the timestamp to the zero time,
/// exactly as the Go assignment `c.UpdatedAt = loaded.UpdatedAt` does.
#[derive(Debug, Default, Deserialize)]
struct CataloguePatch {
    #[serde(default)]
    prices: Option<BTreeMap<String, config::PriceModel>>,
    #[serde(default)]
    updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    sources: Option<BTreeMap<String, String>>,
}

impl Default for CatalogueState {
    fn default() -> Self {
        CatalogueState {
            prices: BTreeMap::new(),
            updated_at: zero_time(),
            sources: BTreeMap::new(),
        }
    }
}

impl Default for Catalogue {
    fn default() -> Self {
        Catalogue::new()
    }
}

impl Serialize for Catalogue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.read_state().serialize(serializer)
    }
}

impl Catalogue {
    /// Creates an empty catalogue.
    pub fn new() -> Catalogue {
        Catalogue {
            inner: RwLock::new(CatalogueState::default()),
        }
    }

    /// Reads the state, recovering from a poisoned lock.
    ///
    /// Go's `sync.RWMutex` has no poisoning: a panic while holding the lock
    /// leaves the data intact and the next caller proceeds. Poisoning would be a
    /// new failure mode, so the guard is taken either way.
    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, CatalogueState> {
        self.inner.read().unwrap_or_else(|err| err.into_inner())
    }

    /// Writes the state, recovering from a poisoned lock.
    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, CatalogueState> {
        self.inner.write().unwrap_or_else(|err| err.into_inner())
    }

    /// Folds collected prices into the catalogue.
    pub fn merge(&self, entries: &[config::PriceEntry]) {
        let mut state = self.write_state();
        for entry in entries {
            let key = format!("{}{}{}", entry.channel, KEY_SEPARATOR, entry.model);
            state.prices.insert(key.clone(), entry.price.clone());
            state.sources.insert(key, entry.price.source.clone());
        }
        state.updated_at = Utc::now();
    }

    /// Renders the catalogue as configuration entries, sorted by model then
    /// channel.
    pub fn entries(&self) -> Vec<config::PriceEntry> {
        let state = self.read_state();
        let mut out = Vec::with_capacity(state.prices.len());
        for (key, price) in &state.prices {
            let Some((channel, model)) = key.split_once(KEY_SEPARATOR) else {
                continue;
            };
            out.push(config::PriceEntry {
                channel: channel.to_string(),
                model: model.to_string(),
                price: price.clone(),
            });
        }
        out.sort_by(|a, b| (&a.model, &a.channel).cmp(&(&b.model, &b.channel)));
        out
    }

    /// Reads a catalogue from disk.
    ///
    /// A missing file leaves the in-memory catalogue untouched and is not an
    /// error: the first collection run creates it.
    pub fn load(&self, path: &str) -> Result<(), IntelError> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(IntelError::Io(err)),
        };
        // Go decodes into a fresh value and then copies the non-nil maps over,
        // so a field missing from the file leaves the current state alone.
        let patch: CataloguePatch = serde_json::from_slice(&bytes)?;
        let mut state = self.write_state();
        if let Some(prices) = patch.prices {
            state.prices = prices;
        }
        if let Some(sources) = patch.sources {
            state.sources = sources;
        }
        state.updated_at = patch.updated_at.unwrap_or_else(zero_time);
        Ok(())
    }

    /// Writes the catalogue to disk.
    ///
    /// Unlike [`Catalogue::merge`] this does not stamp `updated_at`: the field
    /// records when prices were observed, not when the file was written.
    pub fn save(&self, path: &str) -> Result<(), IntelError> {
        let state = self.read_state();
        let bytes = serde_json::to_vec_pretty(&*state)?;
        drop(state);
        write_file_atomic(path, &bytes)
    }
}

impl std::fmt::Debug for Catalogue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Catalogue")
            .field("prices", &self.read_state().prices.len())
            .finish()
    }
}
