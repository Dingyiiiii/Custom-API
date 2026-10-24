//! Package discovery maintains the catalogue of models the gateway can serve.
//!
//! # Why it exists
//!
//! An operator who adds a provider by API key wants to know what that key can
//! actually reach. Asking them to hand-write every model name is both tedious and
//! error-prone: providers add and retire models continuously, and a stale list
//! silently makes models unreachable or routes to ones that no longer exist.
//!
//! The catalogue is therefore assembled from three sources, in decreasing order
//! of authority:
//!
//! 1. a provider's declared `models` and `aliases`, which is explicit operator
//!    intent and always wins;
//! 2. models discovered from the provider's own model-list endpoint;
//! 3. nothing else - a model is never invented.
//!
//! The catalogue is a *view*, not a routing input: routing still resolves through
//! the channel list. That separation matters because discovery can fail or be
//! stale without affecting live traffic, and a provider outage must never remove
//! a model from service.
//!
//! Ported from `internal/discovery`; the wire-visible shapes (the cache file and
//! the admin API) keep the Go JSON field names.

// The Go package keeps this in `discovery.go`; the porting rules mirror that
// file layout, so the module name repeats its directory and the lint's advice is
// declined deliberately.
#[allow(clippy::module_inception)]
mod discovery;
mod io;

pub use self::discovery::{
    ChannelModels, DiscoveryError, ModelChannel, ModelEntry, ModelView, Options, Service, Snapshot,
    SOURCE_CONFIGURED, SOURCE_DISCOVERED,
};

#[cfg(test)]
mod tests;
