//! The cross-sectional **selection** algebra: turn a per-symbol score map into
//! a per-symbol side.
//!
//! Self-contained and independent of [`BasketStrategy`](super::BasketStrategy),
//! which is why it lives here rather than inside that shape's module. Each of
//! the three built-in rules is both a standalone `pub fn` (call it directly
//! with a raw score map) and a struct impl of [`Selection`] that composes: the
//! `of:` slot re-roots a rule inside another's candidate set.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use crate::prelude::*;

// ---------------------------------------------------------------------------
// Selection — the cross-sectional pick from a per-symbol score map to a
// per-symbol side, as a pluggable trait.
//
// Each of the three built-in rules is both a standalone `pub fn` (call
// it directly if you have a raw score map) and a struct impl of
// [`Selection`] (install via `BasketStrategy::selection`).
// ---------------------------------------------------------------------------

/// The two candidate sets a [`Selection`] produces for one bar: the
/// symbols eligible to go long and the symbols eligible to go short.
///
/// The two sets **may overlap** — the [`Everything`] leaf places every
/// symbol in both, and that overlap is the point: every rule downstream
/// just subsets each side from it. [`Selection::pick`] collapses any
/// leftover overlap to one side (**long wins**) at the boundary, so a
/// symbol never reaches the book on both sides.
#[derive(Debug, Clone)]
pub struct Sides<Sym> {
    /// Symbols eligible to go long this bar.
    pub long: HashSet<Sym>,
    /// Symbols eligible to go short this bar.
    pub short: HashSet<Sym>,
}

/// The cross-sectional rule that turns a per-symbol score map into
/// per-symbol trade sides. Consumed as a `Box<dyn Selection<Sym>>` by
/// [`BasketStrategy`](super::BasketStrategy) so a caller can plug in their own rule (a
/// per-sector rank, a risk-parity picker, a machine-learned classifier)
/// without touching the strategy.
///
/// # Composition
///
/// The one required method, [`select`](Self::select), returns two
/// candidate [`Sides`] rather than a committed side per symbol — which is
/// what lets rules **compose by narrowing**. Every built-in is generic
/// over an inner `S: Selection<Sym>` that defaults to the [`Everything`]
/// leaf (every scored symbol eligible for either side): `T::new(...)`
/// roots on that leaf, `T::of(inner, ...)` re-roots on a custom one — the
/// same `new` / `of` convention the indicator leaves use. So
///
/// ```ignore
/// TopBottom::of(Threshold::new(0.5, -0.5), 2, 2)
/// ```
///
/// reads as "the top-2 / bottom-2 *of* the threshold survivors": each
/// stage narrows the two sets it inherits, side by side, with no
/// cross-side bookkeeping. The [`Everything`] leaf deliberately places
/// every symbol in **both** sets — that overlap is the whole point, since
/// every rule downstream just subsets from it — and [`pick`](Self::pick)
/// collapses any remaining overlap to one side (long wins) only at the
/// boundary. (A signed `HashMap<Sym, Side>` can't express that leaf —
/// "eligible for *either* side" — which is why the candidate sets, not
/// sides, are the primitive.)
///
/// Three built-in impls ship: [`TopBottom`], [`Threshold`], [`Quantile`].
/// A blanket impl for any `Fn(&HashMap<Sym, Real>) -> HashMap<Sym, Side>`
/// closure means `.selection(|scores| { ... })` continues to work for
/// ad-hoc rules that don't warrant a new type.
pub trait Selection<Sym>: Send + Sync {
    /// The long and short candidate sets for the current bar's score map.
    /// The two may overlap (the [`Everything`] leaf returns every symbol
    /// in both); [`pick`](Self::pick) resolves overlap long-wins at the
    /// boundary.
    fn select(&self, scores: &HashMap<Sym, Real>) -> Sides<Sym>;

    /// The long candidate set alone — convenience over [`select`](Self::select).
    fn long_set(&self, scores: &HashMap<Sym, Real>) -> HashSet<Sym> {
        self.select(scores).long
    }

    /// The short candidate set alone — convenience over [`select`](Self::select).
    fn short_set(&self, scores: &HashMap<Sym, Real>) -> HashSet<Sym> {
        self.select(scores).short
    }

