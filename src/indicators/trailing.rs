//! Trailing risk-adjusted-return indicators — a rolling
//! [`metrics`](crate::metrics) reading computed *live* over a strategy's own
//! equity curve.
//!
//! Sharpe, Sortino, volatility, max-drawdown and Calmar are all functions of an
//! **equity-curve return stream**, not of a candle-derived price series — so
//! unlike every other indicator these do not wrap a price source. Instead each
//! **owns a [`Strategy`]**, drives it one bar at a time against a private
//! in-memory [`PaperWallet`] (exactly the per-bar loop of
//! [`backtest::run`](crate::backtest::run)), and reduces the resulting
//! marked-to-market equity to the trailing metric over a rolling `period`-bar
//! window. That removes the "run the strategy → dump `returns.csv` → join it
//! back as an overlay" round-trip: the trailing risk estimate is now a first
//! class source, readable as an overlay column or composed into another
//! strategy (size down as a regime proxy's trailing Sharpe degrades, gate an
//! entry on a positive Calmar, …).
//!
//! # Input, tagging, and the embedded wallet
//!
//! `Input = Snapshot<Sym>`, `Output = Real`. Each bar the indicator forwards the
//! **whole** incoming snapshot to the embedded strategy and prices the embedded
//! wallet from every entry — the same per-bar loop as
//! [`backtest::run`](crate::backtest::run). Already-tagged entries flow through
//! untouched, so a **multi-asset** strategy (pairs, basket, or any strategy that
//! names its own symbols via
//! [`Pick::matching`](crate::indicators::Pick::matching)) prices and trades every
//! leg. Any **untagged** entry is first tagged with the strategy's own `symbol`,
//! so a single-asset strategy fed the untagged size-1 snapshot of the CLI
//! overlay path (a `DynValue::Atom` lift) prices its wallet and reads its
//! sole-atom leaves exactly as before — the symbol demotes from a forced re-tag
//! to a fallback for untagged input.
//!
//! The single-vs-multi tripwire is no longer here: a single-asset strategy
//! accidentally fed a 2+-entry snapshot trips the implicit
//! [`Pick::new`](crate::indicators::Pick::new) panic inside the strategy, the one
//! place it already lives.
//!
//! # Formulas and parity with [`metrics`](crate::metrics)
//!
//! The internal [`PaperWallet`] is seeded at construction and `prev_equity` is
//! seeded to that same value, so bar 0 produces a return exactly as
//! [`per_bar_returns`](crate::metrics::per_bar_returns) does. Sharpe / Sortino /
//! volatility therefore equal the whole-run [`metrics`](crate::metrics) numbers
//! when `period` spans the whole run (`sample_stddev`, downside `n`-divisor, and
//! `bpy` annualization all match). Because equity is scale-invariant for the
//! ratio metrics, the wallet seed does not affect the readings.
//!
//! # Warm-up
//!
//! [`warm_up_period`](Indicator::warm_up_period) reports `period` (the bars
//! needed to fill the return / equity window). Note this is a **lower bound**
//! on the first `Some`, not exact: the embedded strategy is flat (zero-variance
//! equity → `None` for Sharpe/Sortino/Calmar) until its own readiness gate
//! elapses and it takes a position, and a strategy that never trades never
//! produces a meaningful reading. For that reason these are deliberately
//! excluded from the exact-warm-up battery in `tests/warm_up.rs` (same footing
//! as [`IfElse`](crate::indicators::IfElse)).

use std::collections::VecDeque;
use std::hash::Hash;

use crate::indicator::Indicator;
use crate::indicators::stats::WindowStats;
use crate::strategy::Strategy;
use crate::types::{Real, Snapshot};
use crate::wallet::{PaperWallet, Wallet};

/// `Some(num / denom)` when `denom` is a positive finite number, else `None` —
/// the same degenerate-denominator guard [`metrics`](crate::metrics) uses for
/// its risk-adjusted ratios.
fn safe_div(num: Real, denom: Real) -> Option<Real> {
    if denom > 0.0 && denom.is_finite() {
        Some(num / denom)
    } else {
        None
    }
}

