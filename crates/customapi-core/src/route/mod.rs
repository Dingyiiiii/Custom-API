//! Package route chooses which upstream channel serves a request.
//!
//! This is where the project's "自动择优路由" requirement is implemented: the
//! router combines the normalized price for the concrete token mix of *this*
//! request (including time- and volume-dependent promotions) with the capability
//! score measured by the intelligence layer, so that a cheap channel that has
//! been serving a degraded model is not preferred merely for being cheap.
//!
//! The module is a direct port of `internal/route`; the exported surface keeps
//! the Go names in snake_case so that the gateway and the intelligence layer
//! read the same way in both implementations.

// The Go package keeps this in `route.go`; the porting rules mirror that file
// layout, so the module name repeats its directory and the lint's advice is
// declined deliberately.
#[allow(clippy::module_inception)]
mod route;

pub use route::{
    format_g, format_g4, Candidate, Decision, Deviation, HealthTracker, MapScoreBook, Options,
    Request, RouteError, ScoreBook, Scores, Selector,
};

#[cfg(test)]
mod tests;
