//! YAML-deserializable [`NodeSpec`] — the one composable expression layer.
//!
//! Every YAML tag is a variant of this enum, whatever it produces: numeric
//! sources (`!close`/`!ema`/`!current`/`!pick`/`!time`/`!get`), the boolean
//! predicates (`!gt`/`!and`/`!crosses_above`/`!changed`/`!is_weekday`), and
//! the string comparisons. A "signal" is not a separate type — it is just a
//! `NodeSpec` whose `output_type()` is `Bool`. A `SideSpec::stop_loss` slot
//! wants a Real-producing node; a `SideSpec::enter` slot wants a
//! Bool-producing one; both are the same `NodeSpec`.
//!
//! Split out of `spec/mod.rs`; the module lives at `crate::spec::expr` and
//! the type is re-exported at `crate::spec::NodeSpec` via `mod.rs`.

use std::sync::Arc;

use fugazi_derive::SpecGrammar;
use serde::Deserialize;

// Field / calendar / current-bar / current-time leaves are referenced through
// their full `crate::indicators::` paths inside the `NodeSpec::build`
// match arms — the source-spec variants share those names (Close, High, Year,
// …) as enum-variant identifiers, so a bare `Close::of(...)` would try to
// resolve on the enum variant. The `Pick` root is the one exception because
// it isn't a `NodeSpec` variant.
use crate::indicators::{
    Ad, Adx, AdxValue, Aroon, AroonValue, Atr, BarsSince, BarsSinceHigh, BarsSinceLow, Bollinger,
    BollingerValue, Book, Cci, Component, Correlation, Dmi, DmiValue, Donchian, DonchianValue, Ema,
    GarmanKlass, GetBool, GetReal, GetStr, Hma, IfElse, Keltner, KeltnerValue, Kurtosis, Latch, Log,
    Macd, MacdValue, Match as MatchIndicator, Mfi, Obv, Parkinson, Percentile, PercentileRank, Pick,
    PickAny, Position, Resample, Rma, RogersSatchell, Rsi, Sar, Skewness, Sma, StdDev, StochRsi,
    Stochastic, TrueRange, Value, ValueStr, VarianceRatio, Vwap, WilliamsR, Wma, ZScore,
};
use crate::prelude::*;
use crate::types::Snapshot;

use super::trailing::{self, AnyStrategyRef, TrailingMetric};
use crate::indicators::compare;
use crate::spec::dyn_indicator::{self, AsAtom, AsBool, AsCandle, AsReal, AsStr, DynIndicator, DynType};

use crate::{Frequency, Selector};
use std::str::FromStr;

/// The implicit atom root of every `source:`-omitted leaf.
///
/// `root` is the **blessed series** of the context doing the build — the
/// declared `symbol:` of a [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy),
/// the leg of a basket / multi-asset factory, the `(symbol, freq)` key of an
/// overlay column. When one is supplied, a bare `!close` resolves *by name*
/// out of the snapshot ([`Pick::rooted`]), which is what lets it coexist with
/// a `!close { source: !pick { symbol: SPY } }` reaching across the same bar.
///
/// A context with no blessed series (a pair — two legs, neither privileged; a
/// portfolio's `weights:`; a snapshot-level `rebalance_on:` gate) passes
/// `None` and gets the empty-selector [`Pick::new`], whose sole-atom unpack
/// panics on a multi-symbol snapshot rather than guessing. Those specs name
/// their asset explicitly with `!pick { symbol: ... }`.
pub(super) fn pick_root(root: Option<&Selector<String>>) -> Pick<String> {
    match root {
        Some(selector) => Pick::<String>::rooted(selector.clone()),
        None => Pick::<String>::new(),
    }
}

/// Symbol-agnostic atom root for calendar accessors (`!year`, `!month`,
/// `!day`, `!hour`, `!minute`, `!second`, `!day_of_week`, `!day_of_year`,
/// `!week_of_year`, `!quarter`, `!unix_seconds`, `!unix_millis`, `!time`)
/// and the wall-clock cadence sugar (`!hourly`, `!daily`, `!weekly`,
/// `!monthly`, `!quarterly`, `!annually`, which desugar into
/// `!changed { source: !<accessor> }`). Every calendar accessor only reads
/// [`Atom::time`], which every entry in a well-formed snapshot shares, so
/// picking the first entry is a stable answer even when the snapshot
/// carries multiple symbols — as in a
/// [`MultiAssetStrategy`](crate::strategies::MultiAssetStrategy),
/// [`BasketStrategy`](crate::strategies::BasketStrategy), or a
/// [`Portfolio`](crate::portfolio::Portfolio) `rebalance_on:` gate.
/// Contrast with [`pick_root`], which panics on a 2+ entry snapshot
/// because price-field leaves (`!close`, `!high`, …) genuinely depend on
/// *which* asset.
///
/// Deliberately takes **no** `root`: re-rooting a calendar leaf onto one
/// symbol would make it read `None` on a bar where that symbol happens to be
/// absent, when the answer it wants — the bar's time — is right there on every
/// other entry.
pub(super) fn pick_any_root() -> PickAny<String> {
    PickAny::<String>::new()
}

pub(super) fn default_source() -> Box<NodeSpec> {
    // The default price source for wrapped indicators (`!ema {}`, `!sma {}`, …)
    // is the bar's `close`. Corporate-action adjustment is a data-sourcing
    // concern handled at ingestion (the `yfinance` provider adjusts its candles
    // at fetch), so `close` is authoritative here — the strategy layer carries
    // no adjustment vocabulary of its own.
    Box::new(NodeSpec::Close { source: None })
}
pub(super) fn default_high() -> Box<NodeSpec> {
    Box::new(NodeSpec::High { source: None })
}
pub(super) fn default_low() -> Box<NodeSpec> {
    Box::new(NodeSpec::Low { source: None })
}
/// Default candle source for bar indicators — the current bar itself.
pub(super) fn default_bar_source() -> Box<NodeSpec> {
    Box::new(NodeSpec::Current { source: None })
}

/// Default base for `!log`: natural log (`e`).
pub(super) fn default_log_base() -> Real {
    std::f64::consts::E
}

/// Default annualized risk-free rate for `!sharpe` / `!sortino`: `0.0`.
pub(super) fn default_risk_free_rate() -> Real {
    0.0
}

// Canonical parameter defaults for the parametric / multi-output indicators.
// These long lived only in the Python constructor signatures; they now live
// here as the **single source** — the `#[serde(default = "…")]` fns below (so
// the YAML may omit them too), the grammar descriptor (which reads the same
// fns), and the pyo3 signatures (which reference these consts directly) all
// agree by construction. See `python/src/constructors.rs`.
/// MACD fast EMA period.
pub const MACD_FAST: usize = 12;
/// MACD slow EMA period.
pub const MACD_SLOW: usize = 26;
/// MACD signal EMA period.
pub const MACD_SIGNAL: usize = 9;
/// Bollinger-band lookback.
pub const BB_PERIOD: usize = 20;
/// Bollinger-band width in standard deviations.
pub const BB_K: Real = 2.0;
/// Keltner middle-band EMA period.
pub const KELTNER_EMA_PERIOD: usize = 20;
/// Keltner channel ATR period.
pub const KELTNER_ATR_PERIOD: usize = 10;
/// Keltner channel width multiplier.
pub const KELTNER_MULTIPLIER: Real = 2.0;
/// Parabolic-SAR acceleration step.
pub const SAR_STEP: Real = 0.02;
/// Parabolic-SAR acceleration cap.
pub const SAR_MAX: Real = 0.2;
/// Stochastic-RSI inner RSI period.
pub const STOCH_RSI_RSI_PERIOD: usize = 14;
/// Stochastic-RSI outer stochastic period.
pub const STOCH_RSI_STOCH_PERIOD: usize = 14;

fn macd_fast_default() -> usize {
    MACD_FAST
}
fn macd_slow_default() -> usize {
    MACD_SLOW
}
fn macd_signal_default() -> usize {
    MACD_SIGNAL
}
fn bb_period_default() -> usize {
    BB_PERIOD
}
fn bb_k_default() -> Real {
    BB_K
}
fn keltner_ema_period_default() -> usize {
    KELTNER_EMA_PERIOD
}
fn keltner_atr_period_default() -> usize {
    KELTNER_ATR_PERIOD
}
fn keltner_multiplier_default() -> Real {
    KELTNER_MULTIPLIER
}
fn sar_step_default() -> Real {
    SAR_STEP
}
fn sar_max_default() -> Real {
    SAR_MAX
}
fn stoch_rsi_rsi_period_default() -> usize {
    STOCH_RSI_RSI_PERIOD
}
fn stoch_rsi_stoch_period_default() -> usize {
    STOCH_RSI_STOCH_PERIOD
}

/// The right-hand operand of `!str_eq` / `!str_ne`.
///
/// A bare YAML string is the literal to match (`rhs: bull`) — the common case.
/// Anything else deserializes as a [`NodeSpec`], so both sides of the
/// comparison are symmetric: the same constant written the long way
/// (`rhs: !value bull`) or a second `Str` column read (`rhs: !get { key: prev }`)
/// both build to a `Str`-output source.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "serde_norway::Value")]
pub enum StrOperand {
    Literal(String),
    Expr(Box<NodeSpec>),
}

impl TryFrom<serde_norway::Value> for StrOperand {
    type Error = String;

    fn try_from(v: serde_norway::Value) -> Result<Self, Self::Error> {
        match v {
            serde_norway::Value::String(s) => Ok(StrOperand::Literal(s)),
            other => NodeSpec::try_from(other).map(|e| StrOperand::Expr(Box::new(e))),
        }
    }
}

impl StrOperand {
    /// Build as a `Str`-output source. A literal materialises the same
    /// [`ValueStr`] constant the `!value <string>` expression form builds.
    pub(super) fn try_build(
        &self,
        anchor: &Position,
        book: &Book,
        portfolio_book: Option<&Book>,
        schema: &Arc<Schema>,
        root: Option<&Selector<String>>,
    ) -> Result<Box<dyn DynIndicator>, String> {
        match self {
            StrOperand::Literal(s) => Ok(dyn_indicator::wrap(
                ValueStr::<crate::types::Snapshot<String>>::new(s.as_str()),
            )),
            StrOperand::Expr(e) => e.try_build(anchor, book, portfolio_book, schema, root),
        }
    }
}

/// Fail unless `node`'s statically-known output type is `want`. An
/// undecidable output (`None` — a `!get`, a hole, a passthrough over one) is
/// **skipped**, exactly as [`crate::spec::typecheck::check_immediate`] skips
/// it: those defer to the build-time `AsReal` / `AsBool` view. The message
/// names the offending tag, the same convention the breadcrumb uses.
fn expect_output(node: &NodeSpec, want: DynType) -> Result<(), String> {
    if let Some(got) = crate::spec::typecheck::output_type(node)
        && got != want
    {
        return Err(format!(
            "{} produces {got}, but a {want}-valued expression is required here",
            crate::spec::typecheck::tag_name(node),
        ));
    }
    Ok(())
}

/// A [`NodeSpec`] slot constrained to a `Real` output at *parse* time.
///
/// The strategy specs use this at their genuine slot boundaries (a
/// `stop_loss:` / `sizing:` / portfolio `weights:` wants a number), so a
/// decidably-wrong node — `stop_loss: !gt { … }` (Bool) — is rejected when the
/// document is read, naming the tag, instead of at build time inside an
/// `AsReal` view. An *undecidable* node (a `!get`, a `!param` hole) still
/// passes here and is checked at build, the same skip rule the type checker
/// uses. **Internal `NodeSpec` fields are never this** — a `!close`'s `source:`
/// must accept a `!pick` (`Atom`); the newtypes live only at the strategy
/// struct's slots.
#[derive(Debug, Clone)]
pub struct RealNode(pub NodeSpec);

/// A [`NodeSpec`] slot constrained to a `Bool` output at *parse* time — the
/// twin of [`RealNode`] for signal slots (`enter:` / `exit:` /
/// `rebalance_on:`). Same undecidable-skip rule.
#[derive(Debug, Clone)]
pub struct BoolNode(pub NodeSpec);

impl<'de> serde::Deserialize<'de> for RealNode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_norway::Value::deserialize(d)?;
        RealNode::try_from(v).map_err(serde::de::Error::custom)
    }
}
impl<'de> serde::Deserialize<'de> for BoolNode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_norway::Value::deserialize(d)?;
        BoolNode::try_from(v).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<serde_norway::Value> for RealNode {
    type Error = String;
    fn try_from(v: serde_norway::Value) -> Result<Self, Self::Error> {
        let node = NodeSpec::try_from(v)?;
        expect_output(&node, DynType::Real)?;
        Ok(RealNode(node))
    }
}
impl TryFrom<serde_norway::Value> for BoolNode {
    type Error = String;
    fn try_from(v: serde_norway::Value) -> Result<Self, Self::Error> {
        let node = NodeSpec::try_from(v)?;
        expect_output(&node, DynType::Bool)?;
        Ok(BoolNode(node))
    }
}

impl RealNode {
    /// The inner node, for probes / typecheck / re-templating.
    pub fn node(&self) -> &NodeSpec {
        &self.0
    }
    pub fn build(
        &self,
        anchor: &Position,
        book: &Book,
        portfolio_book: Option<&Book>,
        schema: &Arc<Schema>,
        root: Option<&Selector<String>>,
    ) -> Box<dyn DynIndicator> {
        self.0.build(anchor, book, portfolio_book, schema, root)
    }
    pub fn try_build(
        &self,
        anchor: &Position,
        book: &Book,
        portfolio_book: Option<&Book>,
        schema: &Arc<Schema>,
        root: Option<&Selector<String>>,
    ) -> Result<Box<dyn DynIndicator>, String> {
        self.0.try_build(anchor, book, portfolio_book, schema, root)
    }
}
impl BoolNode {
    pub fn node(&self) -> &NodeSpec {
        &self.0
    }
    pub fn build(
        &self,
        anchor: &Position,
        book: &Book,
        portfolio_book: Option<&Book>,
        schema: &Arc<Schema>,
        root: Option<&Selector<String>>,
    ) -> Box<dyn DynIndicator> {
        self.0.build(anchor, book, portfolio_book, schema, root)
    }
    pub fn try_build(
        &self,
        anchor: &Position,
        book: &Book,
        portfolio_book: Option<&Book>,
        schema: &Arc<Schema>,
        root: Option<&Selector<String>>,
    ) -> Result<Box<dyn DynIndicator>, String> {
        self.0.try_build(anchor, book, portfolio_book, schema, root)
    }
}

/// The payload of [`NodeSpec::Value`] — a constant leaf: numeric, string,
/// or (in per-child weight-share context) a list-indexed constant.
///
/// A YAML number builds a [`Value`] (`Real` output, the operand of every
/// arithmetic op and comparison); a YAML string builds a
/// [`ValueStr`] (`Arc<str>` output, the operand of `!str_eq` / `!str_ne`
/// against a `Str` overlay column read by `!get`); a YAML list of numbers
/// (`[w0, w1, w2, ...]`) is a per-child indexed constant — meaningful only
/// inside a portfolio weight-share template, where the SpecTemplate's
/// per-child build pass rewrites the list to its `CHILD_INDEX`th element
/// before typed parse:
///
/// ```yaml
/// !gt      { lhs: !rsi { period: 14 }, rhs: !value 70 }        # Real
/// !str_ne  { lhs: !get { key: regime }, rhs: !value bear }     # Str
/// weights: !value [0.4, 0.6]                                    # List (fixed per-child)
/// ```
///
/// Quoting decides the type when the two scalar forms would collide:
/// `!value 70` is the number, `!value "70"` the string. Deserializes
/// through a [`serde_norway::Value`] bridge (rather than
/// `#[serde(untagged)]`) so a wrong-typed literal reports what `!value`
/// accepts instead of the "did not match any variant" untagged error.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "serde_norway::Value")]
pub enum ValueLit {
    Real(Real),
    /// A constant boolean — the merged home for what a bool slot means by
    /// `!value true` / `!value false` (and a bare `true`/`false`). Builds a
    /// `ValueBool` (a `Bool`-output leaf).
    Bool(bool),
    Str(String),
    /// A per-child indexed constant — only meaningful inside a portfolio
    /// weight-share template. `SpecTemplate::build` rewrites this to
    /// `ValueLit::Real(list[CHILD_INDEX])` when `CHILD_INDEX` is present
    /// in the build args; if it isn't, [`NodeSpec::build`] panics because
    /// a list literal has no defined output outside per-child context.
    List(Vec<Real>),
}

impl TryFrom<serde_norway::Value> for ValueLit {
    type Error = String;

    fn try_from(v: serde_norway::Value) -> Result<Self, Self::Error> {
        match v {
            serde_norway::Value::Number(n) => n
                .as_f64()
                .map(ValueLit::Real)
                .ok_or_else(|| format!("!value: {n} is not a finite number")),
            serde_norway::Value::Bool(b) => Ok(ValueLit::Bool(b)),
            serde_norway::Value::String(s) => Ok(ValueLit::Str(s)),
            serde_norway::Value::Sequence(seq) => {
                let mut out = Vec::with_capacity(seq.len());
                for (i, item) in seq.into_iter().enumerate() {
                    let n = match item {
                        serde_norway::Value::Number(n) => n,
                        other => return Err(format!(
                            "!value list element {i}: expected number, got {other:?}"
                        )),
                    };
                    let f = n.as_f64().ok_or_else(|| {
                        format!("!value list element {i}: {n} is not a finite number")
                    })?;
                    out.push(f);
                }
                Ok(ValueLit::List(out))
            }
            other => Err(format!(
                "!value takes a number (a constant scalar), a bool (a constant \
                 signal), a string (a constant string, for !str_eq / !str_ne), \
                 or a list of numbers (a per-child weight vector), got {other:?}"
            )),
        }
    }
}