/// Max drawdown over an equity slice as a non-negative fraction — the largest
/// peak-to-trough decline. Mirrors the depth reduction in
/// [`metrics::drawdown_segments`](crate::metrics::drawdown_segments) /
/// [`max_drawdown`](crate::metrics::max_drawdown).
fn max_drawdown(equity: &[Real]) -> Real {
    let mut peak = equity.first().copied().unwrap_or(0.0);
    let mut mdd = 0.0;
    for &e in equity {
        if e > peak {
            peak = e;
        } else if peak > 0.0 {
            let dd = (peak - e) / peak;
            if dd > mdd {
                mdd = dd;
            }
        }
    }
    mdd
}

/// CAGR of `curve` compounding from `initial` over `bars` bars. `None` on a
/// non-positive endpoint (matching [`metrics`](crate::metrics)' `cagr`).
fn cagr(initial: Real, final_equity: Real, bars: usize, bars_per_year: Real) -> Option<Real> {
    if initial <= 0.0 || final_equity <= 0.0 || bars == 0 || bars_per_year <= 0.0 {
        return None;
    }
    let years = bars as Real / bars_per_year;
    if years <= 0.0 {
        return None;
    }
    Some((final_equity / initial).powf(1.0 / years) - 1.0)
}

// ---------------------------------------------------------------------------
// Shared engine: drive an owned Strategy against a private PaperWallet.
// ---------------------------------------------------------------------------

/// Drives an owned [`Strategy`] over a private [`PaperWallet`], one snapshot per
/// [`step`](Self::step), exposing the marked-to-market equity and the per-bar
/// equity return. Replicates the per-bar loop of
/// [`backtest::run`](crate::backtest::run) verbatim, so an embedded strategy
/// produces the same equity curve a standalone backtest would — including a
/// **multi-asset** one (pairs, basket, or any strategy that names its own
/// symbols).
///
/// The incoming snapshot is forwarded to the strategy *whole*: already-tagged
/// entries flow through untouched (the multi-asset path), and any **untagged**
/// entry is tagged with `fallback_symbol` first — so a single-asset strategy fed
/// the untagged size-1 snapshot of the CLI overlay path still prices its wallet
/// and reads its sole-atom leaves exactly as the old forced re-tag did.
struct StrategyEngine<S: Strategy> {
    strategy: S,
    wallet: PaperWallet<S::Symbol>,
    /// The tag applied to untagged snapshot entries. Demoted from a forced
    /// re-tag to a fallback: tagged entries keep their own symbol.
    fallback_symbol: S::Symbol,
    seed: Real,
    prev_equity: Real,
}

impl<Sym, S> StrategyEngine<S>
where
    Sym: Clone + Eq + Hash,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>>,
{
    fn new(strategy: S, fallback_symbol: Sym, seed: Real) -> Self {
        Self {
            strategy,
            wallet: PaperWallet::new(seed),
            fallback_symbol,
            seed,
            prev_equity: seed,
        }
    }

    /// Tag every untagged entry with `fallback_symbol`; leave tagged entries as
    /// they are. The result is what both the wallet-pricing loop and the
    /// strategy see, so a single-asset strategy on untagged input behaves
    /// exactly as the old forced re-tag did, while a tagged multi-asset snapshot
    /// passes through so each leg prices and trades.
    fn tagged(&self, snap: Snapshot<Sym>) -> Snapshot<Sym> {
        snap.iter()
            .map(|(sym, freq, atom)| {
                let sym = sym.cloned().or_else(|| Some(self.fallback_symbol.clone()));
                (sym, freq, atom.clone())
            })
            .collect()
    }

    /// Advance one bar; return `(equity, per-bar return)`. Prices the wallet on
    /// every (fallback-tagged) entry, routes the fills to `on_fill`, updates the
    /// strategy with the whole snapshot, `trade`s it iff ready, then marks to
    /// market — the per-bar loop of [`backtest::run`](crate::backtest::run).
    fn step(&mut self, snap: Snapshot<Sym>) -> (Real, Real) {
        let snap = self.tagged(snap);
        for (sym, _freq, atom) in snap.iter() {
            let Some(sym) = sym else { continue };
            for fill in self.wallet.update(sym.clone(), atom.candle) {
                self.strategy.on_fill(&fill);
            }
        }
        self.strategy.update(snap);
        if self.strategy.is_ready() {
            self.strategy.trade(&mut self.wallet);
        }
        let equity = self.wallet.equity().0;
        let ret = if self.prev_equity.abs() > f64::EPSILON {
            (equity - self.prev_equity) / self.prev_equity
        } else {
            0.0
        };
        self.prev_equity = equity;
        (equity, ret)
    }

    fn reset(&mut self) {
        self.strategy.reset();
        self.wallet.reset();
        self.prev_equity = self.seed;
    }
}

