//! # Fugazi
//!
//! A library of technical-analysis (TA) building blocks designed around
//! *incremental* computation. Every primitive owns its internal state and is
//! advanced one sample at a time through `update()`, carrying just enough
//! intermediate state to produce the next output in O(1) (or close to it).
//! This makes the same code usable for live streaming and batch backtesting.
//!
//! The crate has three composable layers:
//!
//! * [`Indicator`] — the numeric *sources*. Each incrementally produces a
//!   [`Real`] and **owns its own input source**, so composition is just nesting
//!   constructors: `Ema::new(Current::close(), 20)` is the EMA-20 of the close,
//!   `Ema::new(Sma::new(src, 10), 20)` an EMA of an SMA. Outputs are exposed as
//!   public fields refreshed every [`Indicator::update`]; a single-output
//!   indicator exposes a field named `value`. Leaf sources ([`Value`] for a
//!   constant, [`Identity`] for the raw input, `Current::*` for candle fields)
//!   terminate the chain. Bar indicators ([`Atr`](crate::indicators::Atr),
//!   [`Adx`](crate::indicators::Adx)) consume a [`Candle`] directly.
//! * [`Signal`] — incremental, *composable* booleans, which are simply
//!   [`Indicator`]s whose `Output` is `bool`. Comparison signals are built from
//!   two sources, so a condition like "RSI over 70" is a single object; combine
//!   them further with the [`BoolIndicatorExt`] combinators (`and`/`or`/`xor`/`not`/`changed`).
//!   `Signal` itself names a `bool` indicator fed a [`Candle`] — what a strategy
//!   stores behind `Box<dyn Signal>`.
//! * [`Strategy`] — the *decision* layer. Unlike the pure layers below it, a
//!   strategy *acts*, in two steps: [`update`](Strategy::update) advances its
//!   signals (touching only itself), then [`trade`](Strategy::trade) reads that
//!   state and sets or closes positions on a [`Wallet`] handed to it
//!   (`wallet.set`/`close`, with a [`Side`] and a [`Size`] that is absolute or a
//!   fraction of funds/equity/position). The wallet is priced from outside via
//!   [`Wallet::update`] and returns unit-tagged [`Reference`] / [`Units`]
//!   amounts. [`Wallet`] is a *trait*, so the same strategy runs against a
//!   [`PaperWallet`] backtest or a live broker wallet unchanged; the wallet owns
//!   the portfolio (funds, positions, blotter). Acting on several symbols per bar
//!   makes it serve single- and multi-asset strategies alike — direction,
//!   sizing, and short-selling are all just what the strategy's code does.
//!
//! ```
//! use fugazi::prelude::*;
//! use fugazi::indicators::{Identity, Rsi};
//!
//! // "RSI(14) over 70" as a single bool-valued indicator over a raw price
//! // stream. Indicators own their source, so `Identity` feeds it into the RSI.
//! let mut overbought = Rsi::new(Identity::new(), 14).above(70.0);
//! for price in [44.0, 44.3, 44.1, 43.6, 44.3, 44.8, 45.1, 45.6] {
//!     overbought.update(price);
//! }
//! let _ = overbought.is_true();
//! ```
//!
//! [`Value`]: crate::indicators::Value
//! [`Identity`]: crate::indicators::Identity

// Compile-test the code examples in README.md as doctests, without injecting
// the README's prose into the rendered crate documentation.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

pub mod backtest;
pub mod costs;
pub mod indicator;
pub mod indicators;
pub mod market;
pub mod metrics;
pub mod portfolio;
#[cfg(feature = "runtime")]
pub mod runtime;
pub mod signal;
pub mod snapshot;
#[cfg(feature = "sources")]
pub mod sources;
#[cfg(feature = "spec")]
pub mod spec;
pub mod strategies;
pub mod strategy;
pub mod time;
pub mod types;
pub mod wallet;

pub use backtest::{Fill, Rejected, RunReport};
pub use costs::{CommissionModel, SlippageModel, SpreadModel, TradingCosts};
pub use indicator::Indicator;
pub use indicators::BoolIndicatorExt;
pub use market::{Atom, Candle, OverlayInfo, OverlayType, OverlayValue, Real, Schema, SchemaBuilder};
pub use metrics::{DrawdownSegment, Trade};
pub use portfolio::{Portfolio, PortfolioBuilder, PortfolioWallet};
pub use signal::Signal;
pub use snapshot::{Selector, Snapshot};
pub use strategy::Strategy;
pub use time::{Frequency, Timestamp};
pub use wallet::{
    Ack, Order, OrderId, OrderKind, PaperWallet, Reference, Rejection, Side, Size, Units, Wallet, WalletError,
};

/// Convenient glob-import of the core traits and types.
pub mod prelude {
    pub use crate::costs::TradingCosts;
    pub use crate::indicator::Indicator;
    pub use crate::indicators::{BoolIndicatorExt, IndicatorExt};
    pub use crate::market::{
        Atom, Candle, OverlayInfo, OverlayType, OverlayValue, Real, Schema, SchemaBuilder,
    };
    pub use crate::signal::Signal;
    pub use crate::snapshot::{Selector, Snapshot};
    pub use crate::strategy::Strategy;
    pub use crate::time::{Frequency, Timestamp};
    pub use crate::wallet::{
        Ack, Order, OrderId, OrderKind, PaperWallet, Reference, Rejection, Side, Size, Units, Wallet,
        WalletError,
    };
}
