//! The customs gateway's append-only, tamper-evident audit ledger.
//!
//! The project plan requires that accounting, quota deduction and audit trails
//! are never skipped, by any subject, under any trust level, and that audit
//! evidence lives in a store separate from the business database. This module
//! provides that store.
//!
//! # Tamper evidence
//!
//! Every record is written as one JSON object per line with three chain fields:
//!
//! ```text
//! seq         monotonic sequence number starting at 1
//! prev_hash   entry_hash of the previous record ("genesis" for seq 1)
//! entry_hash  SHA-256 over the canonical JSON of the record with entry_hash
//!             removed
//! ```
//!
//! Because `entry_hash` covers `prev_hash`, altering, reordering, inserting or
//! deleting any record breaks verification from that point onward. An attacker
//! who rewrites the whole chain can still be caught by the periodic anchors
//! written to a separate file (and, in production, shipped off-host).
//!
//! # Module layout
//!
//! | Go file                            | Rust module |
//! |------------------------------------|-------------|
//! | `ledger.go`                        | [`ledger`]  |
//! | `rehash.go`                        | [`rehash`]  |
//! | `owner_unix.go` / `owner_other.go` | `#[cfg(unix)]` helpers next to their callers |

pub mod ledger;
pub mod rehash;

#[cfg(test)]
pub(crate) mod testutil;

pub use ledger::*;
pub use rehash::{rehash, RehashResult};