// ---------------------------------------------------------------------------
// Return-window metrics: Sharpe, Sortino, Volatility.
// ---------------------------------------------------------------------------

/// **Trailing annualized Sharpe ratio** of an owned [`Strategy`]'s equity
/// curve, `(mean·bpy − rf) / (sample_stddev·√bpy)` over the last `period`
/// per-bar returns. `None` while the window is filling and whenever the
/// windowed return volatility is zero. See the [module docs](self).
pub struct Sharpe<S: Strategy> {
    engine: StrategyEngine<S>,
    stats: WindowStats,
    risk_free_rate: Real,
    bars_per_year: Real,
    value: Option<Real>,
}

impl<Sym, S> Sharpe<S>
where
    Sym: Clone + Eq + Hash,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>>,
{
    /// `symbol` is the fallback tag applied to untagged snapshot entries (the
    /// instrument a single-asset embedded strategy prices); a tagged multi-asset
    /// snapshot ignores it. `seed` is the wallet's starting cash (scale-invariant
    /// for the ratio); `risk_free_rate` the annualized rf as a fraction.
    ///
    /// # Panics
    /// Panics if `period == 0`.
    pub fn new(
        strategy: S,
        symbol: Sym,
        seed: Real,
        period: usize,
        risk_free_rate: Real,
        bars_per_year: Real,
    ) -> Self {
        Self {
            engine: StrategyEngine::new(strategy, symbol, seed),
            stats: WindowStats::new(period),
            risk_free_rate,
            bars_per_year,
            value: None,
        }
    }
}

impl<Sym, S> Indicator for Sharpe<S>
where
    Sym: Clone + Eq + Hash,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>>,
{
    type Input = Snapshot<Sym>;
    type Output = Real;

    fn update(&mut self, snap: Snapshot<Sym>) -> Option<Real> {
        if snap.is_empty() {
            return self.value;
        }
        let (_equity, ret) = self.engine.step(snap);
        if self.stats.update(ret) {
            let excess = self.stats.mean() * self.bars_per_year - self.risk_free_rate;
            let vol = self.stats.sample_stddev() * self.bars_per_year.max(0.0).sqrt();
            self.value = safe_div(excess, vol);
        }
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.stats.period()
    }

    fn reset(&mut self) {
        self.engine.reset();
        self.stats.reset();
        self.value = None;
    }
}

/// **Trailing annualized Sortino ratio** of an owned [`Strategy`]'s equity
/// curve, `(mean·bpy − rf) / (downside_dev·√bpy)` over the last `period`
/// per-bar returns, where the downside deviation uses the per-bar rf as its
/// minimum acceptable return. `None` while filling and when no return in the
/// window falls below the threshold. See the [module docs](self).
pub struct Sortino<S: Strategy> {
    engine: StrategyEngine<S>,
    stats: WindowStats,
    risk_free_rate: Real,
    bars_per_year: Real,
    value: Option<Real>,
}

