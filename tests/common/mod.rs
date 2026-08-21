//! Shared support code for the integration-test crates.
//!
//! Each file under `tests/` compiles as its **own crate**, so anything two of
//! them need has to be `mod common;`-included rather than imported. Before this
//! module existed the harnesses were copy-pasted: `unique_path` sat
//! byte-identical in `run.rs` and `costs.rs`, `serve` in `live_okx.rs` and
//! `live_coinbase.rs`, and six files each grew their own `flat_bar`. A fix to
//! one copy never reached the others. The two live suites went further and
//! mirrored each other's whole test bodies — see [`live`].
//!
//! Layout — one module per concern, so a test crate pays only for what its
//! feature set can actually build:
//!
//! | Module | Holds | Gate |
//! |---|---|---|
//! | [`bars`] | synthetic candles, atoms and snapshot streams | — |
//! | [`fixtures`] | `tests/data/` CSV loading + the skip-vs-fail policy | — |
//! | [`cli`] | driving the `fugazi` binary and reading its artefacts | `cli` |
//! | [`net`] | hosting a `wiremock` server for a blocking client | `sources` |
//! | [`live`] | the conformance suite every venue wallet must pass | `live` |
//!
//! Cargo compiles this file into every including crate, so a helper only one
//! test uses is dead code in the rest — hence the blanket allow. That is the
//! cost of the `mod common;` idiom, not an oversight.
#![allow(dead_code)]

pub mod bars;
pub mod fixtures;

// `cli` shells out to the binary, which only exists with the `cli` feature —
// without it `CARGO_BIN_EXE_fugazi` is undefined and the module wouldn't compile.
#[cfg(feature = "cli")]
pub mod cli;

// `net` needs the async stack (`tokio`), which arrives with `sources`.
#[cfg(feature = "sources")]
pub mod net;

// `live` drives the venue wallets, which only exist with the `live` feature.
#[cfg(feature = "live")]
pub mod live;
