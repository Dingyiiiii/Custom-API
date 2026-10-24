//! Package meter turns token counts into money.
//!
//! It also owns the project's price *normalisation* concern: upstream pricing
//! arrives in several incompatible shapes (multiplier tables, flat per-million
//! rates, per-request charges, tiered expressions, subscription plans), and the
//! customs gateway must reduce all of them to a single comparable number - the
//! effective price for a given input/output token mix at a given time.
//!
//! The module is a direct port of `internal/meter` in the Go implementation; the
//! JSON field names, the rounding and the promotion arithmetic are identical so
//! that a quote produced here is comparable with one produced there.

mod price;

pub use price::{
    currency_rate, effective_unit_price, normalize, price_with, utc_offset, AppliedPromotion,
    MeterError, Pricer, Quote, RawPrice, Tier, Tokens,
};

#[cfg(test)]
mod tests;