    /// Project the two candidate sets to one [`Side`] per symbol — what
    /// [`BasketStrategy`](super::BasketStrategy) consumes. **Long wins** for any symbol in both
    /// sets (normal — e.g. when narrowing from the [`Everything`] leaf),
    /// so the strategy is never handed a symbol on both sides. Symbols in
    /// neither set are absent (not selected — an open position on such a
    /// symbol is flattened).
    fn pick(&self, scores: &HashMap<Sym, Real>) -> HashMap<Sym, Side>
    where
        Sym: Hash + Eq,
    {
        let Sides { long, short } = self.select(scores);
        let mut out: HashMap<Sym, Side> = short
            .into_iter()
            .filter(|sym| !long.contains(sym))
            .map(|sym| (sym, Side::Sell))
            .collect();
        out.extend(long.into_iter().map(|sym| (sym, Side::Buy)));
        out
    }
}

/// The [`Selection`] leaf: every scored symbol is eligible for **either**
/// side. This is the implicit inner of a freshly-built rule
/// (`TopBottom::new` / `Threshold::new` / `Quantile::new`) and the root a
/// `T::of(...)` chain narrows from. Because both candidate sets start as
/// the full universe it assigns no side itself — the first rule that
/// consumes it does.
#[derive(Debug, Clone, Copy, Default)]
pub struct Everything;

impl<Sym: Clone + Hash + Eq> Selection<Sym> for Everything {
    fn select(&self, scores: &HashMap<Sym, Real>) -> Sides<Sym> {
        let all: HashSet<Sym> = scores.keys().cloned().collect();
        Sides {
            long: all.clone(),
            short: all,
        }
    }
}

/// A type-erased [`Selection`] usable as the inner of another rule —
/// `TopBottom::of(DynSelection(inner), ..)`. Wrap a `Box<dyn Selection>`
/// when the chain is composed dynamically (e.g. from a spec of unknown
/// depth) rather than by nesting concrete `::of` constructors.
///
/// (A blanket `impl Selection for Box<dyn Selection>` would collide with
/// the closure blanket impl — `Box<dyn Fn ..>` is itself an `Fn` — so
/// type erasure goes through this newtype instead.)
pub struct DynSelection<Sym>(pub Box<dyn Selection<Sym>>);

impl<Sym> Selection<Sym> for DynSelection<Sym> {
    fn select(&self, scores: &HashMap<Sym, Real>) -> Sides<Sym> {
        self.0.select(scores)
    }
}

/// Blanket [`Selection`] impl for any closure returning a committed
/// side-per-symbol map — preserves `.selection(|scores| { ... })`
/// ergonomics. The map's `Buy` / `Sell` entries become the long / short
/// candidate sets (a closure is inherently single-sided, so they never
/// overlap).
impl<Sym, F> Selection<Sym> for F
where
    Sym: Hash + Eq,
    F: Fn(&HashMap<Sym, Real>) -> HashMap<Sym, Side> + Send + Sync,
{
    fn select(&self, scores: &HashMap<Sym, Real>) -> Sides<Sym> {
        let mut long = HashSet::new();
        let mut short = HashSet::new();
        for (sym, side) in (self)(scores) {
            match side {
                Side::Buy => {
                    long.insert(sym);
                }
                Side::Sell => {
                    short.insert(sym);
                }
            }
        }
        Sides { long, short }
    }
}

