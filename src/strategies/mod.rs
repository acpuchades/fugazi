//! A catalogue of **classical single-asset strategies**, each ready to trade
//! into a [`Wallet`](crate::Wallet).
//!
//! Almost every classical single-asset strategy has the same shape — a long /
//! flat / short position driven by a handful of boolean conditions, sized all-in
//! — so the catalogue factors that shape into one generic type,
//! [`SingleAssetStrategy`], and expresses each named strategy as a thin
//! specialisation that builds its particular entry/exit [`Signal`](crate::Signal)s.
//! (`SingleAssetStrategy` is itself just "the user's own type implementing the
//! trait", parameterised over its signals; a strategy that does not fit its
//! long/flat/short, all-in mould — like [`ZScoreReversion`](mean_reversion::ZScoreReversion)'s
//! bespoke sizing — still spells out its own [`Strategy`](crate::Strategy) impl.)
//!
//! Every strategy:
//!
//! * is generic over the symbol type `Sym: Clone` and takes `Input = Atom`;
//! * in [`update`](crate::Strategy::update) advances **all** of its
//!   signals/indicators every bar (never short-circuiting, or a skipped source
//!   desyncs from the price stream), then decides in [`trade`](crate::Strategy::trade);
//! * sizes positions all-in via [`Size::value_frac(1.0)`](crate::Size). Two
//!   flavours of position management appear:
//!   - **long/flat** — go all-in long on an entry edge, [`close`](crate::Wallet::close)
//!     on an exit edge ([`SingleAssetStrategy::long_on`]);
//!   - **long/short** (always-in) — flip with a single
//!     [`set`](crate::Wallet::set) to the other side ([`SingleAssetStrategy::long_on`] +
//!     [`short_on`](SingleAssetStrategy::short_on)).
//!     Because `value_frac` resolves against equity (which survives a reversal,
//!     unlike cash), one `set` reverses and re-sizes all-in exactly — no
//!     flatten-then-reopen.
//!
//! The families:
//!
//! * [`trend`] — crossover / breakout trend-following.
//! * [`mean_reversion`] — oscillator and band reversion.
//! * [`momentum`] — rate-of-change / oscillator-vs-midline.
//! * [`volume`] — volume- and flow-based.
//! * [`composite`] — multi-condition (trend gated by strength, dip-in-uptrend).

pub mod composite;
pub mod mean_reversion;
pub mod momentum;
pub mod single_asset;
pub mod trend;
pub mod volume;

pub use single_asset::SingleAssetStrategy;
