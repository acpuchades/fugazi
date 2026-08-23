# Index columns — generalising the bar clock

**Status:** in progress on `feat/index-columns`.
**Scope:** replace the assumption "a bar stream is indexed by wall-clock time"
with "a bar stream is indexed by an *ordered index*, of which time is one kind".

## Why

Every sampling scheme that is not a time bar — volume bars, dollar bars, tick
bars, imbalance/run bars — produces bars whose boundaries are endogenous to the
data rather than to a clock. The library's *computational* core already supports
these unchanged (see "What already works"), but the **data layer** insists on a
`time` column and the **calendar layer** converts bars to durations with a single
per-run `Frequency`.

The gain is not "we can plot dollar bars". It is that event-sampled returns are
closer to IID and less heteroskedastic, which is the assumption the machinery
this crate has *already invested in* leans on: PSR, DSR (`src/metrics.rs`), and
the moving-block / stationary bootstrap in `src/montecarlo.rs`. Feeding those a
better-behaved return series is worth more than any single new indicator.

## What already works, unchanged

This is the load-bearing observation. The indicator layer is already clock-free,
by design and not by accident:

- `Indicator::update(&mut self, input)` takes no timestamp (`src/indicator.rs:27`).
  Warm-up is counted in **samples** (`warm_up_bars`), never in duration.
- `Candle` has no time field (`src/market.rs:27`). `Atom.time` is
  `Option<Timestamp>` and every calendar leaf already degrades to `None` when it
  is absent (`src/indicators/calendar.rs:20`).
- `Resample` is *already* an alternative sampler — "Bar-count based (no timestamp
  dependency — `Candle` has none)" (`src/indicators/timeframe.rs:41`). A volume
  bar is the same shape with a different bucket-completion predicate.
- `WindowStats`, `Ring`, `WindowExtreme`, `EmaState` all count samples.

So indicators, signals, strategies, `Book`, `PaperWallet` fill mechanics,
`backtest::run` and run-resuming need **no change**. A user can build dollar bars
offline today and run them through `fugazi run --series` — and everything above
is already correct.

## The abstraction already leaked in the right direction

`DataFrame` keys rows on `(String, String, String)` = `(symbol, freq, time)`
(`src/cli/data.rs:225`), where the third component is an **opaque label**. The
load path says so explicitly (`src/cli/data.rs:493`):

> An unparseable label leaves `time` as `None`; the strings still sort the frame
> the way the user typed them.

`Atom.time` is a *derived, optional* interpretation of that label, produced by
`calendar::parse_time_to_millis`. That is already an index column. This work
mostly names it, fixes its ordering, and makes the derived-time degradation
explicit rather than incidental.

## Design decisions

### D1 — The index is the primitive; time is a *kind* of index

The join key is an ordered index. When (and only when) its labels parse as
timestamps do the time-denominated features light up: calendar leaves, carry
pro-rating, annualization, `-w`'s duration form.

### D2 — Do **not** add an event variant to `Frequency`

`Frequency::Ord` is defined on `calendar_seconds_per_bar` (`src/time.rs:119`).
An event-sampled stream has no place in that total order, and forcing one in
would corrupt cadence comparison everywhere. `Frequency` stays what it is: a
duration. The index *kind* lives beside it.

### D3 — Ordering must be declared, not inferred lexicographically

This is the one place "opaque string" is genuinely not enough. Today ordering is
lexicographic in two places that must agree:

- `BTreeMap<(String, String, String), Row>` (`src/cli/data.rs:225`) — the
  contract that makes `DataFrame::atoms` yield sorted output.
- `join_pair_by_time`'s `left[i].0.cmp(&right[j].0)` (`src/cli/run.rs:1225`) and
  `join_universe_by_time`'s `.min()` over `&str` (`src/cli/run.rs:897`).

ISO-8601 sorts lexicographically, which is why this has been invisible. An
integer sequence index does not: `"10" < "9"`. Left alone, a numeric index
column produces a **silently scrambled bar order** — no error, just wrong
results. That failure mode is the reason this cannot ship as a pure rename.

**Chosen representation:**

```rust
/// The ordered join key of a bar stream.
enum IndexKey {
    /// A numeric sequence index — ordered numerically.
    Ordinal(i64),
    /// A label (timestamp, or anything else) — ordered lexicographically,
    /// exactly as today.
    Label(String),
}
```

`Label` preserves today's behaviour bit-for-bit for every existing input, which
keeps the change reviewable and the fixtures stable. A frame mixing the two
variants is a data error and is **refused**, consistent with the cadence
census's "ambiguity is refused, disagreement is warned" rule
(`src/cli/cadence.rs:32`).

### D4 — `Selector.freq` becomes a stream discriminant

`Selector { symbol, freq: Option<Frequency> }` (`src/snapshot.rs:312`) is a
closed enum of durations, but `DataFrame` already keys the freq component as a
**verbatim uninterpreted string**. Under an index world that field means "which
stream of this symbol", of which a duration is one parseable form.

Low risk: TODO.md records that the freq tag is never pushed under `run` at all
(the drivers push `None`), so nothing downstream depends on it being a duration
today. Deferred to Stage 3 regardless — it is not needed for single-symbol
event bars, which is where the value is.

### D5 — Refuse, don't guess

Per the "safe defaults, opt-in overrides" invariant. Where a time-denominated
number cannot be derived, refuse and name the flag that supplies it, rather than
falling back to a plausible default. The one active offender is
`calendar::resolve`, which currently *guesses* `bars_per_year`.

