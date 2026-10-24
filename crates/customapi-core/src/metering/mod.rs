//! The gateway's independent view of token usage.
//!
//! The customs gateway never trusts an upstream's reported usage. For every
//! exchange it computes its own estimate from the request and response bodies
//! and compares the two; a deviation beyond a configured tolerance is an audit
//! event (see [`Usage::compare`]) and feeds channel de-weighting.
//!
//! This module is a direct port of `internal/metering` in the Go
//! implementation: the estimator's calibration constants, the rounding, the
//! comparison rules and the JSON field names are identical, so a record written
//! by one implementation reads back the same in the other.
//!
//! # Estimator design
//!
//! See the `tokenizer` submodule for the reasoning behind the vocabulary-free
//! estimator.

mod counter;
mod tokenizer;

pub use counter::{
    clamp_finite, count_request, count_request_anthropic, count_request_responses, count_response,
    count_response_anthropic, count_response_responses, relative_delta, Counter, Deviation, Usage,
    MAX_FINITE_DELTA, MESSAGE_OVERHEAD, MIN_ABSOLUTE_DELTA,
};
pub use tokenizer::{
    count_words, family_for, ratios_for, sentence_piece_delimiter, words_per_token, Estimator,
    FamilyRatios, TokenizerFamily, CJK_RATIO, FAMILY_CL100K, FAMILY_CLAUDE, FAMILY_GEMINI,
    FAMILY_GENERIC, FAMILY_O200K, MIN_WORDS_FOR_CORRECTION,
};

#[cfg(test)]
mod tests;