/// Keep the `count` symbols at one end of `pool` by score: the highest
/// (`from_top`) or the lowest. Symbols missing from `scores` are dropped.
/// Shared by [`TopBottom`] and [`Quantile`], each of which ranks the long
/// and short sides separately.
///
/// Ranks the *wanted* end to the front for either side, so both read off
/// the same `take(count)`.
///
/// # Ties break on the symbol, ascending
///
/// Equal scores are not exotic — a score saturates (`stoch_rsi` at 0/100),
/// peaks at a bound (`close / rolling_max` is exactly 1.0 at a new high),
/// or is a constant sentinel in an [`IfElse`](crate::indicators::IfElse)
/// branch — and a basket whose whole universe ties is an ordinary bar, not
/// a pathological one.
///
/// The tie-break therefore has to be a *total order on symbols*: sorting
/// on score alone leaves tied symbols in `pool`'s `HashSet` iteration
/// order, which `RandomState` reseeds every process, so the same spec over
/// the same bars picks a different basket — and returns a different equity
/// curve — on every run. This is the one nondeterminism a backtester
/// cannot absorb, since nothing downstream can tell a real edge from a
/// lucky seed.
///
/// Ascending symbol is arbitrary but *stable and explicable*, which is the
/// whole requirement. It costs an `Ord` bound that [`Threshold`] (a cutoff
/// rule, so nothing to break) and [`Everything`] do not need. Sorting the
/// values instead — the trick
/// [`PaperWallet::marked_equity`](crate::wallet::PaperWallet) uses to
/// canonicalize its sum without an `Ord` bound — cannot work here: the
/// tied values are equal by definition, and it is the *symbols* that must
/// be ordered.
///
/// NaN is unrankable, so it sorts last at whichever end is wanted, and is
/// only ever selected when the pool is too small to avoid it.
fn ranked_take<Sym: Clone + Hash + Eq + Ord>(
    pool: &HashSet<Sym>,
    scores: &HashMap<Sym, Real>,
    count: usize,
    from_top: bool,
) -> HashSet<Sym> {
    let mut ranked: Vec<(&Sym, Real)> = pool
        .iter()
        .filter_map(|sym| scores.get(sym).map(|&v| (sym, v)))
        .collect();
    let order = |a: &(&Sym, Real), b: &(&Sym, Real)| {
        let (x, y) = (a.1, b.1);
        let by_score = match (x.is_nan(), y.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            // Wanted end first: descending for the top, ascending for the
            // bottom.
            (false, false) => {
                let (lhs, rhs) = if from_top { (y, x) } else { (x, y) };
                lhs.partial_cmp(&rhs).unwrap_or(Ordering::Equal)
            }
        };
        by_score.then_with(|| a.0.cmp(b.0))
    };
    let k = count.min(ranked.len());
    // Only the first `k` of the pool are wanted, so partition rather than sort:
    // O(n) instead of O(n log n), and no scratch allocation (the stable sort
    // takes one). A basket ranks its whole universe on every rebalance bar, so
    // at 64 symbols taking 4 this was most of the ordering work.
    //
    // **This is result-identical, not merely equivalent-up-to-ties**, and the
    // comment above is why: `order` breaks score ties on the symbol, so it is a
    // *total* order over a pool that cannot contain a symbol twice. No two
    // elements compare `Equal`, so "the `k` smallest" is a uniquely determined
    // set — which is what an unstable partition is allowed to disturb, and here
    // there is nothing to disturb. The result then collects into a `HashSet`,
    // so the order *within* the prefix is not observable either.
    if k < ranked.len() {
        ranked.select_nth_unstable_by(k, order);
    }
    ranked[..k]
        .iter()
        .map(|(sym, _)| (*sym).clone())
        .collect()
}

/// The ranked "long the highest `longs`, short the lowest `shorts`" rule,
/// narrowing an inner selection (default [`Everything`], i.e. the full
/// universe). As an inner it ranks *within* whatever candidate sets it is
/// handed: the top `longs` of the inner's long set, the bottom `shorts`
/// of its short set.
#[derive(Debug, Clone, Copy)]
pub struct TopBottom<S = Everything> {
    /// Inner selection whose per-side candidate sets are ranked.
    pub inner: S,
    pub longs: usize,
    pub shorts: usize,
}

impl TopBottom<Everything> {
    /// Rank the full universe: top `longs` → long, bottom `shorts` →
    /// short. Roots on the [`Everything`] leaf.
    pub fn new(longs: usize, shorts: usize) -> Self {
        Self {
            inner: Everything,
            longs,
            shorts,
        }
    }
}

impl<S> TopBottom<S> {
    /// Rank *within* `inner`'s candidate sets rather than the full
    /// universe — the top `longs` of `inner`'s long set, the bottom
    /// `shorts` of its short set.
    pub fn of(inner: S, longs: usize, shorts: usize) -> Self {
        Self {
            inner,
            longs,
            shorts,
        }
    }
}

impl<Sym, S> Selection<Sym> for TopBottom<S>
where
    Sym: Clone + Hash + Eq + Ord,
    S: Selection<Sym>,
{
    fn select(&self, scores: &HashMap<Sym, Real>) -> Sides<Sym> {
        let base = self.inner.select(scores);
        Sides {
            long: ranked_take(&base.long, scores, self.longs, true),
            short: ranked_take(&base.short, scores, self.shorts, false),
        }
    }
}

/// The "long above `long_min`, short below `short_max`" cutoff rule,
/// narrowing an inner selection (default [`Everything`]). Each side keeps
/// the inner's candidates that clear its cutoff. A symbol clearing both
/// cutoffs (mis-ordered `long_min <= short_max`) lands in both sets and is
/// resolved long-wins by [`Selection::pick`].
#[derive(Debug, Clone, Copy)]
pub struct Threshold<S = Everything> {
    /// Inner selection whose per-side candidate sets are filtered.
    pub inner: S,
    pub long_min: Real,
    pub short_max: Real,
}

