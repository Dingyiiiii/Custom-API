//! The operator commands.
//!
//! The split mirrors `cmd/customsctl/main.go`: offline commands (config, audit,
//! prices) read files directly, while the user commands talk to the gateway's
//! admin API. Output text is part of the interface — the deploy scripts parse
//! it — so each command reproduces the Go format strings exactly.

pub mod audit;
pub mod config;
pub mod prices;
pub mod users;

/// Prints text produced by [`crate::ui::TabWriter`] without adding a newline.
pub fn emit(text: &str) {
    print!("{text}");
}
