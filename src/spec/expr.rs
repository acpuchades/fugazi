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

use std::num::NonZeroUsize;
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
    Abs, Ad, Adx, AdxValue, Aroon, AroonValue, Atr, BarsSince, BarsSinceHigh, BarsSinceLow, Beta,
    Bollinger, BollingerValue, Book, Cci, Component, Correlation, Covariance, CumMax, CumMin,
    CumSum, Dmi, DmiValue, Donchian, DonchianValue, Ema, Exp, GarmanKlass, GetBool, GetReal,
    GetStr, Hma, IfElse, Keltner, KeltnerValue, Kurtosis, Latch, LinReg, Log, Macd, MacdValue,
    Match as MatchIndicator, Max as MaxOf, Mfi, Min as MinOf, Obv, Parkinson, Percentile,
    PercentileRank, Pick, PickAny, Position, Pow, Resample, Rma, RogersSatchell, Rsi, Sar, Sigmoid,
    Sign, Skewness, Sma, Sqrt, StdDev, StochRsi, Stochastic, Tanh, TrueRange, Value, ValueStr,
    VarianceRatio, Vwap, WilliamsR, Wma, ZScore,
};
use crate::prelude::*;
use crate::types::Snapshot;

use super::trailing::{self, AnyStrategyRef, TrailingMetric};
use crate::indicators::compare;
use crate::runtime::{AnyChain, AtomChain, BoolChain, CandleChain, RealChain, StrChain, any};
use crate::spec::dyn_indicator::PayloadType;

use crate::Selector;
use crate::types::Symbol;

/// Where a `source:`-omitted leaf reads from — the **blessed series** of the
/// context doing the build, or the reason there isn't one.
///
/// A blessed series is the declared `symbol:` of a
/// [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy), the leg of
/// a basket / multi-asset factory, the `(symbol, freq)` key of an overlay
/// column. With one in play a bare `!close` resolves *by name* out of the
/// snapshot ([`Pick::rooted`]), which is what lets it coexist with a
/// `!close { source: !pick { symbol: SPY } }` reaching across the same bar.
///
/// # Why the absence of one is two different things
///
/// This used to be a plain `Option<&Selector<Symbol>>`, and `None` meant both
/// "single-series, so unpack the sole entry" and "genuinely multi-asset, so a
/// bare price leaf has no answer". The second case built a [`Pick::new`]
/// anyway and **panicked at run time** on the first 2-entry snapshot — after
/// `check` had passed the document, so there was no way to find out ahead of
/// the run. A `pairs:` document sizing on `!vol_target` (which reads prices,
/// though it looks like a scalar knob) hit exactly that.
///
/// Distinguishing the two lets the ambiguous case be a *build error* instead,
/// per the crate's "build errors are values" invariant. Every leaf already
/// funnels through `root_source` / `build_pick`, so there is no second table
/// to keep in sync — a new tag inherits the check for free.
#[derive(Clone, Copy, Default)]
pub struct Root<'a> {
    blessed: Option<&'a super::root::RootSpec>,
    /// Set only when there is no blessed series *and* the shape guarantees
    /// more than one asset. Names the shape, for the error message.
    ambiguous: Option<&'static str>,
}

impl<'a> Root<'a> {
    /// This context blesses `spec` — a bare leaf reads the series that
    /// expression names.
    pub fn blessed(spec: &'a super::root::RootSpec) -> Self {
        Self {
            blessed: Some(spec),
            ambiguous: None,
        }
    }

    /// No blessed series, but the snapshot is expected to carry one entry, so
    /// a bare leaf unpacks it. The default, and what every single-series
    /// context (and every caller that never had a root to give) wants.
    pub fn sole() -> Self {
        Self::default()
    }

    /// No blessed series and **more than one asset by construction**, so a
    /// bare price leaf is a bad document rather than an unlucky bar. `shape`
    /// names the document shape for the error.
    ///
    /// Reserved for shapes whose arity is known at *build* time. A portfolio
    /// `rebalance_on:` gate is not one: its universe may well be a single
    /// symbol, and rejecting a bare `!close` there would break documents that
    /// work today.
    pub fn ambiguous(shape: &'static str) -> Self {
        Self {
            blessed: None,
            ambiguous: Some(shape),
        }
    }

    /// Bless `spec` when the context has one, else fall back to the
    /// sole-atom unpack — for callers holding an `Option` already.
    pub fn or_sole(spec: Option<&'a super::root::RootSpec>) -> Self {
        Self {
            blessed: spec,
            ambiguous: None,
        }
    }

    /// The blessed root expression, if any.
    pub(super) fn spec(self) -> Option<&'a super::root::RootSpec> {
        self.blessed
    }

    /// The single symbol the blessed root names, when it names exactly one.
    ///
    /// Feeds the one place a leaf still needs the *name* rather than the
    /// series: an explicit `!pick { freq: … }` that omits `symbol:` and means
    /// "my own series at that cadence". `None` when there is no root, or when
    /// the root names none or several — in which case the leaf falls back to
    /// the same sole-atom unpack it would have used without a root.
    pub(super) fn blessed_symbol(self) -> Option<Symbol> {
        let named = self.blessed?.named_symbols();
        let mut it = named.into_iter();
        match (it.next(), it.next()) {
            (Some(one), None) => Some(crate::types::symbol(&one)),
            _ => None,
        }
    }

    /// The error a bare price leaf earns in this context, if it earns one.
    fn ambiguity(self) -> Option<String> {
        let shape = self.ambiguous?;
        Some(format!(
            "no `source:` and no blessed series to fall back on — a {shape} \
             document holds more than one asset, so which one this reads is \
             ambiguous. Name it explicitly, e.g. \
             `!close {{ source: !pick {{ symbol: BTCUSDT }} }}`"
        ))
    }
}