impl Threshold<Everything> {
    /// Long every symbol scoring at/above `long_min`, short every symbol
    /// at/below `short_max`. Roots on the [`Everything`] leaf.
    pub fn new(long_min: Real, short_max: Real) -> Self {
        Self {
            inner: Everything,
            long_min,
            short_max,
        }
    }
}

impl<S> Threshold<S> {
    /// Apply the cutoffs *within* `inner`'s candidate sets rather than the
    /// full universe.
    pub fn of(inner: S, long_min: Real, short_max: Real) -> Self {
        Self {
            inner,
            long_min,
            short_max,
        }
    }
}

impl<Sym, S> Selection<Sym> for Threshold<S>
where
    Sym: Clone + Hash + Eq,
    S: Selection<Sym>,
{
    fn select(&self, scores: &HashMap<Sym, Real>) -> Sides<Sym> {
        let base = self.inner.select(scores);
        let long = base
            .long
            .into_iter()
            .filter(|sym| scores.get(sym).is_some_and(|&v| v >= self.long_min))
            .collect();
        let short = base
            .short
            .into_iter()
            .filter(|sym| scores.get(sym).is_some_and(|&v| v <= self.short_max))
            .collect();
        Sides { long, short }
    }
}

/// The "long the top `long_q`, short the bottom `short_q`" fractional
/// rule, narrowing an inner selection (default [`Everything`]). Counts are
/// `ceil(q * n)` where `n` is the size of the corresponding inner side's
/// candidate set, so as an inner-of it ranks fractions *of the survivors*.
#[derive(Debug, Clone, Copy)]
pub struct Quantile<S = Everything> {
    /// Inner selection whose per-side candidate sets are ranked.
    pub inner: S,
    pub long_q: Real,
    pub short_q: Real,
}

impl Quantile<Everything> {
    /// Long the top `long_q` fraction of the full universe, short the
    /// bottom `short_q`. Roots on the [`Everything`] leaf.
    pub fn new(long_q: Real, short_q: Real) -> Self {
        Self {
            inner: Everything,
            long_q,
            short_q,
        }
    }
}

impl<S> Quantile<S> {
    /// Take the fractions *within* `inner`'s candidate sets rather than
    /// the full universe.
    pub fn of(inner: S, long_q: Real, short_q: Real) -> Self {
        Self {
            inner,
            long_q,
            short_q,
        }
    }
}

impl<Sym, S> Selection<Sym> for Quantile<S>
where
    Sym: Clone + Hash + Eq + Ord,
    S: Selection<Sym>,
{
    fn select(&self, scores: &HashMap<Sym, Real>) -> Sides<Sym> {
        let base = self.inner.select(scores);
        let long_count = quantile_count(self.long_q, base.long.len());
        let short_count = quantile_count(self.short_q, base.short.len());
        Sides {
            long: ranked_take(&base.long, scores, long_count, true),
            short: ranked_take(&base.short, scores, short_count, false),
        }
    }
}

/// Rank `scores` and return the `longs` highest-scoring symbols as
/// [`Side::Buy`] and the `shorts` lowest-scoring as [`Side::Sell`].
///
/// The two sides never overlap: when the pool is smaller than
/// `longs + shorts`, longs are taken first (highest scores) and shorts
/// drawn from what remains. **Equal scores break on the symbol,
/// ascending** — see `ranked_take` for why the tie-break has to be a
/// total order on symbols rather than a property of the scores.
///
/// Symbols not in the returned map are not selected. See [`TopBottom`]
/// for the [`Selection`] trait wrapper.
pub fn top_bottom<Sym: Clone + Hash + Eq + Ord>(
    scores: &HashMap<Sym, Real>,
    longs: usize,
    shorts: usize,
) -> HashMap<Sym, Side> {
    TopBottom::new(longs, shorts).pick(scores)
}

/// Long every symbol whose score reads at or above `long_min`; short every
/// symbol whose score reads at or below `short_max`. Symbols in the gap
/// (or missing from `scores`) are not selected.
///
/// When both cutoffs apply to the same score (mis-ordered thresholds with
/// `long_min <= short_max`), **long wins** — the strategy will not put a
/// symbol on both sides at once.
///
/// See [`Threshold`] for the [`Selection`] trait wrapper.
pub fn threshold<Sym: Clone + Hash + Eq>(
    scores: &HashMap<Sym, Real>,
    long_min: Real,
    short_max: Real,
) -> HashMap<Sym, Side> {
    Threshold::new(long_min, short_max).pick(scores)
}