impl<Sym, S> Sortino<S>
where
    Sym: Clone + Eq + Hash,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>>,
{
    /// # Panics
    /// Panics if `period == 0`.
    pub fn new(
        strategy: S,
        symbol: Sym,
        seed: Real,
        period: usize,
        risk_free_rate: Real,
        bars_per_year: Real,
    ) -> Self {
        Self {
            engine: StrategyEngine::new(strategy, symbol, seed),
            stats: WindowStats::new(period),
            risk_free_rate,
            bars_per_year,
            value: None,
        }
    }
}

impl<Sym, S> Indicator for Sortino<S>
where
    Sym: Clone + Eq + Hash,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>>,
{
    type Input = Snapshot<Sym>;
    type Output = Real;

    fn update(&mut self, snap: Snapshot<Sym>) -> Option<Real> {
        if snap.is_empty() {
            return self.value;
        }
        let (_equity, ret) = self.engine.step(snap);
        if self.stats.update(ret) {
            let rf_per_bar = if self.bars_per_year > 0.0 {
                self.risk_free_rate / self.bars_per_year
            } else {
                0.0
            };
            let excess = self.stats.mean() * self.bars_per_year - self.risk_free_rate;
            let downside = self.stats.downside_dev(rf_per_bar) * self.bars_per_year.max(0.0).sqrt();
            self.value = safe_div(excess, downside);
        }
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.stats.period()
    }

    fn reset(&mut self) {
        self.engine.reset();
        self.stats.reset();
        self.value = None;
    }
}

/// **Trailing annualized volatility** of an owned [`Strategy`]'s equity curve,
/// `sample_stddev·√bpy` over the last `period` per-bar returns. Always `Some`
/// (and `>= 0`) once the window fills. See the [module docs](self).
pub struct Volatility<S: Strategy> {
    engine: StrategyEngine<S>,
    stats: WindowStats,
    bars_per_year: Real,
    value: Option<Real>,
}

impl<Sym, S> Volatility<S>
where
    Sym: Clone + Eq + Hash,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>>,
{
    /// # Panics
    /// Panics if `period == 0`.
    pub fn new(strategy: S, symbol: Sym, seed: Real, period: usize, bars_per_year: Real) -> Self {
        Self {
            engine: StrategyEngine::new(strategy, symbol, seed),
            stats: WindowStats::new(period),
            bars_per_year,
            value: None,
        }
    }
}

impl<Sym, S> Indicator for Volatility<S>
where
    Sym: Clone + Eq + Hash,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>>,
{
    type Input = Snapshot<Sym>;
    type Output = Real;

    fn update(&mut self, snap: Snapshot<Sym>) -> Option<Real> {
        if snap.is_empty() {
            return self.value;
        }
        let (_equity, ret) = self.engine.step(snap);
        if self.stats.update(ret) {
            self.value = Some(self.stats.sample_stddev() * self.bars_per_year.max(0.0).sqrt());
        }
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.stats.period()
    }

    fn reset(&mut self) {
        self.engine.reset();
        self.stats.reset();
        self.value = None;
    }
}

// ---------------------------------------------------------------------------
// Equity-window metrics: MaxDrawdown, Calmar.
// ---------------------------------------------------------------------------

/// **Trailing maximum drawdown** of an owned [`Strategy`]'s equity curve — the
/// largest peak-to-trough decline over the trailing window, as a non-negative
/// fraction (`0.20` = a 20% drawdown). Always `Some` once the window fills
/// (`0.0` on a flat or monotonically-rising window). See the [module docs](self).
pub struct MaxDrawdown<S: Strategy> {
    engine: StrategyEngine<S>,
    period: usize,
    equity: VecDeque<Real>,
    value: Option<Real>,
}

impl<Sym, S> MaxDrawdown<S>
where
    Sym: Clone + Eq + Hash,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>>,
{
    /// # Panics
    /// Panics if `period == 0`.
    pub fn new(strategy: S, symbol: Sym, seed: Real, period: usize) -> Self {
        assert!(period > 0, "period must be > 0");
        let mut equity = VecDeque::with_capacity(period + 1);
        // Seed the window with the wallet's opening equity so the first full
        // window lands at bar `period - 1` (warm-up `period`, matching the
        // return-window metrics).
        equity.push_back(seed);
        Self {
            engine: StrategyEngine::new(strategy, symbol, seed),
            period,
            equity,
            value: None,
        }
    }
}