/// One case in a `!match` dispatch: the pattern to compare `on` against
/// and the branch to emit on a hit. `when:` is a scalar — either a
/// number (for numeric dispatch, `on` produces `Real`) or a string (for
/// string dispatch, `on` produces `Str`); the two forms can't be mixed
/// within one `!match` (build-time error). `value:` is the NodeSpec
/// branch to emit — the "when X, value is Y" pairing.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchCase {
    /// The pattern this case fires on. Reuses [`ValueLit`]'s scalar
    /// variants (`Real` and `Str`) — the `List` variant is not a valid
    /// pattern (it has no defined equality against `on`'s reading) and
    /// is rejected at build.
    pub when: ValueLit,
    /// The branch to emit when the pattern matches. Advanced every bar
    /// regardless of match, per the same "keep warming up" convention as
    /// `!if_else`'s non-selected branch. Named `value:` for the
    /// "when X, value is Y" reading — a full [`NodeSpec`] (not just a
    /// literal; wrap constants in `!value` as elsewhere).
    pub value: Box<NodeSpec>,
}

// ---------------------------------------------------------------------------
// Real-valued sources
// ---------------------------------------------------------------------------

/// A real-valued source over a candle stream — the YAML form of any
/// `Indicator<Input = Candle, Output = Real>`.
///
/// Every atom-input leaf (`!close`, `!high`, …, all calendar accessors, and
/// `!get`) carries a **defaulted optional `source: Option<Box<NodeSpec>>`**
/// field. When omitted, the leaf reads its atom from the implicit
/// empty-selector [`Pick::<String>::new()`] — the single-entry snapshot
/// unpack that keeps single-series strategies working. When provided
/// (typically a `!pick { symbol, freq }`), the leaf reads from that
/// atom-emitting subtree, which is how cross-asset composition is spelled:
///
/// ```yaml
/// # BTC vs ETH close spread:
/// !sub
///   lhs: !close { source: !pick { symbol: BTC } }
///   rhs: !close { source: !pick { symbol: ETH } }
/// ```
///
/// Three input forms all deserialize to the same variant:
/// - A bare word — `close`
/// - A bare YAML tag — `!close`
/// - A tagged map — `!close { source: !pick { symbol: BTC } }`
///
/// The bare-word / bare-tag forms use the implicit `Pick` root; the tagged
/// map form threads the given atom source through the leaf. The custom
/// [`TryFrom<serde_norway::Value>`] impl below normalises the string and
/// tagged shapes into the map shape `NodeSpecRaw` expects, and
/// `NodeSpecRaw` carries the derived externally-tagged deserialization
/// logic.
#[derive(Debug, Clone, Deserialize, SpecGrammar)]
#[serde(try_from = "serde_norway::Value")]
#[grammar(group = "node")]
pub enum NodeSpec {
    // --- atom-input leaves (candle fields) ---
    #[grammar(kind = "source")]
    Close {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    #[grammar(kind = "source")]
    High {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    #[grammar(kind = "source")]
    Low {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    #[grammar(kind = "source")]
    Open {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    #[grammar(kind = "source")]
    Volume {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    #[grammar(kind = "source")]
    Typical {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    #[grammar(kind = "source")]
    Median {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The current bar itself — the whole [`Candle`], not a scalar. The default
    /// bar source of every bar-consuming indicator (`!atr`, `!obv`, `!adx`, …);
    /// wrap in [`NodeSpec::Resample`] to lift a downstream bar indicator
    /// onto a higher timeframe.
    #[grammar(kind = "source", output = "candle")]
    Current {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },

    /// Cross-asset projection: project one asset's [`Atom`] out of the
    /// snapshot the CLI feeds each bar. Both fields are optional — an empty
    /// `!pick {}` behaves identically to the implicit single-entry unpack
    /// every atom-input leaf uses by default. Compose with any atom-input
    /// leaf via `source: !pick { symbol, freq }`.
    ///
    /// `freq` accepts the same `N<unit>` alphabet as `--frequency`
    /// (`1m` / `4h` / `1d` / `1w` / `1M`), so a cross-frequency snapshot
    /// disambiguates via `!pick { symbol: BTC, freq: 1h }`.
    #[grammar(kind = "source", output = "atom")]
    Pick {
        #[serde(default)]
        symbol: Option<String>,
        #[serde(default)]
        freq: Option<String>,
    },

    /// A constant value — a number (`!value 70`, a `Real` source) or a string
    /// (`!value bull`, a `Str` source for `!str_eq` / `!str_ne`). See
    /// [`ValueLit`].
    #[grammar(kind = "source", output = "any")]
    Value(ValueLit),

    /// The current position's entry price — a [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy) anchor,
    /// for building stop-loss / take-profit levels.
    #[grammar(kind = "source")]
    Entry,
    /// The running high since entry (a long trailing-stop anchor).
    #[grammar(kind = "source")]
    Peak,
    /// The running low since entry (a short trailing-stop anchor).
    #[grammar(kind = "source")]
    Trough,

    // --- book source-selectors. These are build-time only — they carry no
    // runtime value; they resolve, at build, to a `Book<String>` handle that
    // a book-reading node (a bare book leaf like `!drawdown`, or a
    // book-anchored recipe like `!drawdown_throttle`) picks up via its
    // `source:` field. Bare (used as an expression on its own) is invalid
    // and panics at build.
    /// The **strategy book** — the `Book` owned by the enclosing strategy
    /// scope (single/pairs/basket/multi/the current per-child instance of
    /// a portfolio's `weights:` expression). This is the default source of
    /// every book-reading node when its `source:` is omitted.
    #[grammar(kind = "source", output = "book")]
    StrategyBook,
    /// The **portfolio aggregate book** — the mark-to-market view of the
    /// composite [`Portfolio`](crate::portfolio::Portfolio). Only meaningful
    /// inside a portfolio's `weights:` expression; panics at build if
    /// referenced elsewhere.
    #[grammar(kind = "source", output = "book")]
    PortfolioBook,

    // --- book-anchored leaves. Each takes an optional `source:` that
    // resolves to the book they read (see [`NodeSpec::StrategyBook`] /
    // [`NodeSpec::PortfolioBook`]). Omitted → `!strategy_book`.
    /// The marked-to-market equity of the book. Always `Some`
    /// (seeded at the book's `initial_equity`). See
    /// [`crate::indicators::Book::equity`].
    #[grammar(kind = "source")]
    Equity {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The running peak of the book's equity. Always `Some`.
    /// See [`crate::indicators::Book::equity_peak`].
    #[grammar(kind = "source")]
    EquityPeak {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The book's current drawdown as a non-positive fraction —
    /// `(equity - peak) / peak`, `0` at a fresh peak. See
    /// [`crate::indicators::Book::drawdown`].
    #[grammar(kind = "source")]
    Drawdown {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The just-completed bar's equity return —
    /// `(equity - prev_equity) / prev_equity`. `None` on the first bar
    /// (`warm_up_period() = 2`). See
    /// [`crate::indicators::Book::return_per_bar`].
    #[grammar(kind = "source")]
    ReturnPerBar {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The realized P&L of the just-closed aggregate trade in
    /// reference-currency terms. `Some` only on the bar whose fill closed
    /// the trade. See [`crate::indicators::Book::trade_pnl`].
    ///
    /// On the portfolio aggregate book (`source: !portfolio_book`) this is
    /// always `None` — the aggregate book is mark-driven and doesn't route
    /// fills, so no "portfolio trade" is defined.
    #[grammar(kind = "source")]
    TradePnl {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The just-closed trade's return as a fraction of the equity at
    /// trade open. `Some` only on the close bar. See
    /// [`crate::indicators::Book::trade_return`]. Also `None` on the
    /// portfolio aggregate book for the same reason as [`NodeSpec::TradePnl`].
    #[grammar(kind = "source")]
    TradeReturn {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },

    /// Read one overlay column by name from each atom's side-channel data.
    ///
    /// The column's declared [`OverlayType`] in the atom stream's schema
    /// picks the output type at build time: a `Real` column yields a
    /// `Real`-output source (fits everywhere a numeric source does), a
    /// `Bool` column yields a `Bool`-output source (fits in any signal
    /// position — `!get` reads as a signal directly), a `Str` column yields
    /// a `Str`-output source (feeds into `!str_eq` / `!str_ne` on the
    /// signal side).
    ///
    /// Builds panic on an unknown key or a type mismatch — a `Str` column
    /// in a Real-typed source position is caught downstream at `AsReal::new`
    /// with the "expected Real" panic, the same failure mode as any other
    /// type-clashed spec.
    #[grammar(kind = "source", output = "any")]
    Get {
        key: String,
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },

    // --- price-series indicators (a source + parameters) ---
    #[grammar(kind = "indicator")]
    Ema {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    Sma {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    Rma {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    Wma {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    Hma {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    Rsi {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[serde(rename = "stddev")]
    #[grammar(kind = "indicator")]
    StdDev {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    Skewness {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    Kurtosis {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[serde(rename = "zscore")]
    #[grammar(kind = "indicator")]
    ZScore {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    /// The `pct`-quantile of a source over the trailing `period` bars —
    /// `pct: 0.5` is the rolling median. Linearly interpolated (R type-7), the
    /// same convention the report-level percentiles use. Prefer
    /// `!rolling_max` / `!rolling_min` over `pct: 1.0` / `pct: 0.0`; those are
    /// O(1). See [`crate::indicators::Percentile`].
    #[grammar(kind = "indicator")]
    Percentile {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
        pct: Real,
    },
    /// Where the current reading sits in its own trailing distribution, as
    /// `count(v <= x) / period` in `(0, 1]`. See
    /// [`crate::indicators::PercentileRank`].
    #[grammar(kind = "indicator")]
    PercentileRank {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    /// Bars elapsed since `source` (a **signal**) last read true — `0` on the
    /// firing bar. `None` until it has fired at least once, which makes every
    /// threshold against it read false until then. See
    /// [`crate::indicators::BarsSince`].
    #[grammar(kind = "indicator")]
    BarsSince { source: Box<NodeSpec> },
    /// Bars elapsed since `source` last set a new `period`-bar high, in
    /// `[0, period - 1]`. See [`crate::indicators::BarsSinceHigh`].
    #[grammar(kind = "indicator")]
    BarsSinceHigh {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    /// Bars elapsed since `source` last set a new `period`-bar low.
    /// See [`crate::indicators::BarsSinceLow`].
    #[grammar(kind = "indicator")]
    BarsSinceLow {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    /// Rolling Pearson correlation between two Real sources. Both operands are
    /// required — there is no single natural default for a two-source stat.
    #[grammar(kind = "indicator")]
    Correlation {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        period: usize,
    },
    /// Lo-MacKinlay variance-ratio regime classifier (`> 1` trending, `< 1`
    /// mean-reverting) over the source's first differences.
    #[grammar(kind = "indicator")]
    VarianceRatio {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
        lag: usize,
    },
    #[grammar(kind = "indicator")]
    Cci {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    Stochastic {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    StochRsi {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "stoch_rsi_rsi_period_default")]
        rsi_period: usize,
        #[serde(default = "stoch_rsi_stoch_period_default")]
        stoch_period: usize,
    },

    // --- multi-output indicators, one variant per component ---
    #[grammar(kind = "indicator")]
    MacdLine {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "macd_fast_default")]
        fast: usize,
        #[serde(default = "macd_slow_default")]
        slow: usize,
        #[serde(default = "macd_signal_default")]
        signal: usize,
    },
    #[grammar(kind = "indicator")]
    MacdSignal {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "macd_fast_default")]
        fast: usize,
        #[serde(default = "macd_slow_default")]
        slow: usize,
        #[serde(default = "macd_signal_default")]
        signal: usize,
    },
    #[grammar(kind = "indicator")]
    MacdHistogram {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "macd_fast_default")]
        fast: usize,
        #[serde(default = "macd_slow_default")]
        slow: usize,
        #[serde(default = "macd_signal_default")]
        signal: usize,
    },
    #[grammar(kind = "indicator")]
    BbUpper {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "bb_period_default")]
        period: usize,
        #[serde(default = "bb_k_default")]
        k: Real,
    },
    #[grammar(kind = "indicator")]
    BbMiddle {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "bb_period_default")]
        period: usize,
        #[serde(default = "bb_k_default")]
        k: Real,
    },
    #[grammar(kind = "indicator")]
    BbLower {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "bb_period_default")]
        period: usize,
        #[serde(default = "bb_k_default")]
        k: Real,
    },
    #[grammar(kind = "indicator")]
    KeltnerUpper {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<NodeSpec>,
        #[serde(default = "keltner_ema_period_default")]
        ema_period: usize,
        #[serde(default = "keltner_atr_period_default")]
        atr_period: usize,
        #[serde(default = "keltner_multiplier_default")]
        multiplier: Real,
    },
    #[grammar(kind = "indicator")]
    KeltnerMiddle {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<NodeSpec>,
        #[serde(default = "keltner_ema_period_default")]
        ema_period: usize,
        #[serde(default = "keltner_atr_period_default")]
        atr_period: usize,
        #[serde(default = "keltner_multiplier_default")]
        multiplier: Real,
    },
    #[grammar(kind = "indicator")]
    KeltnerLower {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<NodeSpec>,
        #[serde(default = "keltner_ema_period_default")]
        ema_period: usize,
        #[serde(default = "keltner_atr_period_default")]
        atr_period: usize,
        #[serde(default = "keltner_multiplier_default")]
        multiplier: Real,
    },
    #[grammar(kind = "indicator")]
    DonchianUpper {
        #[serde(default = "default_high")]
        high: Box<NodeSpec>,
        #[serde(default = "default_low")]
        low: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    DonchianMiddle {
        #[serde(default = "default_high")]
        high: Box<NodeSpec>,
        #[serde(default = "default_low")]
        low: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    DonchianLower {
        #[serde(default = "default_high")]
        high: Box<NodeSpec>,
        #[serde(default = "default_low")]
        low: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    Adx {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    PlusDi {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    MinusDi {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    DmiPlusDi {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    DmiMinusDi {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    AroonUp {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    AroonDown {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    AroonOscillator {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },

    // --- single-output bar indicators ---
    #[grammar(kind = "indicator")]
    Atr {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    /// Parkinson high/low range volatility estimator over `period`.
    #[grammar(kind = "indicator")]
    Parkinson {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    /// Garman–Klass OHLC volatility estimator over `period`.
    #[grammar(kind = "indicator")]
    GarmanKlass {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    /// Rogers–Satchell drift-independent OHLC volatility estimator over `period`.
    #[grammar(kind = "indicator")]
    RogersSatchell {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    Mfi {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    WilliamsR {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    Obv {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    #[grammar(kind = "indicator")]
    Vwap {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "indicator")]
    Ad {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    #[grammar(kind = "indicator")]
    TrueRange {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    #[grammar(kind = "indicator")]
    Sar {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        #[serde(default = "sar_step_default")]
        step: Real,
        #[serde(default = "sar_max_default")]
        max: Real,
    },

    // --- sizing helpers (real-valued, single-series; read the strategy's
    // own asset via the implicit empty-selector `Pick`). Meant for the
    // `sizing:` slot on `SingleStrategySpec` / `PairsStrategySpec`, but usable
    // anywhere a real-valued source fits. The book-anchored ones
    // (`DrawdownThrottle`, `EquityVolTarget`, `FractionalKelly`) additionally
    // require the strategy to own a `Book` — `SingleStrategySpec` does;
    // `PairsStrategySpec` does not (they'll emit `None` there).
    //
    // `!equal_weight <N>` used to be a variant here, but it's really
    // just `!value <1/N>` — a per-leg constant that normalizes to
    // `1/N`. It's now recognized as sugar and rewritten to `!value`
    // during `NodeSpec::try_from` before typed parse. See
    // [`rewrite_sugar_tags`].
    /// Inverse realized-vol sizing —
    /// `target / (stddev(log_returns(close), window) * sqrt(bars_per_year))`.
    /// `source` defaults to the single-asset empty-selector `Pick`; in a
    /// [`BasketStrategySpec`](super::basket::BasketStrategySpec) set it to
    /// `!pick { symbol: !arg SYM }` so each leg reads its own asset. See
    /// [`crate::indicators::sizing::vol_target`] /
    /// [`crate::indicators::sizing::vol_target_of`].
    #[grammar(kind = "function")]
    VolTarget {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
        target: Real,
        window: usize,
        bars_per_year: Real,
    },
    /// Fixed per-trade risk sized by ATR —
    /// `risk_frac * close / (atr_multiple * ATR(period))`. `source` defaults
    /// to the single-asset empty-selector `Pick`; in a basket set it to
    /// `!pick { symbol: !arg SYM }`. See
    /// [`crate::indicators::sizing::atr_risk`] /
    /// [`crate::indicators::sizing::atr_risk_of`].
    #[grammar(kind = "function")]
    AtrRisk {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
        risk_frac: Real,
        period: usize,
        atr_multiple: Real,
    },
    /// Drawdown-throttled sizing — `max(0, min(1, 1 + book.drawdown() /
    /// max_drawdown))`. Reads a book via `source:` (default:
    /// [`NodeSpec::StrategyBook`]). See
    /// [`crate::indicators::sizing::drawdown_throttle`].
    #[grammar(kind = "function")]
    DrawdownThrottle {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
        max_drawdown: Real,
    },
    /// Realized-vol targeting on the book's equity return series
    /// — `target / (stddev(book.return_per_bar, window) *
    /// sqrt(bars_per_year))`. Reads a book via `source:` (default:
    /// [`NodeSpec::StrategyBook`]). See
    /// [`crate::indicators::sizing::equity_vol_target`].
    #[grammar(kind = "function")]
    EquityVolTarget {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
        target: Real,
        window: usize,
        bars_per_year: Real,
    },
    /// Fractional Kelly over the last `window` closed-trade returns —
    /// `kelly_fraction * mean / variance`, clamped to `>= 0`. Reads a book
    /// via `source:` (default: [`NodeSpec::StrategyBook`]). See
    /// [`crate::indicators::sizing::fractional_kelly`].
    #[grammar(kind = "function")]
    FractionalKelly {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
        kelly_fraction: Real,
        window: usize,
    },

    // --- trailing risk indicators (own an embedded single-asset strategy,
    // drive it against a private paper wallet, and reduce its equity curve to
    // a rolling risk metric over the last `period` bars). Unlike every other
    // source these do not wrap a price — the `strategy` field is a whole
    // single-asset strategy document (inline or `!import`ed), and `symbol`
    // inside it names the instrument the embedded wallet prices. The natural
    // home is a `fugazi get -x` overlay column (a live regime feature), which
    // removes the "run a strategy → dump returns.csv → re-join" round-trip.
    /// Trailing annualized Sharpe of `strategy`'s equity curve over the last
    /// `period` bars. See [`crate::indicators::Sharpe`].
    #[grammar(kind = "indicator")]
    Sharpe {
        strategy: Box<AnyStrategyRef>,
        period: usize,
        bars_per_year: Real,
        #[serde(default = "default_risk_free_rate")]
        risk_free_rate: Real,
    },
    /// Trailing annualized Sortino of `strategy`'s equity curve. See
    /// [`crate::indicators::Sortino`].
    #[grammar(kind = "indicator")]
    Sortino {
        strategy: Box<AnyStrategyRef>,
        period: usize,
        bars_per_year: Real,
        #[serde(default = "default_risk_free_rate")]
        risk_free_rate: Real,
    },
    /// Trailing annualized volatility of `strategy`'s equity return stream.
    /// See [`crate::indicators::Volatility`].
    #[grammar(kind = "indicator")]
    Volatility {
        strategy: Box<AnyStrategyRef>,
        period: usize,
        bars_per_year: Real,
    },
    /// Trailing maximum drawdown of `strategy`'s equity curve, as a
    /// non-negative fraction. See [`crate::indicators::MaxDrawdown`].
    #[grammar(kind = "indicator")]
    MaxDrawdown {
        strategy: Box<AnyStrategyRef>,
        period: usize,
    },
    /// Trailing Calmar (windowed CAGR / max drawdown) of `strategy`'s equity
    /// curve. See [`crate::indicators::Calmar`].
    #[grammar(kind = "indicator")]
    Calmar {
        strategy: Box<AnyStrategyRef>,
        period: usize,
        bars_per_year: Real,
    },

    // --- transform operators ---
    #[grammar(kind = "operator")]
    Add {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    #[grammar(kind = "operator")]
    Sub {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    #[grammar(kind = "operator")]
    Mul {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    #[grammar(kind = "operator")]
    Div {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    /// Three-source ternary: reads `cond` (a bool signal), emits
    /// `then`'s value when `cond` is true, `otherwise`'s when false, and
    /// `None` when `cond` is `None`. All three sources are advanced every
    /// bar so a branch that doesn't fire this bar keeps warming up in the
    /// background. Warm-up is the max of the three; the ternary reports
    /// `None` until every source has warmed. See
    /// [`crate::indicators::IfElse`].
    #[grammar(kind = "operator", output = "any")]
    IfElse {
        cond: Box<NodeSpec>,
        then: Box<NodeSpec>,
        otherwise: Box<NodeSpec>,
    },
    /// N-way dispatch by value equality — reads `on` once per bar and
    /// picks the *first* case whose pattern equals `on`'s reading; falls
    /// through to `default` when no case matches. Every branch (all
    /// cases + default) is advanced every bar so its warm-up progresses
    /// even on bars it isn't selected — same convention as
    /// [`NodeSpec::IfElse`](Self::IfElse).
    ///
    /// Case patterns are homogeneous: either all numeric (dispatching
    /// on a `Real`-output `on`) or all string (dispatching on a
    /// `Str`-output `on`, typically `!value { arg: CHILD_GROUP }`).
    /// Mixed patterns are rejected at build. See
    /// [`crate::indicators::Match`].
    #[grammar(kind = "operator", output = "any")]
    Match {
        on: Box<NodeSpec>,
        cases: Vec<MatchCase>,
        default: Box<NodeSpec>,
    },
    #[grammar(kind = "operator")]
    Lag {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "operator")]
    Diff {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "operator")]
    Ratio {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "operator")]
    Roc {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "operator")]
    RollingMax {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[grammar(kind = "operator")]
    RollingMin {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    /// Logarithm of `source` in `base` (defaults to natural log, `e`).
    /// Emits `None` on samples where the source's output is non-positive.
    #[grammar(kind = "operator")]
    Log {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "default_log_base")]
        base: Real,
    },
    /// Holds the most recent `Some` output of `source`, re-emitting it on
    /// ticks where `source` returns `None`. Wrap the outermost recursive
    /// smoother of a resampled pipeline so per-base-tick consumers see the
    /// finished higher-timeframe value between boundaries — see
    /// [`crate::indicators::Latch`].
    #[grammar(kind = "operator", output = "any")]
    Latch { source: Box<NodeSpec> },
    /// Aggregates `every` base candles into one higher-timeframe candle and
    /// runs the `inner` source over it, emitting `inner`'s output on each
    /// completed bucket and `None` in between. `inner` is any source that
    /// reads a candle (`close`/`high`/`typical`, `!ema { period: N, source:
    /// close }`, `!add { lhs, rhs }`, …); it advances only on emissions from
    /// the resample, so an `!ema` inside `!resample` recurses over the HTF
    /// closes, not the base ones. **The resample's clock stays
    /// base-timeframe**: it's fed one base candle per tick and reports at
    /// that same cadence; the emitted `Option<Real>` marks whether the inner
    /// produced a value on a completed bucket. Wrap the whole downstream
    /// chain in [`Latch`](NodeSpec::Latch) so per-base-tick reads see the
    /// finished value between boundaries.
    #[grammar(kind = "operator")]
    Resample {
        every: usize,
        inner: Box<NodeSpec>,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Passthrough wrapper that reports `unstable_period() = 0`. The output
    /// and warm-up of `source` are unchanged; the strategy-readiness gate
    /// (which counts up to `stable_period()`) no longer waits for this
    /// subtree's IIR settling tail. The explicit opt-out to the "wait for
    /// every source to be past its unstable tail" safe default; see
    /// [`crate::indicators::Unstable`].
    #[grammar(kind = "operator", output = "any")]
    Unstable { source: Box<NodeSpec> },

    // --- calendar accessors (read `atom.time`, emit Real; None when time is
    // absent). Each takes an optional `source` for cross-asset use — the
    // bare form (`!year`) is the default single-series shortcut,
    // `!year { source: !pick { ... } }` reads the picked asset's time.
    /// The Gregorian year (e.g. `2024.0`).
    #[grammar(kind = "source")]
    Year {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The Gregorian month, `1.0` (Jan) through `12.0` (Dec).
    #[grammar(kind = "source")]
    Month {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The day of the month, `1.0` through `31.0`.
    #[grammar(kind = "source")]
    Day {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The hour of the day (UTC), `0.0` through `23.0`.
    #[grammar(kind = "source")]
    Hour {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The minute of the hour, `0.0` through `59.0`.
    #[grammar(kind = "source")]
    Minute {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The second of the minute, `0.0` through `59.0`.
    #[grammar(kind = "source")]
    Second {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// ISO 8601 weekday, `1.0` (Monday) through `7.0` (Sunday).
    #[grammar(kind = "source")]
    DayOfWeek {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Day of the year, `1.0` through `366.0`.
    #[grammar(kind = "source")]
    DayOfYear {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// ISO 8601 week of the year, `1.0` through `53.0`.
    #[grammar(kind = "source")]
    WeekOfYear {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Calendar quarter, `1.0` through `4.0`.
    #[grammar(kind = "source")]
    Quarter {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Unix seconds since the epoch (as a Real).
    #[grammar(kind = "source")]
    UnixSeconds {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Unix milliseconds since the epoch (as a Real).
    #[grammar(kind = "source")]
    UnixMillis {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The raw bar-open [`Timestamp`] payload (yields
    /// `DynType::Time`, not a scalar). The `Timestamp` twin of
    /// [`NodeSpec::Current`].
    #[grammar(kind = "source", output = "time")]
    Time {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },

    // --- boolean signals (absorbed from the former SignalSpec; every one
    // produces `Bool`). Comparisons carry an optional absolute `epsilon`
    // (default `DEFAULT_EPSILON`).
    #[grammar(kind = "predicate", output = "bool")]
    Gt {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        epsilon: Option<Real>,
    },
    #[grammar(kind = "predicate", output = "bool")]
    Lt {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        epsilon: Option<Real>,
    },
    #[grammar(kind = "predicate", output = "bool")]
    Ge {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        epsilon: Option<Real>,
    },
    #[grammar(kind = "predicate", output = "bool")]
    Le {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        epsilon: Option<Real>,
    },
    /// Polymorphic equality — Real or Str, dispatched on the lhs at build.
    #[grammar(kind = "predicate", output = "bool")]
    Eq {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        epsilon: Option<Real>,
    },
    #[grammar(kind = "predicate", output = "bool")]
    Ne {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        epsilon: Option<Real>,
    },
    /// `source > level` against a constant.
    #[grammar(kind = "predicate", output = "bool")]
    Above {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        level: Real,
    },
    /// `source < level` against a constant.
    #[grammar(kind = "predicate", output = "bool")]
    Below {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        level: Real,
    },
    #[grammar(kind = "predicate", output = "bool")]
    CrossesAbove {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    #[grammar(kind = "predicate", output = "bool")]
    CrossesBelow {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    #[grammar(kind = "operator", output = "bool")]
    And {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    #[grammar(kind = "operator", output = "bool")]
    Or {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    #[grammar(kind = "operator", output = "bool")]
    Xor {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    /// AND-fold of a list (empty ⇒ constant `true`).
    #[grammar(kind = "operator", output = "bool")]
    All(Vec<NodeSpec>),
    /// OR-fold of a list (empty ⇒ constant `false`).
    #[grammar(kind = "operator", output = "bool")]
    Any(Vec<NodeSpec>),
    #[grammar(kind = "operator", output = "bool")]
    Not(Box<NodeSpec>),
    /// Toggle detector — fires on either edge. Dispatches on the child's
    /// output type at build: a Bool inner is a rising-or-falling toggle, a
    /// Real inner fires on any value change. Subsumes the former
    /// `Changed` / `ChangedReal` split of the signal layer.
    #[grammar(kind = "predicate", output = "bool")]
    Changed(Box<NodeSpec>),
    /// Rising-edge detector for a Bool inner (`false → true`).
    #[grammar(kind = "predicate", output = "bool")]
    BecameTrue(Box<NodeSpec>),
    /// Falling-edge detector (`true → false`).
    #[grammar(kind = "predicate", output = "bool")]
    BecameFalse(Box<NodeSpec>),
    /// `lhs == rhs` on two `Str`-typed operands.
    #[grammar(kind = "predicate", output = "bool")]
    StrEq {
        lhs: Box<NodeSpec>,
        rhs: StrOperand,
    },
    /// `lhs != rhs` on two `Str`-typed operands.
    #[grammar(kind = "predicate", output = "bool")]
    StrNe {
        lhs: Box<NodeSpec>,
        rhs: StrOperand,
    },
    /// Sugar for `!value false` — reads better on a `rebalance_on:` field
    /// where the intent is "never".
    #[grammar(kind = "predicate", output = "bool")]
    Never,
    /// A periodic pulse — [`Every(N)`](crate::indicators::Every) with a
    /// *delayed* first fire on bar `N-1` (0-indexed), then every `N` bars.
    #[grammar(kind = "predicate", output = "bool")]
    Every(usize),
    /// True Monday through Friday; `None` when `atom.time` is absent.
    #[grammar(kind = "predicate", output = "bool")]
    IsWeekday,
    /// True Saturday/Sunday; `None` when `atom.time` is absent.
    #[grammar(kind = "predicate", output = "bool")]
    IsWeekend,
    /// Schema-level check: `true` if the overlay column `name` exists.
    #[grammar(kind = "predicate", output = "bool")]
    HasColumn { name: String },
}

// Mirror enum: identical shape as NodeSpec but with derived Deserialize —
// used inside TryFrom<serde_norway::Value> to run the standard externally-
// tagged deserialization once bare-string / tagged shapes are normalised.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
// Unknown keys are a typo, not something to ignore: `!if_else { then, els }`
// silently dropping `els` and reporting only "missing field `else`" tells a
// reader what is absent but not that what they wrote was discarded — so the
// obvious next edit adds the right key and leaves the wrong one behind.
// Denying produces serde's "unknown field `els`, expected one of ..." instead.
#[serde(deny_unknown_fields)]
enum NodeSpecRaw {

    // --- atom-input leaves (candle fields) ---
    Close {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    High {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    Low {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    Open {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    Volume {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    Typical {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    Median {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The current bar itself — the whole [`Candle`], not a scalar. The default
    /// bar source of every bar-consuming indicator (`!atr`, `!obv`, `!adx`, …);
    /// wrap in [`NodeSpec::Resample`] to lift a downstream bar indicator
    /// onto a higher timeframe.
    Current {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },

    /// Cross-asset projection: project one asset's [`Atom`] out of the
    /// snapshot the CLI feeds each bar. Both fields are optional — an empty
    /// `!pick {}` behaves identically to the implicit single-entry unpack
    /// every atom-input leaf uses by default. Compose with any atom-input
    /// leaf via `source: !pick { symbol, freq }`.
    ///
    /// `freq` accepts the same `N<unit>` alphabet as `--frequency`
    /// (`1m` / `4h` / `1d` / `1w` / `1M`), so a cross-frequency snapshot
    /// disambiguates via `!pick { symbol: BTC, freq: 1h }`.
    Pick {
        #[serde(default)]
        symbol: Option<String>,
        #[serde(default)]
        freq: Option<String>,
    },

    /// A constant value — a number (`!value 70`, a `Real` source) or a string
    /// (`!value bull`, a `Str` source for `!str_eq` / `!str_ne`). See
    /// [`ValueLit`].
    Value(ValueLit),

    /// The current position's entry price — a [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy) anchor,
    /// for building stop-loss / take-profit levels.
    Entry,
    /// The running high since entry (a long trailing-stop anchor).
    Peak,
    /// The running low since entry (a short trailing-stop anchor).
    Trough,

    // --- book source-selectors. These are build-time only — they carry no
    // runtime value; they resolve, at build, to a `Book<String>` handle that
    // a book-reading node (a bare book leaf like `!drawdown`, or a
    // book-anchored recipe like `!drawdown_throttle`) picks up via its
    // `source:` field. Bare (used as an expression on its own) is invalid
    // and panics at build.
    /// The **strategy book** — the `Book` owned by the enclosing strategy
    /// scope (single/pairs/basket/multi/the current per-child instance of
    /// a portfolio's `weights:` expression). This is the default source of
    /// every book-reading node when its `source:` is omitted.
    StrategyBook,
    /// The **portfolio aggregate book** — the mark-to-market view of the
    /// composite [`Portfolio`](crate::portfolio::Portfolio). Only meaningful
    /// inside a portfolio's `weights:` expression; panics at build if
    /// referenced elsewhere.
    PortfolioBook,

    // --- book-anchored leaves. Each takes an optional `source:` that
    // resolves to the book they read (see [`NodeSpec::StrategyBook`] /
    // [`NodeSpec::PortfolioBook`]). Omitted → `!strategy_book`.
    /// The marked-to-market equity of the book. Always `Some`
    /// (seeded at the book's `initial_equity`). See
    /// [`crate::indicators::Book::equity`].
    Equity {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The running peak of the book's equity. Always `Some`.
    /// See [`crate::indicators::Book::equity_peak`].
    EquityPeak {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The book's current drawdown as a non-positive fraction —
    /// `(equity - peak) / peak`, `0` at a fresh peak. See
    /// [`crate::indicators::Book::drawdown`].
    Drawdown {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The just-completed bar's equity return —
    /// `(equity - prev_equity) / prev_equity`. `None` on the first bar
    /// (`warm_up_period() = 2`). See
    /// [`crate::indicators::Book::return_per_bar`].
    ReturnPerBar {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The realized P&L of the just-closed aggregate trade in
    /// reference-currency terms. `Some` only on the bar whose fill closed
    /// the trade. See [`crate::indicators::Book::trade_pnl`].
    ///
    /// On the portfolio aggregate book (`source: !portfolio_book`) this is
    /// always `None` — the aggregate book is mark-driven and doesn't route
    /// fills, so no "portfolio trade" is defined.
    TradePnl {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The just-closed trade's return as a fraction of the equity at
    /// trade open. `Some` only on the close bar. See
    /// [`crate::indicators::Book::trade_return`]. Also `None` on the
    /// portfolio aggregate book for the same reason as [`NodeSpec::TradePnl`].
    TradeReturn {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },

    /// Read one overlay column by name from each atom's side-channel data.
    ///
    /// The column's declared [`OverlayType`] in the atom stream's schema
    /// picks the output type at build time: a `Real` column yields a
    /// `Real`-output source (fits everywhere a numeric source does), a
    /// `Bool` column yields a `Bool`-output source (fits in any signal
    /// position — `!get` reads as a signal directly), a `Str` column yields
    /// a `Str`-output source (feeds into `!str_eq` / `!str_ne` on the
    /// signal side).
    ///
    /// Builds panic on an unknown key or a type mismatch — a `Str` column
    /// in a Real-typed source position is caught downstream at `AsReal::new`
    /// with the "expected Real" panic, the same failure mode as any other
    /// type-clashed spec.
    Get {
        key: String,
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },

    // --- price-series indicators (a source + parameters) ---
    Ema {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    Sma {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    Rma {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    Wma {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    Hma {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    Rsi {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[serde(rename = "stddev")]
    StdDev {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    Skewness {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    Kurtosis {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    #[serde(rename = "zscore")]
    ZScore {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    /// The `pct`-quantile of a source over the trailing `period` bars —
    /// `pct: 0.5` is the rolling median. Linearly interpolated (R type-7), the
    /// same convention the report-level percentiles use. Prefer
    /// `!rolling_max` / `!rolling_min` over `pct: 1.0` / `pct: 0.0`; those are
    /// O(1). See [`crate::indicators::Percentile`].
    Percentile {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
        pct: Real,
    },
    /// Where the current reading sits in its own trailing distribution, as
    /// `count(v <= x) / period` in `(0, 1]`. See
    /// [`crate::indicators::PercentileRank`].
    PercentileRank {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    /// Bars elapsed since `source` (a **signal**) last read true — `0` on the
    /// firing bar. `None` until it has fired at least once, which makes every
    /// threshold against it read false until then. See
    /// [`crate::indicators::BarsSince`].
    BarsSince { source: Box<NodeSpec> },
    /// Bars elapsed since `source` last set a new `period`-bar high, in
    /// `[0, period - 1]`. See [`crate::indicators::BarsSinceHigh`].
    BarsSinceHigh {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    /// Bars elapsed since `source` last set a new `period`-bar low.
    /// See [`crate::indicators::BarsSinceLow`].
    BarsSinceLow {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    /// Rolling Pearson correlation between two Real sources. Both operands are
    /// required — there is no single natural default for a two-source stat.
    Correlation {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        period: usize,
    },
    /// Lo-MacKinlay variance-ratio regime classifier (`> 1` trending, `< 1`
    /// mean-reverting) over the source's first differences.
    VarianceRatio {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
        lag: usize,
    },
    Cci {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    Stochastic {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    StochRsi {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "stoch_rsi_rsi_period_default")]
        rsi_period: usize,
        #[serde(default = "stoch_rsi_stoch_period_default")]
        stoch_period: usize,
    },

    // --- multi-output indicators, one variant per component ---
    MacdLine {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "macd_fast_default")]
        fast: usize,
        #[serde(default = "macd_slow_default")]
        slow: usize,
        #[serde(default = "macd_signal_default")]
        signal: usize,
    },
    MacdSignal {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "macd_fast_default")]
        fast: usize,
        #[serde(default = "macd_slow_default")]
        slow: usize,
        #[serde(default = "macd_signal_default")]
        signal: usize,
    },
    MacdHistogram {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "macd_fast_default")]
        fast: usize,
        #[serde(default = "macd_slow_default")]
        slow: usize,
        #[serde(default = "macd_signal_default")]
        signal: usize,
    },
    BbUpper {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "bb_period_default")]
        period: usize,
        #[serde(default = "bb_k_default")]
        k: Real,
    },
    BbMiddle {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "bb_period_default")]
        period: usize,
        #[serde(default = "bb_k_default")]
        k: Real,
    },
    BbLower {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "bb_period_default")]
        period: usize,
        #[serde(default = "bb_k_default")]
        k: Real,
    },
    KeltnerUpper {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<NodeSpec>,
        #[serde(default = "keltner_ema_period_default")]
        ema_period: usize,
        #[serde(default = "keltner_atr_period_default")]
        atr_period: usize,
        #[serde(default = "keltner_multiplier_default")]
        multiplier: Real,
    },
    KeltnerMiddle {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<NodeSpec>,
        #[serde(default = "keltner_ema_period_default")]
        ema_period: usize,
        #[serde(default = "keltner_atr_period_default")]
        atr_period: usize,
        #[serde(default = "keltner_multiplier_default")]
        multiplier: Real,
    },
    KeltnerLower {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<NodeSpec>,
        #[serde(default = "keltner_ema_period_default")]
        ema_period: usize,
        #[serde(default = "keltner_atr_period_default")]
        atr_period: usize,
        #[serde(default = "keltner_multiplier_default")]
        multiplier: Real,
    },
    DonchianUpper {
        #[serde(default = "default_high")]
        high: Box<NodeSpec>,
        #[serde(default = "default_low")]
        low: Box<NodeSpec>,
        period: usize,
    },
    DonchianMiddle {
        #[serde(default = "default_high")]
        high: Box<NodeSpec>,
        #[serde(default = "default_low")]
        low: Box<NodeSpec>,
        period: usize,
    },
    DonchianLower {
        #[serde(default = "default_high")]
        high: Box<NodeSpec>,
        #[serde(default = "default_low")]
        low: Box<NodeSpec>,
        period: usize,
    },
    Adx {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    PlusDi {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    MinusDi {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    DmiPlusDi {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    DmiMinusDi {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    AroonUp {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    AroonDown {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    AroonOscillator {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },

    // --- single-output bar indicators ---
    Atr {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    /// Parkinson high/low range volatility estimator over `period`.
    Parkinson {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    /// Garman–Klass OHLC volatility estimator over `period`.
    GarmanKlass {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    /// Rogers–Satchell drift-independent OHLC volatility estimator over `period`.
    RogersSatchell {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    Mfi {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    WilliamsR {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    Obv {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    Vwap {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    Ad {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    TrueRange {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    Sar {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
        #[serde(default = "sar_step_default")]
        step: Real,
        #[serde(default = "sar_max_default")]
        max: Real,
    },

    // --- sizing helpers (real-valued, single-series; read the strategy's
    // own asset via the implicit empty-selector `Pick`). Meant for the
    // `sizing:` slot on `SingleStrategySpec` / `PairsStrategySpec`, but usable
    // anywhere a real-valued source fits. The book-anchored ones
    // (`DrawdownThrottle`, `EquityVolTarget`, `FractionalKelly`) additionally
    // require the strategy to own a `Book` — `SingleStrategySpec` does;
    // `PairsStrategySpec` does not (they'll emit `None` there).
    //
    // `!equal_weight <N>` used to be a variant here, but it's really
    // just `!value <1/N>` — a per-leg constant that normalizes to
    // `1/N`. It's now recognized as sugar and rewritten to `!value`
    // during `NodeSpec::try_from` before typed parse. See
    // [`rewrite_sugar_tags`].
    /// Inverse realized-vol sizing —
    /// `target / (stddev(log_returns(close), window) * sqrt(bars_per_year))`.
    /// `source` defaults to the single-asset empty-selector `Pick`; in a
    /// [`BasketStrategySpec`](super::basket::BasketStrategySpec) set it to
    /// `!pick { symbol: !arg SYM }` so each leg reads its own asset. See
    /// [`crate::indicators::sizing::vol_target`] /
    /// [`crate::indicators::sizing::vol_target_of`].
    VolTarget {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
        target: Real,
        window: usize,
        bars_per_year: Real,
    },
    /// Fixed per-trade risk sized by ATR —
    /// `risk_frac * close / (atr_multiple * ATR(period))`. `source` defaults
    /// to the single-asset empty-selector `Pick`; in a basket set it to
    /// `!pick { symbol: !arg SYM }`. See
    /// [`crate::indicators::sizing::atr_risk`] /
    /// [`crate::indicators::sizing::atr_risk_of`].
    AtrRisk {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
        risk_frac: Real,
        period: usize,
        atr_multiple: Real,
    },
    /// Drawdown-throttled sizing — `max(0, min(1, 1 + book.drawdown() /
    /// max_drawdown))`. Reads a book via `source:` (default:
    /// [`NodeSpec::StrategyBook`]). See
    /// [`crate::indicators::sizing::drawdown_throttle`].
    DrawdownThrottle {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
        max_drawdown: Real,
    },
    /// Realized-vol targeting on the book's equity return series
    /// — `target / (stddev(book.return_per_bar, window) *
    /// sqrt(bars_per_year))`. Reads a book via `source:` (default:
    /// [`NodeSpec::StrategyBook`]). See
    /// [`crate::indicators::sizing::equity_vol_target`].
    EquityVolTarget {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
        target: Real,
        window: usize,
        bars_per_year: Real,
    },
    /// Fractional Kelly over the last `window` closed-trade returns —
    /// `kelly_fraction * mean / variance`, clamped to `>= 0`. Reads a book
    /// via `source:` (default: [`NodeSpec::StrategyBook`]). See
    /// [`crate::indicators::sizing::fractional_kelly`].
    FractionalKelly {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
        kelly_fraction: Real,
        window: usize,
    },

    // --- trailing risk indicators (own an embedded single-asset strategy,
    // drive it against a private paper wallet, and reduce its equity curve to
    // a rolling risk metric over the last `period` bars). Unlike every other
    // source these do not wrap a price — the `strategy` field is a whole
    // single-asset strategy document (inline or `!import`ed), and `symbol`
    // inside it names the instrument the embedded wallet prices. The natural
    // home is a `fugazi get -x` overlay column (a live regime feature), which
    // removes the "run a strategy → dump returns.csv → re-join" round-trip.
    /// Trailing annualized Sharpe of `strategy`'s equity curve over the last
    /// `period` bars. See [`crate::indicators::Sharpe`].
    Sharpe {
        strategy: Box<AnyStrategyRef>,
        period: usize,
        bars_per_year: Real,
        #[serde(default = "default_risk_free_rate")]
        risk_free_rate: Real,
    },
    /// Trailing annualized Sortino of `strategy`'s equity curve. See
    /// [`crate::indicators::Sortino`].
    Sortino {
        strategy: Box<AnyStrategyRef>,
        period: usize,
        bars_per_year: Real,
        #[serde(default = "default_risk_free_rate")]
        risk_free_rate: Real,
    },
    /// Trailing annualized volatility of `strategy`'s equity return stream.
    /// See [`crate::indicators::Volatility`].
    Volatility {
        strategy: Box<AnyStrategyRef>,
        period: usize,
        bars_per_year: Real,
    },
    /// Trailing maximum drawdown of `strategy`'s equity curve, as a
    /// non-negative fraction. See [`crate::indicators::MaxDrawdown`].
    MaxDrawdown {
        strategy: Box<AnyStrategyRef>,
        period: usize,
    },
    /// Trailing Calmar (windowed CAGR / max drawdown) of `strategy`'s equity
    /// curve. See [`crate::indicators::Calmar`].
    Calmar {
        strategy: Box<AnyStrategyRef>,
        period: usize,
        bars_per_year: Real,
    },

    // --- transform operators ---
    Add {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    Sub {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    Mul {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    Div {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    /// Three-source ternary: reads `cond` (a bool signal), emits
    /// `then`'s value when `cond` is true, `otherwise`'s when false, and
    /// `None` when `cond` is `None`. All three sources are advanced every
    /// bar so a branch that doesn't fire this bar keeps warming up in the
    /// background. Warm-up is the max of the three; the ternary reports
    /// `None` until every source has warmed. See
    /// [`crate::indicators::IfElse`].
    IfElse {
        cond: Box<NodeSpec>,
        then: Box<NodeSpec>,
        otherwise: Box<NodeSpec>,
    },
    /// N-way dispatch by value equality — reads `on` once per bar and
    /// picks the *first* case whose pattern equals `on`'s reading; falls
    /// through to `default` when no case matches. Every branch (all
    /// cases + default) is advanced every bar so its warm-up progresses
    /// even on bars it isn't selected — same convention as
    /// [`NodeSpec::IfElse`](Self::IfElse).
    ///
    /// Case patterns are homogeneous: either all numeric (dispatching
    /// on a `Real`-output `on`) or all string (dispatching on a
    /// `Str`-output `on`, typically `!value { arg: CHILD_GROUP }`).
    /// Mixed patterns are rejected at build. See
    /// [`crate::indicators::Match`].
    Match {
        on: Box<NodeSpec>,
        cases: Vec<MatchCase>,
        default: Box<NodeSpec>,
    },
    Lag {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    Diff {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    Ratio {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    Roc {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    RollingMax {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    RollingMin {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        period: usize,
    },
    /// Logarithm of `source` in `base` (defaults to natural log, `e`).
    /// Emits `None` on samples where the source's output is non-positive.
    Log {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "default_log_base")]
        base: Real,
    },
    /// Holds the most recent `Some` output of `source`, re-emitting it on
    /// ticks where `source` returns `None`. Wrap the outermost recursive
    /// smoother of a resampled pipeline so per-base-tick consumers see the
    /// finished higher-timeframe value between boundaries — see
    /// [`crate::indicators::Latch`].
    Latch { source: Box<NodeSpec> },
    /// Aggregates `every` base candles into one higher-timeframe candle and
    /// runs the `inner` source over it, emitting `inner`'s output on each
    /// completed bucket and `None` in between. `inner` is any source that
    /// reads a candle (`close`/`high`/`typical`, `!ema { period: N, source:
    /// close }`, `!add { lhs, rhs }`, …); it advances only on emissions from
    /// the resample, so an `!ema` inside `!resample` recurses over the HTF
    /// closes, not the base ones. **The resample's clock stays
    /// base-timeframe**: it's fed one base candle per tick and reports at
    /// that same cadence; the emitted `Option<Real>` marks whether the inner
    /// produced a value on a completed bucket. Wrap the whole downstream
    /// chain in [`Latch`](NodeSpec::Latch) so per-base-tick reads see the
    /// finished value between boundaries.
    Resample {
        every: usize,
        inner: Box<NodeSpec>,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Passthrough wrapper that reports `unstable_period() = 0`. The output
    /// and warm-up of `source` are unchanged; the strategy-readiness gate
    /// (which counts up to `stable_period()`) no longer waits for this
    /// subtree's IIR settling tail. The explicit opt-out to the "wait for
    /// every source to be past its unstable tail" safe default; see
    /// [`crate::indicators::Unstable`].
    Unstable { source: Box<NodeSpec> },

    // --- calendar accessors (read `atom.time`, emit Real; None when time is
    // absent). Each takes an optional `source` for cross-asset use — the
    // bare form (`!year`) is the default single-series shortcut,
    // `!year { source: !pick { ... } }` reads the picked asset's time.
    /// The Gregorian year (e.g. `2024.0`).
    Year {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The Gregorian month, `1.0` (Jan) through `12.0` (Dec).
    Month {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The day of the month, `1.0` through `31.0`.
    Day {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The hour of the day (UTC), `0.0` through `23.0`.
    Hour {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The minute of the hour, `0.0` through `59.0`.
    Minute {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The second of the minute, `0.0` through `59.0`.
    Second {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// ISO 8601 weekday, `1.0` (Monday) through `7.0` (Sunday).
    DayOfWeek {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Day of the year, `1.0` through `366.0`.
    DayOfYear {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// ISO 8601 week of the year, `1.0` through `53.0`.
    WeekOfYear {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Calendar quarter, `1.0` through `4.0`.
    Quarter {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Unix seconds since the epoch (as a Real).
    UnixSeconds {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Unix milliseconds since the epoch (as a Real).
    UnixMillis {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The raw bar-open [`Timestamp`] payload (yields
    /// `DynType::Time`, not a scalar). The `Timestamp` twin of
    /// [`NodeSpec::Current`].
    Time {
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },

    // --- boolean signals (mirror of the NodeSpec additions above) ---
    Gt {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        epsilon: Option<Real>,
    },
    Lt {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        epsilon: Option<Real>,
    },
    Ge {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        epsilon: Option<Real>,
    },
    Le {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        epsilon: Option<Real>,
    },
    Eq {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        epsilon: Option<Real>,
    },
    Ne {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        epsilon: Option<Real>,
    },
    Above {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        level: Real,
    },
    Below {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        level: Real,
    },
    CrossesAbove {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    CrossesBelow {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    And {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    Or {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    Xor {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    All(Vec<NodeSpec>),
    Any(Vec<NodeSpec>),
    Not(Box<NodeSpec>),
    // Constructed by the edge dispatch in `parse_unchecked`, never reached via
    // the derived deserialize — present so the variant is enumerated in
    // `known_expr_tags` and covered by `From`.
    Changed(Box<NodeSpec>),
    BecameTrue(Box<NodeSpec>),
    BecameFalse(Box<NodeSpec>),
    StrEq {
        lhs: Box<NodeSpec>,
        rhs: StrOperand,
    },
    StrNe {
        lhs: Box<NodeSpec>,
        rhs: StrOperand,
    },
    Never,
    Every(usize),
    IsWeekday,
    IsWeekend,
    HasColumn { name: String },
}

impl From<NodeSpecRaw> for NodeSpec {
    fn from(v: NodeSpecRaw) -> Self {
        match v {
            NodeSpecRaw::Close { source } => NodeSpec::Close { source },
            NodeSpecRaw::High { source } => NodeSpec::High { source },
            NodeSpecRaw::Low { source } => NodeSpec::Low { source },
            NodeSpecRaw::Open { source } => NodeSpec::Open { source },
            NodeSpecRaw::Volume { source } => NodeSpec::Volume { source },
            NodeSpecRaw::Typical { source } => NodeSpec::Typical { source },
            NodeSpecRaw::Median { source } => NodeSpec::Median { source },
            NodeSpecRaw::Current { source } => NodeSpec::Current { source },
            NodeSpecRaw::Pick { symbol, freq } => NodeSpec::Pick { symbol, freq },
            NodeSpecRaw::Value(x) => NodeSpec::Value(x),
            NodeSpecRaw::Entry => NodeSpec::Entry,
            NodeSpecRaw::Peak => NodeSpec::Peak,
            NodeSpecRaw::Trough => NodeSpec::Trough,
            NodeSpecRaw::StrategyBook => NodeSpec::StrategyBook,
            NodeSpecRaw::PortfolioBook => NodeSpec::PortfolioBook,
            NodeSpecRaw::Equity { source } => NodeSpec::Equity { source },
            NodeSpecRaw::EquityPeak { source } => NodeSpec::EquityPeak { source },
            NodeSpecRaw::Drawdown { source } => NodeSpec::Drawdown { source },
            NodeSpecRaw::ReturnPerBar { source } => NodeSpec::ReturnPerBar { source },
            NodeSpecRaw::TradePnl { source } => NodeSpec::TradePnl { source },
            NodeSpecRaw::TradeReturn { source } => NodeSpec::TradeReturn { source },
            NodeSpecRaw::Get { key, source } => NodeSpec::Get { key, source },
            NodeSpecRaw::Ema { source, period } => NodeSpec::Ema { source, period },
            NodeSpecRaw::Sma { source, period } => NodeSpec::Sma { source, period },
            NodeSpecRaw::Rma { source, period } => NodeSpec::Rma { source, period },
            NodeSpecRaw::Wma { source, period } => NodeSpec::Wma { source, period },
            NodeSpecRaw::Hma { source, period } => NodeSpec::Hma { source, period },
            NodeSpecRaw::Rsi { source, period } => NodeSpec::Rsi { source, period },
            NodeSpecRaw::StdDev { source, period } => NodeSpec::StdDev { source, period },
            NodeSpecRaw::Skewness { source, period } => NodeSpec::Skewness { source, period },
            NodeSpecRaw::Kurtosis { source, period } => NodeSpec::Kurtosis { source, period },
            NodeSpecRaw::ZScore { source, period } => NodeSpec::ZScore { source, period },
            NodeSpecRaw::Percentile {
                source,
                period,
                pct,
            } => NodeSpec::Percentile {
                source,
                period,
                pct,
            },
            NodeSpecRaw::PercentileRank { source, period } => {
                NodeSpec::PercentileRank { source, period }
            }
            NodeSpecRaw::BarsSince { source } => NodeSpec::BarsSince { source },
            NodeSpecRaw::BarsSinceHigh { source, period } => {
                NodeSpec::BarsSinceHigh { source, period }
            }
            NodeSpecRaw::BarsSinceLow { source, period } => {
                NodeSpec::BarsSinceLow { source, period }
            }
            NodeSpecRaw::Correlation { lhs, rhs, period } => NodeSpec::Correlation { lhs, rhs, period },
            NodeSpecRaw::VarianceRatio {
                source,
                period,
                lag,
            } => NodeSpec::VarianceRatio {
                source,
                period,
                lag,
            },
            NodeSpecRaw::Cci { source, period } => NodeSpec::Cci { source, period },
            NodeSpecRaw::Stochastic { source, period } => NodeSpec::Stochastic { source, period },
            NodeSpecRaw::StochRsi { source, rsi_period, stoch_period } => NodeSpec::StochRsi { source, rsi_period, stoch_period },
            NodeSpecRaw::MacdLine { source, fast, slow, signal } => NodeSpec::MacdLine { source, fast, slow, signal },
            NodeSpecRaw::MacdSignal { source, fast, slow, signal } => NodeSpec::MacdSignal { source, fast, slow, signal },
            NodeSpecRaw::MacdHistogram { source, fast, slow, signal } => NodeSpec::MacdHistogram { source, fast, slow, signal },
            NodeSpecRaw::BbUpper { source, period, k } => NodeSpec::BbUpper { source, period, k },
            NodeSpecRaw::BbMiddle { source, period, k } => NodeSpec::BbMiddle { source, period, k },
            NodeSpecRaw::BbLower { source, period, k } => NodeSpec::BbLower { source, period, k },
            NodeSpecRaw::KeltnerUpper { source, candle_source, ema_period, atr_period, multiplier } => NodeSpec::KeltnerUpper { source, candle_source, ema_period, atr_period, multiplier },
            NodeSpecRaw::KeltnerMiddle { source, candle_source, ema_period, atr_period, multiplier } => NodeSpec::KeltnerMiddle { source, candle_source, ema_period, atr_period, multiplier },
            NodeSpecRaw::KeltnerLower { source, candle_source, ema_period, atr_period, multiplier } => NodeSpec::KeltnerLower { source, candle_source, ema_period, atr_period, multiplier },
            NodeSpecRaw::DonchianUpper { high, low, period } => NodeSpec::DonchianUpper { high, low, period },
            NodeSpecRaw::DonchianMiddle { high, low, period } => NodeSpec::DonchianMiddle { high, low, period },
            NodeSpecRaw::DonchianLower { high, low, period } => NodeSpec::DonchianLower { high, low, period },
            NodeSpecRaw::Adx { source, period } => NodeSpec::Adx { source, period },
            NodeSpecRaw::PlusDi { source, period } => NodeSpec::PlusDi { source, period },
            NodeSpecRaw::MinusDi { source, period } => NodeSpec::MinusDi { source, period },
            NodeSpecRaw::DmiPlusDi { source, period } => NodeSpec::DmiPlusDi { source, period },
            NodeSpecRaw::DmiMinusDi { source, period } => NodeSpec::DmiMinusDi { source, period },
            NodeSpecRaw::AroonUp { source, period } => NodeSpec::AroonUp { source, period },
            NodeSpecRaw::AroonDown { source, period } => NodeSpec::AroonDown { source, period },
            NodeSpecRaw::AroonOscillator { source, period } => NodeSpec::AroonOscillator { source, period },
            NodeSpecRaw::Atr { source, period } => NodeSpec::Atr { source, period },
            NodeSpecRaw::Parkinson { source, period } => NodeSpec::Parkinson { source, period },
            NodeSpecRaw::GarmanKlass { source, period } => NodeSpec::GarmanKlass { source, period },
            NodeSpecRaw::RogersSatchell { source, period } => {
                NodeSpec::RogersSatchell { source, period }
            }
            NodeSpecRaw::Mfi { source, period } => NodeSpec::Mfi { source, period },
            NodeSpecRaw::WilliamsR { source, period } => NodeSpec::WilliamsR { source, period },
            NodeSpecRaw::Obv { source } => NodeSpec::Obv { source },
            NodeSpecRaw::Vwap { source, period } => NodeSpec::Vwap { source, period },
            NodeSpecRaw::Ad { source } => NodeSpec::Ad { source },
            NodeSpecRaw::TrueRange { source } => NodeSpec::TrueRange { source },
            NodeSpecRaw::Sar { source, step, max } => NodeSpec::Sar { source, step, max },
            NodeSpecRaw::VolTarget { source, target, window, bars_per_year } => NodeSpec::VolTarget { source, target, window, bars_per_year },
            NodeSpecRaw::AtrRisk { source, risk_frac, period, atr_multiple } => NodeSpec::AtrRisk { source, risk_frac, period, atr_multiple },
            NodeSpecRaw::DrawdownThrottle { source, max_drawdown } => NodeSpec::DrawdownThrottle { source, max_drawdown },
            NodeSpecRaw::EquityVolTarget { source, target, window, bars_per_year } => NodeSpec::EquityVolTarget { source, target, window, bars_per_year },
            NodeSpecRaw::FractionalKelly { source, kelly_fraction, window } => NodeSpec::FractionalKelly { source, kelly_fraction, window },
            NodeSpecRaw::Sharpe { strategy, period, bars_per_year, risk_free_rate } => NodeSpec::Sharpe { strategy, period, bars_per_year, risk_free_rate },
            NodeSpecRaw::Sortino { strategy, period, bars_per_year, risk_free_rate } => NodeSpec::Sortino { strategy, period, bars_per_year, risk_free_rate },
            NodeSpecRaw::Volatility { strategy, period, bars_per_year } => NodeSpec::Volatility { strategy, period, bars_per_year },
            NodeSpecRaw::MaxDrawdown { strategy, period } => NodeSpec::MaxDrawdown { strategy, period },
            NodeSpecRaw::Calmar { strategy, period, bars_per_year } => NodeSpec::Calmar { strategy, period, bars_per_year },
            NodeSpecRaw::Add { lhs, rhs } => NodeSpec::Add { lhs, rhs },
            NodeSpecRaw::Sub { lhs, rhs } => NodeSpec::Sub { lhs, rhs },
            NodeSpecRaw::Mul { lhs, rhs } => NodeSpec::Mul { lhs, rhs },
            NodeSpecRaw::Div { lhs, rhs } => NodeSpec::Div { lhs, rhs },
            NodeSpecRaw::IfElse {
                cond,
                then,
                otherwise,
            } => NodeSpec::IfElse {
                cond,
                then,
                otherwise,
            },
            NodeSpecRaw::Match {
                on,
                cases,
                default,
            } => NodeSpec::Match {
                on,
                cases,
                default,
            },
            NodeSpecRaw::Lag { source, period } => NodeSpec::Lag { source, period },
            NodeSpecRaw::Diff { source, period } => NodeSpec::Diff { source, period },
            NodeSpecRaw::Ratio { source, period } => NodeSpec::Ratio { source, period },
            NodeSpecRaw::Roc { source, period } => NodeSpec::Roc { source, period },
            NodeSpecRaw::RollingMax { source, period } => NodeSpec::RollingMax { source, period },
            NodeSpecRaw::RollingMin { source, period } => NodeSpec::RollingMin { source, period },
            NodeSpecRaw::Log { source, base } => NodeSpec::Log { source, base },
            NodeSpecRaw::Latch { source } => NodeSpec::Latch { source },
            NodeSpecRaw::Resample { every, inner, source } => NodeSpec::Resample { every, inner, source },
            NodeSpecRaw::Unstable { source } => NodeSpec::Unstable { source },
            NodeSpecRaw::Year { source } => NodeSpec::Year { source },
            NodeSpecRaw::Month { source } => NodeSpec::Month { source },
            NodeSpecRaw::Day { source } => NodeSpec::Day { source },
            NodeSpecRaw::Hour { source } => NodeSpec::Hour { source },
            NodeSpecRaw::Minute { source } => NodeSpec::Minute { source },
            NodeSpecRaw::Second { source } => NodeSpec::Second { source },
            NodeSpecRaw::DayOfWeek { source } => NodeSpec::DayOfWeek { source },
            NodeSpecRaw::DayOfYear { source } => NodeSpec::DayOfYear { source },
            NodeSpecRaw::WeekOfYear { source } => NodeSpec::WeekOfYear { source },
            NodeSpecRaw::Quarter { source } => NodeSpec::Quarter { source },
            NodeSpecRaw::UnixSeconds { source } => NodeSpec::UnixSeconds { source },
            NodeSpecRaw::UnixMillis { source } => NodeSpec::UnixMillis { source },
            NodeSpecRaw::Time { source } => NodeSpec::Time { source },
            NodeSpecRaw::Gt { lhs, rhs, epsilon } => NodeSpec::Gt { lhs, rhs, epsilon },
            NodeSpecRaw::Lt { lhs, rhs, epsilon } => NodeSpec::Lt { lhs, rhs, epsilon },
            NodeSpecRaw::Ge { lhs, rhs, epsilon } => NodeSpec::Ge { lhs, rhs, epsilon },
            NodeSpecRaw::Le { lhs, rhs, epsilon } => NodeSpec::Le { lhs, rhs, epsilon },
            NodeSpecRaw::Eq { lhs, rhs, epsilon } => NodeSpec::Eq { lhs, rhs, epsilon },
            NodeSpecRaw::Ne { lhs, rhs, epsilon } => NodeSpec::Ne { lhs, rhs, epsilon },
            NodeSpecRaw::Above { source, level } => NodeSpec::Above { source, level },
            NodeSpecRaw::Below { source, level } => NodeSpec::Below { source, level },
            NodeSpecRaw::CrossesAbove { lhs, rhs } => NodeSpec::CrossesAbove { lhs, rhs },
            NodeSpecRaw::CrossesBelow { lhs, rhs } => NodeSpec::CrossesBelow { lhs, rhs },
            NodeSpecRaw::And { lhs, rhs } => NodeSpec::And { lhs, rhs },
            NodeSpecRaw::Or { lhs, rhs } => NodeSpec::Or { lhs, rhs },
            NodeSpecRaw::Xor { lhs, rhs } => NodeSpec::Xor { lhs, rhs },
            NodeSpecRaw::All(v) => NodeSpec::All(v),
            NodeSpecRaw::Any(v) => NodeSpec::Any(v),
            NodeSpecRaw::Not(inner) => NodeSpec::Not(inner),
            NodeSpecRaw::Changed(inner) => NodeSpec::Changed(inner),
            NodeSpecRaw::BecameTrue(inner) => NodeSpec::BecameTrue(inner),
            NodeSpecRaw::BecameFalse(inner) => NodeSpec::BecameFalse(inner),
            NodeSpecRaw::StrEq { lhs, rhs } => NodeSpec::StrEq { lhs, rhs },
            NodeSpecRaw::StrNe { lhs, rhs } => NodeSpec::StrNe { lhs, rhs },
            NodeSpecRaw::Never => NodeSpec::Never,
            NodeSpecRaw::Every(n) => NodeSpec::Every(n),
            NodeSpecRaw::IsWeekday => NodeSpec::IsWeekday,
            NodeSpecRaw::IsWeekend => NodeSpec::IsWeekend,
            NodeSpecRaw::HasColumn { name } => NodeSpec::HasColumn { name },
        }
    }
}

impl TryFrom<serde_norway::Value> for NodeSpec {
    type Error = String;

    /// Normalise the incoming YAML value into a [`serde_norway::Value::Tagged`],
    /// then deserialize into `NodeSpecRaw`.
    ///
    /// `serde_norway`'s `Value` deserializer only routes an *enum* input
    /// through its `Value::Tagged` variant — a plain single-key `Mapping`
    /// (the shape serde_json / yaml_to_json produces for an externally-
    /// tagged enum) is not accepted as an enum. So we normalise every
    /// incoming shape into a `Value::Tagged` before handing it to serde:
    ///
    /// - `Value::String(s)` (a bare word like `close`) → `Value::Tagged { tag:
    ///   s, value: Null }`, matching variant `s` with all fields defaulted.
    /// - `Value::Tagged` — forwarded verbatim (the YAML `!close` /
    ///   `!ema { ... }` form already has the right shape).
    /// - `Value::Mapping` with a single string key — rewritten as
    ///   `Value::Tagged { tag, value }`, so a serde_json → serde_norway::Value
    ///   bridge (which produces `Mapping`s for externally-tagged enums)
    ///   reaches the same code path.
    /// - Anything else (a `Number` for `Value(x)`, etc.) is forwarded verbatim
    ///   and serde_norway will report a helpful "unexpected type" error.
    ///
    /// Recursion into `Box<NodeSpec>` fields re-enters this same
    /// `TryFrom` — so a nested bare-word inside a tagged form is normalised
    /// on the way down.
    fn try_from(v: serde_norway::Value) -> Result<Self, Self::Error> {
        let spec = NodeSpec::parse_unchecked(v)?;
        // Reject a child whose output type this tag cannot consume —
        // `!sma { source: !value bull }` and friends. The engine would catch it
        // anyway, but only on reaching `AsReal::new`'s `assert_eq!` mid-build,
        // which reports `left: Str, right: Real` and no location. Doing it here
        // turns that into a parse error naming the tag and the slot.
        //
        // Runs on **every** parse, not just `fugazi check`: a spec that would
        // panic during `run` or `optimize` is better rejected at load. The pass
        // only reports mismatches it can prove (an undecidable child type is
        // skipped), and `typecheck`'s tests pin its table against what `build`
        // actually demands, so it cannot reject a spec the engine would accept.
        crate::spec::typecheck::check_immediate(&spec)?;
        Ok(spec)
    }
}

impl NodeSpec {
    /// The normalisation + typed parse, **without** the type check
    /// [`TryFrom`] applies on top.
    ///
    /// Exists so `typecheck`'s own tests can construct the deliberately
    /// ill-typed trees they exercise — which the public parse path now
    /// rejects, by design. Not a way around validation for real callers:
    /// every deserialization of an `NodeSpec` goes through `TryFrom`.
    pub(crate) fn parse_unchecked(v: serde_norway::Value) -> Result<Self, String> {
        use serde_norway::value::{Tag, TaggedValue};

        // A `check`-mode hole standing in for a whole expression (a `!param`
        // that resolves to an entire source). Only present under `check`, so
        // this never matches in a real run. A constant `0.0` is a valid
        // Real-typed source, enough for the surrounding shape to validate.
        if crate::spec::undefined::is_undefined(&v) {
            return Ok(NodeSpec::Value(ValueLit::Real(0.0)));
        }

        // Unit-variant tags: their content stays as `Value::Null` because
        // serde's derived Deserialize expects unit content for a unit
        // variant. Every other variant is a struct with all-defaulted
        // fields, and a Null content there needs to be promoted to an
        // empty `Mapping` — serde's `deserialize_struct` accepts an empty
        // map (all fields default) but not `Null` (which errors with
        // "invalid type: unit value, expected struct variant"). The two
        // shapes both have to be normalised at the same layer because a
        // downstream `!pick` can appear as either an empty struct-variant
        // (`!pick {}` / `!pick`) or a filled one (`!pick { symbol: BTC }`).
        const UNIT_VARIANTS: &[&str] = &[
            "entry",
            "peak",
            "trough",
            "strategy_book",
            "portfolio_book",
            // absorbed boolean-signal unit variants
            "never",
            "is_weekday",
            "is_weekend",
            // wall-clock cadence sugar (rewritten to `!changed { source:
            // !<accessor> }` before the raw deserialize; unit tags so a bare
            // `daily` stays `Null` for the rewrite to pick up).
            "hourly",
            "daily",
            "weekly",
            "monthly",
            "quarterly",
            "annually",
        ];

        let promote_null_for = |tag: &str, v: serde_norway::Value| {
            if UNIT_VARIANTS.contains(&tag) {
                // Unit variants take no payload — `!entry`, `entry:` (null),
                // and `entry: {}` (empty mapping) all mean the same thing.
                // Serde's derived Deserialize expects `unit` content for a
                // unit variant, so collapse the empty-map form to null too.
                match v {
                    serde_norway::Value::Mapping(m) if m.is_empty() => {
                        serde_norway::Value::Null
                    }
                    other => other,
                }
            } else if matches!(v, serde_norway::Value::Null) {
                // Non-unit variants need a struct-variant content — an
                // empty map lets serde default every field. Serde
                // rejects `Null` for a struct variant even when every
                // field defaults.
                serde_norway::Value::Mapping(serde_norway::Mapping::new())
            } else {
                v
            }
        };

        let normalised = match v {
            serde_norway::Value::String(s) => {
                let value = if UNIT_VARIANTS.contains(&s.as_str()) {
                    serde_norway::Value::Null
                } else {
                    serde_norway::Value::Mapping(serde_norway::Mapping::new())
                };
                serde_norway::Value::Tagged(Box::new(TaggedValue {
                    tag: Tag::new(s),
                    value,
                }))
            }
            serde_norway::Value::Tagged(tagged) => {
                let TaggedValue { tag, value } = *tagged;
                let tag_name = tag.to_string();
                let name = tag_name.strip_prefix('!').unwrap_or(&tag_name);
                let value = promote_null_for(name, value);
                serde_norway::Value::Tagged(Box::new(TaggedValue { tag, value }))
            }
            serde_norway::Value::Mapping(m) if m.len() == 1 => {
                let (k, v) = m.into_iter().next().unwrap();
                match k {
                    serde_norway::Value::String(name) => {
                        let value = promote_null_for(&name, v);
                        serde_norway::Value::Tagged(Box::new(TaggedValue {
                            tag: Tag::new(name),
                            value,
                        }))
                    }
                    other => {
                        let mut m = serde_norway::Mapping::new();
                        m.insert(other, v);
                        serde_norway::Value::Mapping(m)
                    }
                }
            }
            // Bare number literal — auto-wrap as `!value N`. Numbers are
            // never leaf names, so this is unambiguous: any position
            // expecting an NodeSpec that got `70` really means "the
            // constant 70". Removes the `!value` boilerplate from the
            // most common comparison shape (`!gt { lhs: !close, rhs: 70 }`
            // instead of `rhs: !value 70`).
            serde_norway::Value::Number(n) => {
                serde_norway::Value::Tagged(Box::new(TaggedValue {
                    tag: Tag::new("value"),
                    value: serde_norway::Value::Number(n),
                }))
            }
            // Bare bool literal — auto-wrap as `!value true|false`. Bools are
            // never leaf names, so this is unambiguous: `enter: true` means the
            // constant-true signal. Subsumes the former signal-layer `!value
            // <bool>` / `!never` boilerplate.
            serde_norway::Value::Bool(b) => {
                serde_norway::Value::Tagged(Box::new(TaggedValue {
                    tag: Tag::new("value"),
                    value: serde_norway::Value::Bool(b),
                }))
            }
            // Bare list of numbers — auto-wrap as `!value [...]`. Only
            // meaningful inside a portfolio weight-share template
            // (`weights: [0.4, 0.6]` for the per-child fixed-weights
            // case). The typed parse of `!value` handles the shape
            // check; a list of anything else (strings, nested maps)
            // isn't a valid NodeSpec, so falling through to `other =>
            // other` and letting serde report the mismatch is fine.
            serde_norway::Value::Sequence(seq)
                if seq
                    .iter()
                    .all(|item| matches!(item, serde_norway::Value::Number(_))) =>
            {
                serde_norway::Value::Tagged(Box::new(TaggedValue {
                    tag: Tag::new("value"),
                    value: serde_norway::Value::Sequence(seq),
                }))
            }
            other => other,
        };
        // Sugar tags — rewrite to their canonical form before typed
        // parse. `!equal_weight <N>` is really just `!value <1/N>`
        // (a per-leg constant that normalizes to `1/N`); collapsing
        // it here means there's one primitive (`!value`) instead of
        // two variants doing the same thing.
        let normalised = rewrite_sugar_tags(normalised)?;
        // Wall-clock cadence sugar (`!daily` → `!changed { source: !day }`),
        // then the wrapper dispatch: edge detectors (`!changed`,
        // `!became_true`, `!became_false`) and `!unstable`'s bare-inner form
        // are constructed directly from their extracted inner, because that
        // inner is a bare tagged node rather than a `{ field: ... }` map the
        // derived Raw parse expects.
        let normalised = rewrite_cadence_sugar(normalised);
        if let Some(rewritten) = try_dispatch_wrappers(&normalised)? {
            return Ok(rewritten);
        }
        // The tag this node parses as, for the error breadcrumb below. Known
        // here even when the typed parse fails, which is exactly when it is
        // needed.
        let tag = match &normalised {
            serde_norway::Value::Tagged(t) => {
                let s = t.tag.to_string();
                Some(s.strip_prefix('!').unwrap_or(&s).to_string())
            }
            _ => None,
        };
        let raw: NodeSpecRaw = crate::spec::undefined::from_value(normalised)
            // Nesting this at every level turns a bare "expects a Real source"
            // into a trail from the outermost tag down to the offending one —
            // the closest thing to a source location available, since the
            // `!import` / `!param` passes rewrite the tree and drop any spans
            // the original text had.
            .map_err(|e| match &tag {
                // A ` > `-separated path, built inside-out as the error rises.
                // Rendered as one `at:` line by `diagnostics::split_trail`; if
                // that ever fails to run, the raw string still reads as a path
                // rather than as a stack of `in x: in y:` prefixes.
                Some(t) => format!("!{t} > {e}"),
                None => e.to_string(),
            })?;
        Ok(raw.into())
    }
}

/// Rewrite NodeSpec sugar tags to their canonical `!value` forms. Runs
/// after shape-normalization (so tagged / bare / single-key-map inputs
/// all reach this pass in `Value::Tagged` form). Currently covers
/// `!equal_weight <N>` → `!value <1/N>`; other sugar tags can be added
/// the same way if the pattern repeats.
fn rewrite_sugar_tags(v: serde_norway::Value) -> Result<serde_norway::Value, String> {
    use serde_norway::value::{Tag, TaggedValue};
    if let serde_norway::Value::Tagged(tagged) = v {
        let TaggedValue { tag, value } = *tagged;
        let tag_str = tag.to_string();
        let name = tag_str.strip_prefix('!').unwrap_or(&tag_str);
        if name == "equal_weight" {
            let n = match &value {
                serde_norway::Value::Number(n) => n
                    .as_u64()
                    .ok_or_else(|| format!(
                        "!equal_weight: expected a positive integer leg count, got {n}"
                    ))?,
                other => {
                    return Err(format!(
                        "!equal_weight: expected a positive integer leg count, got {other:?}"
                    ));
                }
            };
            if n == 0 {
                return Err(
                    "!equal_weight: leg count must be strictly positive".to_string()
                );
            }
            let weight = 1.0_f64 / n as f64;
            return Ok(serde_norway::Value::Tagged(Box::new(TaggedValue {
                tag: Tag::new("value"),
                value: serde_norway::Value::Number(weight.into()),
            })));
        }
        // Not a sugar tag — repack and return.
        return Ok(serde_norway::Value::Tagged(Box::new(TaggedValue {
            tag,
            value,
        })));
    }
    Ok(v)
}

/// Rewrite the six wall-clock cadence sugar tags (`!hourly`, `!daily`,
/// `!weekly`, `!monthly`, `!quarterly`, `!annually`) to
/// `!changed { source: !<calendar_accessor> }` before the raw deserialize
/// runs. Kept in the parse layer so downstream debug prints show the
/// desugared form.
fn rewrite_cadence_sugar(v: serde_norway::Value) -> serde_norway::Value {
    use serde_norway::value::{Tag, TaggedValue};
    let name = match &v {
        serde_norway::Value::Tagged(tv) => {
            let tag = tv.tag.to_string();
            tag.strip_prefix('!').unwrap_or(&tag).to_string()
        }
        _ => return v,
    };
    let accessor_tag = match name.as_str() {
        "hourly" => "hour",
        "daily" => "day",
        "weekly" => "week_of_year",
        "monthly" => "month",
        "quarterly" => "quarter",
        "annually" => "year",
        _ => return v,
    };
    let accessor_val = serde_norway::Value::Tagged(Box::new(TaggedValue {
        tag: Tag::new(accessor_tag),
        value: serde_norway::Value::Mapping(serde_norway::Mapping::new()),
    }));
    let mut inner_map = serde_norway::Mapping::new();
    inner_map.insert(
        serde_norway::Value::String("source".to_string()),
        accessor_val,
    );
    serde_norway::Value::Tagged(Box::new(TaggedValue {
        tag: Tag::new("changed"),
        value: serde_norway::Value::Mapping(inner_map),
    }))
}

/// Extract the inner payload of a unary wrapper tag, accepting both the bare
/// form (`!changed !gt { ... }`) and the `{ source: <inner> }` mapping form
/// (`!changed { source: !month }`). `None` when the outer tag doesn't match
/// `wanted`.
fn extract_edge_inner(v: &serde_norway::Value, wanted: &str) -> Option<serde_norway::Value> {
    let inner_payload = match v {
        serde_norway::Value::Tagged(tv)
            if tv.tag.to_string().trim_start_matches('!') == wanted =>
        {
            &tv.value
        }
        _ => return None,
    };
    match inner_payload {
        serde_norway::Value::Mapping(m) if m.len() == 1 => match m.iter().next() {
            Some((serde_norway::Value::String(k), source)) if k == "source" => Some(source.clone()),
            _ => Some(inner_payload.clone()),
        },
        _ => Some(inner_payload.clone()),
    }
}

/// Dispatch the wrapper tags whose inner is a bare tagged node: `!changed`,
/// `!became_true`, `!became_false`, and `!unstable`'s bare form. Returns
/// `Ok(Some(spec))` on match, `Ok(None)` otherwise, `Err` when the inner
/// fails to parse.
///
/// The Real-vs-Bool decision for `!changed` now happens at *build* time
/// (dispatch on the inner's `output_type`), so the inner is parsed once as a
/// general [`NodeSpec`] with no parse-time fallback dance.
fn try_dispatch_wrappers(v: &serde_norway::Value) -> Result<Option<NodeSpec>, String> {
    if let Some(inner) = extract_edge_inner(v, "changed") {
        return Ok(Some(NodeSpec::Changed(Box::new(NodeSpec::try_from(inner)?))));
    }
    if let Some(inner) = extract_edge_inner(v, "became_true") {
        return Ok(Some(NodeSpec::BecameTrue(Box::new(NodeSpec::try_from(
            inner,
        )?))));
    }
    if let Some(inner) = extract_edge_inner(v, "became_false") {
        return Ok(Some(NodeSpec::BecameFalse(Box::new(NodeSpec::try_from(
            inner,
        )?))));
    }
    // `!unstable`: only intercept the bare-inner forms (a tagged node or a
    // bare word). The `{ source: X }` mapping form and any mis-spelled field
    // (`{ signal: X }`) fall through to the derived Raw parse, which handles
    // `source` and rejects the unknown field cleanly.
    if let serde_norway::Value::Tagged(tv) = v {
        let name = tv.tag.to_string();
        let name = name.strip_prefix('!').unwrap_or(&name);
        if name == "unstable"
            && matches!(
                tv.value,
                serde_norway::Value::Tagged(_) | serde_norway::Value::String(_)
            )
        {
            let inner = NodeSpec::try_from(tv.value.clone())?;
            return Ok(Some(NodeSpec::Unstable {
                source: Box::new(inner),
            }));
        }
    }
    Ok(None)
}

/// Resolve an optional tolerance to its concrete value.
fn eps(epsilon: &Option<Real>) -> Real {
    epsilon.unwrap_or(crate::indicators::DEFAULT_EPSILON)
}

/// Build the polymorphic `!eq` / `!ne` — the Real-or-Str dispatcher. Inspects
/// `lhs`'s built output type and threads it into the matching [`compare`]
/// primitive. `epsilon` is only meaningful on the `Real` path (string
/// equality is exact). `negate = false` builds `!eq`; `true` builds `!ne`.
#[allow(clippy::too_many_arguments)]
fn build_polymorphic_eq(
    lhs: &NodeSpec,
    rhs: &NodeSpec,
    epsilon: Option<Real>,
    negate: bool,
    anchor: &Position,
    book: &Book,
    portfolio_book: Option<&Book>,
    schema: &Arc<Schema>,
    root: Option<&Selector<String>>,
) -> Result<Box<dyn DynIndicator>, String> {
    let lhs_built = lhs.try_build(anchor, book, portfolio_book, schema, root)?;
    Ok(match lhs_built.output_type() {
        DynType::Real => {
            let l = AsReal::new(lhs_built);
            let r = AsReal::try_new(rhs.try_build(anchor, book, portfolio_book, schema, root)?)
                .map_err(|e| trail(rhs, e))?;
            let e = eps(&epsilon);
            if negate {
                dyn_indicator::wrap(compare::Ne::with_epsilon(l, r, e))
            } else {
                dyn_indicator::wrap(compare::Eq::with_epsilon(l, r, e))
            }
        }
        DynType::Str => {
            let l = AsStr::new(lhs_built);
            let r = AsStr::try_new(rhs.try_build(anchor, book, portfolio_book, schema, root)?)
                .map_err(|e| trail(rhs, e))?;
            if negate {
                dyn_indicator::wrap(compare::StrNe::new(l, r))
            } else {
                dyn_indicator::wrap(compare::StrEq::new(l, r))
            }
        }
        other => {
            return Err(format!(
                "lhs must produce Real or Str, got {other} — Bool / Candle / \
                 Atom / Snapshot / Time outputs have no defined equality \
                 semantics here"
            ));
        }
    })
}

/// Resolve an optional cross-asset `source` spec into a concrete
/// atom-emitting source. When the spec is `None`, returns the implicit
/// empty-selector `Pick` (single-entry unpack); when `Some`, builds the
/// user's subtree (typically a `!pick { symbol, freq }`) and wraps as an
/// [`AsAtom`] view for the leaf's `T::of(source)` constructor.
/// Build the runtime [`crate::indicators::Match`] chain for
/// [`NodeSpec::Match`]. Case patterns are homogeneous — either all
/// numeric (`ValueLit::Real`, dispatching on a `Real`-output `on`) or
/// all string (`ValueLit::Str`, dispatching on a `Str`-output `on`).
/// Mixed and `List` patterns are rejected with a build-time panic (loud
/// on bad YAML — the CLI convention).
///
/// # Panics
/// Panics if `cases` is empty, if any case's `value:` is a `List`, or
/// if the cases mix `Real` and `Str` patterns. The typed
/// [`Deserialize`] path already rejects `deny_unknown_fields` typos.
// Five of the eight are the build context (`anchor` / `book` /
// `portfolio_book` / `schema` / `root`) that every recursive build carries and
// this helper only forwards. Grouping them into a context struct is a
// worthwhile refactor of `NodeSpec::build` as a whole, not of the one private
// helper that happens to sit one argument over the threshold.
#[allow(clippy::too_many_arguments)]
fn build_match(
    on: &NodeSpec,
    cases: &[MatchCase],
    default: &NodeSpec,
    anchor: &Position,
    book: &Book,
    portfolio_book: Option<&Book>,
    schema: &Arc<Schema>,
    root: Option<&Selector<String>>,
) -> Result<Box<dyn DynIndicator>, String> {
    if cases.is_empty() {
        return Err("`cases` must not be empty (use `!if_else` for a single branch, \
                    or reduce to `default` if there's nothing to match)"
            .to_string());
    }

    // Sniff the pattern type once — every case must agree, else the
    // library-level `Match<S, T, K>` can't be given a single `K`.
    let is_str = match &cases[0].when {
        ValueLit::Str(_) => true,
        ValueLit::Real(_) => false,
        ValueLit::Bool(_) => {
            return Err("case 0 `when:` is a bool — a match dispatches on a number \
                        or a string, not a boolean"
                .to_string());
        }
        ValueLit::List(_) => {
            return Err("case 0 `when:` is a `!value <list>` — list literals have \
                        no defined equality against `on` and aren't a valid \
                        match pattern"
                .to_string());
        }
    };
    for (i, c) in cases.iter().enumerate() {
        match &c.when {
            ValueLit::Str(_) if !is_str => {
                return Err(format!(
                    "case {i} `when:` is a string but case 0 is a number — \
                     all cases must dispatch on the same type"
                ));
            }
            ValueLit::Real(_) if is_str => {
                return Err(format!(
                    "case {i} `when:` is a number but case 0 is a string — \
                     all cases must dispatch on the same type"
                ));
            }
            ValueLit::List(_) => {
                return Err(format!(
                    "case {i} `when:` is a `!value <list>` — list literals have \
                     no defined equality against `on` and aren't a valid match \
                     pattern"
                ));
            }
            ValueLit::Bool(_) => {
                return Err(format!(
                    "case {i} `when:` is a bool — a match dispatches on a number \
                     or a string, not a boolean"
                ));
            }
            _ => {}
        }
    }

    let as_real_branch = |s: &NodeSpec| -> Result<AsReal, String> {
        let built = s.try_build(anchor, book, portfolio_book, schema, root)?;
        AsReal::try_new(built).map_err(|e| trail(s, e))
    };

    let default_ind = as_real_branch(default)?;

    if is_str {
        let on_built = on.try_build(anchor, book, portfolio_book, schema, root)?;
        let on_ind = AsStr::try_new(on_built).map_err(|e| trail(on, e))?;
        let pairs: Vec<(Arc<str>, AsReal)> = cases
            .iter()
            .map(|c| {
                let pattern: Arc<str> = match &c.when {
                    ValueLit::Str(s) => Arc::from(s.as_str()),
                    _ => unreachable!("string-pattern branch, already validated"),
                };
                Ok((pattern, as_real_branch(&c.value)?))
            })
            .collect::<Result<_, String>>()?;
        Ok(dyn_indicator::wrap(MatchIndicator::new(
            on_ind,
            pairs,
            default_ind,
        )))
    } else {
        let on_ind = as_real_branch(on)?;
        let pairs: Vec<(Real, AsReal)> = cases
            .iter()
            .map(|c| {
                let pattern: Real = match &c.when {
                    ValueLit::Real(x) => *x,
                    _ => unreachable!("numeric-pattern branch, already validated"),
                };
                Ok((pattern, as_real_branch(&c.value)?))
            })
            .collect::<Result<_, String>>()?;
        Ok(dyn_indicator::wrap(MatchIndicator::new(
            on_ind,
            pairs,
            default_ind,
        )))
    }
}

fn atom_source_of(
    source: Option<&NodeSpec>,
    anchor: &Position,
    book: &Book,
    portfolio_book: Option<&Book>,
    schema: &Arc<Schema>,
    root: Option<&Selector<String>>,
) -> Result<AsAtom, String> {
    match source {
        None => Ok(AsAtom::new(dyn_indicator::wrap(pick_root(root)))),
        Some(s) => {
            let built = s.try_build(anchor, book, portfolio_book, schema, root)?;
            AsAtom::try_new(built).map_err(|e| trail(s, e))
        }
    }
}

/// Twin of [`atom_source_of`] for symbol-agnostic leaves — the calendar
/// accessor family and the wall-clock cadence sugar. When `source` is
/// `None`, roots on the "any entry" [`PickAny`] instead of the
/// panic-on-2+ [`Pick`], so a bare `!month` / `!daily` / `!is_weekday`
/// composes cleanly inside a
/// [`MultiAssetStrategy`](crate::strategies::MultiAssetStrategy),
/// [`BasketStrategy`](crate::strategies::BasketStrategy), or a
/// [`Portfolio`](crate::portfolio::Portfolio) `rebalance_on:` gate.
/// An explicit `!pick { symbol: ... }` source is honored verbatim (same
/// as [`atom_source_of`]), so callers who want a specific symbol's time
/// keep that ability.
fn atom_source_any_of(
    source: Option<&NodeSpec>,
    anchor: &Position,
    book: &Book,
    portfolio_book: Option<&Book>,
    schema: &Arc<Schema>,
    root: Option<&Selector<String>>,
) -> Result<AsAtom, String> {
    match source {
        None => Ok(AsAtom::new(dyn_indicator::wrap(pick_any_root()))),
        Some(s) => {
            let built = s.try_build(anchor, book, portfolio_book, schema, root)?;
            AsAtom::try_new(built).map_err(|e| trail(s, e))
        }
    }
}

/// Resolve an optional book-source spec into the concrete [`Book`] a
/// book-reading node should read from. The vocabulary is intentionally
/// minimal — only the two build-time source-selector tags
/// ([`NodeSpec::StrategyBook`] and [`NodeSpec::PortfolioBook`]) are
/// accepted; anything else in a book-reading node's `source:` slot is a
/// hard build error, since the resulting expression would have no defined
/// interpretation.
///
/// - `None` → `book` (default: the strategy book).
/// - `Some(!strategy_book)` → `book`.
/// - `Some(!portfolio_book)` → `portfolio_book` (`Err` if `None` — caller
///   isn't in a portfolio weight scope).
/// - `Some(anything else)` → `Err` with a helpful message.
fn resolve_book_source<'a>(
    source: Option<&NodeSpec>,
    book: &'a Book,
    portfolio_book: Option<&'a Book>,
) -> Result<&'a Book, String> {
    match source {
        None | Some(NodeSpec::StrategyBook) => Ok(book),
        Some(NodeSpec::PortfolioBook) => portfolio_book.ok_or_else(|| {
            "!portfolio_book: not inside a portfolio weight scope — this \
             source only makes sense in a portfolio's `weights:` expression"
                .to_string()
        }),
        Some(other) => Err(format!(
            "expected a book source (!strategy_book or !portfolio_book) in the \
             `source:` slot of a book-reading node, got {}",
            crate::spec::typecheck::tag_name(other),
        )),
    }
}

impl NodeSpec {
    /// Construct the live, runtime-typed source this spec describes as a
    /// `Box<dyn DynIndicator>`. `anchor` is the owning strategy's
    /// [`Position`], shared by any `entry` / `peak` / `trough` leaves in the
    /// tree; `book` is the owning strategy's [`Book`], the default source of
    /// any book-reading node (`!drawdown`, `!equity`, `!drawdown_throttle`,
    /// `!equity_vol_target`, `!fractional_kelly`) whose `source:` is omitted
    /// or set to `!strategy_book`; `portfolio_book` is the portfolio's
    /// aggregate `Book` — only `Some` inside a
    /// [`Portfolio`](crate::portfolio::Portfolio) weight scope, and read
    /// only by book-reading nodes whose `source:` is `!portfolio_book`;
    /// `schema` is the overlay [`Schema`] the atom stream carries, used by
    /// `!get { key }` to look up the column's declared [`OverlayType`] and
    /// dispatch to the right typed leaf; `root` is the **blessed series** —
    /// which asset a `source:`-omitted leaf reads out of the snapshot (see
    /// `pick_root`). Pass `None` from a context with no single blessed
    /// series and every price leaf must name its asset.
    pub fn build(
        &self,
        anchor: &Position,
        book: &Book,
        portfolio_book: Option<&Book>,
        schema: &Arc<Schema>,
        root: Option<&Selector<String>>,
    ) -> Box<dyn DynIndicator> {
        self.try_build(anchor, book, portfolio_book, schema, root)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// The fallible twin of [`build`](Self::build) — the one the spec drivers
    /// call.
    ///
    /// Every construction failure reachable from a user-authored document is an
    /// `Err` here rather than a panic: an unknown `!get` column, a malformed
    /// `!pick { freq }`, a slot handed the wrong type, a `!value <list>` or
    /// `!portfolio_book` used outside the one scope where it means anything.
    ///
    /// The error is a plain message carrying the crate's `!tag > ` breadcrumb
    /// convention: each level prepends its own tag as the error rises, so the
    /// caller receives `!above > !add > !get > unknown overlay column "foo"`
    /// and [`diagnostics::split_trail`](crate::spec::diagnostics::split_trail)
    /// renders the path on its own line.
    pub fn try_build(
        &self,
        anchor: &Position,
        book: &Book,
        portfolio_book: Option<&Book>,
        schema: &Arc<Schema>,
        root: Option<&Selector<String>>,
    ) -> Result<Box<dyn DynIndicator>, String> {
        self.try_build_inner(anchor, book, portfolio_book, schema, root)
            .map_err(|e| trail(self, e))
    }

    /// The match itself. Wrapped by [`try_build`](Self::try_build), which
    /// prepends this node's tag — so an error raised anywhere in the tree
    /// arrives at the caller carrying the full path down to it.
    fn try_build_inner(
        &self,
        anchor: &Position,
        book: &Book,
        portfolio_book: Option<&Book>,
        schema: &Arc<Schema>,
        root: Option<&Selector<String>>,
    ) -> Result<Box<dyn DynIndicator>, String> {
        use NodeSpec::*;
        // Recursive-build shorthands: build `s`, view it as a library-typed
        // `Indicator<Input=Snapshot, Output=Real>` (or Candle) so it drops
        // into a concrete library constructor. A type mismatch is attributed to
        // the *child* that produced the wrong output, which is where the author
        // has to look.
        let real = |s: &NodeSpec| -> Result<AsReal, String> {
            let built = s.try_build(anchor, book, portfolio_book, schema, root)?;
            AsReal::try_new(built).map_err(|e| trail(s, e))
        };
        let candle = |s: &NodeSpec| -> Result<AsCandle, String> {
            let built = s.try_build(anchor, book, portfolio_book, schema, root)?;
            AsCandle::try_new(built).map_err(|e| trail(s, e))
        };
        // Boolean-signal shorthands, for the absorbed signal variants.
        let boolean = |s: &NodeSpec| -> Result<AsBool, String> {
            let built = s.try_build(anchor, book, portfolio_book, schema, root)?;
            AsBool::try_new(built).map_err(|e| trail(s, e))
        };
        let str_view = |s: &NodeSpec| -> Result<AsStr, String> {
            let built = s.try_build(anchor, book, portfolio_book, schema, root)?;
            AsStr::try_new(built).map_err(|e| trail(s, e))
        };
        let str_operand = |s: &StrOperand| -> Result<AsStr, String> {
            let built = s.try_build(anchor, book, portfolio_book, schema, root)?;
            AsStr::try_new(built).map_err(|e| match s {
                StrOperand::Expr(e2) => trail(e2, e),
                StrOperand::Literal(_) => e,
            })
        };
        // The `Pick`-shaped `source:` field on every atom-input leaf.
        let atom_src = |source: Option<&Box<NodeSpec>>| {
            atom_source_of(
                source.map(|b| &**b),
                anchor,
                book,
                portfolio_book,
                schema,
                root,
            )
        };
        // Symbol-agnostic variant for calendar accessors + `!time`: an
        // omitted `source:` defaults to the "any entry" PickAny rather
        // than the sole-atom Pick, so a bare `!month` / `!hour` / `!time`
        // (and the cadence sugar `!daily` / `!monthly` / …) works on
        // multi-symbol snapshots — every entry shares atom.time.
        let atom_src_any = |source: Option<&Box<NodeSpec>>| {
            atom_source_any_of(
                source.map(|b| &**b),
                anchor,
                book,
                portfolio_book,
                schema,
                root,
            )
        };

        Ok(match self {
            // --- atom-input leaves ---
            Close { source } => {
                let s = atom_src(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::Close::of(s))
            }
            High { source } => {
                let s = atom_src(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::High::of(s))
            }
            Low { source } => {
                let s = atom_src(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::Low::of(s))
            }
            Open { source } => {
                let s = atom_src(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::Open::of(s))
            }
            Volume { source } => {
                let s = atom_src(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::Volume::of(s))
            }
            Typical { source } => {
                let s = atom_src(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::Typical::of(s))
            }
            Median { source } => {
                let s = atom_src(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::Median::of(s))
            }
            Current { source } => {
                let s = atom_src(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::CurrentBar::of(s))
            }

            Pick { symbol, freq } => build_pick(symbol.as_deref(), freq.as_deref(), root)?,

            Value(ValueLit::Real(x)) => dyn_indicator::wrap(self::Value::<Snapshot<String>>::new(*x)),
            Value(ValueLit::Bool(b)) => {
                dyn_indicator::wrap(crate::indicators::ValueBool::<Snapshot<String>>::new(*b))
            }
            Value(ValueLit::Str(s)) => {
                dyn_indicator::wrap(ValueStr::<Snapshot<String>>::new(s.as_str()))
            }
            Value(ValueLit::List(_)) => {
                return Err("a list literal is only meaningful in a \
                            portfolio weight-share template — the per-child \
                            build pass rewrites it to !value <list[CHILD_INDEX]> \
                            before this arm ever runs. Either it's being used \
                            outside a portfolio, or PortfolioSpec::build failed \
                            to install the CHILD_INDEX arg."
                    .to_string());
            }
            Entry => dyn_indicator::wrap(anchor.entry::<Snapshot<String>>()),
            Peak => dyn_indicator::wrap(anchor.peak::<Snapshot<String>>()),
            Trough => dyn_indicator::wrap(anchor.trough::<Snapshot<String>>()),

            StrategyBook | PortfolioBook => {
                return Err("a build-time source selector — it only makes \
                            sense as the `source:` of a book-reading node (e.g. \
                            `!drawdown { source: !portfolio_book }`), not as a \
                            standalone expression"
                    .to_string());
            }

            Equity { source } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                dyn_indicator::wrap(b.equity::<Snapshot<String>>())
            }
            EquityPeak { source } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                dyn_indicator::wrap(b.equity_peak::<Snapshot<String>>())
            }
            Drawdown { source } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                dyn_indicator::wrap(b.drawdown::<Snapshot<String>>())
            }
            ReturnPerBar { source } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                dyn_indicator::wrap(b.return_per_bar::<Snapshot<String>>())
            }
            TradePnl { source } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                dyn_indicator::wrap(b.trade_pnl::<Snapshot<String>>())
            }
            TradeReturn { source } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                dyn_indicator::wrap(b.trade_return::<Snapshot<String>>())
            }

            Get { key, source } => {
                let s = atom_src(source.as_ref())?;
                build_get(schema, key, s)?
            }

            Ema { source, period } => dyn_indicator::wrap(self::Ema::new(real(source)?, *period)),
            Sma { source, period } => dyn_indicator::wrap(self::Sma::new(real(source)?, *period)),
            Rma { source, period } => dyn_indicator::wrap(self::Rma::new(real(source)?, *period)),
            Wma { source, period } => dyn_indicator::wrap(self::Wma::new(real(source)?, *period)),
            Hma { source, period } => dyn_indicator::wrap(self::Hma::new(real(source)?, *period)),
            Rsi { source, period } => dyn_indicator::wrap(self::Rsi::new(real(source)?, *period)),
            StdDev { source, period } => {
                dyn_indicator::wrap(self::StdDev::new(real(source)?, *period))
            }
            Skewness { source, period } => {
                dyn_indicator::wrap(self::Skewness::new(real(source)?, *period))
            }
            Kurtosis { source, period } => {
                dyn_indicator::wrap(self::Kurtosis::new(real(source)?, *period))
            }
            ZScore { source, period } => {
                dyn_indicator::wrap(self::ZScore::new(real(source)?, *period))
            }
            Percentile {
                source,
                period,
                pct,
            } => dyn_indicator::wrap(self::Percentile::new(real(source)?, *period, *pct)),
            PercentileRank { source, period } => {
                dyn_indicator::wrap(self::PercentileRank::new(real(source)?, *period))
            }
            BarsSince { source } => {
                // Same shape as `IfElse`'s `cond`: a signal leg is built
                // a boolean-output NodeSpec, viewed as bool.
                let sig = {
                    let built = source.try_build(anchor, book, portfolio_book, schema, root)?;
                    AsBool::try_new(built).map_err(|e| trail(source, e))?
                };
                dyn_indicator::wrap(self::BarsSince::new(sig))
            }
            BarsSinceHigh { source, period } => {
                dyn_indicator::wrap(self::BarsSinceHigh::new(real(source)?, *period))
            }
            BarsSinceLow { source, period } => {
                dyn_indicator::wrap(self::BarsSinceLow::new(real(source)?, *period))
            }
            Correlation { lhs, rhs, period } => {
                dyn_indicator::wrap(self::Correlation::new(real(lhs)?, real(rhs)?, *period))
            }
            VarianceRatio {
                source,
                period,
                lag,
            } => dyn_indicator::wrap(self::VarianceRatio::new(real(source)?, *period, *lag)),
            Cci { source, period } => dyn_indicator::wrap(self::Cci::new(real(source)?, *period)),
            Stochastic { source, period } => {
                dyn_indicator::wrap(self::Stochastic::new(real(source)?, *period))
            }
            StochRsi {
                source,
                rsi_period,
                stoch_period,
            } => dyn_indicator::wrap(self::StochRsi::new(
                self::Rsi::new(real(source)?, *rsi_period),
                *stoch_period,
            )),

            MacdLine {
                source,
                fast,
                slow,
                signal,
            } => dyn_indicator::wrap(Component::new(
                Macd::new(real(source)?, *fast, *slow, *signal),
                |v: MacdValue| v.macd,
            )),
            MacdSignal {
                source,
                fast,
                slow,
                signal,
            } => dyn_indicator::wrap(Component::new(
                Macd::new(real(source)?, *fast, *slow, *signal),
                |v: MacdValue| v.signal,
            )),
            MacdHistogram {
                source,
                fast,
                slow,
                signal,
            } => dyn_indicator::wrap(Component::new(
                Macd::new(real(source)?, *fast, *slow, *signal),
                |v: MacdValue| v.histogram,
            )),

            BbUpper { source, period, k } => dyn_indicator::wrap(Component::new(
                Bollinger::new(real(source)?, *period, *k),
                |v: BollingerValue| v.upper,
            )),
            BbMiddle { source, period, k } => dyn_indicator::wrap(Component::new(
                Bollinger::new(real(source)?, *period, *k),
                |v: BollingerValue| v.middle,
            )),
            BbLower { source, period, k } => dyn_indicator::wrap(Component::new(
                Bollinger::new(real(source)?, *period, *k),
                |v: BollingerValue| v.lower,
            )),

            KeltnerUpper {
                source,
                candle_source,
                ema_period,
                atr_period,
                multiplier,
            } => dyn_indicator::wrap(Component::new(
                Keltner::new(
                    real(source)?,
                    candle(candle_source)?,
                    *ema_period,
                    *atr_period,
                    *multiplier,
                ),
                |v: KeltnerValue| v.upper,
            )),
            KeltnerMiddle {
                source,
                candle_source,
                ema_period,
                atr_period,
                multiplier,
            } => dyn_indicator::wrap(Component::new(
                Keltner::new(
                    real(source)?,
                    candle(candle_source)?,
                    *ema_period,
                    *atr_period,
                    *multiplier,
                ),
                |v: KeltnerValue| v.middle,
            )),
            KeltnerLower {
                source,
                candle_source,
                ema_period,
                atr_period,
                multiplier,
            } => dyn_indicator::wrap(Component::new(
                Keltner::new(
                    real(source)?,
                    candle(candle_source)?,
                    *ema_period,
                    *atr_period,
                    *multiplier,
                ),
                |v: KeltnerValue| v.lower,
            )),

            DonchianUpper { high, low, period } => dyn_indicator::wrap(Component::new(
                Donchian::new(real(high)?, real(low)?, *period),
                |v: DonchianValue| v.upper,
            )),
            DonchianMiddle { high, low, period } => dyn_indicator::wrap(Component::new(
                Donchian::new(real(high)?, real(low)?, *period),
                |v: DonchianValue| v.middle,
            )),
            DonchianLower { high, low, period } => dyn_indicator::wrap(Component::new(
                Donchian::new(real(high)?, real(low)?, *period),
                |v: DonchianValue| v.lower,
            )),

            Adx { source, period } => dyn_indicator::wrap(Component::new(
                self::Adx::new(candle(source)?, *period),
                |v: AdxValue| v.adx,
            )),
            PlusDi { source, period } => dyn_indicator::wrap(Component::new(
                self::Adx::new(candle(source)?, *period),
                |v: AdxValue| v.plus_di,
            )),
            MinusDi { source, period } => dyn_indicator::wrap(Component::new(
                self::Adx::new(candle(source)?, *period),
                |v: AdxValue| v.minus_di,
            )),
            DmiPlusDi { source, period } => dyn_indicator::wrap(Component::new(
                self::Dmi::new(candle(source)?, *period),
                |v: DmiValue| v.plus_di,
            )),
            DmiMinusDi { source, period } => dyn_indicator::wrap(Component::new(
                self::Dmi::new(candle(source)?, *period),
                |v: DmiValue| v.minus_di,
            )),

            AroonUp { source, period } => dyn_indicator::wrap(Component::new(
                self::Aroon::new(candle(source)?, *period),
                |v: AroonValue| v.up,
            )),
            AroonDown { source, period } => dyn_indicator::wrap(Component::new(
                self::Aroon::new(candle(source)?, *period),
                |v: AroonValue| v.down,
            )),
            AroonOscillator { source, period } => dyn_indicator::wrap(Component::new(
                self::Aroon::new(candle(source)?, *period),
                |v: AroonValue| v.oscillator,
            )),

            Atr { source, period } => dyn_indicator::wrap(self::Atr::new(candle(source)?, *period)),
            Parkinson { source, period } => {
                dyn_indicator::wrap(self::Parkinson::new(candle(source)?, *period))
            }
            GarmanKlass { source, period } => {
                dyn_indicator::wrap(self::GarmanKlass::new(candle(source)?, *period))
            }
            RogersSatchell { source, period } => {
                dyn_indicator::wrap(self::RogersSatchell::new(candle(source)?, *period))
            }
            Mfi { source, period } => dyn_indicator::wrap(self::Mfi::new(candle(source)?, *period)),
            WilliamsR { source, period } => {
                dyn_indicator::wrap(self::WilliamsR::new(candle(source)?, *period))
            }
            Obv { source } => dyn_indicator::wrap(self::Obv::new(candle(source)?)),
            Vwap { source, period } => {
                dyn_indicator::wrap(self::Vwap::new(candle(source)?, *period))
            }
            Ad { source } => dyn_indicator::wrap(self::Ad::new(candle(source)?)),
            TrueRange { source } => dyn_indicator::wrap(self::TrueRange::new(candle(source)?)),
            Sar { source, step, max } => {
                dyn_indicator::wrap(self::Sar::new(candle(source)?, *step, *max))
            }

            VolTarget {
                source,
                target,
                window,
                bars_per_year,
            } => {
                let s = atom_src(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::sizing::vol_target_of::<String, _>(
                    s,
                    *target,
                    *window,
                    *bars_per_year,
                ))
            }
            AtrRisk {
                source,
                risk_frac,
                period,
                atr_multiple,
            } => {
                let s = atom_src(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::sizing::atr_risk_of::<String, _>(
                    s,
                    *risk_frac,
                    *period,
                    *atr_multiple,
                ))
            }
            DrawdownThrottle {
                source,
                max_drawdown,
            } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                dyn_indicator::wrap(crate::indicators::sizing::drawdown_throttle::<String>(
                    b,
                    *max_drawdown,
                ))
            }
            EquityVolTarget {
                source,
                target,
                window,
                bars_per_year,
            } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                dyn_indicator::wrap(
                    crate::indicators::sizing::equity_vol_target::<String>(
                        b,
                        *target,
                        *window,
                        *bars_per_year,
                    ),
                )
            }
            FractionalKelly {
                source,
                kelly_fraction,
                window,
            } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                dyn_indicator::wrap(crate::indicators::sizing::fractional_kelly::<String>(
                    b,
                    *kelly_fraction,
                    *window,
                ))
            }

            // Trailing risk indicators own an embedded strategy; they ignore
            // the enclosing `anchor`/`book` (the embedded strategy builds its
            // own) and delegate to the rebuild-on-clone wrapper.
            Sharpe {
                strategy,
                period,
                bars_per_year,
                risk_free_rate,
            } => trailing::build(
                TrailingMetric::Sharpe,
                strategy,
                *period,
                *risk_free_rate,
                *bars_per_year,
                schema,
            ),
            Sortino {
                strategy,
                period,
                bars_per_year,
                risk_free_rate,
            } => trailing::build(
                TrailingMetric::Sortino,
                strategy,
                *period,
                *risk_free_rate,
                *bars_per_year,
                schema,
            ),
            Volatility {
                strategy,
                period,
                bars_per_year,
            } => trailing::build(
                TrailingMetric::Volatility,
                strategy,
                *period,
                0.0,
                *bars_per_year,
                schema,
            ),
            MaxDrawdown { strategy, period } => trailing::build(
                TrailingMetric::MaxDrawdown,
                strategy,
                *period,
                0.0,
                0.0,
                schema,
            ),
            Calmar {
                strategy,
                period,
                bars_per_year,
            } => trailing::build(
                TrailingMetric::Calmar,
                strategy,
                *period,
                0.0,
                *bars_per_year,
                schema,
            ),

            Add { lhs, rhs } => dyn_indicator::wrap(real(lhs)?.add(real(rhs)?)),
            Sub { lhs, rhs } => dyn_indicator::wrap(real(lhs)?.sub(real(rhs)?)),
            Mul { lhs, rhs } => dyn_indicator::wrap(real(lhs)?.mul(real(rhs)?)),
            Div { lhs, rhs } => dyn_indicator::wrap(real(lhs)?.div(real(rhs)?)),
            IfElse {
                cond,
                then,
                otherwise,
            } => {
                let cond_ind = {
                    let built = cond.try_build(anchor, book, portfolio_book, schema, root)?;
                    AsBool::try_new(built).map_err(|e| trail(cond, e))?
                };
                let t_ind = real(then)?;
                let f_ind = real(otherwise)?;
                dyn_indicator::wrap(self::IfElse::new(cond_ind, t_ind, f_ind))
            }
            Match { on, cases, default } => {
                build_match(on, cases, default, anchor, book, portfolio_book, schema, root)?
            }
            Lag { source, period } => dyn_indicator::wrap(real(source)?.lag(*period)),
            Diff { source, period } => dyn_indicator::wrap(real(source)?.diff(*period)),
            Ratio { source, period } => dyn_indicator::wrap(real(source)?.ratio(*period)),
            Roc { source, period } => dyn_indicator::wrap(real(source)?.roc(*period)),
            RollingMax { source, period } => {
                dyn_indicator::wrap(real(source)?.rolling_max(*period))
            }
            RollingMin { source, period } => {
                dyn_indicator::wrap(real(source)?.rolling_min(*period))
            }
            Log { source, base } => dyn_indicator::wrap(self::Log::new(real(source)?, *base)),
            Latch { source } => {
                let inner = {
                    let built = source.try_build(anchor, book, portfolio_book, schema, root)?;
                    AsReal::try_new(built).map_err(|e| trail(source, e))?
                };
                dyn_indicator::wrap(self::Latch::new(inner))
            }
            Resample {
                every,
                inner,
                source,
            } => {
                assert!(*every > 0, "resample every must be greater than zero");
                let candle_src = candle(source)?;
                let resample_dyn = dyn_indicator::wrap(self::Resample::new(candle_src, *every));
                let inner_dyn = inner.try_build(anchor, book, portfolio_book, schema, root)?;
                dyn_indicator::try_chain(resample_dyn, inner_dyn).map_err(|e| trail(inner, e))?
            }
            Unstable { source } => {
                dyn_indicator::unstable_wrap(source.try_build(anchor, book, portfolio_book, schema, root)?)
            }

            Year { source } => {
                let s = atom_src_any(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::Year::of(s))
            }
            Month { source } => {
                let s = atom_src_any(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::Month::of(s))
            }
            Day { source } => {
                let s = atom_src_any(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::Day::of(s))
            }
            Hour { source } => {
                let s = atom_src_any(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::Hour::of(s))
            }
            Minute { source } => {
                let s = atom_src_any(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::Minute::of(s))
            }
            Second { source } => {
                let s = atom_src_any(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::Second::of(s))
            }
            DayOfWeek { source } => {
                let s = atom_src_any(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::DayOfWeek::of(s))
            }
            DayOfYear { source } => {
                let s = atom_src_any(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::DayOfYear::of(s))
            }
            WeekOfYear { source } => {
                let s = atom_src_any(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::WeekOfYear::of(s))
            }
            Quarter { source } => {
                let s = atom_src_any(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::Quarter::of(s))
            }
            UnixSeconds { source } => {
                let s = atom_src_any(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::UnixSeconds::of(s))
            }
            UnixMillis { source } => {
                let s = atom_src_any(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::UnixMillis::of(s))
            }
            Time { source } => {
                let s = atom_src_any(source.as_ref())?;
                dyn_indicator::wrap(crate::indicators::CurrentTime::of(s))
            }

            // --- absorbed boolean signals ---
            Gt { lhs, rhs, epsilon } => dyn_indicator::wrap(compare::Gt::with_epsilon(
                real(lhs)?,
                real(rhs)?,
                eps(epsilon),
            )),
            Lt { lhs, rhs, epsilon } => dyn_indicator::wrap(compare::Lt::with_epsilon(
                real(lhs)?,
                real(rhs)?,
                eps(epsilon),
            )),
            Ge { lhs, rhs, epsilon } => dyn_indicator::wrap(compare::Ge::with_epsilon(
                real(lhs)?,
                real(rhs)?,
                eps(epsilon),
            )),
            Le { lhs, rhs, epsilon } => dyn_indicator::wrap(compare::Le::with_epsilon(
                real(lhs)?,
                real(rhs)?,
                eps(epsilon),
            )),
            Eq { lhs, rhs, epsilon } => build_polymorphic_eq(
                lhs, rhs, *epsilon, false, anchor, book, portfolio_book, schema, root,
            )?,
            Ne { lhs, rhs, epsilon } => build_polymorphic_eq(
                lhs, rhs, *epsilon, true, anchor, book, portfolio_book, schema, root,
            )?,
            Above { source, level } => dyn_indicator::wrap(real(source)?.above(*level)),
            Below { source, level } => dyn_indicator::wrap(real(source)?.below(*level)),
            CrossesAbove { lhs, rhs } => {
                let (l, r) = (real(lhs)?, real(rhs)?);
                let cmp = l.gt(r);
                dyn_indicator::wrap(cmp.clone().and(cmp.changed()))
            }
            CrossesBelow { lhs, rhs } => {
                let (l, r) = (real(lhs)?, real(rhs)?);
                let cmp = l.lt(r);
                dyn_indicator::wrap(cmp.clone().and(cmp.changed()))
            }
            And { lhs, rhs } => dyn_indicator::wrap(boolean(lhs)?.and(boolean(rhs)?)),
            Or { lhs, rhs } => dyn_indicator::wrap(boolean(lhs)?.or(boolean(rhs)?)),
            Xor { lhs, rhs } => dyn_indicator::wrap(boolean(lhs)?.xor(boolean(rhs)?)),
            All(specs) => {
                if specs.is_empty() {
                    dyn_indicator::wrap(crate::indicators::ValueBool::<Snapshot<String>>::new(true))
                } else {
                    let mut acc = boolean(&specs[0])?;
                    for s in &specs[1..] {
                        let next = boolean(s)?;
                        acc = AsBool::new(dyn_indicator::wrap(acc.and(next)));
                    }
                    dyn_indicator::wrap(acc)
                }
            }
            Any(specs) => {
                if specs.is_empty() {
                    dyn_indicator::wrap(crate::indicators::ValueBool::<Snapshot<String>>::new(false))
                } else {
                    let mut acc = boolean(&specs[0])?;
                    for s in &specs[1..] {
                        let next = boolean(s)?;
                        acc = AsBool::new(dyn_indicator::wrap(acc.or(next)));
                    }
                    dyn_indicator::wrap(acc)
                }
            }
            Not(inner) => dyn_indicator::wrap(boolean(inner)?.not()),
            Changed(inner) => {
                let built = inner.try_build(anchor, book, portfolio_book, schema, root)?;
                match built.output_type() {
                    DynType::Bool => dyn_indicator::wrap(AsBool::new(built).changed()),
                    DynType::Real => dyn_indicator::wrap(AsReal::new(built).changed()),
                    other => {
                        return Err(trail(
                            inner,
                            format!("!changed needs a Bool or Real inner, got {other}"),
                        ));
                    }
                }
            }
            BecameTrue(inner) => dyn_indicator::wrap(boolean(inner)?.became_true()),
            BecameFalse(inner) => dyn_indicator::wrap(boolean(inner)?.became_false()),
            StrEq { lhs, rhs } => {
                dyn_indicator::wrap(compare::StrEq::new(str_view(lhs)?, str_operand(rhs)?))
            }
            StrNe { lhs, rhs } => {
                dyn_indicator::wrap(compare::StrNe::new(str_view(lhs)?, str_operand(rhs)?))
            }
            Never => {
                dyn_indicator::wrap(crate::indicators::ValueBool::<Snapshot<String>>::new(false))
            }
            Every(n) => dyn_indicator::wrap(crate::indicators::Every::<Snapshot<String>>::new(*n)),
            IsWeekday => dyn_indicator::wrap(crate::indicators::IsWeekday::of(pick_any_root())),
            IsWeekend => dyn_indicator::wrap(crate::indicators::IsWeekend::of(pick_any_root())),
            HasColumn { name } => {
                let exists = schema.index_of(name.as_str()).is_some();
                dyn_indicator::wrap(crate::indicators::ValueBool::<Snapshot<String>>::new(exists))
            }
        })
    }
}

/// Prepend `spec`'s own tag to an error message, building the ` > `-separated
/// breadcrumb inside-out as the failure rises through the recursive build.
///
/// The same convention the parse layer uses (see the `!tag > ` prefixing in
/// [`NodeSpec::try_from`]), so build errors and parse errors render through the
/// one [`split_trail`](crate::spec::diagnostics::split_trail) path.
fn trail(spec: &NodeSpec, message: impl std::fmt::Display) -> String {
    format!("{} > {message}", crate::spec::typecheck::tag_name(spec))
}

/// Build a `!pick { symbol, freq }` leaf. Both fields are optional; the
/// empty selector (`!pick {}`) behaves as the single-entry sole-atom unpack
/// every atom-input leaf uses by default. A `freq` string is parsed via
/// [`Frequency::from_str`] (the `N<unit>` alphabet: `1m`/`4h`/`1d`/`1w`/`1M`);
/// a parse failure is an `Err` with the offending string included.
///
/// An omitted `symbol:` adopts the blessed series' symbol when a `root` is in
/// play, so inside a rooted context both `!pick {}` and `!pick { freq: 1h }`
/// mean "my own series" rather than "whichever entry sorts first". Naming a
/// symbol explicitly always wins — that's how a leaf reaches across to another
/// asset, and it stays a strict [`Pick::matching`] that reads `None` on a bar
/// where the named asset is absent.
fn build_pick(
    symbol: Option<&str>,
    freq: Option<&str>,
    root: Option<&Selector<String>>,
) -> Result<Box<dyn DynIndicator>, String> {
    let named = symbol.is_some();
    let sym = symbol
        .map(String::from)
        .or_else(|| root.and_then(|r| r.symbol.clone()));
    let f = match freq {
        Some(s) => Some(Frequency::from_str(s).map_err(|e| {
            format!("invalid frequency {s:?}: {e}")
        })?),
        None => None,
    };
    let selector = Selector::<String> {
        symbol: sym,
        freq: f,
    };
    Ok(if selector.is_empty() {
        dyn_indicator::wrap(Pick::<String>::new())
    } else if named {
        dyn_indicator::wrap(Pick::<String>::matching(selector))
    } else {
        // Symbol came from the root, so this is still the implicit
        // "this series" read — keep the sole-atom fallback that makes an
        // untagged single-entry snapshot resolve. See `Pick::rooted`.
        dyn_indicator::wrap(Pick::<String>::rooted(selector))
    })
}

/// Build a `!get { key, source }` leaf: look up the column's declared
/// [`OverlayType`] in `schema` and dispatch to the matching typed
/// [`GetReal`] / [`GetBool`] / [`GetStr`] leaf, rooted on the caller-provided
/// atom source (typically the implicit `Pick::new()` unpack, or an explicit
/// `!pick { symbol, freq }` for cross-asset overlays).
///
/// Returns `Err` with a helpful message if `key` isn't registered — the message
/// lists the schema's registered keys so a typo is easy to spot. The message
/// distinguishes the empty-schema case ("no overlay side channel — feed
/// `--series` or `csv:` data with additional columns to attach overlays")
/// from the non-empty case ("registered: a, b, c").
fn build_get(
    schema: &Arc<Schema>,
    key: &str,
    source: AsAtom,
) -> Result<Box<dyn DynIndicator>, String> {
    match schema.type_of_key(key) {
        Some(OverlayType::Real) => Ok(dyn_indicator::wrap(GetReal::of(schema, key, source))),
        Some(OverlayType::Bool) => Ok(dyn_indicator::wrap(GetBool::of(schema, key, source))),
        Some(OverlayType::Str) => Ok(dyn_indicator::wrap(GetStr::of(schema, key, source))),
        None => {
            let registered: Vec<&str> = schema.keys().collect();
            if registered.is_empty() {
                Err(format!(
                    "overlay column {key:?}: no overlay side channel is bound — feed \
                     `--series` data or a `csv:` source that carries additional \
                     (non-OHLCV) columns to attach overlays",
                ))
            } else {
                Err(format!(
                    "overlay column {key:?} is not registered. Registered columns: {}",
                    registered.join(", "),
                ))
            }
        }
    }
}