/// Long the top `long_q` fraction of the score distribution, short the
/// bottom `short_q`. Counts are `ceil(q * n)` clamped to `[0, n]`.
///
/// The two sides never overlap: longs are drawn first, then shorts from
/// what remains, so `long_q + short_q > 1.0` truncates the shorts.
/// Zero-quantile sides are legal (a top-decile long-only basket is
/// `quantile(scores, 0.1, 0.0)`).
///
/// See [`Quantile`] for the [`Selection`] trait wrapper. Delegates the
/// actual rank to [`top_bottom`] once the two counts are resolved.
pub fn quantile<Sym: Clone + Hash + Eq + Ord>(
    scores: &HashMap<Sym, Real>,
    long_q: Real,
    short_q: Real,
) -> HashMap<Sym, Side> {
    Quantile::new(long_q, short_q).pick(scores)
}

/// `ceil(q * n)` clamped to `[0, n]`. Converts a fractional cutoff into a
/// count for [`quantile`].
fn quantile_count(q: Real, n: usize) -> usize {
    if q <= 0.0 {
        0
    } else {
        (q * n as Real).ceil() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Snapshot;
    // A few of these drive a whole `BasketStrategy` — they check a selection
    // rule's effect once installed, not the rule in isolation.
    use crate::indicators::sizing::equal_weight;
    use crate::indicators::{Close, Pick};
    use crate::strategies::BasketStrategy;

    fn snap(entries: &[(&'static str, Real)]) -> Snapshot<&'static str> {
        let mut s = Snapshot::new();
        for &(sym, close) in entries {
            let atom = Atom::new(Candle::new(close, close, close, close, 0.0));
            s.push(Some(sym), None, atom);
        }
        s
    }

    // ---------------- Selection functions --------------------------------

    // ---------------- Selection functions --------------------------------

    #[test]
    fn top_bottom_ranks_and_splits_by_score() {
        let mut scores = HashMap::new();
        scores.insert("A", 5.0);
        scores.insert("B", 4.0);
        scores.insert("C", 3.0);
        scores.insert("D", 2.0);
        scores.insert("E", 1.0);
        let picked = top_bottom(&scores, 2, 2);
        // A, B → long; D, E → short; C rests.
        assert_eq!(picked.get("A"), Some(&Side::Buy));
        assert_eq!(picked.get("B"), Some(&Side::Buy));
        assert_eq!(picked.get("D"), Some(&Side::Sell));
        assert_eq!(picked.get("E"), Some(&Side::Sell));
        assert_eq!(picked.get("C"), None);
    }

    #[test]
    fn top_bottom_never_overlaps_when_pool_is_small() {
        let mut scores = HashMap::new();
        scores.insert("A", 3.0);
        scores.insert("B", 2.0);
        scores.insert("C", 1.0);
        // 5 longs and 5 shorts on 3 candidates: longs take all 3, shorts
        // get nothing (no overlap).
        let picked = top_bottom(&scores, 5, 5);
        assert_eq!(picked.len(), 3);
        assert!(picked.values().all(|s| *s == Side::Buy));
    }

    #[test]
    fn threshold_selects_by_cutoffs() {
        let mut scores = HashMap::new();
        scores.insert("A", 1.2);
        scores.insert("B", 0.5);
        scores.insert("C", 0.0);
        scores.insert("D", -0.5);
        scores.insert("E", -1.5);
        let picked = threshold(&scores, 1.0, -1.0);
        assert_eq!(picked.get("A"), Some(&Side::Buy));
        assert_eq!(picked.get("E"), Some(&Side::Sell));
        assert_eq!(picked.get("B"), None);
        assert_eq!(picked.get("C"), None);
        assert_eq!(picked.get("D"), None);
    }

    #[test]
    fn quantile_uses_ceil_and_avoids_overlap() {
        // 10 candidates, top-decile long, bottom-decile short → 1 each.
        let mut scores = HashMap::new();
        for i in 0..10 {
            scores.insert(format!("S{i:02}"), i as Real);
        }
        let picked = quantile(&scores, 0.1, 0.1);
        assert_eq!(picked.get("S09"), Some(&Side::Buy));
        assert_eq!(picked.get("S00"), Some(&Side::Sell));
        assert_eq!(picked.len(), 2);
    }

    #[test]
    fn quantile_truncates_shorts_on_overflow() {
        // long_q + short_q > 1: shorts get whatever is left after longs.
        let mut scores = HashMap::new();
        scores.insert("A", 4.0);
        scores.insert("B", 3.0);
        scores.insert("C", 2.0);
        scores.insert("D", 1.0);
        // long_count = ceil(0.8 * 4) = 4 → all long, no shorts.
        let picked = quantile(&scores, 0.8, 0.5);
        assert_eq!(picked.len(), 4);
        assert!(picked.values().all(|s| *s == Side::Buy));
    }

    #[test]
    fn everything_leaf_longs_and_shorts_every_symbol() {
        // The leaf places every scored symbol in *both* candidate sets —
        // that full overlap is what every other rule subsets from.
        let mut scores = HashMap::new();
        scores.insert("A", 1.0);
        scores.insert("B", 2.0);
        scores.insert("C", 3.0);
        let sides = Everything.select(&scores);
        assert_eq!(sides.long.len(), 3);
        assert_eq!(sides.short.len(), 3);
        assert!(sides.long.contains(&"A") && sides.short.contains(&"A"));
        // Projected to sides, long wins on the total overlap → all long.
        let picked = Everything.pick(&scores);
        assert_eq!(picked.len(), 3);
        assert!(picked.values().all(|s| *s == Side::Buy));
    }

    #[test]
    fn top_bottom_of_threshold_ranks_within_survivors() {
        // Threshold admits A,B,C on the long side (>= 0.5) and E on the
        // short side (<= -0.5); D sits in the gap. TopBottom(1,1) then
        // keeps only the best long (A) and the worst short (E).
        let mut scores = HashMap::new();
        scores.insert("A", 0.9);
        scores.insert("B", 0.7);
        scores.insert("C", 0.6);
        scores.insert("D", 0.0);
        scores.insert("E", -0.8);
        let sel = TopBottom::of(Threshold::new(0.5, -0.5), 1, 1);
        let picked = sel.pick(&scores);
        assert_eq!(picked.get("A"), Some(&Side::Buy));
        assert_eq!(picked.get("E"), Some(&Side::Sell));
        // Survived threshold but lost the rank:
        assert_eq!(picked.get("B"), None);
        assert_eq!(picked.get("C"), None);
        // Never cleared threshold at all:
        assert_eq!(picked.get("D"), None);
        assert_eq!(picked.len(), 2);
    }

    #[test]
    fn threshold_of_top_bottom_gates_the_ranked_picks() {
        // TopBottom(2,2) proposes A,B long and D,E short; the outer
        // Threshold then drops any long scoring below 0.85 (→ only A) and
        // any short above -0.85 (→ only E). Order of composition matters,
        // and each side is gated independently.
        let mut scores = HashMap::new();
        scores.insert("A", 0.9);
        scores.insert("B", 0.8);
        scores.insert("C", 0.0);
        scores.insert("D", -0.8);
        scores.insert("E", -0.9);
        let sel = Threshold::of(TopBottom::new(2, 2), 0.85, -0.85);
        let picked = sel.pick(&scores);
        assert_eq!(picked.get("A"), Some(&Side::Buy));
        assert_eq!(picked.get("E"), Some(&Side::Sell));
        assert_eq!(picked.get("B"), None); // ranked long, gated out
        assert_eq!(picked.get("D"), None); // ranked short, gated out
        assert_eq!(picked.len(), 2);
    }

    #[test]
    fn pick_is_single_sided_even_when_sets_overlap() {
        // A wide TopBottom over a threshold that admits everything on both
        // sides leaves the two candidate sets fully overlapping; pick must
        // still hand back one side per symbol (long wins).
        let mut scores = HashMap::new();
        for i in 0..5 {
            scores.insert(format!("S{i}"), i as Real);
        }
        let sel = TopBottom::of(Threshold::new(-100.0, 100.0), 5, 5);
        let sides = sel.select(&scores);
        assert_eq!(sides.long.len(), 5);
        assert_eq!(sides.short.len(), 5); // the overlap is real...
        let picked = sel.pick(&scores);
        assert_eq!(picked.len(), 5); // ...but the projection is single-sided
        assert!(picked.values().all(|s| *s == Side::Buy));
    }

    #[test]
    fn threshold_and_quantile_compose_either_order() {
        // Composition is fully general — any rule nests in any other, and
        // order matters. Scores: A..F = 10,8,6,4,2,0.
        let mut scores = HashMap::new();
        for (s, v) in [
            ("A", 10.0),
            ("B", 8.0),
            ("C", 6.0),
            ("D", 4.0),
            ("E", 2.0),
            ("F", 0.0),
        ] {
            scores.insert(s, v);
        }

        // threshold OF quantile: take the top/bottom half (A,B,C long /
        // D,E,F short), THEN keep only members clearing the cutoffs.
        let t_of_q = Threshold::of(Quantile::new(0.5, 0.5), 7.0, 3.0);
        let p = t_of_q.pick(&scores);
        assert_eq!(p.get("A"), Some(&Side::Buy));
        assert_eq!(p.get("B"), Some(&Side::Buy));
        assert_eq!(p.get("E"), Some(&Side::Sell));
        assert_eq!(p.get("F"), Some(&Side::Sell));
        assert_eq!(p.get("C"), None); // top-half but below long_min
        assert_eq!(p.get("D"), None); // bottom-half but above short_max
        assert_eq!(p.len(), 4);

        // quantile OF threshold: gate to {A,B} long / {E,F} short first,
        // THEN take the top/bottom half of each survivor pool — a narrower
        // result. Different picks ⇒ order is meaningful.
        let q_of_t = Quantile::of(Threshold::new(7.0, 3.0), 0.5, 0.5);
        let p2 = q_of_t.pick(&scores);
        assert_eq!(p2.get("A"), Some(&Side::Buy)); // top of {A,B}
        assert_eq!(p2.get("F"), Some(&Side::Sell)); // bottom of {E,F}
        assert_eq!(p2.len(), 2);
    }

    #[test]
    fn custom_selection_closure_is_installed_verbatim() {
        // A whimsical rule: long any symbol whose name starts with 'A'.
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .selection(|scores: &HashMap<&'static str, Real>| {
                    let mut out = HashMap::new();
                    for sym in scores.keys() {
                        if sym.starts_with('A') {
                            out.insert(*sym, Side::Buy);
                        }
                    }
                    out
                });
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
        let tick = |strat: &mut BasketStrategy<&'static str>,
                    wallet: &mut PaperWallet<&'static str>,
                    entries: &[(&'static str, Real)]| {
            let s = snap(entries);
            for (sym_opt, _f, atom) in s.iter() {
                let sym = sym_opt.copied().unwrap();
                let Some(candle) = atom.candle else { continue };
                for fill in wallet.update(sym, candle) {
                    strat.on_fill(&fill);
                }
            }
            strat.update(s);
            strat.trade(wallet);
        };
        tick(&mut strat, &mut wallet, &[("AAPL", 100.0), ("BTC", 50.0)]);
        tick(&mut strat, &mut wallet, &[("AAPL", 100.0), ("BTC", 50.0)]);
        assert!(wallet.position(&"AAPL").amount > 0.0, "AAPL long via custom rule");
        assert!(
            wallet.position(&"BTC").amount.abs() < 1e-9,
            "BTC not picked, so flat"
        );
    }

    #[test]
    fn custom_selection_impl_plugs_in_via_the_trait() {
        // Same behavior as the closure test above but with a dedicated
        // struct impl of Selection — proves the trait, not just the
        // closure blanket, is the extension seam. A caller wanting to
        // ship a reusable rule (per-sector top-N, risk-parity, etc.)
        // implements the trait once and installs it via `.selection(...)`.
        struct StartsWithSelection(char);
        impl Selection<&'static str> for StartsWithSelection {
            fn select(
                &self,
                scores: &HashMap<&'static str, Real>,
            ) -> Sides<&'static str> {
                Sides {
                    long: scores
                        .keys()
                        .filter(|s| s.starts_with(self.0))
                        .copied()
                        .collect(),
                    short: HashSet::new(),
                }
            }
        }

        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .selection(StartsWithSelection('A'));
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
        let tick = |strat: &mut BasketStrategy<&'static str>,
                    wallet: &mut PaperWallet<&'static str>,
                    entries: &[(&'static str, Real)]| {
            let s = snap(entries);
            for (sym_opt, _f, atom) in s.iter() {
                let sym = sym_opt.copied().unwrap();
                let Some(candle) = atom.candle else { continue };
                for fill in wallet.update(sym, candle) {
                    strat.on_fill(&fill);
                }
            }
            strat.update(s);
            strat.trade(wallet);
        };
        tick(&mut strat, &mut wallet, &[("AAPL", 100.0), ("BTC", 50.0)]);
        tick(&mut strat, &mut wallet, &[("AAPL", 100.0), ("BTC", 50.0)]);
        assert!(wallet.position(&"AAPL").amount > 0.0, "AAPL long via custom impl");
        assert!(
            wallet.position(&"BTC").amount.abs() < 1e-9,
            "BTC not picked, so flat"
        );
    }

    // -----------------------------------------------------------------
    // Determinism. A tied score is an ordinary bar — a saturating
    // oscillator, a ratio pinned at its bound, a constant `!if_else`
    // branch — so the rank's tie-break is what makes a basket backtest
    // reproducible at all. These run the rank many times *within* one
    // process, which is the weaker half of the guarantee: `RandomState`
    // reseeds per process, so `tests/determinism.rs` re-runs the binary
    // to cover the half a unit test structurally cannot.
    // -----------------------------------------------------------------

    /// The reported repro: a whole universe scoring 0.0, top-3 of 6.
    /// Pre-fix this returned up to five distinct baskets in eight runs.
    #[test]
    fn top_bottom_breaks_a_total_tie_on_the_symbol() {
        let syms = [
            "BTCUSDT", "ETHUSDT", "SOLUSDT", "ADAUSDT", "BNBUSDT", "LINKUSDT",
        ];
        let scores: HashMap<&str, Real> = syms.iter().map(|s| (*s, 0.0)).collect();
        for _ in 0..64 {
            let picked = top_bottom(&scores, 3, 0);
            let mut got: Vec<&str> = picked.keys().copied().collect();
            got.sort();
            assert_eq!(got, ["ADAUSDT", "BNBUSDT", "BTCUSDT"]);
        }
    }

    /// Both sides tie-break ascending, so the rule stays symmetric: the
    /// short side is not the long side's tie order reversed.
    #[test]
    fn both_sides_break_ties_ascending() {
        let scores: HashMap<&str, Real> =
            [("A", 1.0), ("B", 1.0), ("C", 1.0), ("X", -1.0), ("Y", -1.0), ("Z", -1.0)]
                .into_iter()
                .collect();
        for _ in 0..64 {
            let picked = top_bottom(&scores, 2, 2);
            assert_eq!(picked.get("A"), Some(&Side::Buy));
            assert_eq!(picked.get("B"), Some(&Side::Buy));
            assert_eq!(picked.get("C"), None);
            assert_eq!(picked.get("X"), Some(&Side::Sell));
            assert_eq!(picked.get("Y"), Some(&Side::Sell));
            assert_eq!(picked.get("Z"), None);
        }
    }

    /// `quantile` resolves counts then delegates to the same rank, so it
    /// inherits the tie-break rather than needing its own.
    #[test]
    fn quantile_breaks_ties_on_the_symbol_too() {
        let scores: HashMap<&str, Real> =
            ["D", "C", "B", "A"].into_iter().map(|s| (s, 5.0)).collect();
        for _ in 0..64 {
            let picked = quantile(&scores, 0.5, 0.0);
            let mut got: Vec<&str> = picked.keys().copied().collect();
            got.sort();
            assert_eq!(got, ["A", "B"]);
        }
    }

    /// NaN is unrankable, so it sorts last at *either* end rather than
    /// displacing a symbol that actually scored. The old comparator
    /// mapped it to `Equal`, which left it wherever the hash landed it.
    #[test]
    fn nan_scores_are_ranked_last_at_both_ends() {
        let scores: HashMap<&str, Real> = [
            ("A", Real::NAN),
            ("B", 3.0),
            ("C", 1.0),
            ("D", Real::NAN),
        ]
        .into_iter()
        .collect();
        for _ in 0..64 {
            let picked = top_bottom(&scores, 1, 1);
            assert_eq!(picked.get("B"), Some(&Side::Buy), "highest real score");
            assert_eq!(picked.get("C"), Some(&Side::Sell), "lowest real score");
            assert_eq!(picked.get("A"), None);
            assert_eq!(picked.get("D"), None);
        }
    }

    /// A pool with nothing but NaN still has to fill the requested count —
    /// "last" is a rank, not an exclusion — and still deterministically.
    #[test]
    fn all_nan_still_ranks_deterministically() {
        let scores: HashMap<&str, Real> = ["C", "B", "A"]
            .into_iter()
            .map(|s| (s, Real::NAN))
            .collect();
        for _ in 0..64 {
            let picked = top_bottom(&scores, 2, 0);
            let mut got: Vec<&str> = picked.keys().copied().collect();
            got.sort();
            assert_eq!(got, ["A", "B"]);
        }
    }

}