impl<Sym, S> Indicator for MaxDrawdown<S>
where
    Sym: Clone + Eq + Hash,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>>,
{
    type Input = Snapshot<Sym>;
    type Output = Real;

    fn update(&mut self, snap: Snapshot<Sym>) -> Option<Real> {
        if snap.is_empty() {
            return self.value;
        }
        let (equity, _ret) = self.engine.step(snap);
        self.equity.push_back(equity);
        if self.equity.len() > self.period + 1 {
            self.equity.pop_front();
        }
        if self.equity.len() == self.period + 1 {
            self.value = Some(max_drawdown(self.equity.make_contiguous()));
        }
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.period
    }

    fn reset(&mut self) {
        self.engine.reset();
        self.equity.clear();
        self.equity.push_back(self.engine.seed);
        self.value = None;
    }
}

/// **Trailing Calmar ratio** of an owned [`Strategy`]'s equity curve —
/// windowed CAGR over trailing max drawdown. `None` while filling, when the
/// window has no drawdown (zero denominator), or when the CAGR endpoints are
/// non-positive. See the [module docs](self).
pub struct Calmar<S: Strategy> {
    engine: StrategyEngine<S>,
    period: usize,
    bars_per_year: Real,
    equity: VecDeque<Real>,
    value: Option<Real>,
}

impl<Sym, S> Calmar<S>
where
    Sym: Clone + Eq + Hash,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>>,
{
    /// # Panics
    /// Panics if `period == 0`.
    pub fn new(strategy: S, symbol: Sym, seed: Real, period: usize, bars_per_year: Real) -> Self {
        assert!(period > 0, "period must be > 0");
        let mut equity = VecDeque::with_capacity(period + 1);
        equity.push_back(seed);
        Self {
            engine: StrategyEngine::new(strategy, symbol, seed),
            period,
            bars_per_year,
            equity,
            value: None,
        }
    }
}