/// The implicit atom root of every `source:`-omitted leaf. `Err` when the
/// context has no blessed series and more than one asset — see [`Root`].
pub(super) fn root_source(
    root: Root<'_>,
    anchor: &Position,
    book: &Book,
    portfolio_book: Option<&Book>,
    schema: &Arc<Schema>,
) -> Result<AtomChain, String> {
    match root.spec() {
        // A root that is a plain selector installs `Pick::rooted` directly —
        // the blessed root's *match, else sole-atom unpack* semantics, which
        // the strict `Pick::matching` that `!pick`'s own build arm produces
        // does not have. See `RootSpec::as_pick`.
        Some(spec) if spec.as_pick().is_some() => {
            let (symbol, freq) = spec.as_pick().expect("just checked");
            // Taken verbatim: a stream id is opaque, and a cadence is only its
            // most common spelling. Parsing it here used to reject every
            // identifier that was not a duration.
            let selector = Selector::<Symbol> {
                symbol: symbol.map(crate::types::symbol),
                stream: freq.map(crate::types::stream),
            };
            Ok(crate::runtime::erase(if selector.is_empty() {
                Pick::<Symbol>::new()
            } else {
                Pick::<Symbol>::rooted(selector)
            }))
        }
        // Anything richer is built as the expression it is, with `Root::sole()`
        // as *its* own root — which is what terminates the recursion: a root
        // cannot ask for itself.
        Some(spec) => {
            let node = spec.node();
            let built = node.try_build(anchor, book, portfolio_book, schema, Root::sole())?;
            built.into_atom().map_err(|e| trail(node, e))
        }
        None => match root.ambiguity() {
            Some(error) => Err(error),
            None => Ok(crate::runtime::erase(Pick::<Symbol>::new())),
        },
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
/// Contrast with [`root_source`], which has to either root, unpack a sole
/// entry, or refuse — price-field leaves (`!close`, `!high`, …) genuinely
/// depend on *which* asset.
///
/// Deliberately takes **no** `root`: re-rooting a calendar leaf onto one
/// symbol would make it read `None` on a bar where that symbol happens to be
/// absent, when the answer it wants — the bar's time — is right there on every
/// other entry.
pub(super) fn pick_any_root() -> PickAny<Symbol> {
    PickAny::<Symbol>::new()
}

/// Default `seed:` for the two self-referential sizers — full base size.
///
/// `1.0` rather than `0.0` because a zero seed *is* the deadlock: sizing `0`
/// skips the trade, so the recipe never gets the closed trade (or the equity
/// movement) it needs to produce a real number. Full size until it knows
/// better is also what the recipes approximate once warm.
fn default_sizing_seed() -> Real {
    1.0
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
/// Validate a sampler's bucket threshold as a **build error**, not a panic.
///
/// `Accumulate::new` asserts, which is right for a Rust caller who wrote the
/// number in code. A threshold reaching us from a YAML document is *input*, and
/// bad input is reported — the same rule every other `try_build` arm follows.
/// `NonZeroUsize` does this job for `!resample`'s `every`; a `Real` has no such
/// spelling, so the check is explicit here.
fn positive_threshold(threshold: Real, tag: &str) -> Result<Real, String> {
    if threshold.is_finite() && threshold > 0.0 {
        Ok(threshold)
    } else {
        Err(format!(
            "`{tag}` needs a finite threshold greater than zero, got {threshold}"
        ))
    }
}

pub(super) fn default_bar_source() -> Box<NodeSpec> {
    Box::new(NodeSpec::Current { source: None })
}

/// Default base for `!log`: natural log (`e`).
pub(super) fn default_log_base() -> Real {
    std::f64::consts::E
}

/// Default base for `!exp`: natural exponential (`e`).
pub(super) fn default_exp_base() -> Real {
    std::f64::consts::E
}

/// The bases `!log` and `!exp` admit — finite, positive, and distinct from
/// `1.0`, the same set for both so the pair stays inverse. Both constructors
/// `assert!` on it; a document naming a bad base is bad *input*, so it is
/// checked here and reported instead. Serde can't express the constraint, so
/// there is no earlier gate.
fn checked_base(base: Real) -> Result<Real, String> {
    if base.is_finite() && base > 0.0 && base != 1.0 {
        Ok(base)
    } else {
        Err(format!(
            "`base` must be a finite positive number distinct from 1.0, got {base}"
        ))
    }
}

/// The `LinReg` behind the four `!linreg_*` readings, with the one bound
/// `NonZeroUsize` cannot carry.
///
/// A single point has no slope: the fit is vertical, and the degenerate answer
/// would be a silent `0.0` rather than a refusal. The constructor `assert!`s on
/// it, so the check happens here and is reported as the bad *input* it is.
fn linreg<S>(source: S, period: NonZeroUsize) -> Result<LinReg<S>, String> {
    let period = period.get();
    if period < 2 {
        return Err(format!(
            "`period` must be at least 2, got {period} — a single point has no slope"
        ));
    }
    Ok(LinReg::new(source, period))
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
/// Wilder's `14` — RSI, ATR, ADX/DI and the DMI pair. The period Wilder
/// published each of them with, and the one every charting package ships.
pub const WILDER_PERIOD: usize = 14;
/// Money Flow Index lookback — a volume-weighted RSI, so RSI's `14`.
pub const MFI_PERIOD: usize = 14;
/// Williams %R lookback.
pub const WILLIAMS_R_PERIOD: usize = 14;
/// Stochastic %K lookback — the `14` of the conventional (14, 3, 3).
pub const STOCHASTIC_PERIOD: usize = 14;
/// Aroon lookback. TA-Lib and TradingView both ship `14`; Chande's original
/// paper used `25`, and a document wanting that spells it.
pub const AROON_PERIOD: usize = 14;
/// Commodity Channel Index lookback — Lambert's original `20`, which is also
/// what TradingView and StockCharts default to. (TA-Lib is the outlier at 14.)
pub const CCI_PERIOD: usize = 20;
/// Donchian channel lookback — the Turtles' `20`-day breakout.
pub const DONCHIAN_PERIOD: usize = 20;
/// The lookback of `!lag` / `!diff` / `!ratio` / `!roc`: one bar. `!roc {}` is
/// the per-bar return and `!diff {}` the first difference — the readings those
/// tags exist for.
pub const LOOKBACK_PERIOD: usize = 1;
/// `!percentile`'s quantile — the rolling median.
pub const PERCENTILE_PCT: Real = 0.5;
/// Lo–MacKinlay differencing lag — `q = 2`, the shortest horizon a variance
/// ratio is defined over.
pub const VARIANCE_RATIO_LAG: usize = 2;
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

/// A canonical period constant as a `NonZeroUsize`. Every caller passes one of
/// the `pub const` literals above, all of which are non-zero, so the `expect`
/// is unreachable — it is here rather than an `unwrap` so a future zero
/// constant names itself.
fn nz(period: usize) -> NonZeroUsize {
    NonZeroUsize::new(period).expect("canonical period constants are non-zero")
}

fn rsi_period_default() -> NonZeroUsize {
    nz(WILDER_PERIOD)
}
fn atr_period_default() -> NonZeroUsize {
    nz(WILDER_PERIOD)
}
fn adx_period_default() -> NonZeroUsize {
    nz(WILDER_PERIOD)
}
fn dmi_period_default() -> NonZeroUsize {
    nz(WILDER_PERIOD)
}
fn mfi_period_default() -> NonZeroUsize {
    nz(MFI_PERIOD)
}
fn williams_r_period_default() -> NonZeroUsize {
    nz(WILLIAMS_R_PERIOD)
}
fn stochastic_period_default() -> NonZeroUsize {
    nz(STOCHASTIC_PERIOD)
}
fn aroon_period_default() -> NonZeroUsize {
    nz(AROON_PERIOD)
}
fn cci_period_default() -> NonZeroUsize {
    nz(CCI_PERIOD)
}
fn donchian_period_default() -> NonZeroUsize {
    nz(DONCHIAN_PERIOD)
}
fn lookback_period_default() -> NonZeroUsize {
    nz(LOOKBACK_PERIOD)
}
fn percentile_pct_default() -> Real {
    PERCENTILE_PCT
}
fn variance_ratio_lag_default() -> NonZeroUsize {
    nz(VARIANCE_RATIO_LAG)
}
fn macd_fast_default() -> NonZeroUsize {
    nz(MACD_FAST)
}
fn macd_slow_default() -> NonZeroUsize {
    nz(MACD_SLOW)
}
fn macd_signal_default() -> NonZeroUsize {
    nz(MACD_SIGNAL)
}
fn bb_period_default() -> NonZeroUsize {
    nz(BB_PERIOD)
}
fn bb_k_default() -> Real {
    BB_K
}
fn keltner_ema_period_default() -> NonZeroUsize {
    nz(KELTNER_EMA_PERIOD)
}
fn keltner_atr_period_default() -> NonZeroUsize {
    nz(KELTNER_ATR_PERIOD)
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
fn stoch_rsi_rsi_period_default() -> NonZeroUsize {
    nz(STOCH_RSI_RSI_PERIOD)
}
fn stoch_rsi_stoch_period_default() -> NonZeroUsize {
    nz(STOCH_RSI_STOCH_PERIOD)
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
        root: Root<'_>,
    ) -> Result<AnyChain, String> {
        match self {
            StrOperand::Literal(s) => Ok(any(ValueStr::<crate::types::Snapshot<Symbol>>::new(
                s.as_str(),
            ))),
            StrOperand::Expr(e) => e.try_build(anchor, book, portfolio_book, schema, root),
        }
    }
}

/// Fail unless `node`'s statically-known output type is `want`. An
/// undecidable output (`None` — a `!get`, a hole, a passthrough over one) is
/// **skipped**, exactly as [`crate::spec::typecheck::check_immediate`] skips
/// it: those defer to the build-time `AsReal` / `AsBool` view. The message
/// names the offending tag, the same convention the breadcrumb uses.
fn expect_output(node: &NodeSpec, want: PayloadType) -> Result<(), String> {
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
        expect_output(&node, PayloadType::Real)?;
        Ok(RealNode(node))
    }
}
impl TryFrom<serde_norway::Value> for BoolNode {
    type Error = String;
    fn try_from(v: serde_norway::Value) -> Result<Self, Self::Error> {
        let node = NodeSpec::try_from(v)?;
        expect_output(&node, PayloadType::Bool)?;
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
        root: Root<'_>,
    ) -> AnyChain {
        self.0.build(anchor, book, portfolio_book, schema, root)
    }
    pub fn try_build(
        &self,
        anchor: &Position,
        book: &Book,
        portfolio_book: Option<&Book>,
        schema: &Arc<Schema>,
        root: Root<'_>,
    ) -> Result<AnyChain, String> {
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
        root: Root<'_>,
    ) -> AnyChain {
        self.0.build(anchor, book, portfolio_book, schema, root)
    }
    pub fn try_build(
        &self,
        anchor: &Position,
        book: &Book,
        portfolio_book: Option<&Book>,
        schema: &Arc<Schema>,
        root: Root<'_>,
    ) -> Result<AnyChain, String> {
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
                        other => {
                            return Err(format!(
                                "!value list element {i}: expected number, got {other:?}"
                            ));
                        }
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
/// empty-selector [`Pick::<Symbol>::new()`] — the single-entry snapshot
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
    /// The bar's close price.
    #[grammar(kind = "source")]
    Close {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The bar's high price.
    #[grammar(kind = "source")]
    High {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The bar's low price.
    #[grammar(kind = "source")]
    Low {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The bar's open price.
    #[grammar(kind = "source")]
    Open {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The bar's traded volume.
    #[grammar(kind = "source")]
    Volume {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The typical price, `(high + low + close) / 3`.
    #[grammar(kind = "source")]
    Typical {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The median price, `(high + low) / 2`.
    #[grammar(kind = "source")]
    Median {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The current bar itself — the whole [`Candle`], not a scalar. The default
    /// bar source of every bar-consuming indicator (`!atr`, `!obv`, `!adx`, …);
    /// wrap in [`NodeSpec::Resample`] to lift a downstream bar indicator
    /// onto a higher timeframe.
    #[grammar(kind = "source", output = "candle")]
    Current {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
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
        /// Asset to project from the snapshot; defaults to the blessed series.
        #[serde(default)]
        symbol: Option<String>,
        /// Bar cadence for a cross-frequency snapshot (e.g. `1h`, `1d`).
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
    // runtime value; they resolve, at build, to a `Book<Symbol>` handle that
    // a book-reading node (a bare book leaf like `!drawdown`, or a
    // book-anchored recipe like `!drawdown_throttle`) picks up via its
    // `source:` field. Bare (used as an expression on its own) is invalid
    // and reports a build error.
    /// The **strategy book** — the `Book` owned by the enclosing strategy
    /// scope (single/pairs/basket/multi/the current per-child instance of
    /// a portfolio's `weights:` expression). This is the default source of
    /// every book-reading node when its `source:` is omitted.
    #[grammar(kind = "source", output = "book")]
    StrategyBook,
    /// The **portfolio aggregate book** — the mark-to-market view of the
    /// composite [`Portfolio`](crate::portfolio::Portfolio). Only meaningful
    /// inside a portfolio's `weights:` expression; reported as a build
    /// error if referenced elsewhere.
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
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The running peak of the book's equity. Always `Some`.
    /// See [`crate::indicators::Book::equity_peak`].
    #[grammar(kind = "source")]
    EquityPeak {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The book's current drawdown as a non-positive fraction —
    /// `(equity - peak) / peak`, `0` at a fresh peak. See
    /// [`crate::indicators::Book::drawdown`].
    #[grammar(kind = "source")]
    Drawdown {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The just-completed bar's equity return —
    /// `(equity - prev_equity) / prev_equity`. `None` on the first bar
    /// (`warm_up_bars() = 2`). See
    /// [`crate::indicators::Book::return_per_bar`].
    #[grammar(kind = "source")]
    ReturnPerBar {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
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
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The just-closed trade's return as a fraction of the equity at
    /// trade open. `Some` only on the close bar. See
    /// [`crate::indicators::Book::trade_return`]. Also `None` on the
    /// portfolio aggregate book for the same reason as [`NodeSpec::TradePnl`].
    #[grammar(kind = "source")]
    TradeReturn {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
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
        /// Overlay column name to read from each bar's side-channel data.
        key: String,
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },

    // --- price-series indicators (a source + parameters) ---
    /// Exponential moving average of `source` over `period`.
    #[grammar(kind = "indicator")]
    Ema {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Simple moving average of `source` over `period`.
    #[grammar(kind = "indicator")]
    Sma {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Wilder's smoothed moving average (RMA) of `source` over `period`.
    #[grammar(kind = "indicator")]
    Rma {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Linearly-weighted moving average of `source` over `period`.
    #[grammar(kind = "indicator")]
    Wma {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Hull moving average of `source` over `period` — fast and smooth.
    #[grammar(kind = "indicator")]
    Hma {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Relative Strength Index of `source` over `period`, in `[0, 100]`.
    #[grammar(kind = "indicator")]
    Rsi {
        /// Lookback window, in bars; defaults to Wilder's `14`.
        #[serde(default = "rsi_period_default")]
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Rolling sample standard deviation of `source` over `period`.
    #[serde(rename = "stddev")]
    #[grammar(kind = "indicator")]
    StdDev {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Rolling sample skewness of `source` over `period`.
    #[grammar(kind = "indicator")]
    Skewness {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Rolling excess kurtosis of `source` over `period`.
    #[grammar(kind = "indicator")]
    Kurtosis {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Rolling z-score of `source` — `(x − mean) / stddev` over `period`.
    #[serde(rename = "zscore")]
    #[grammar(kind = "indicator")]
    ZScore {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// The `pct`-quantile of a source over the trailing `period` bars —
    /// `pct: 0.5` is the rolling median. Linearly interpolated (R type-7), the
    /// same convention the report-level percentiles use. Prefer
    /// `!rolling_max` / `!rolling_min` over `pct: 1.0` / `pct: 0.0`; those are
    /// O(1). See [`crate::indicators::Percentile`].
    #[grammar(kind = "indicator")]
    Percentile {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Quantile in `[0, 1]`; defaults to `0.5`, the rolling median.
        #[serde(default = "percentile_pct_default")]
        pct: Real,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Where the current reading sits in its own trailing distribution, as
    /// `count(v <= x) / period` in `(0, 1]`. See
    /// [`crate::indicators::PercentileRank`].
    #[grammar(kind = "indicator")]
    PercentileRank {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Bars elapsed since `source` (a **signal**) last read true — `0` on the
    /// firing bar. `None` until it has fired at least once, which makes every
    /// threshold against it read false until then. See
    /// [`crate::indicators::BarsSince`].
    #[grammar(kind = "indicator")]
    BarsSince {
        /// The source signal this counts the bars since (required).
        source: Box<NodeSpec>,
    },
    /// Bars elapsed since `source` last set a new `period`-bar high, in
    /// `[0, period - 1]`. See [`crate::indicators::BarsSinceHigh`].
    #[grammar(kind = "indicator")]
    BarsSinceHigh {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Bars elapsed since `source` last set a new `period`-bar low.
    /// See [`crate::indicators::BarsSinceLow`].
    #[grammar(kind = "indicator")]
    BarsSinceLow {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Rolling Pearson correlation between two Real sources. Both operands are
    /// required — there is no single natural default for a two-source stat.
    #[grammar(kind = "indicator")]
    Correlation {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
        /// Lookback window, in bars.
        period: NonZeroUsize,
    },
    /// Rolling population covariance between two Real sources — correlation
    /// without the normalisation, so it keeps the units and the magnitude. Both
    /// operands are required.
    #[grammar(kind = "indicator", since = "0.69")]
    Covariance {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
        /// Lookback window, in bars.
        period: NonZeroUsize,
    },
    /// Rolling beta of `lhs` against `rhs` — the least-squares slope explaining
    /// the first series by the second, so the order is "asset, then benchmark"
    /// and swapping them is a different number. Feed returns (`!roc { period: 1 }`)
    /// rather than prices unless you want the price-level relationship: this
    /// takes what it is handed and does not difference behind your back. Reads
    /// `0` when the benchmark is flat over the window.
    #[grammar(kind = "indicator", since = "0.69")]
    Beta {
        /// The series being explained.
        lhs: Box<NodeSpec>,
        /// The benchmark it is explained by.
        rhs: Box<NodeSpec>,
        /// Lookback window, in bars.
        period: NonZeroUsize,
    },
    /// Slope of the least-squares line fitting `source` against the bar index,
    /// in source units **per bar**. The trend primitive nothing else in the
    /// grammar can spell: no composition of lagged differences produces a
    /// regression. Pair it with `!linreg_r2` — `slope · r²` discounts a steep
    /// fit that nothing actually follows.
    #[grammar(kind = "indicator", since = "0.69")]
    LinregSlope {
        /// Lookback window, in bars. At least 2 — one point has no slope.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// The `!linreg_slope` fit evaluated at the **oldest** bar in the window —
    /// the level the current trend started from.
    #[grammar(kind = "indicator", since = "0.69")]
    LinregIntercept {
        /// Lookback window, in bars. At least 2 — one point has no slope.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// The `!linreg_slope` fit evaluated at the **newest** bar in the window — a
    /// de-noised reading of the level now, and the least-squares counterpart of
    /// a moving average.
    #[grammar(kind = "indicator", since = "0.69")]
    LinregValue {
        /// Lookback window, in bars. At least 2 — one point has no slope.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Coefficient of determination of the `!linreg_slope` fit, in `[0, 1]`: how
    /// much of the source's variation over the window a straight line accounts
    /// for.
    #[grammar(kind = "indicator", since = "0.69")]
    LinregR2 {
        /// Lookback window, in bars. At least 2 — one point has no slope.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Lo-MacKinlay variance-ratio regime classifier (`> 1` trending, `< 1`
    /// mean-reverting) over the source's first differences.
    #[grammar(kind = "indicator")]
    VarianceRatio {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Differencing lag, in bars; defaults to `2`, the shortest horizon the ratio is defined over.
        #[serde(default = "variance_ratio_lag_default")]
        lag: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Commodity Channel Index of `source` over `period`.
    #[grammar(kind = "indicator")]
    Cci {
        /// Lookback window, in bars; defaults to `20`.
        #[serde(default = "cci_period_default")]
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Stochastic oscillator %K of `source` over `period`, in `[0, 100]`.
    #[grammar(kind = "indicator")]
    Stochastic {
        /// Lookback window, in bars; defaults to `14`.
        #[serde(default = "stochastic_period_default")]
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Stochastic RSI — the stochastic transform of an RSI of `source`.
    #[grammar(kind = "indicator")]
    StochRsi {
        /// Period of the inner RSI.
        #[serde(default = "stoch_rsi_rsi_period_default")]
        rsi_period: NonZeroUsize,
        /// Stochastic period applied over the RSI.
        #[serde(default = "stoch_rsi_stoch_period_default")]
        stoch_period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },

    // --- multi-output indicators, one variant per component ---
    /// MACD line: `EMA(fast) − EMA(slow)` of `source`.
    #[grammar(kind = "indicator")]
    MacdLine {
        /// Fast EMA period.
        #[serde(default = "macd_fast_default")]
        fast: NonZeroUsize,
        /// Slow EMA period.
        #[serde(default = "macd_slow_default")]
        slow: NonZeroUsize,
        /// Signal EMA period.
        #[serde(default = "macd_signal_default")]
        signal: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// MACD signal line: the `signal`-period EMA of the MACD line.
    #[grammar(kind = "indicator")]
    MacdSignal {
        /// Fast EMA period.
        #[serde(default = "macd_fast_default")]
        fast: NonZeroUsize,
        /// Slow EMA period.
        #[serde(default = "macd_slow_default")]
        slow: NonZeroUsize,
        /// Signal EMA period.
        #[serde(default = "macd_signal_default")]
        signal: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// MACD histogram: the MACD line minus its signal line.
    #[grammar(kind = "indicator")]
    MacdHistogram {
        /// Fast EMA period.
        #[serde(default = "macd_fast_default")]
        fast: NonZeroUsize,
        /// Slow EMA period.
        #[serde(default = "macd_slow_default")]
        slow: NonZeroUsize,
        /// Signal EMA period.
        #[serde(default = "macd_signal_default")]
        signal: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Bollinger upper band: `SMA(period) + k · stddev`.
    #[grammar(kind = "indicator")]
    BbUpper {
        /// Lookback window, in bars.
        #[serde(default = "bb_period_default")]
        period: NonZeroUsize,
        /// Band half-width, in standard deviations.
        #[serde(default = "bb_k_default")]
        k: Real,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Bollinger middle band: the `period`-bar SMA of `source`.
    #[grammar(kind = "indicator")]
    BbMiddle {
        /// Lookback window, in bars.
        #[serde(default = "bb_period_default")]
        period: NonZeroUsize,
        /// Band half-width, in standard deviations.
        #[serde(default = "bb_k_default")]
        k: Real,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Bollinger lower band: `SMA(period) − k · stddev`.
    #[grammar(kind = "indicator")]
    BbLower {
        /// Lookback window, in bars.
        #[serde(default = "bb_period_default")]
        period: NonZeroUsize,
        /// Band half-width, in standard deviations.
        #[serde(default = "bb_k_default")]
        k: Real,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Keltner upper band: the EMA middle plus `multiplier · ATR`.
    #[grammar(kind = "indicator")]
    KeltnerUpper {
        /// EMA period of the middle band.
        #[serde(default = "keltner_ema_period_default")]
        ema_period: NonZeroUsize,
        /// ATR period setting the channel width.
        #[serde(default = "keltner_atr_period_default")]
        atr_period: NonZeroUsize,
        /// Channel half-width, as a multiple of ATR.
        #[serde(default = "keltner_multiplier_default")]
        multiplier: Real,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        candle_source: Box<NodeSpec>,
    },
    /// Keltner middle band: the `ema_period` EMA of `source`.
    #[grammar(kind = "indicator")]
    KeltnerMiddle {
        /// EMA period of the middle band.
        #[serde(default = "keltner_ema_period_default")]
        ema_period: NonZeroUsize,
        /// ATR period setting the channel width.
        #[serde(default = "keltner_atr_period_default")]
        atr_period: NonZeroUsize,
        /// Channel half-width, as a multiple of ATR.
        #[serde(default = "keltner_multiplier_default")]
        multiplier: Real,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        candle_source: Box<NodeSpec>,
    },
    /// Keltner lower band: the EMA middle minus `multiplier · ATR`.
    #[grammar(kind = "indicator")]
    KeltnerLower {
        /// EMA period of the middle band.
        #[serde(default = "keltner_ema_period_default")]
        ema_period: NonZeroUsize,
        /// ATR period setting the channel width.
        #[serde(default = "keltner_atr_period_default")]
        atr_period: NonZeroUsize,
        /// Channel half-width, as a multiple of ATR.
        #[serde(default = "keltner_multiplier_default")]
        multiplier: Real,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        candle_source: Box<NodeSpec>,
    },
    /// Donchian upper channel: the highest `high` over `period` bars.
    #[grammar(kind = "indicator")]
    DonchianUpper {
        /// Lookback window, in bars; defaults to the Turtle `20`.
        #[serde(default = "donchian_period_default")]
        period: NonZeroUsize,
        /// The high series; defaults to `!high` when omitted.
        #[serde(default = "default_high")]
        high: Box<NodeSpec>,
        /// The low series; defaults to `!low` when omitted.
        #[serde(default = "default_low")]
        low: Box<NodeSpec>,
    },
    /// Donchian middle channel: the mean of the upper and lower bands.
    #[grammar(kind = "indicator")]
    DonchianMiddle {
        /// Lookback window, in bars; defaults to the Turtle `20`.
        #[serde(default = "donchian_period_default")]
        period: NonZeroUsize,
        /// The high series; defaults to `!high` when omitted.
        #[serde(default = "default_high")]
        high: Box<NodeSpec>,
        /// The low series; defaults to `!low` when omitted.
        #[serde(default = "default_low")]
        low: Box<NodeSpec>,
    },
    /// Donchian lower channel: the lowest `low` over `period` bars.
    #[grammar(kind = "indicator")]
    DonchianLower {
        /// Lookback window, in bars; defaults to the Turtle `20`.
        #[serde(default = "donchian_period_default")]
        period: NonZeroUsize,
        /// The high series; defaults to `!high` when omitted.
        #[serde(default = "default_high")]
        high: Box<NodeSpec>,
        /// The low series; defaults to `!low` when omitted.
        #[serde(default = "default_low")]
        low: Box<NodeSpec>,
    },
    /// Average Directional Index over `period` — trend strength, in `[0, 100]`.
    #[grammar(kind = "indicator")]
    Adx {
        /// Lookback window, in bars; defaults to Wilder's `14`.
        #[serde(default = "adx_period_default")]
        period: NonZeroUsize,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Positive Directional Indicator (+DI) over `period`.
    #[grammar(kind = "indicator")]
    PlusDi {
        /// Lookback window, in bars; defaults to Wilder's `14`.
        #[serde(default = "adx_period_default")]
        period: NonZeroUsize,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Negative Directional Indicator (−DI) over `period`.
    #[grammar(kind = "indicator")]
    MinusDi {
        /// Lookback window, in bars; defaults to Wilder's `14`.
        #[serde(default = "adx_period_default")]
        period: NonZeroUsize,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Directional Movement +DI over `period` (the DMI system's variant).
    #[grammar(kind = "indicator")]
    DmiPlusDi {
        /// Lookback window, in bars; defaults to Wilder's `14`.
        #[serde(default = "dmi_period_default")]
        period: NonZeroUsize,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Directional Movement −DI over `period` (the DMI system's variant).
    #[grammar(kind = "indicator")]
    DmiMinusDi {
        /// Lookback window, in bars; defaults to Wilder's `14`.
        #[serde(default = "dmi_period_default")]
        period: NonZeroUsize,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Aroon Up over `period`, in `[0, 100]` — recency of the period high.
    #[grammar(kind = "indicator")]
    AroonUp {
        /// Lookback window, in bars; defaults to `14`.
        #[serde(default = "aroon_period_default")]
        period: NonZeroUsize,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Aroon Down over `period`, in `[0, 100]` — recency of the period low.
    #[grammar(kind = "indicator")]
    AroonDown {
        /// Lookback window, in bars; defaults to `14`.
        #[serde(default = "aroon_period_default")]
        period: NonZeroUsize,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Aroon Oscillator: Aroon Up minus Aroon Down.
    #[grammar(kind = "indicator")]
    AroonOscillator {
        /// Lookback window, in bars; defaults to `14`.
        #[serde(default = "aroon_period_default")]
        period: NonZeroUsize,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },

    // --- single-output bar indicators ---
    /// Average True Range over `period` — mean bar range in price units.
    #[grammar(kind = "indicator")]
    Atr {
        /// Lookback window, in bars; defaults to Wilder's `14`.
        #[serde(default = "atr_period_default")]
        period: NonZeroUsize,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Parkinson high/low range volatility estimator over `period`.
    #[grammar(kind = "indicator")]
    Parkinson {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Garman–Klass OHLC volatility estimator over `period`.
    #[grammar(kind = "indicator")]
    GarmanKlass {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Rogers–Satchell drift-independent OHLC volatility estimator over `period`.
    #[grammar(kind = "indicator")]
    RogersSatchell {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Money Flow Index over `period`, in `[0, 100]` — a volume-weighted RSI.
    #[grammar(kind = "indicator")]
    Mfi {
        /// Lookback window, in bars; defaults to `14`.
        #[serde(default = "mfi_period_default")]
        period: NonZeroUsize,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Williams %R over `period`, in `[−100, 0]`.
    #[grammar(kind = "indicator")]
    WilliamsR {
        /// Lookback window, in bars; defaults to `14`.
        #[serde(default = "williams_r_period_default")]
        period: NonZeroUsize,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// On-Balance Volume — the running signed-volume accumulation.
    #[grammar(kind = "indicator")]
    Obv {
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Volume-Weighted Average Price over `period`.
    #[grammar(kind = "indicator")]
    Vwap {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Accumulation/Distribution line — the running money-flow accumulation.
    #[grammar(kind = "indicator")]
    Ad {
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// True Range of the current bar, in price units.
    #[grammar(kind = "indicator")]
    TrueRange {
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Parabolic SAR — the trailing stop-and-reverse level.
    #[grammar(kind = "indicator")]
    Sar {
        /// Acceleration-factor increment.
        #[serde(default = "sar_step_default")]
        step: Real,
        /// Acceleration-factor cap.
        #[serde(default = "sar_max_default")]
        max: Real,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
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
        /// Target annualized volatility, as a fraction.
        target: Real,
        /// Lookback window, in bars.
        window: NonZeroUsize,
        /// Bars per year, for annualizing (252 stocks, 365 crypto).
        bars_per_year: Real,
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Fixed per-trade risk sized by ATR —
    /// `risk_frac * close / (atr_multiple * ATR(period))`. `source` defaults
    /// to the single-asset empty-selector `Pick`; in a basket set it to
    /// `!pick { symbol: !arg SYM }`. See
    /// [`crate::indicators::sizing::atr_risk`] /
    /// [`crate::indicators::sizing::atr_risk_of`].
    #[grammar(kind = "function")]
    AtrRisk {
        /// Fraction of equity risked per trade.
        risk_frac: Real,
        /// ATR lookback window, in bars; defaults to Wilder's `14`.
        #[serde(default = "atr_period_default")]
        period: NonZeroUsize,
        /// Stop distance, as a multiple of ATR.
        atr_multiple: Real,
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Drawdown-throttled sizing — `max(0, min(1, 1 + book.drawdown() /
    /// max_drawdown))`. Reads a book via `source:` (default:
    /// [`NodeSpec::StrategyBook`]). See
    /// [`crate::indicators::sizing::drawdown_throttle`].
    #[grammar(kind = "function")]
    DrawdownThrottle {
        /// Drawdown fraction at which sizing throttles to zero.
        max_drawdown: Real,
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Realized-vol targeting on the book's equity return series
    /// — `target / (stddev(book.return_per_bar, window) *
    /// sqrt(bars_per_year))`. Reads a book via `source:` (default:
    /// [`NodeSpec::StrategyBook`]). See
    /// [`crate::indicators::sizing::equity_vol_target`].
    #[grammar(kind = "function")]
    EquityVolTarget {
        /// Target annualized volatility, as a fraction.
        target: Real,
        /// Lookback window, in bars.
        window: NonZeroUsize,
        /// Bars per year, for annualizing (252 stocks, 365 crypto).
        bars_per_year: Real,
        /// Size used until the recipe has enough data to size itself.
        /// Both recipes measure something that only exists because the
        /// strategy traded, so without this they hold every entry forever
        /// (`sizing: None` skips the trade) and never trade at all.
        #[serde(default = "default_sizing_seed")]
        seed: Real,
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Fractional Kelly over the last `window` closed-trade returns —
    /// `kelly_fraction * mean / variance`, clamped to `>= 0`. Reads a book
    /// via `source:` (default: [`NodeSpec::StrategyBook`]). See
    /// [`crate::indicators::sizing::fractional_kelly`].
    #[grammar(kind = "function")]
    FractionalKelly {
        /// Fraction of the full Kelly stake to take.
        kelly_fraction: Real,
        /// Lookback window, in bars.
        window: NonZeroUsize,
        /// Size used until the recipe has enough data to size itself.
        /// Both recipes measure something that only exists because the
        /// strategy traded, so without this they hold every entry forever
        /// (`sizing: None` skips the trade) and never trade at all.
        #[serde(default = "default_sizing_seed")]
        seed: Real,
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
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
        /// The embedded single-asset strategy whose equity curve this measures.
        strategy: Box<AnyStrategyRef>,
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Bars per year, for annualizing (252 stocks, 365 crypto).
        bars_per_year: Real,
        /// Annualized risk-free rate; defaults to `0` when omitted.
        #[serde(default = "default_risk_free_rate")]
        risk_free_rate: Real,
    },
    /// Trailing annualized Sortino of `strategy`'s equity curve. See
    /// [`crate::indicators::Sortino`].
    #[grammar(kind = "indicator")]
    Sortino {
        /// The embedded single-asset strategy whose equity curve this measures.
        strategy: Box<AnyStrategyRef>,
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Bars per year, for annualizing (252 stocks, 365 crypto).
        bars_per_year: Real,
        /// Annualized risk-free rate; defaults to `0` when omitted.
        #[serde(default = "default_risk_free_rate")]
        risk_free_rate: Real,
    },
    /// Trailing annualized volatility of `strategy`'s equity return stream.
    /// See [`crate::indicators::Volatility`].
    #[grammar(kind = "indicator")]
    Volatility {
        /// The embedded single-asset strategy whose equity curve this measures.
        strategy: Box<AnyStrategyRef>,
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Bars per year, for annualizing (252 stocks, 365 crypto).
        bars_per_year: Real,
    },
    /// Trailing maximum drawdown of `strategy`'s equity curve, as a
    /// non-negative fraction. See [`crate::indicators::MaxDrawdown`].
    #[grammar(kind = "indicator")]
    MaxDrawdown {
        /// The embedded single-asset strategy whose equity curve this measures.
        strategy: Box<AnyStrategyRef>,
        /// Lookback window, in bars.
        period: NonZeroUsize,
    },
    /// Trailing Calmar (windowed CAGR / max drawdown) of `strategy`'s equity
    /// curve. See [`crate::indicators::Calmar`].
    #[grammar(kind = "indicator")]
    Calmar {
        /// The embedded single-asset strategy whose equity curve this measures.
        strategy: Box<AnyStrategyRef>,
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Bars per year, for annualizing (252 stocks, 365 crypto).
        bars_per_year: Real,
    },

    // --- transform operators ---
    /// Sum of `lhs` and `rhs`.
    #[grammar(kind = "operator")]
    Add {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
    },
    /// `lhs` minus `rhs`.
    #[grammar(kind = "operator")]
    Sub {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
    },
    /// Product of `lhs` and `rhs`.
    #[grammar(kind = "operator")]
    Mul {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
    },
    /// `lhs` divided by `rhs` (`None` when `rhs` is zero).
    #[grammar(kind = "operator")]
    Div {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
    },
    /// `lhs` raised to the power `rhs`. Emits `None` where the result is not a
    /// finite real — a negative base at a fractional exponent, `0` to a negative
    /// power, or an overflow.
    #[grammar(kind = "operator", since = "0.69")]
    Pow {
        /// The base.
        lhs: Box<NodeSpec>,
        /// The exponent.
        rhs: Box<NodeSpec>,
    },
    /// The larger of `lhs` and `rhs`, bar by bar. Not `!rolling_max`, which
    /// maximises one series over a window rather than two series against each
    /// other.
    #[grammar(kind = "operator", since = "0.69")]
    Max {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
    },
    /// The smaller of `lhs` and `rhs`, bar by bar — the twin of `!max`.
    #[grammar(kind = "operator", since = "0.69")]
    Min {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
    },
    /// `source` confined to `[lower, upper]` — `!min` of `!max`, spelled as one
    /// node because a bounded size is one idea. Both bounds are expressions, so
    /// a band can itself be computed. Inverted bounds (`lower` above `upper`)
    /// collapse to `upper` rather than erroring: that is what the composed form
    /// does, and it is the honest answer to a contradictory band.
    #[grammar(kind = "operator", since = "0.69")]
    Clamp {
        /// Series to bound (required).
        source: Box<NodeSpec>,
        /// Lower bound.
        lower: Box<NodeSpec>,
        /// Upper bound.
        upper: Box<NodeSpec>,
    },
    /// Absolute value of `source`.
    #[grammar(kind = "operator", since = "0.69")]
    Abs {
        /// Series to transform (required).
        source: Box<NodeSpec>,
    },
    /// Sign of `source`: `1` above zero, `-1` below, `0` at exactly zero.
    #[grammar(kind = "operator", since = "0.69")]
    Sign {
        /// Series to transform (required).
        source: Box<NodeSpec>,
    },
    /// Square root of `source`. Emits `None` on samples where the source is
    /// negative.
    #[grammar(kind = "operator", since = "0.69")]
    Sqrt {
        /// Series to transform (required).
        source: Box<NodeSpec>,
    },
    /// Hyperbolic tangent of `source`, squashing any input into `(-1, 1)`. The
    /// bounded response a sizing expression wants: linear near zero, saturating
    /// past |x| ≈ 2, and smooth throughout — unlike a `!clamp`.
    #[grammar(kind = "operator", since = "0.69")]
    Tanh {
        /// Series to transform (required).
        source: Box<NodeSpec>,
    },
    /// Logistic sigmoid of `source`, `1 / (1 + e^-x)`, squashing any input into
    /// `(0, 1)` — the one-sided twin of `!tanh`, for a fraction rather than a
    /// signed response.
    #[grammar(kind = "operator", since = "0.69")]
    Sigmoid {
        /// Series to transform (required).
        source: Box<NodeSpec>,
    },
    /// Running total of every value `source` has produced, from the first bar of
    /// the run onward. Where it starts is part of its meaning: `!obv` and `!ad`
    /// are two hard-wired instances of this shape.
    #[grammar(kind = "operator", since = "0.69")]
    CumSum {
        /// Series to accumulate (required).
        source: Box<NodeSpec>,
    },
    /// Running maximum of `source` since the start of the run — the unbounded
    /// `!rolling_max`. `!div { lhs: x, rhs: !cum_max { source: x } }` less one is
    /// the drawdown of any series, generalising the book-anchored `!drawdown`.
    #[grammar(kind = "operator", since = "0.69")]
    CumMax {
        /// Series to accumulate (required).
        source: Box<NodeSpec>,
    },
    /// Running minimum of `source` since the start of the run — the unbounded
    /// `!rolling_min`.
    #[grammar(kind = "operator", since = "0.69")]
    CumMin {
        /// Series to accumulate (required).
        source: Box<NodeSpec>,
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
        /// Boolean condition selecting between `then` and `otherwise`.
        cond: Box<NodeSpec>,
        /// Value emitted when `cond` is true.
        then: Box<NodeSpec>,
        /// Value emitted when `cond` is false.
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
        /// Value dispatched on, matched against each case's pattern.
        on: Box<NodeSpec>,
        /// Ordered cases; the first whose pattern equals `on` wins.
        cases: Vec<MatchCase>,
        /// Fallback value when no case matches.
        default: Box<NodeSpec>,
    },
    /// The value of `source` from `period` bars ago.
    #[grammar(kind = "operator")]
    Lag {
        /// Lookback window, in bars; defaults to `1`, the previous bar.
        #[serde(default = "lookback_period_default")]
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// `source` minus its value `period` bars ago.
    #[grammar(kind = "operator")]
    Diff {
        /// Lookback window, in bars; defaults to `1`, the previous bar.
        #[serde(default = "lookback_period_default")]
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// `source` divided by its value `period` bars ago.
    #[grammar(kind = "operator")]
    Ratio {
        /// Lookback window, in bars; defaults to `1`, the previous bar.
        #[serde(default = "lookback_period_default")]
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Rate of change of `source` over `period` bars, as a fraction.
    #[grammar(kind = "operator")]
    Roc {
        /// Lookback window, in bars; defaults to `1`, the previous bar.
        #[serde(default = "lookback_period_default")]
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Rolling maximum of `source` over `period` bars.
    #[grammar(kind = "operator")]
    RollingMax {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Rolling minimum of `source` over `period` bars.
    #[grammar(kind = "operator")]
    RollingMin {
        /// Lookback window, in bars.
        period: NonZeroUsize,
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Logarithm of `source` in `base` (defaults to natural log, `e`).
    /// Emits `None` on samples where the source's output is non-positive.
    #[grammar(kind = "operator")]
    Log {
        /// Series to read; defaults to the bar's `!close` when omitted.
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        /// Logarithm base; defaults to `e` (natural log) when omitted.
        #[serde(default = "default_log_base")]
        base: Real,
    },
    /// Exponential of `source` in `base` — `base^x`, the inverse of `!log`
    /// (defaults to the natural exponential, `e`). Emits `None` on samples
    /// whose result overflows the finite range.
    #[grammar(kind = "operator", since = "0.64")]
    Exp {
        /// Series to exponentiate (required).
        source: Box<NodeSpec>,
        /// Exponential base; defaults to `e` (natural exponential) when omitted.
        #[serde(default = "default_exp_base")]
        base: Real,
    },
    /// Holds the most recent `Some` output of `source`, re-emitting it on
    /// ticks where `source` returns `None`. Wrap the outermost recursive
    /// smoother of a resampled pipeline so per-base-tick consumers see the
    /// finished higher-timeframe value between boundaries — see
    /// [`crate::indicators::Latch`].
    #[grammar(kind = "operator", output = "any")]
    Latch {
        /// The source whose last `Some` output is held between ticks (required).
        source: Box<NodeSpec>,
    },
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
        /// Number of base bars aggregated into each higher-timeframe bar.
        every: NonZeroUsize,
        /// Source run over each completed higher-timeframe bar.
        inner: Box<NodeSpec>,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Aggregates base candles into one bar per `threshold` units of traded
    /// **quantity**, then runs `inner` over each completed bar. Sibling of
    /// [`Resample`](NodeSpec::Resample) in every respect except what closes a
    /// bar: volume rather than elapsed bars. Emits on the tick that completes a
    /// bucket and `None` between, so a recursive `inner` recurses over the
    /// sampled bars rather than the base ones; wrap the downstream chain in
    /// [`Latch`](NodeSpec::Latch) for per-base-tick reads.
    ///
    /// A bucket closes on the first candle that takes it *at or past* the
    /// threshold and the overshoot is not carried, so precision is bounded by
    /// how fine the base candles are.
    #[grammar(kind = "operator")]
    VolumeBars {
        /// Traded quantity that fills one bar.
        threshold: Real,
        /// Source run over each completed bar.
        inner: Box<NodeSpec>,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Aggregates base candles into one bar per `threshold` units of traded
    /// **notional** (`typical x volume`), then runs `inner` over each completed bar. Sibling of
    /// [`Resample`](NodeSpec::Resample) in every respect except what closes a
    /// bar: notional rather than elapsed bars. Emits on the tick that completes a
    /// bucket and `None` between, so a recursive `inner` recurses over the
    /// sampled bars rather than the base ones; wrap the downstream chain in
    /// [`Latch`](NodeSpec::Latch) for per-base-tick reads.
    ///
    /// A bucket closes on the first candle that takes it *at or past* the
    /// threshold and the overshoot is not carried, so precision is bounded by
    /// how fine the base candles are.
    #[grammar(kind = "operator")]
    DollarBars {
        /// Traded notional that fills one bar.
        threshold: Real,
        /// Source run over each completed bar.
        inner: Box<NodeSpec>,
        /// Bar source — the whole candle; defaults to the current bar when omitted.
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Passthrough wrapper that reports `unstable_bars() = 0`. The output
    /// and warm-up of `source` are unchanged; the strategy-readiness gate
    /// (which counts up to `stable_bars()`) no longer waits for this
    /// subtree's IIR settling tail. The explicit opt-out to the "wait for
    /// every source to be past its unstable tail" safe default; see
    /// [`crate::indicators::Unstable`].
    #[grammar(kind = "operator", output = "any", alt = "unary_source")]
    Unstable {
        /// The source whose unstable settling tail is ignored (required).
        source: Box<NodeSpec>,
    },

    // --- calendar accessors (read `atom.time`, emit Real; None when time is
    // absent). Each takes an optional `source` for cross-asset use — the
    // bare form (`!year`) is the default single-series shortcut,
    // `!year { source: !pick { ... } }` reads the picked asset's time.
    /// The Gregorian year (e.g. `2024.0`).
    #[grammar(kind = "source")]
    Year {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The Gregorian month, `1.0` (Jan) through `12.0` (Dec).
    #[grammar(kind = "source")]
    Month {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The day of the month, `1.0` through `31.0`.
    #[grammar(kind = "source")]
    Day {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The hour of the day (UTC), `0.0` through `23.0`.
    #[grammar(kind = "source")]
    Hour {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The minute of the hour, `0.0` through `59.0`.
    #[grammar(kind = "source")]
    Minute {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The second of the minute, `0.0` through `59.0`.
    #[grammar(kind = "source")]
    Second {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// ISO 8601 weekday, `1.0` (Monday) through `7.0` (Sunday).
    #[grammar(kind = "source")]
    DayOfWeek {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Day of the year, `1.0` through `366.0`.
    #[grammar(kind = "source")]
    DayOfYear {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// ISO 8601 week of the year, `1.0` through `53.0`.
    #[grammar(kind = "source")]
    WeekOfYear {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Calendar quarter, `1.0` through `4.0`.
    #[grammar(kind = "source")]
    Quarter {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Unix seconds since the epoch (as a Real).
    #[grammar(kind = "source")]
    UnixSeconds {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Unix milliseconds since the epoch (as a Real).
    #[grammar(kind = "source")]
    UnixMillis {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// The raw bar-open [`Timestamp`] payload (yields
    /// `PayloadType::Time`, not a scalar). The `Timestamp` twin of
    /// [`NodeSpec::Current`].
    #[grammar(kind = "source", output = "time")]
    Time {
        /// Optional cross-asset source (e.g. `!pick { symbol: … }`); defaults to the strategy's own series.
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },

    // --- boolean signals (absorbed from the former SignalSpec; every one
    // produces `Bool`). Comparisons carry an optional *absolute* `epsilon`;
    // omitting it uses the hybrid `DEFAULT_TOLERANCE`, which scales with the
    // operands (see `indicators::compare`).
    /// True when `lhs` is greater than `rhs` (beyond `epsilon`).
    #[grammar(kind = "predicate", output = "bool")]
    Gt {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
        /// Absolute comparison tolerance, in the operands' own units — a deadband
        /// you mean literally. Omit it for the scale-aware default, which is a
        /// `1e-12` floor plus `1e-9` of the larger operand.
        epsilon: Option<Real>,
    },
    /// True when `lhs` is less than `rhs` (beyond `epsilon`).
    #[grammar(kind = "predicate", output = "bool")]
    Lt {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
        /// Absolute comparison tolerance, in the operands' own units — a deadband
        /// you mean literally. Omit it for the scale-aware default, which is a
        /// `1e-12` floor plus `1e-9` of the larger operand.
        epsilon: Option<Real>,
    },
    /// True when `lhs` is greater than or equal to `rhs` (within `epsilon`).
    #[grammar(kind = "predicate", output = "bool")]
    Ge {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
        /// Absolute comparison tolerance, in the operands' own units — a deadband
        /// you mean literally. Omit it for the scale-aware default, which is a
        /// `1e-12` floor plus `1e-9` of the larger operand.
        epsilon: Option<Real>,
    },
    /// True when `lhs` is less than or equal to `rhs` (within `epsilon`).
    #[grammar(kind = "predicate", output = "bool")]
    Le {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
        /// Absolute comparison tolerance, in the operands' own units — a deadband
        /// you mean literally. Omit it for the scale-aware default, which is a
        /// `1e-12` floor plus `1e-9` of the larger operand.
        epsilon: Option<Real>,
    },
    /// Polymorphic equality — Real or Str, dispatched on the lhs at build.
    #[grammar(kind = "predicate", output = "bool")]
    Eq {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
        /// Absolute comparison tolerance, in the operands' own units — a deadband
        /// you mean literally. Omit it for the scale-aware default, which is a
        /// `1e-12` floor plus `1e-9` of the larger operand.
        epsilon: Option<Real>,
    },
    /// True when `lhs` and `rhs` differ by more than `epsilon`.
    #[grammar(kind = "predicate", output = "bool")]
    Ne {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
        /// Absolute comparison tolerance, in the operands' own units — a deadband
        /// you mean literally. Omit it for the scale-aware default, which is a
        /// `1e-12` floor plus `1e-9` of the larger operand.
        epsilon: Option<Real>,
    },
    /// `source > level` against a constant.
    #[grammar(kind = "predicate", output = "bool")]
    Above {
        /// Series the level applies to. **Required** — unlike `!ema`'s
        /// `source:`, a threshold has no meaningful default series, and
        /// silently testing the raw `close` against a `70` meant for an RSI
        /// is a document that builds, runs, and never fires.
        source: Box<NodeSpec>,
        /// Constant threshold `source` is compared against.
        level: Real,
    },
    /// `source < level` against a constant.
    #[grammar(kind = "predicate", output = "bool")]
    Below {
        /// Series the level applies to. **Required** — unlike `!ema`'s
        /// `source:`, a threshold has no meaningful default series, and
        /// silently testing the raw `close` against a `70` meant for an RSI
        /// is a document that builds, runs, and never fires.
        source: Box<NodeSpec>,
        /// Constant threshold `source` is compared against.
        level: Real,
    },
    /// Fires on the bar `lhs` crosses from below to above `rhs`.
    #[grammar(kind = "predicate", output = "bool")]
    CrossesAbove {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
    },
    /// Fires on the bar `lhs` crosses from above to below `rhs`.
    #[grammar(kind = "predicate", output = "bool")]
    CrossesBelow {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
    },
    /// Logical AND — true when both `lhs` and `rhs` are true.
    #[grammar(kind = "operator", output = "bool")]
    And {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
    },
    /// Logical OR — true when either `lhs` or `rhs` is true.
    #[grammar(kind = "operator", output = "bool")]
    Or {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
    },
    /// Logical XOR — true when exactly one of `lhs` / `rhs` is true.
    #[grammar(kind = "operator", output = "bool")]
    Xor {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand.
        rhs: Box<NodeSpec>,
    },
    /// AND-fold of a list (empty ⇒ constant `true`).
    #[grammar(kind = "operator", output = "bool")]
    All(Vec<NodeSpec>),
    /// OR-fold of a list (empty ⇒ constant `false`).
    #[grammar(kind = "operator", output = "bool")]
    Any(Vec<NodeSpec>),
    /// Logical negation of the inner signal.
    #[grammar(kind = "operator", output = "bool")]
    Not(Box<NodeSpec>),
    /// Toggle detector — fires on either edge. Dispatches on the child's
    /// output type at build: a Bool inner is a rising-or-falling toggle, a
    /// Real inner fires on any value change. Subsumes the former
    /// `Changed` / `ChangedReal` split of the signal layer.
    #[grammar(kind = "predicate", output = "bool", alt = "unary_source")]
    Changed(Box<NodeSpec>),
    /// Rising-edge detector for a Bool inner (`false → true`).
    #[grammar(kind = "predicate", output = "bool", alt = "unary_source")]
    BecameTrue(Box<NodeSpec>),
    /// Falling-edge detector (`true → false`).
    #[grammar(kind = "predicate", output = "bool", alt = "unary_source")]
    BecameFalse(Box<NodeSpec>),
    /// `lhs == rhs` on two `Str`-typed operands.
    #[grammar(kind = "predicate", output = "bool")]
    StrEq {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand — a string literal or a `Str`-typed expression.
        rhs: StrOperand,
    },
    /// `lhs != rhs` on two `Str`-typed operands.
    #[grammar(kind = "predicate", output = "bool")]
    StrNe {
        /// Left-hand operand.
        lhs: Box<NodeSpec>,
        /// Right-hand operand — a string literal or a `Str`-typed expression.
        rhs: StrOperand,
    },
    /// Sugar for `!value false` — reads better on a `rebalance_on:` field
    /// where the intent is "never".
    ///
    /// **Writing `!never` is not the same as omitting the field.** Each shape
    /// has its own default: `single:` / `pairs:` / `multi:` default to `!never`,
    /// but `basket:` defaults to `!every 1` — every bar, the *most* active
    /// setting. On a basket, omitting `rebalance_on:` and writing `!never` are
    /// opposite ends of the range. See the Defaults table under *Rebalance* in
    /// `docs/STRATEGIES.md`.
    #[grammar(kind = "predicate", output = "bool")]
    Never,
    /// A periodic pulse — [`Every(N)`](crate::indicators::Every) with a
    /// *delayed* first fire on bar `N-1` (0-indexed), then every `N` bars.
    #[grammar(kind = "predicate", output = "bool")]
    Every(NonZeroUsize),
    /// True Monday through Friday; `None` when `atom.time` is absent.
    #[grammar(kind = "predicate", output = "bool")]
    IsWeekday,
    /// True Saturday/Sunday; `None` when `atom.time` is absent.
    #[grammar(kind = "predicate", output = "bool")]
    IsWeekend,
    /// Schema-level check: `true` if the overlay column `name` exists.
    #[grammar(kind = "predicate", output = "bool")]
    HasColumn {
        /// Overlay column name to test for existence.
        name: String,
    },
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
    // runtime value; they resolve, at build, to a `Book<Symbol>` handle that
    // a book-reading node (a bare book leaf like `!drawdown`, or a
    // book-anchored recipe like `!drawdown_throttle`) picks up via its
    // `source:` field. Bare (used as an expression on its own) is invalid
    // and reports a build error.
    /// The **strategy book** — the `Book` owned by the enclosing strategy
    /// scope (single/pairs/basket/multi/the current per-child instance of
    /// a portfolio's `weights:` expression). This is the default source of
    /// every book-reading node when its `source:` is omitted.
    StrategyBook,
    /// The **portfolio aggregate book** — the mark-to-market view of the
    /// composite [`Portfolio`](crate::portfolio::Portfolio). Only meaningful
    /// inside a portfolio's `weights:` expression; reported as a build
    /// error if referenced elsewhere.
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
    /// (`warm_up_bars() = 2`). See
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
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    Sma {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    Rma {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    Wma {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    Hma {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    Rsi {
        #[serde(default = "rsi_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    #[serde(rename = "stddev")]
    StdDev {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    Skewness {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    Kurtosis {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    #[serde(rename = "zscore")]
    ZScore {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// The `pct`-quantile of a source over the trailing `period` bars —
    /// `pct: 0.5` is the rolling median. Linearly interpolated (R type-7), the
    /// same convention the report-level percentiles use. Prefer
    /// `!rolling_max` / `!rolling_min` over `pct: 1.0` / `pct: 0.0`; those are
    /// O(1). See [`crate::indicators::Percentile`].
    Percentile {
        period: NonZeroUsize,
        #[serde(default = "percentile_pct_default")]
        pct: Real,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Where the current reading sits in its own trailing distribution, as
    /// `count(v <= x) / period` in `(0, 1]`. See
    /// [`crate::indicators::PercentileRank`].
    PercentileRank {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Bars elapsed since `source` (a **signal**) last read true — `0` on the
    /// firing bar. `None` until it has fired at least once, which makes every
    /// threshold against it read false until then. See
    /// [`crate::indicators::BarsSince`].
    BarsSince {
        /// The source signal this counts the bars since (required).
        source: Box<NodeSpec>,
    },
    /// Bars elapsed since `source` last set a new `period`-bar high, in
    /// `[0, period - 1]`. See [`crate::indicators::BarsSinceHigh`].
    BarsSinceHigh {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Bars elapsed since `source` last set a new `period`-bar low.
    /// See [`crate::indicators::BarsSinceLow`].
    BarsSinceLow {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Rolling Pearson correlation between two Real sources. Both operands are
    /// required — there is no single natural default for a two-source stat.
    Correlation {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        period: NonZeroUsize,
    },
    Covariance {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        period: NonZeroUsize,
    },
    Beta {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
        period: NonZeroUsize,
    },
    LinregSlope {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    LinregIntercept {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    LinregValue {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    LinregR2 {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Lo-MacKinlay variance-ratio regime classifier (`> 1` trending, `< 1`
    /// mean-reverting) over the source's first differences.
    VarianceRatio {
        period: NonZeroUsize,
        #[serde(default = "variance_ratio_lag_default")]
        lag: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    Cci {
        #[serde(default = "cci_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    Stochastic {
        #[serde(default = "stochastic_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    StochRsi {
        #[serde(default = "stoch_rsi_rsi_period_default")]
        rsi_period: NonZeroUsize,
        #[serde(default = "stoch_rsi_stoch_period_default")]
        stoch_period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },

    // --- multi-output indicators, one variant per component ---
    MacdLine {
        #[serde(default = "macd_fast_default")]
        fast: NonZeroUsize,
        #[serde(default = "macd_slow_default")]
        slow: NonZeroUsize,
        #[serde(default = "macd_signal_default")]
        signal: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    MacdSignal {
        #[serde(default = "macd_fast_default")]
        fast: NonZeroUsize,
        #[serde(default = "macd_slow_default")]
        slow: NonZeroUsize,
        #[serde(default = "macd_signal_default")]
        signal: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    MacdHistogram {
        #[serde(default = "macd_fast_default")]
        fast: NonZeroUsize,
        #[serde(default = "macd_slow_default")]
        slow: NonZeroUsize,
        #[serde(default = "macd_signal_default")]
        signal: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    BbUpper {
        #[serde(default = "bb_period_default")]
        period: NonZeroUsize,
        #[serde(default = "bb_k_default")]
        k: Real,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    BbMiddle {
        #[serde(default = "bb_period_default")]
        period: NonZeroUsize,
        #[serde(default = "bb_k_default")]
        k: Real,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    BbLower {
        #[serde(default = "bb_period_default")]
        period: NonZeroUsize,
        #[serde(default = "bb_k_default")]
        k: Real,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    KeltnerUpper {
        #[serde(default = "keltner_ema_period_default")]
        ema_period: NonZeroUsize,
        #[serde(default = "keltner_atr_period_default")]
        atr_period: NonZeroUsize,
        #[serde(default = "keltner_multiplier_default")]
        multiplier: Real,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<NodeSpec>,
    },
    KeltnerMiddle {
        #[serde(default = "keltner_ema_period_default")]
        ema_period: NonZeroUsize,
        #[serde(default = "keltner_atr_period_default")]
        atr_period: NonZeroUsize,
        #[serde(default = "keltner_multiplier_default")]
        multiplier: Real,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<NodeSpec>,
    },
    KeltnerLower {
        #[serde(default = "keltner_ema_period_default")]
        ema_period: NonZeroUsize,
        #[serde(default = "keltner_atr_period_default")]
        atr_period: NonZeroUsize,
        #[serde(default = "keltner_multiplier_default")]
        multiplier: Real,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "default_bar_source")]
        candle_source: Box<NodeSpec>,
    },
    DonchianUpper {
        #[serde(default = "donchian_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_high")]
        high: Box<NodeSpec>,
        #[serde(default = "default_low")]
        low: Box<NodeSpec>,
    },
    DonchianMiddle {
        #[serde(default = "donchian_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_high")]
        high: Box<NodeSpec>,
        #[serde(default = "default_low")]
        low: Box<NodeSpec>,
    },
    DonchianLower {
        #[serde(default = "donchian_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_high")]
        high: Box<NodeSpec>,
        #[serde(default = "default_low")]
        low: Box<NodeSpec>,
    },
    Adx {
        #[serde(default = "adx_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    PlusDi {
        #[serde(default = "adx_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    MinusDi {
        #[serde(default = "adx_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    DmiPlusDi {
        #[serde(default = "dmi_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    DmiMinusDi {
        #[serde(default = "dmi_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    AroonUp {
        #[serde(default = "aroon_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    AroonDown {
        #[serde(default = "aroon_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    AroonOscillator {
        #[serde(default = "aroon_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },

    // --- single-output bar indicators ---
    Atr {
        #[serde(default = "atr_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Parkinson high/low range volatility estimator over `period`.
    Parkinson {
        period: NonZeroUsize,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Garman–Klass OHLC volatility estimator over `period`.
    GarmanKlass {
        period: NonZeroUsize,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Rogers–Satchell drift-independent OHLC volatility estimator over `period`.
    RogersSatchell {
        period: NonZeroUsize,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    Mfi {
        #[serde(default = "mfi_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    WilliamsR {
        #[serde(default = "williams_r_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    Obv {
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    Vwap {
        period: NonZeroUsize,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
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
        #[serde(default = "sar_step_default")]
        step: Real,
        #[serde(default = "sar_max_default")]
        max: Real,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
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
        target: Real,
        window: NonZeroUsize,
        bars_per_year: Real,
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Fixed per-trade risk sized by ATR —
    /// `risk_frac * close / (atr_multiple * ATR(period))`. `source` defaults
    /// to the single-asset empty-selector `Pick`; in a basket set it to
    /// `!pick { symbol: !arg SYM }`. See
    /// [`crate::indicators::sizing::atr_risk`] /
    /// [`crate::indicators::sizing::atr_risk_of`].
    AtrRisk {
        risk_frac: Real,
        #[serde(default = "atr_period_default")]
        period: NonZeroUsize,
        atr_multiple: Real,
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Drawdown-throttled sizing — `max(0, min(1, 1 + book.drawdown() /
    /// max_drawdown))`. Reads a book via `source:` (default:
    /// [`NodeSpec::StrategyBook`]). See
    /// [`crate::indicators::sizing::drawdown_throttle`].
    DrawdownThrottle {
        max_drawdown: Real,
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Realized-vol targeting on the book's equity return series
    /// — `target / (stddev(book.return_per_bar, window) *
    /// sqrt(bars_per_year))`. Reads a book via `source:` (default:
    /// [`NodeSpec::StrategyBook`]). See
    /// [`crate::indicators::sizing::equity_vol_target`].
    EquityVolTarget {
        target: Real,
        window: NonZeroUsize,
        bars_per_year: Real,
        #[serde(default = "default_sizing_seed")]
        seed: Real,
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
    },
    /// Fractional Kelly over the last `window` closed-trade returns —
    /// `kelly_fraction * mean / variance`, clamped to `>= 0`. Reads a book
    /// via `source:` (default: [`NodeSpec::StrategyBook`]). See
    /// [`crate::indicators::sizing::fractional_kelly`].
    FractionalKelly {
        kelly_fraction: Real,
        window: NonZeroUsize,
        #[serde(default = "default_sizing_seed")]
        seed: Real,
        #[serde(default)]
        source: Option<Box<NodeSpec>>,
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
        period: NonZeroUsize,
        bars_per_year: Real,
        #[serde(default = "default_risk_free_rate")]
        risk_free_rate: Real,
    },
    /// Trailing annualized Sortino of `strategy`'s equity curve. See
    /// [`crate::indicators::Sortino`].
    Sortino {
        strategy: Box<AnyStrategyRef>,
        period: NonZeroUsize,
        bars_per_year: Real,
        #[serde(default = "default_risk_free_rate")]
        risk_free_rate: Real,
    },
    /// Trailing annualized volatility of `strategy`'s equity return stream.
    /// See [`crate::indicators::Volatility`].
    Volatility {
        strategy: Box<AnyStrategyRef>,
        period: NonZeroUsize,
        bars_per_year: Real,
    },
    /// Trailing maximum drawdown of `strategy`'s equity curve, as a
    /// non-negative fraction. See [`crate::indicators::MaxDrawdown`].
    MaxDrawdown {
        strategy: Box<AnyStrategyRef>,
        period: NonZeroUsize,
    },
    /// Trailing Calmar (windowed CAGR / max drawdown) of `strategy`'s equity
    /// curve. See [`crate::indicators::Calmar`].
    Calmar {
        strategy: Box<AnyStrategyRef>,
        period: NonZeroUsize,
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
    Pow {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    Max {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    Min {
        lhs: Box<NodeSpec>,
        rhs: Box<NodeSpec>,
    },
    Clamp {
        source: Box<NodeSpec>,
        lower: Box<NodeSpec>,
        upper: Box<NodeSpec>,
    },
    Abs {
        source: Box<NodeSpec>,
    },
    Sign {
        source: Box<NodeSpec>,
    },
    Sqrt {
        source: Box<NodeSpec>,
    },
    Tanh {
        source: Box<NodeSpec>,
    },
    Sigmoid {
        source: Box<NodeSpec>,
    },
    CumSum {
        source: Box<NodeSpec>,
    },
    CumMax {
        source: Box<NodeSpec>,
    },
    CumMin {
        source: Box<NodeSpec>,
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
        #[serde(default = "lookback_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    Diff {
        #[serde(default = "lookback_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    Ratio {
        #[serde(default = "lookback_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    Roc {
        #[serde(default = "lookback_period_default")]
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    RollingMax {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    RollingMin {
        period: NonZeroUsize,
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
    },
    /// Logarithm of `source` in `base` (defaults to natural log, `e`).
    /// Emits `None` on samples where the source's output is non-positive.
    Log {
        #[serde(default = "default_source")]
        source: Box<NodeSpec>,
        #[serde(default = "default_log_base")]
        base: Real,
    },
    /// Exponential of `source` in `base` — `base^x`, the inverse of `!log`
    /// (defaults to the natural exponential, `e`). Emits `None` on samples
    /// whose result overflows the finite range.
    Exp {
        source: Box<NodeSpec>,
        #[serde(default = "default_exp_base")]
        base: Real,
    },
    /// Holds the most recent `Some` output of `source`, re-emitting it on
    /// ticks where `source` returns `None`. Wrap the outermost recursive
    /// smoother of a resampled pipeline so per-base-tick consumers see the
    /// finished higher-timeframe value between boundaries — see
    /// [`crate::indicators::Latch`].
    Latch {
        /// The source whose last `Some` output is held between ticks (required).
        source: Box<NodeSpec>,
    },
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
        every: NonZeroUsize,
        inner: Box<NodeSpec>,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// See [`NodeSpec::VolumeBars`].
    VolumeBars {
        threshold: Real,
        inner: Box<NodeSpec>,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// See [`NodeSpec::DollarBars`].
    DollarBars {
        threshold: Real,
        inner: Box<NodeSpec>,
        #[serde(default = "default_bar_source")]
        source: Box<NodeSpec>,
    },
    /// Passthrough wrapper that reports `unstable_bars() = 0`. The output
    /// and warm-up of `source` are unchanged; the strategy-readiness gate
    /// (which counts up to `stable_bars()`) no longer waits for this
    /// subtree's IIR settling tail. The explicit opt-out to the "wait for
    /// every source to be past its unstable tail" safe default; see
    /// [`crate::indicators::Unstable`].
    Unstable {
        /// The source whose unstable settling tail is ignored (required).
        source: Box<NodeSpec>,
    },

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
    /// `PayloadType::Time`, not a scalar). The `Timestamp` twin of
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
        source: Box<NodeSpec>,
        level: Real,
    },
    Below {
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
    // `known_node_tags` and covered by `From`.
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
    Every(NonZeroUsize),
    IsWeekday,
    IsWeekend,
    HasColumn {
        /// Overlay column name to test for existence.
        name: String,
    },
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
            NodeSpecRaw::Correlation { lhs, rhs, period } => {
                NodeSpec::Correlation { lhs, rhs, period }
            }
            NodeSpecRaw::Covariance { lhs, rhs, period } => {
                NodeSpec::Covariance { lhs, rhs, period }
            }
            NodeSpecRaw::Beta { lhs, rhs, period } => NodeSpec::Beta { lhs, rhs, period },
            NodeSpecRaw::LinregSlope { source, period } => NodeSpec::LinregSlope { source, period },
            NodeSpecRaw::LinregIntercept { source, period } => {
                NodeSpec::LinregIntercept { source, period }
            }
            NodeSpecRaw::LinregValue { source, period } => NodeSpec::LinregValue { source, period },
            NodeSpecRaw::LinregR2 { source, period } => NodeSpec::LinregR2 { source, period },
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
            NodeSpecRaw::StochRsi {
                source,
                rsi_period,
                stoch_period,
            } => NodeSpec::StochRsi {
                source,
                rsi_period,
                stoch_period,
            },
            NodeSpecRaw::MacdLine {
                source,
                fast,
                slow,
                signal,
            } => NodeSpec::MacdLine {
                source,
                fast,
                slow,
                signal,
            },
            NodeSpecRaw::MacdSignal {
                source,
                fast,
                slow,
                signal,
            } => NodeSpec::MacdSignal {
                source,
                fast,
                slow,
                signal,
            },
            NodeSpecRaw::MacdHistogram {
                source,
                fast,
                slow,
                signal,
            } => NodeSpec::MacdHistogram {
                source,
                fast,
                slow,
                signal,
            },
            NodeSpecRaw::BbUpper { source, period, k } => NodeSpec::BbUpper { source, period, k },
            NodeSpecRaw::BbMiddle { source, period, k } => NodeSpec::BbMiddle { source, period, k },
            NodeSpecRaw::BbLower { source, period, k } => NodeSpec::BbLower { source, period, k },
            NodeSpecRaw::KeltnerUpper {
                source,
                candle_source,
                ema_period,
                atr_period,
                multiplier,
            } => NodeSpec::KeltnerUpper {
                source,
                candle_source,
                ema_period,
                atr_period,
                multiplier,
            },
            NodeSpecRaw::KeltnerMiddle {
                source,
                candle_source,
                ema_period,
                atr_period,
                multiplier,
            } => NodeSpec::KeltnerMiddle {
                source,
                candle_source,
                ema_period,
                atr_period,
                multiplier,
            },
            NodeSpecRaw::KeltnerLower {
                source,
                candle_source,
                ema_period,
                atr_period,
                multiplier,
            } => NodeSpec::KeltnerLower {
                source,
                candle_source,
                ema_period,
                atr_period,
                multiplier,
            },
            NodeSpecRaw::DonchianUpper { high, low, period } => {
                NodeSpec::DonchianUpper { high, low, period }
            }
            NodeSpecRaw::DonchianMiddle { high, low, period } => {
                NodeSpec::DonchianMiddle { high, low, period }
            }
            NodeSpecRaw::DonchianLower { high, low, period } => {
                NodeSpec::DonchianLower { high, low, period }
            }
            NodeSpecRaw::Adx { source, period } => NodeSpec::Adx { source, period },
            NodeSpecRaw::PlusDi { source, period } => NodeSpec::PlusDi { source, period },
            NodeSpecRaw::MinusDi { source, period } => NodeSpec::MinusDi { source, period },
            NodeSpecRaw::DmiPlusDi { source, period } => NodeSpec::DmiPlusDi { source, period },
            NodeSpecRaw::DmiMinusDi { source, period } => NodeSpec::DmiMinusDi { source, period },
            NodeSpecRaw::AroonUp { source, period } => NodeSpec::AroonUp { source, period },
            NodeSpecRaw::AroonDown { source, period } => NodeSpec::AroonDown { source, period },
            NodeSpecRaw::AroonOscillator { source, period } => {
                NodeSpec::AroonOscillator { source, period }
            }
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
            NodeSpecRaw::VolTarget {
                source,
                target,
                window,
                bars_per_year,
            } => NodeSpec::VolTarget {
                source,
                target,
                window,
                bars_per_year,
            },
            NodeSpecRaw::AtrRisk {
                source,
                risk_frac,
                period,
                atr_multiple,
            } => NodeSpec::AtrRisk {
                source,
                risk_frac,
                period,
                atr_multiple,
            },
            NodeSpecRaw::DrawdownThrottle {
                source,
                max_drawdown,
            } => NodeSpec::DrawdownThrottle {
                source,
                max_drawdown,
            },
            NodeSpecRaw::EquityVolTarget {
                source,
                target,
                window,
                bars_per_year,
                seed,
            } => NodeSpec::EquityVolTarget {
                source,
                target,
                window,
                bars_per_year,
                seed,
            },
            NodeSpecRaw::FractionalKelly {
                source,
                kelly_fraction,
                window,
                seed,
            } => NodeSpec::FractionalKelly {
                source,
                kelly_fraction,
                window,
                seed,
            },
            NodeSpecRaw::Sharpe {
                strategy,
                period,
                bars_per_year,
                risk_free_rate,
            } => NodeSpec::Sharpe {
                strategy,
                period,
                bars_per_year,
                risk_free_rate,
            },
            NodeSpecRaw::Sortino {
                strategy,
                period,
                bars_per_year,
                risk_free_rate,
            } => NodeSpec::Sortino {
                strategy,
                period,
                bars_per_year,
                risk_free_rate,
            },
            NodeSpecRaw::Volatility {
                strategy,
                period,
                bars_per_year,
            } => NodeSpec::Volatility {
                strategy,
                period,
                bars_per_year,
            },
            NodeSpecRaw::MaxDrawdown { strategy, period } => {
                NodeSpec::MaxDrawdown { strategy, period }
            }
            NodeSpecRaw::Calmar {
                strategy,
                period,
                bars_per_year,
            } => NodeSpec::Calmar {
                strategy,
                period,
                bars_per_year,
            },
            NodeSpecRaw::Add { lhs, rhs } => NodeSpec::Add { lhs, rhs },
            NodeSpecRaw::Sub { lhs, rhs } => NodeSpec::Sub { lhs, rhs },
            NodeSpecRaw::Mul { lhs, rhs } => NodeSpec::Mul { lhs, rhs },
            NodeSpecRaw::Div { lhs, rhs } => NodeSpec::Div { lhs, rhs },
            NodeSpecRaw::Pow { lhs, rhs } => NodeSpec::Pow { lhs, rhs },
            NodeSpecRaw::Max { lhs, rhs } => NodeSpec::Max { lhs, rhs },
            NodeSpecRaw::Min { lhs, rhs } => NodeSpec::Min { lhs, rhs },
            NodeSpecRaw::Clamp {
                source,
                lower,
                upper,
            } => NodeSpec::Clamp {
                source,
                lower,
                upper,
            },
            NodeSpecRaw::Abs { source } => NodeSpec::Abs { source },
            NodeSpecRaw::Sign { source } => NodeSpec::Sign { source },
            NodeSpecRaw::Sqrt { source } => NodeSpec::Sqrt { source },
            NodeSpecRaw::Tanh { source } => NodeSpec::Tanh { source },
            NodeSpecRaw::Sigmoid { source } => NodeSpec::Sigmoid { source },
            NodeSpecRaw::CumSum { source } => NodeSpec::CumSum { source },
            NodeSpecRaw::CumMax { source } => NodeSpec::CumMax { source },
            NodeSpecRaw::CumMin { source } => NodeSpec::CumMin { source },
            NodeSpecRaw::IfElse {
                cond,
                then,
                otherwise,
            } => NodeSpec::IfElse {
                cond,
                then,
                otherwise,
            },
            NodeSpecRaw::Match { on, cases, default } => NodeSpec::Match { on, cases, default },
            NodeSpecRaw::Lag { source, period } => NodeSpec::Lag { source, period },
            NodeSpecRaw::Diff { source, period } => NodeSpec::Diff { source, period },
            NodeSpecRaw::Ratio { source, period } => NodeSpec::Ratio { source, period },
            NodeSpecRaw::Roc { source, period } => NodeSpec::Roc { source, period },
            NodeSpecRaw::RollingMax { source, period } => NodeSpec::RollingMax { source, period },
            NodeSpecRaw::RollingMin { source, period } => NodeSpec::RollingMin { source, period },
            NodeSpecRaw::Log { source, base } => NodeSpec::Log { source, base },
            NodeSpecRaw::Exp { source, base } => NodeSpec::Exp { source, base },
            NodeSpecRaw::Latch { source } => NodeSpec::Latch { source },
            NodeSpecRaw::Resample {
                every,
                inner,
                source,
            } => NodeSpec::Resample {
                every,
                inner,
                source,
            },
            NodeSpecRaw::VolumeBars {
                threshold,
                inner,
                source,
            } => NodeSpec::VolumeBars {
                threshold,
                inner,
                source,
            },
            NodeSpecRaw::DollarBars {
                threshold,
                inner,
                source,
            } => NodeSpec::DollarBars {
                threshold,
                inner,
                source,
            },
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
                    serde_norway::Value::Mapping(m) if m.is_empty() => serde_norway::Value::Null,
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
            serde_norway::Value::Number(n) => serde_norway::Value::Tagged(Box::new(TaggedValue {
                tag: Tag::new("value"),
                value: serde_norway::Value::Number(n),
            })),
            // Bare bool literal — auto-wrap as `!value true|false`. Bools are
            // never leaf names, so this is unambiguous: `enter: true` means the
            // constant-true signal. Subsumes the former signal-layer `!value
            // <bool>` / `!never` boilerplate.
            serde_norway::Value::Bool(b) => serde_norway::Value::Tagged(Box::new(TaggedValue {
                tag: Tag::new("value"),
                value: serde_norway::Value::Bool(b),
            })),
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
                serde_norway::Value::Number(n) => n.as_u64().ok_or_else(|| {
                    format!("!equal_weight: expected a positive integer leg count, got {n}")
                })?,
                other => {
                    return Err(format!(
                        "!equal_weight: expected a positive integer leg count, got {other:?}"
                    ));
                }
            };
            if n == 0 {
                return Err("!equal_weight: leg count must be strictly positive".to_string());
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

/// One spelling of a unary wrapper's inner expression, extracted.
struct WrapperInner {
    /// The inner expression, still untyped.
    value: serde_norway::Value,
    /// Whether the body named `source:` explicitly. This decides what happens
    /// when [`value`](Self::value) turns out not to parse: a keyed body has no
    /// other possible reading, so its error is *the* error; an unkeyed mapping
    /// might instead be a mis-spelled field, and the derived parse gives a
    /// better message for that than "unknown tag" would.
    keyed: bool,
}

/// Extract the inner payload of a unary wrapper tag. `None` when the outer tag
/// doesn't match `wanted`.
///
/// Three spellings, all equivalent — the `unary_source` pattern the grammar
/// descriptor declares (`spec::grammar::GrammarForm`):
///
/// - **bare** — `!changed close`, or the JSON bridge's
///   `{ "changed": { "sma": … } }`. A tagged inner cannot be written bare in
///   YAML (two tags on one node is a syntax error), so the tagged spelling is
///   reachable only through the bridge, which is exactly what a programmatic
///   consumer emits.
/// - **keyed** — `!changed { source: !month }`.
/// - **bare word** — `!changed close`, a plain string.
fn extract_wrapper_inner(v: &serde_norway::Value, wanted: &str) -> Option<WrapperInner> {
    let inner_payload = match v {
        serde_norway::Value::Tagged(tv) if tv.tag.to_string().trim_start_matches('!') == wanted => {
            &tv.value
        }
        _ => return None,
    };
    match inner_payload {
        serde_norway::Value::Mapping(m) if m.len() == 1 => match m.iter().next() {
            Some((serde_norway::Value::String(k), source)) if k == "source" => Some(WrapperInner {
                value: source.clone(),
                keyed: true,
            }),
            _ => Some(WrapperInner {
                value: inner_payload.clone(),
                keyed: false,
            }),
        },
        _ => Some(WrapperInner {
            value: inner_payload.clone(),
            keyed: false,
        }),
    }
}

/// The unary wrappers whose inner may be written bare or under `source:`, and
/// the node each builds. Kept as one table so the four cannot drift apart —
/// `!unstable` used to take a narrower set of spellings than the three edge
/// detectors, which meant the JSON bridge form `{"unstable": {"sma": …}}` was
/// rejected while `{"changed": {"sma": …}}` was accepted. Each of these is
/// declared to the grammar descriptor by `#[grammar(alt = "unary_source")]`,
/// and `tests/spec_grammar.rs` probes both spellings of every entry.
type WrapperCtor = fn(Box<NodeSpec>) -> NodeSpec;
const UNARY_WRAPPERS: &[(&str, WrapperCtor)] = &[
    ("changed", NodeSpec::Changed),
    ("became_true", NodeSpec::BecameTrue),
    ("became_false", NodeSpec::BecameFalse),
    ("unstable", |source| NodeSpec::Unstable { source }),
];

/// Dispatch the unary wrapper tags, whose inner is a bare node rather than the
/// `{ field: … }` map the derived Raw parse expects. Returns `Ok(Some(spec))`
/// on match, `Ok(None)` when this isn't one (or when the body is better
/// reported by the derived parse), `Err` when a keyed inner fails to parse.
///
/// The Real-vs-Bool decision for `!changed` happens at *build* time (dispatch
/// on the inner's `output_type`), so the inner is parsed once as a general
/// [`NodeSpec`] with no parse-time fallback dance.
fn try_dispatch_wrappers(v: &serde_norway::Value) -> Result<Option<NodeSpec>, String> {
    for (wanted, build) in UNARY_WRAPPERS {
        let Some(inner) = extract_wrapper_inner(v, wanted) else {
            continue;
        };
        return match NodeSpec::try_from(inner.value) {
            Ok(spec) => Ok(Some(build(Box::new(spec)))),
            // `{ source: <broken> }` says what it is; report the inner's error.
            Err(e) if inner.keyed => Err(e),
            // An unkeyed body that isn't a node — most likely a mis-spelled
            // field (`!unstable { signal: X }`). Fall through so the derived
            // parse can answer with `unknown field ...`, which names the slot;
            // "unknown tag `signal`" would not.
            Err(_) => Ok(None),
        };
    }
    Ok(None)
}

/// Resolve a spec's optional `epsilon:` into a concrete [`Tolerance`].
///
/// An explicitly-written `epsilon:` is **absolute**, in the operands' own units
/// — the user is stating a deadband they mean literally ("ignore moves under a
/// tick"). Omitting it yields
/// [`DEFAULT_TOLERANCE`](crate::indicators::DEFAULT_TOLERANCE), which is hybrid,
/// because the default has to work at every scale an expression can produce.
fn eps(epsilon: &Option<Real>) -> crate::indicators::Tolerance {
    epsilon.map_or(
        crate::indicators::DEFAULT_TOLERANCE,
        crate::indicators::Tolerance::absolute,
    )
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
    root: Root<'_>,
) -> Result<AnyChain, String> {
    let lhs_built = lhs.try_build(anchor, book, portfolio_book, schema, root)?;
    Ok(match lhs_built.output_type() {
        PayloadType::Real => {
            let l = lhs_built.into_real()?;
            let r = (rhs.try_build(anchor, book, portfolio_book, schema, root)?)
                .into_real()
                .map_err(|e| trail(rhs, e))?;
            let e = eps(&epsilon);
            if negate {
                any(compare::Ne::with_tolerance(l, r, e))
            } else {
                any(compare::Eq::with_tolerance(l, r, e))
            }
        }
        PayloadType::Str => {
            let l = lhs_built.into_str()?;
            let r = (rhs.try_build(anchor, book, portfolio_book, schema, root)?)
                .into_str()
                .map_err(|e| trail(rhs, e))?;
            if negate {
                any(compare::StrNe::new(l, r))
            } else {
                any(compare::StrEq::new(l, r))
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
    root: Root<'_>,
) -> Result<AnyChain, String> {
    if cases.is_empty() {
        return Err(
            "`cases` must not be empty (use `!if_else` for a single branch, \
                    or reduce to `default` if there's nothing to match)"
                .to_string(),
        );
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

    let as_real_branch = |s: &NodeSpec| -> Result<RealChain, String> {
        let built = s.try_build(anchor, book, portfolio_book, schema, root)?;
        built.into_real().map_err(|e| trail(s, e))
    };

    let default_ind = as_real_branch(default)?;

    if is_str {
        let on_built = on.try_build(anchor, book, portfolio_book, schema, root)?;
        let on_ind = on_built.into_str().map_err(|e| trail(on, e))?;
        let pairs: Vec<(Arc<str>, RealChain)> = cases
            .iter()
            .map(|c| {
                let pattern: Arc<str> = match &c.when {
                    ValueLit::Str(s) => Arc::from(s.as_str()),
                    _ => unreachable!("string-pattern branch, already validated"),
                };
                Ok((pattern, as_real_branch(&c.value)?))
            })
            .collect::<Result<_, String>>()?;
        Ok(any(MatchIndicator::new(on_ind, pairs, default_ind)))
    } else {
        let on_ind = as_real_branch(on)?;
        let pairs: Vec<(Real, RealChain)> = cases
            .iter()
            .map(|c| {
                let pattern: Real = match &c.when {
                    ValueLit::Real(x) => *x,
                    _ => unreachable!("numeric-pattern branch, already validated"),
                };
                Ok((pattern, as_real_branch(&c.value)?))
            })
            .collect::<Result<_, String>>()?;
        Ok(any(MatchIndicator::new(on_ind, pairs, default_ind)))
    }
}

fn atom_source_of(
    source: Option<&NodeSpec>,
    anchor: &Position,
    book: &Book,
    portfolio_book: Option<&Book>,
    schema: &Arc<Schema>,
    root: Root<'_>,
) -> Result<AtomChain, String> {
    match source {
        None => root_source(root, anchor, book, portfolio_book, schema),
        Some(s) => {
            let built = s.try_build(anchor, book, portfolio_book, schema, root)?;
            built.into_atom().map_err(|e| trail(s, e))
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
    root: Root<'_>,
) -> Result<AtomChain, String> {
    match source {
        None => Ok(crate::runtime::erase(pick_any_root())),
        Some(s) => {
            let built = s.try_build(anchor, book, portfolio_book, schema, root)?;
            built.into_atom().map_err(|e| trail(s, e))
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
    /// `Box<dyn PayloadIndicator>`. `anchor` is the owning strategy's
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
    /// `root_source`). Pass `None` from a context with no single blessed
    /// series and every price leaf must name its asset.
    pub fn build(
        &self,
        anchor: &Position,
        book: &Book,
        portfolio_book: Option<&Book>,
        schema: &Arc<Schema>,
        root: Root<'_>,
    ) -> AnyChain {
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
        root: Root<'_>,
    ) -> Result<AnyChain, String> {
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
        root: Root<'_>,
    ) -> Result<AnyChain, String> {
        use NodeSpec::*;
        // Recursive-build shorthands: build `s`, view it as a library-typed
        // `Indicator<Input=Snapshot, Output=Real>` (or Candle) so it drops
        // into a concrete library constructor. A type mismatch is attributed to
        // the *child* that produced the wrong output, which is where the author
        // has to look.
        let real = |s: &NodeSpec| -> Result<RealChain, String> {
            let built = s.try_build(anchor, book, portfolio_book, schema, root)?;
            built.into_real().map_err(|e| trail(s, e))
        };
        let candle = |s: &NodeSpec| -> Result<CandleChain, String> {
            let built = s.try_build(anchor, book, portfolio_book, schema, root)?;
            built.into_candle().map_err(|e| trail(s, e))
        };
        // Boolean-signal shorthands, for the absorbed signal variants.
        let boolean = |s: &NodeSpec| -> Result<BoolChain, String> {
            let built = s.try_build(anchor, book, portfolio_book, schema, root)?;
            built.into_bool().map_err(|e| trail(s, e))
        };
        let str_view = |s: &NodeSpec| -> Result<StrChain, String> {
            let built = s.try_build(anchor, book, portfolio_book, schema, root)?;
            built.into_str().map_err(|e| trail(s, e))
        };
        let str_operand = |s: &StrOperand| -> Result<StrChain, String> {
            let built = s.try_build(anchor, book, portfolio_book, schema, root)?;
            built.into_str().map_err(|e| match s {
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
                any(crate::indicators::Close::of(s))
            }
            High { source } => {
                let s = atom_src(source.as_ref())?;
                any(crate::indicators::High::of(s))
            }
            Low { source } => {
                let s = atom_src(source.as_ref())?;
                any(crate::indicators::Low::of(s))
            }
            Open { source } => {
                let s = atom_src(source.as_ref())?;
                any(crate::indicators::Open::of(s))
            }
            Volume { source } => {
                let s = atom_src(source.as_ref())?;
                any(crate::indicators::Volume::of(s))
            }
            Typical { source } => {
                let s = atom_src(source.as_ref())?;
                any(crate::indicators::Typical::of(s))
            }
            Median { source } => {
                let s = atom_src(source.as_ref())?;
                any(crate::indicators::Median::of(s))
            }
            Current { source } => {
                let s = atom_src(source.as_ref())?;
                any(crate::indicators::CurrentBar::of(s))
            }

            Pick { symbol, freq } => build_pick(
                symbol.as_deref(),
                freq.as_deref(),
                root,
                anchor,
                book,
                portfolio_book,
                schema,
            )?,

            Value(ValueLit::Real(x)) => any(self::Value::<Snapshot<Symbol>>::new(*x)),
            Value(ValueLit::Bool(b)) => {
                any(crate::indicators::ValueBool::<Snapshot<Symbol>>::new(*b))
            }
            Value(ValueLit::Str(s)) => any(ValueStr::<Snapshot<Symbol>>::new(s.as_str())),
            Value(ValueLit::List(_)) => {
                return Err("a list literal is only meaningful in a \
                            portfolio weight-share template — the per-child \
                            build pass rewrites it to !value <list[CHILD_INDEX]> \
                            before this arm ever runs. Either it's being used \
                            outside a portfolio, or PortfolioSpec::build failed \
                            to install the CHILD_INDEX arg."
                    .to_string());
            }
            Entry => any(anchor.entry::<Snapshot<Symbol>>()),
            Peak => any(anchor.peak::<Snapshot<Symbol>>()),
            Trough => any(anchor.trough::<Snapshot<Symbol>>()),

            StrategyBook | PortfolioBook => {
                return Err("a build-time source selector — it only makes \
                            sense as the `source:` of a book-reading node (e.g. \
                            `!drawdown { source: !portfolio_book }`), not as a \
                            standalone expression"
                    .to_string());
            }

            Equity { source } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                any(b.equity::<Snapshot<Symbol>>())
            }
            EquityPeak { source } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                any(b.equity_peak::<Snapshot<Symbol>>())
            }
            Drawdown { source } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                any(b.drawdown::<Snapshot<Symbol>>())
            }
            ReturnPerBar { source } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                any(b.return_per_bar::<Snapshot<Symbol>>())
            }
            TradePnl { source } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                any(b.trade_pnl::<Snapshot<Symbol>>())
            }
            TradeReturn { source } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                any(b.trade_return::<Snapshot<Symbol>>())
            }

            Get { key, source } => {
                let s = atom_src(source.as_ref())?;
                build_get(schema, key, s)?
            }

            Ema { source, period } => any(self::Ema::new(real(source)?, period.get())),
            Sma { source, period } => any(self::Sma::new(real(source)?, period.get())),
            Rma { source, period } => any(self::Rma::new(real(source)?, period.get())),
            Wma { source, period } => any(self::Wma::new(real(source)?, period.get())),
            Hma { source, period } => any(self::Hma::new(real(source)?, period.get())),
            Rsi { source, period } => any(self::Rsi::new(real(source)?, period.get())),
            StdDev { source, period } => any(self::StdDev::new(real(source)?, period.get())),
            Skewness { source, period } => any(self::Skewness::new(real(source)?, period.get())),
            Kurtosis { source, period } => any(self::Kurtosis::new(real(source)?, period.get())),
            ZScore { source, period } => any(self::ZScore::new(real(source)?, period.get())),
            Percentile {
                source,
                period,
                pct,
            } => any(self::Percentile::new(real(source)?, period.get(), *pct)),
            PercentileRank { source, period } => {
                any(self::PercentileRank::new(real(source)?, period.get()))
            }
            BarsSince { source } => {
                // Same shape as `IfElse`'s `cond`: a signal leg is built
                // a boolean-output NodeSpec, viewed as bool.
                let sig = {
                    let built = source.try_build(anchor, book, portfolio_book, schema, root)?;
                    built.into_bool().map_err(|e| trail(source, e))?
                };
                any(self::BarsSince::new(sig))
            }
            BarsSinceHigh { source, period } => {
                any(self::BarsSinceHigh::new(real(source)?, period.get()))
            }
            BarsSinceLow { source, period } => {
                any(self::BarsSinceLow::new(real(source)?, period.get()))
            }
            Correlation { lhs, rhs, period } => {
                any(self::Correlation::new(real(lhs)?, real(rhs)?, period.get()))
            }
            Covariance { lhs, rhs, period } => {
                any(self::Covariance::new(real(lhs)?, real(rhs)?, period.get()))
            }
            Beta { lhs, rhs, period } => any(self::Beta::new(real(lhs)?, real(rhs)?, period.get())),
            // The four regression readings share one fit, and each names which
            // end of it it wants. `NonZeroUsize` gets `period` past 0; the fit
            // needs a second point before a slope exists at all, which no type
            // here can express — so it is a build error, as `!variance_ratio`'s
            // relational bound is.
            LinregSlope { source, period } => any(linreg(real(source)?, *period)?.slope()),
            LinregIntercept { source, period } => any(linreg(real(source)?, *period)?.intercept()),
            LinregValue { source, period } => any(linreg(real(source)?, *period)?.value()),
            LinregR2 { source, period } => any(linreg(real(source)?, *period)?.r2()),
            VarianceRatio {
                source,
                period,
                lag,
            } => {
                // `NonZeroUsize` gets the field past 0, but this indicator has
                // *relational* bounds a type can't carry: it needs at least two
                // overlapping blocks to compare variances against. Both were
                // `assert!`s in the constructor.
                let (period, lag) = (period.get(), lag.get());
                if lag < 2 {
                    return Err(format!("`lag` must be at least 2, got {lag}"));
                }
                if period < lag + 2 {
                    return Err(format!(
                        "`period` must be at least `lag` + 2 (got period {period}, lag {lag}) \
                         — a shorter window has no overlapping blocks to compare"
                    ));
                }
                any(self::VarianceRatio::new(real(source)?, period, lag))
            }
            Cci { source, period } => any(self::Cci::new(real(source)?, period.get())),
            Stochastic { source, period } => {
                any(self::Stochastic::new(real(source)?, period.get()))
            }
            StochRsi {
                source,
                rsi_period,
                stoch_period,
            } => any(self::StochRsi::new(
                self::Rsi::new(real(source)?, rsi_period.get()),
                stoch_period.get(),
            )),

            MacdLine {
                source,
                fast,
                slow,
                signal,
            } => any(Component::new(
                Macd::new(real(source)?, fast.get(), slow.get(), signal.get()),
                |v: MacdValue| v.macd,
            )),
            MacdSignal {
                source,
                fast,
                slow,
                signal,
            } => any(Component::new(
                Macd::new(real(source)?, fast.get(), slow.get(), signal.get()),
                |v: MacdValue| v.signal,
            )),
            MacdHistogram {
                source,
                fast,
                slow,
                signal,
            } => any(Component::new(
                Macd::new(real(source)?, fast.get(), slow.get(), signal.get()),
                |v: MacdValue| v.histogram,
            )),

            BbUpper { source, period, k } => any(Component::new(
                Bollinger::new(real(source)?, period.get(), *k),
                |v: BollingerValue| v.upper,
            )),
            BbMiddle { source, period, k } => any(Component::new(
                Bollinger::new(real(source)?, period.get(), *k),
                |v: BollingerValue| v.middle,
            )),
            BbLower { source, period, k } => any(Component::new(
                Bollinger::new(real(source)?, period.get(), *k),
                |v: BollingerValue| v.lower,
            )),

            KeltnerUpper {
                source,
                candle_source,
                ema_period,
                atr_period,
                multiplier,
            } => any(Component::new(
                Keltner::new(
                    real(source)?,
                    candle(candle_source)?,
                    ema_period.get(),
                    atr_period.get(),
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
            } => any(Component::new(
                Keltner::new(
                    real(source)?,
                    candle(candle_source)?,
                    ema_period.get(),
                    atr_period.get(),
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
            } => any(Component::new(
                Keltner::new(
                    real(source)?,
                    candle(candle_source)?,
                    ema_period.get(),
                    atr_period.get(),
                    *multiplier,
                ),
                |v: KeltnerValue| v.lower,
            )),

            DonchianUpper { high, low, period } => any(Component::new(
                Donchian::new(real(high)?, real(low)?, period.get()),
                |v: DonchianValue| v.upper,
            )),
            DonchianMiddle { high, low, period } => any(Component::new(
                Donchian::new(real(high)?, real(low)?, period.get()),
                |v: DonchianValue| v.middle,
            )),
            DonchianLower { high, low, period } => any(Component::new(
                Donchian::new(real(high)?, real(low)?, period.get()),
                |v: DonchianValue| v.lower,
            )),

            Adx { source, period } => any(Component::new(
                self::Adx::new(candle(source)?, period.get()),
                |v: AdxValue| v.adx,
            )),
            PlusDi { source, period } => any(Component::new(
                self::Adx::new(candle(source)?, period.get()),
                |v: AdxValue| v.plus_di,
            )),
            MinusDi { source, period } => any(Component::new(
                self::Adx::new(candle(source)?, period.get()),
                |v: AdxValue| v.minus_di,
            )),
            DmiPlusDi { source, period } => any(Component::new(
                self::Dmi::new(candle(source)?, period.get()),
                |v: DmiValue| v.plus_di,
            )),
            DmiMinusDi { source, period } => any(Component::new(
                self::Dmi::new(candle(source)?, period.get()),
                |v: DmiValue| v.minus_di,
            )),

            AroonUp { source, period } => any(Component::new(
                self::Aroon::new(candle(source)?, period.get()),
                |v: AroonValue| v.up,
            )),
            AroonDown { source, period } => any(Component::new(
                self::Aroon::new(candle(source)?, period.get()),
                |v: AroonValue| v.down,
            )),
            AroonOscillator { source, period } => any(Component::new(
                self::Aroon::new(candle(source)?, period.get()),
                |v: AroonValue| v.oscillator,
            )),

            Atr { source, period } => any(self::Atr::new(candle(source)?, period.get())),
            Parkinson { source, period } => {
                any(self::Parkinson::new(candle(source)?, period.get()))
            }
            GarmanKlass { source, period } => {
                any(self::GarmanKlass::new(candle(source)?, period.get()))
            }
            RogersSatchell { source, period } => {
                any(self::RogersSatchell::new(candle(source)?, period.get()))
            }
            Mfi { source, period } => any(self::Mfi::new(candle(source)?, period.get())),
            WilliamsR { source, period } => {
                any(self::WilliamsR::new(candle(source)?, period.get()))
            }
            Obv { source } => any(self::Obv::new(candle(source)?)),
            Vwap { source, period } => any(self::Vwap::new(candle(source)?, period.get())),
            Ad { source } => any(self::Ad::new(candle(source)?)),
            TrueRange { source } => any(self::TrueRange::new(candle(source)?)),
            Sar { source, step, max } => any(self::Sar::new(candle(source)?, *step, *max)),

            VolTarget {
                source,
                target,
                window,
                bars_per_year,
            } => {
                let s = atom_src(source.as_ref())?;
                any(crate::indicators::sizing::vol_target_of::<Symbol, _>(
                    s,
                    *target,
                    window.get(),
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
                any(crate::indicators::sizing::atr_risk_of::<Symbol, _>(
                    s,
                    *risk_frac,
                    period.get(),
                    *atr_multiple,
                ))
            }
            DrawdownThrottle {
                source,
                max_drawdown,
            } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                any(crate::indicators::sizing::drawdown_throttle::<Symbol>(
                    b,
                    *max_drawdown,
                ))
            }
            EquityVolTarget {
                source,
                target,
                window,
                bars_per_year,
                seed,
            } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                any(crate::indicators::sizing::equity_vol_target::<Symbol>(
                    b,
                    *target,
                    window.get(),
                    *bars_per_year,
                    *seed,
                ))
            }
            FractionalKelly {
                source,
                kelly_fraction,
                window,
                seed,
            } => {
                let b = resolve_book_source(source.as_deref(), book, portfolio_book)?;
                any(crate::indicators::sizing::fractional_kelly::<Symbol>(
                    b,
                    *kelly_fraction,
                    window.get(),
                    *seed,
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
                period.get(),
                *risk_free_rate,
                *bars_per_year,
                schema,
            )?,
            Sortino {
                strategy,
                period,
                bars_per_year,
                risk_free_rate,
            } => trailing::build(
                TrailingMetric::Sortino,
                strategy,
                period.get(),
                *risk_free_rate,
                *bars_per_year,
                schema,
            )?,
            Volatility {
                strategy,
                period,
                bars_per_year,
            } => trailing::build(
                TrailingMetric::Volatility,
                strategy,
                period.get(),
                0.0,
                *bars_per_year,
                schema,
            )?,
            MaxDrawdown { strategy, period } => trailing::build(
                TrailingMetric::MaxDrawdown,
                strategy,
                period.get(),
                0.0,
                0.0,
                schema,
            )?,
            Calmar {
                strategy,
                period,
                bars_per_year,
            } => trailing::build(
                TrailingMetric::Calmar,
                strategy,
                period.get(),
                0.0,
                *bars_per_year,
                schema,
            )?,

            Add { lhs, rhs } => any(real(lhs)?.add(real(rhs)?)),
            Sub { lhs, rhs } => any(real(lhs)?.sub(real(rhs)?)),
            Mul { lhs, rhs } => any(real(lhs)?.mul(real(rhs)?)),
            Div { lhs, rhs } => any(real(lhs)?.div(real(rhs)?)),
            Pow { lhs, rhs } => any(self::Pow::new(real(lhs)?, real(rhs)?)),
            Max { lhs, rhs } => any(MaxOf::new(real(lhs)?, real(rhs)?)),
            Min { lhs, rhs } => any(MinOf::new(real(lhs)?, real(rhs)?)),
            Clamp {
                source,
                lower,
                upper,
            } => any(MinOf::new(
                MaxOf::new(real(source)?, real(lower)?),
                real(upper)?,
            )),
            Abs { source } => any(self::Abs::new(real(source)?)),
            Sign { source } => any(self::Sign::new(real(source)?)),
            Sqrt { source } => any(self::Sqrt::new(real(source)?)),
            Tanh { source } => any(self::Tanh::new(real(source)?)),
            Sigmoid { source } => any(self::Sigmoid::new(real(source)?)),
            CumSum { source } => any(self::CumSum::new(real(source)?)),
            CumMax { source } => any(self::CumMax::new(real(source)?)),
            CumMin { source } => any(self::CumMin::new(real(source)?)),
            IfElse {
                cond,
                then,
                otherwise,
            } => {
                let cond_ind = {
                    let built = cond.try_build(anchor, book, portfolio_book, schema, root)?;
                    built.into_bool().map_err(|e| trail(cond, e))?
                };
                let t_ind = real(then)?;
                let f_ind = real(otherwise)?;
                any(self::IfElse::new(cond_ind, t_ind, f_ind))
            }
            Match { on, cases, default } => build_match(
                on,
                cases,
                default,
                anchor,
                book,
                portfolio_book,
                schema,
                root,
            )?,
            Lag { source, period } => any(real(source)?.lag(period.get())),
            Diff { source, period } => any(real(source)?.diff(period.get())),
            Ratio { source, period } => any(real(source)?.ratio(period.get())),
            Roc { source, period } => any(real(source)?.roc(period.get())),
            RollingMax { source, period } => any(real(source)?.rolling_max(period.get())),
            RollingMin { source, period } => any(real(source)?.rolling_min(period.get())),
            Log { source, base } => any(self::Log::new(real(source)?, checked_base(*base)?)),
            Exp { source, base } => any(self::Exp::new(real(source)?, checked_base(*base)?)),
            Latch { source } => {
                let inner = {
                    let built = source.try_build(anchor, book, portfolio_book, schema, root)?;
                    built.into_real().map_err(|e| trail(source, e))?
                };
                any(self::Latch::new(inner))
            }
            Resample {
                every,
                inner,
                source,
            } => {
                // No zero guard: `every` is a `NonZeroUsize`, so serde rejected
                // 0 before this node existed.
                let candle_src = candle(source)?;
                let resampled = crate::runtime::erase(self::Resample::new(candle_src, every.get()));
                let inner_dyn = inner.try_build(anchor, book, portfolio_book, schema, root)?;
                crate::runtime::chain_over_candle(resampled, inner_dyn)
            }
            VolumeBars {
                threshold,
                inner,
                source,
            } => {
                let threshold = positive_threshold(*threshold, "volume_bars")?;
                let candle_src = candle(source)?;
                let sampled = crate::runtime::erase(crate::indicators::VolumeBars::new(
                    candle_src, threshold,
                ));
                let inner_dyn = inner.try_build(anchor, book, portfolio_book, schema, root)?;
                crate::runtime::chain_over_candle(sampled, inner_dyn)
            }
            DollarBars {
                threshold,
                inner,
                source,
            } => {
                let threshold = positive_threshold(*threshold, "dollar_bars")?;
                let candle_src = candle(source)?;
                let sampled = crate::runtime::erase(crate::indicators::DollarBars::new(
                    candle_src, threshold,
                ));
                let inner_dyn = inner.try_build(anchor, book, portfolio_book, schema, root)?;
                crate::runtime::chain_over_candle(sampled, inner_dyn)
            }
            Unstable { source } => source
                .try_build(anchor, book, portfolio_book, schema, root)?
                .unstable(),

            Year { source } => {
                let s = atom_src_any(source.as_ref())?;
                any(crate::indicators::Year::of(s))
            }
            Month { source } => {
                let s = atom_src_any(source.as_ref())?;
                any(crate::indicators::Month::of(s))
            }
            Day { source } => {
                let s = atom_src_any(source.as_ref())?;
                any(crate::indicators::Day::of(s))
            }
            Hour { source } => {
                let s = atom_src_any(source.as_ref())?;
                any(crate::indicators::Hour::of(s))
            }
            Minute { source } => {
                let s = atom_src_any(source.as_ref())?;
                any(crate::indicators::Minute::of(s))
            }
            Second { source } => {
                let s = atom_src_any(source.as_ref())?;
                any(crate::indicators::Second::of(s))
            }
            DayOfWeek { source } => {
                let s = atom_src_any(source.as_ref())?;
                any(crate::indicators::DayOfWeek::of(s))
            }
            DayOfYear { source } => {
                let s = atom_src_any(source.as_ref())?;
                any(crate::indicators::DayOfYear::of(s))
            }
            WeekOfYear { source } => {
                let s = atom_src_any(source.as_ref())?;
                any(crate::indicators::WeekOfYear::of(s))
            }
            Quarter { source } => {
                let s = atom_src_any(source.as_ref())?;
                any(crate::indicators::Quarter::of(s))
            }
            UnixSeconds { source } => {
                let s = atom_src_any(source.as_ref())?;
                any(crate::indicators::UnixSeconds::of(s))
            }
            UnixMillis { source } => {
                let s = atom_src_any(source.as_ref())?;
                any(crate::indicators::UnixMillis::of(s))
            }
            Time { source } => {
                let s = atom_src_any(source.as_ref())?;
                any(crate::indicators::CurrentTime::of(s))
            }

            // --- absorbed boolean signals ---
            Gt { lhs, rhs, epsilon } => any(compare::Gt::with_tolerance(
                real(lhs)?,
                real(rhs)?,
                eps(epsilon),
            )),
            Lt { lhs, rhs, epsilon } => any(compare::Lt::with_tolerance(
                real(lhs)?,
                real(rhs)?,
                eps(epsilon),
            )),
            Ge { lhs, rhs, epsilon } => any(compare::Ge::with_tolerance(
                real(lhs)?,
                real(rhs)?,
                eps(epsilon),
            )),
            Le { lhs, rhs, epsilon } => any(compare::Le::with_tolerance(
                real(lhs)?,
                real(rhs)?,
                eps(epsilon),
            )),
            Eq { lhs, rhs, epsilon } => build_polymorphic_eq(
                lhs,
                rhs,
                *epsilon,
                false,
                anchor,
                book,
                portfolio_book,
                schema,
                root,
            )?,
            Ne { lhs, rhs, epsilon } => build_polymorphic_eq(
                lhs,
                rhs,
                *epsilon,
                true,
                anchor,
                book,
                portfolio_book,
                schema,
                root,
            )?,
            Above { source, level } => any(real(source)?.above(*level)),
            Below { source, level } => any(real(source)?.below(*level)),
            CrossesAbove { lhs, rhs } => {
                let (l, r) = (real(lhs)?, real(rhs)?);
                let cmp = l.gt(r);
                any(cmp.clone().and(cmp.changed()))
            }
            CrossesBelow { lhs, rhs } => {
                let (l, r) = (real(lhs)?, real(rhs)?);
                let cmp = l.lt(r);
                any(cmp.clone().and(cmp.changed()))
            }
            And { lhs, rhs } => any(boolean(lhs)?.and(boolean(rhs)?)),
            Or { lhs, rhs } => any(boolean(lhs)?.or(boolean(rhs)?)),
            Xor { lhs, rhs } => any(boolean(lhs)?.xor(boolean(rhs)?)),
            All(specs) => {
                if specs.is_empty() {
                    any(crate::indicators::ValueBool::<Snapshot<Symbol>>::new(true))
                } else {
                    let mut acc = boolean(&specs[0])?;
                    for s in &specs[1..] {
                        let next = boolean(s)?;
                        acc = crate::runtime::erase(acc.and(next));
                    }
                    any(acc)
                }
            }
            Any(specs) => {
                if specs.is_empty() {
                    any(crate::indicators::ValueBool::<Snapshot<Symbol>>::new(false))
                } else {
                    let mut acc = boolean(&specs[0])?;
                    for s in &specs[1..] {
                        let next = boolean(s)?;
                        acc = crate::runtime::erase(acc.or(next));
                    }
                    any(acc)
                }
            }
            Not(inner) => any(boolean(inner)?.not()),
            Changed(inner) => {
                let built = inner.try_build(anchor, book, portfolio_book, schema, root)?;
                match built.output_type() {
                    PayloadType::Bool => any(built.into_bool()?.changed()),
                    PayloadType::Real => any(built.into_real()?.changed()),
                    other => {
                        return Err(trail(
                            inner,
                            format!("!changed needs a Bool or Real inner, got {other}"),
                        ));
                    }
                }
            }
            BecameTrue(inner) => any(boolean(inner)?.became_true()),
            BecameFalse(inner) => any(boolean(inner)?.became_false()),
            StrEq { lhs, rhs } => any(compare::StrEq::new(str_view(lhs)?, str_operand(rhs)?)),
            StrNe { lhs, rhs } => any(compare::StrNe::new(str_view(lhs)?, str_operand(rhs)?)),
            Never => any(crate::indicators::ValueBool::<Snapshot<Symbol>>::new(false)),
            Every(n) => any(crate::indicators::Every::<Snapshot<Symbol>>::new(n.get())),
            IsWeekday => any(crate::indicators::IsWeekday::of(pick_any_root())),
            IsWeekend => any(crate::indicators::IsWeekend::of(pick_any_root())),
            HasColumn { name } => {
                let exists = schema.index_of(name.as_str()).is_some();
                any(crate::indicators::ValueBool::<Snapshot<Symbol>>::new(
                    exists,
                ))
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
    root: Root<'_>,
    anchor: &Position,
    book: &Book,
    portfolio_book: Option<&Book>,
    schema: &Arc<Schema>,
) -> Result<AnyChain, String> {
    let named = symbol.is_some();
    // The document gives a `&str`; interning happens here, once at build time,
    // so the resulting `Selector` clones as a refcount bump for the whole run.
    let sym = symbol
        .map(crate::types::symbol)
        .or_else(|| root.blessed_symbol());
    let selector = Selector::<Symbol> {
        symbol: sym,
        stream: freq.map(crate::types::stream),
    };
    Ok(if selector.is_empty() {
        // A bare `!pick {}` naming neither symbol nor freq, with no root to
        // borrow one from: the same unanswerable question `root_source`
        // refuses. (A freq-only selector is fine — `Pick::rooted` falls back
        // through `sole_atom_or_none` and reads `None` rather than panicking.)
        any(root_source(root, anchor, book, portfolio_book, schema)?)
    } else if named {
        any(Pick::<Symbol>::matching(selector))
    } else {
        // Symbol came from the root, so this is still the implicit
        // "this series" read — keep the sole-atom fallback that makes an
        // untagged single-entry snapshot resolve. See `Pick::rooted`.
        any(Pick::<Symbol>::rooted(selector))
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
/// `--series` or `file:` data with additional columns to attach overlays")
/// from the non-empty case ("registered: a, b, c").
fn build_get(schema: &Arc<Schema>, key: &str, source: AtomChain) -> Result<AnyChain, String> {
    match schema.type_of_key(key) {
        Some(OverlayType::Real) => Ok(any(GetReal::of(schema, key, source))),
        Some(OverlayType::Bool) => Ok(any(GetBool::of(schema, key, source))),
        Some(OverlayType::Str) => Ok(any(GetStr::of(schema, key, source))),
        None => {
            let registered: Vec<&str> = schema.keys().collect();
            if registered.is_empty() {
                Err(format!(
                    "overlay column {key:?}: no overlay side channel is bound — feed \
                     `--series` data or a `file:` source that carries additional \
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