## What breaks, and the fix for each

Every one of these already carries an `Option` at exactly the boundary where
time stops being knowable — good evidence the factoring is right rather than
retrofitted.

| # | Consumer | State today | Fix |
|---|---|---|---|
| B1 | Calendar leaves (`!day_of_week`, `!is_weekday`) | Already degrade to `None` per bar | none |
| B2 | `-w` / walk-forward duration form (`src/spec/calendar.rs:262`) | Already errors when `bar_freq` is `None` | reword the message to name the index kind |
| B3 | Carry `year_fraction` | `CarryContext::year_fraction` is already `Option<Real>` **per call** (`src/costs/mod.rs:190`); only `PaperWallet`'s storage is a run constant (`src/wallet/paper.rs:432`) | derive per bar from consecutive `atom.time` deltas |
| B4 | `bars_per_year` | `calendar::resolve` falls back rather than refusing | require `--bars-per-year` when there is no time index |
| B5 | Cadence census (`src/cli/cadence.rs`) | Medians timestamp gaps, snaps to a named `Frequency`; on dispersed gaps it either errors or confidently reports nonsense | third `Resolution`: index-sampled, no cadence |
| B6 | Freq-scoped costs `SYMBOL[FREQ]:` | Scoped on `Frequency` | follows D4, Stage 3 |
| B7 | `volume_participation` slippage (`src/costs/mod.rs:760`) | `units / candle.volume` | **degenerates** under volume bars (constant denominator). Document it; do not silently rescale |
| B8 | `TrailingSharpe` (`src/indicators/trailing.rs:307`) | multiplies by `bars_per_year` *inside* the run, per bar | genuinely wrong under event bars; gate on a time index |

## Multi-symbol — narrower than it first looks

`join_universe_by_time` groups on **exact key equality**. Two symbols' dollar
bars never share a boundary, so:

- `single:` — works.
- `pairs:` — inner join yields ~zero bars, fails loudly. Good failure, leave it.
- `basket:` / `multi:` / `portfolio:` — every snapshot holds one symbol, and
  cross-sectional selection (`TopBottom`, `Quantile`) can never see more than
  one score. Unavailable.

An index column does **not** make per-symbol dollar bars joinable — symbol A's
bar #5 and symbol B's bar #5 are different instants, and pretending otherwise
manufactures exactly the cross-series lookahead that TODO.md's `--join-on-date`
entry rejects permanently.

What it *does* enable is declaring a **shared exogenous index**: a basket-level
dollar-volume clock, an auction or session counter, any sequence all symbols are
sampled against. That case is causally sound and today inexpressible.

`cli::overlap` needs **no change** and becomes the right diagnostic unchanged —
it measures observed co-occurrence, so a declared index that is not actually
shared surfaces as a fragmented universe rather than as a plausible backtest.

## Staging

Each stage is independently shippable and independently useful.

### Stage 1 — make the existing path honest *(no new syntax)*

The offline path (build bars in Python → `--series` CSV) already half-works.
These are the places it is currently *silently wrong*:

1. **B3** — per-bar `year_fraction` from `atom.time` deltas.
2. **B4** — `bars_per_year` refuses instead of guessing when there is no cadence.
3. **B8** — gate `TrailingSharpe` on a resolvable time index.
4. **B2** — reword.

Regression tests per the standing rule: every layer the bug crosses, each
verified to fail against the old code first.

### Stage 2 — the index key *(the foundation)*

5. **D3** — introduce `IndexKey`, thread it through `DataFrame`'s key,
   `AtomSeries`, `join_pair_by_time`, `join_universe_by_time`, and the
   `--from`/`--to` slicing (`daterange`). Refuse a mixed-variant frame.
6. **B5** — the index-sampled `Resolution` in the cadence census.
7. Accept `index` as an alias for the `time` column at load
   (`src/cli/data.rs:369`, `src/cli/csv_source.rs:143`), and emit whichever the
   input used.

### Stage 3 — in-crate samplers *(the cheap 80%)*

8. A `!volume_bars` / `!dollar_bars` sampler as a sibling of `Resample` — same
   `Option<Candle>`-emitting shape, different completion predicate, obvious YAML
   home next to `!resample`. Dollar bars built from 1m klines are close enough
   for most work and need **no new data provider**: no provider serves anything
   but time bars today (`SeriesSource::atoms(symbol, interval, since, until)`).
9. **D4** / **B6** — the stream discriminant, if a concrete shape asks for it.

### Explicitly not in scope

- Trade-print ingestion (`aggTrades` archives) for true tick / imbalance bars.
- The multi-symbol as-of join. Same fork TODO.md has parked twice; revisit only
  with a concrete strategy shape asking for it, and start from what a
  cross-index leaf reads *between* bars.

## Parity and CI obligations

- Python mirror in the same PR for any Rust API added or renamed
  (`python/src/`), then regenerate stubs (`python tools/gen_python_stubs.py`).
- `scripts/ci-local.sh` before pushing — `cargo test` + `clippy` is not the gate.
- New metric or metric-affecting change → `tests/metrics_coverage.rs` demands a
  reference value or a written exemption.
- Stage 2 touches the frame's ordering contract: `tests/` fixtures under
  `tests/data/` must be re-verified, not regenerated, since a changed ordering
  would rebaseline silently.