impl<Sym, S> Indicator for Calmar<S>
where
    Sym: Clone + Eq + Hash,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>>,
{
    type Input = Snapshot<Sym>;
    type Output = Real;

    fn update(&mut self, snap: Snapshot<Sym>) -> Option<Real> {
        if snap.is_empty() {
            return self.value;
        }
        let (equity, _ret) = self.engine.step(snap);
        self.equity.push_back(equity);
        if self.equity.len() > self.period + 1 {
            self.equity.pop_front();
        }
        if self.equity.len() == self.period + 1 {
            let marks = self.equity.make_contiguous();
            let initial = marks[0];
            let curve = &marks[1..];
            let final_equity = *curve.last().expect("curve has period >= 1 marks");
            self.value = match cagr(initial, final_equity, self.period, self.bars_per_year) {
                Some(c) => safe_div(c, max_drawdown(marks)),
                None => None,
            };
        }
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.period
    }

    fn reset(&mut self) {
        self.engine.reset();
        self.equity.clear();
        self.equity.push_back(self.engine.seed);
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest;
    use crate::metrics;
    use crate::strategies::SingleAssetStrategy;
    use crate::types::{Atom, Candle};

    const SYM: &str = "X";
    const SEED: Real = 1_000.0;
    const BPY: Real = 252.0;

    fn bar(close: Real) -> Candle {
        Candle::new(close, close, close, close, 0.0)
    }

    fn snap(close: Real) -> Snapshot<&'static str> {
        Snapshot::single(SYM, Atom::new(bar(close)))
    }

    /// A rising-then-wobbling price path long enough to fill a window and
    /// produce non-degenerate returns.
    fn prices() -> Vec<Real> {
        vec![
            100.0, 102.0, 101.0, 104.0, 103.0, 108.0, 110.0, 107.0, 112.0, 115.0, 113.0, 118.0,
            120.0, 119.0, 124.0, 126.0,
        ]
    }

    fn buy_and_hold() -> SingleAssetStrategy<&'static str> {
        SingleAssetStrategy::buy_and_hold(SYM)
    }

    #[test]
    fn sharpe_is_none_until_window_fills_then_some() {
        let mut s = Sharpe::new(buy_and_hold(), SYM, SEED, 5, 0.0, BPY);
        assert_eq!(s.warm_up_period(), 5);
        let px = prices();
        for &p in &px[..4] {
            assert_eq!(s.update(snap(p)), None);
        }
        // 5th bar fills the return window.
        assert!(s.update(snap(px[4])).is_some());
    }

    #[test]
    fn full_window_sharpe_matches_whole_run_metric() {
        // A trailing Sharpe whose window spans the whole run must equal the
        // standalone metrics::sharpe over the same strategy's equity curve.
        let px = prices();
        let n = px.len();

        // Standalone backtest to get the reference equity curve + metric.
        let mut strat = buy_and_hold();
        let mut wallet = PaperWallet::new(SEED);
        let snaps: Vec<Snapshot<&'static str>> = px.iter().map(|&p| snap(p)).collect();
        let report = backtest::run(&mut strat, &mut wallet, snaps.iter().cloned());
        let returns = metrics::per_bar_returns(&report.equity_curve, report.initial_equity);
        let expected = metrics::sharpe(&returns, 0.0, BPY).expect("reference sharpe defined");

        // Rolling Sharpe with period = n, read at the last bar.
        let mut s = Sharpe::new(buy_and_hold(), SYM, SEED, n, 0.0, BPY);
        let mut last = None;
        for &p in &px {
            last = s.update(snap(p));
        }
        let got = last.expect("rolling sharpe defined at the final bar");
        assert!(
            (got - expected).abs() < 1e-9,
            "rolling sharpe {got} != whole-run {expected}"
        );
    }

    #[test]
    fn full_window_volatility_matches_whole_run_metric() {
        let px = prices();
        let n = px.len();

        let mut strat = buy_and_hold();
        let mut wallet = PaperWallet::new(SEED);
        let snaps: Vec<Snapshot<&'static str>> = px.iter().map(|&p| snap(p)).collect();
        let report = backtest::run(&mut strat, &mut wallet, snaps.iter().cloned());
        let returns = metrics::per_bar_returns(&report.equity_curve, report.initial_equity);
        let expected = metrics::annualized_volatility(&returns, BPY);

        let mut v = Volatility::new(buy_and_hold(), SYM, SEED, n, BPY);
        let mut last = None;
        for &p in &px {
            last = v.update(snap(p));
        }
        assert!((last.unwrap() - expected).abs() < 1e-9);
    }

    #[test]
    fn sortino_is_defined_and_positive_on_rising_path() {
        let px = prices();
        let mut s = Sortino::new(buy_and_hold(), SYM, SEED, 6, 0.0, BPY);
        let mut last = None;
        for &p in &px {
            last = s.update(snap(p));
        }
        // The path has down bars (downside exists) and a net gain → Sortino > 0.
        assert!(last.expect("sortino defined") > 0.0);
    }

    #[test]
    fn max_drawdown_tracks_the_trailing_dip() {
        // Rise to 120, then a clean 20% dip to 96, over a window that spans it.
        let px = [100.0, 110.0, 120.0, 108.0, 96.0];
        let mut m = MaxDrawdown::new(buy_and_hold(), SYM, SEED, px.len());
        let mut last = None;
        for &p in &px {
            last = m.update(snap(p));
        }
        // Equity mirrors price for a fully-invested buy-and-hold: peak at 120,
        // trough at 96 → 20% drawdown.
        let dd = last.expect("max drawdown defined");
        assert!((dd - 0.20).abs() < 1e-6, "expected ~0.20 drawdown, got {dd}");
    }

    #[test]
    fn calmar_is_none_without_a_drawdown() {
        // Strictly rising equity → zero trailing drawdown → Calmar undefined.
        let px = [100.0, 101.0, 102.0, 103.0, 104.0];
        let mut c = Calmar::new(buy_and_hold(), SYM, SEED, px.len(), BPY);
        let mut last = Some(0.0);
        for &p in &px {
            last = c.update(snap(p));
        }
        assert_eq!(last, None);
    }

    #[test]
    fn reset_restores_first_bar_behaviour() {
        let mut s = Sharpe::new(buy_and_hold(), SYM, SEED, 4, 0.0, BPY);
        for &p in &prices() {
            s.update(snap(p));
        }
        assert!(s.value().is_some());
        s.reset();
        assert_eq!(s.value(), None);
        // Warms up again exactly as a fresh instance.
        for &p in &prices()[..3] {
            assert_eq!(s.update(snap(p)), None);
        }
        assert!(s.update(snap(prices()[3])).is_some());
    }

    // --- multi-asset engine path --------------------------------------------

    use crate::wallet::{Side, Size, Wallet};

    /// A minimal two-symbol strategy: once ready it goes long `A` and short `B`,
    /// each half the equity, and holds. Reads its positions from the wallet
    /// (like `MaCross` in `backtest.rs`) so it needs no signals. Used to prove
    /// the trailing engine prices *both* legs from a 2-entry snapshot.
    struct LongShortPair {
        a: &'static str,
        b: &'static str,
    }

    impl Strategy for LongShortPair {
        type Input = Snapshot<&'static str>;
        type Symbol = &'static str;
        fn update(&mut self, _snap: Snapshot<&'static str>) {}
        fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
            if wallet.position(&self.a).amount.abs() < 1e-9 {
                let _ = wallet.set(self.a, Side::Buy, Size::value_frac(0.5));
            }
            if wallet.position(&self.b).amount.abs() < 1e-9 {
                let _ = wallet.set(self.b, Side::Sell, Size::value_frac(0.5));
            }
        }
        fn reset(&mut self) {}
    }

    /// A 2-entry snapshot tagging both legs — the shape the old `sole_atom`
    /// path would panic on.
    fn pair_snap(a_px: Real, b_px: Real) -> Snapshot<&'static str> {
        let mut s = Snapshot::new();
        s.push(Some("A"), None, Atom::new(bar(a_px)));
        s.push(Some("B"), None, Atom::new(bar(b_px)));
        s
    }

    #[test]
    fn drives_a_multi_asset_strategy_from_tagged_snapshots() {
        // A drifts up, B drifts down: a long-A / short-B pair earns on both
        // legs, so the engine must price *both* entries each bar. Under the old
        // sole-atom collapse this 2-entry snapshot would have panicked.
        let a = [100.0, 102.0, 101.0, 104.0, 106.0, 105.0, 108.0, 110.0];
        let b = [100.0, 99.0, 100.0, 97.0, 96.0, 97.0, 95.0, 93.0];

        let mut vol = Volatility::new(LongShortPair { a: "A", b: "B" }, "A", SEED, 5, BPY);
        let mut sharpe = Sharpe::new(LongShortPair { a: "A", b: "B" }, "A", SEED, 5, 0.0, BPY);

        let mut last_vol = None;
        let mut last_sharpe = None;
        for i in 0..a.len() {
            last_vol = vol.update(pair_snap(a[i], b[i]));
            last_sharpe = sharpe.update(pair_snap(a[i], b[i]));
        }

        // Both legs traded and marked to market → the equity curve has real
        // variance, so volatility is defined and strictly positive, and the
        // net-profitable pair yields a positive Sharpe.
        assert!(
            last_vol.expect("volatility defined once the window fills") > 0.0,
            "expected positive equity-curve volatility from the traded pair"
        );
        assert!(
            last_sharpe.expect("sharpe defined on the pair") > 0.0,
            "long-A/short-B on diverging legs should be net profitable"
        );
    }
}
