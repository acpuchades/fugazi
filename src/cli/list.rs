//! `fugazi list` — printed catalogue of what the CLI knows about.
//!
//! Three things a user might want to enumerate:
//!
//! * `fugazi list indicators` — every tag [`crate::spec`] accepts (real-valued
//!   sources, boolean signals, the `!param` placeholder), one flat catalogue
//!   of categories sorted alphabetically, so a user does not have to leave the
//!   terminal to remember an operator's name or arguments.
//! * `fugazi list sources` — every remote candle provider the `get` subcommand
//!   can fetch from (`binance:BTCUSDT[1d]`, `yfinance:SPY[1d]`, …), rendered
//!   from the same table `get` dispatches against.
//! * `fugazi list tickers <provider> [PATTERN]` — every symbol the given
//!   provider currently exposes (backed by a real HTTP call — Binance
//!   advertises its spot vocabulary through `/api/v3/exchangeInfo`; Yahoo
//!   Finance and most retail equity APIs have no such endpoint and surface an
//!   "unsupported" error), optionally filtered by a shell-style glob
//!   ([`crate::glob`]): `fugazi list tickers binance 'b*'` starts-with,
//!   `'*b*'` contains. A provider's vocabulary runs to thousands of symbols, so
//!   the filter is what makes the command usable without a pipe.

use std::borrow::Cow;
use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result};
use clap::Subcommand;
use tokio::runtime::Builder as RuntimeBuilder;

use super::get::{KNOWN_PROVIDERS, tickers_of};
use crate::glob;
use crate::style;

/// Column separation, in spaces, between adjacent items in the TTY grid.
const COLUMN_GAP: usize = 2;
/// Widest cell the TTY grid will render before eliding the item with `…`.
///
/// A ticker list can be badly skewed — CoinGecko's 17.5k coin ids have a median
/// length of 10, but ~730 of them run 30–72 characters
/// (`state-street-technology-select-sector-spdr-etf-robinhood-tokenized-stock`),
/// scattered through the alphabet. Column-major layout puts one of those in
/// *every* column, so honouring them in full collapses the whole grid to a
/// single column. Capping the cell keeps the grid dense and scannable.
///
/// **The cap is a TTY-display concern only.** When stdout is piped or
/// redirected, [`write_tickers`] prints one exact id per line and never elides —
/// that is the path `| grep`, `| wc -l` and copy-paste-into-`get` consume.
const MAX_CELL_WIDTH: usize = 24;
/// Fallback terminal width when `console` can't query the tty (e.g. no ioctl).
const FALLBACK_WIDTH: usize = 80;

/// What `fugazi list` should print. Nested-subcommand shape so the ticker form
/// can carry its own required positional (`fugazi list tickers <provider>`)
/// without leaking a "PROVIDER — required when kind = tickers" caveat into the
/// `indicators` / `sources` forms.
#[derive(Subcommand, Clone, Debug)]
pub enum ListCmd {
    /// The strategy-YAML tag catalogue (sources, signals, `!param`).
    Indicators,
    /// The remote candle providers the `get` subcommand can fetch from.
    Sources,
    /// Every symbol the given provider currently exposes.
    Tickers {
        /// The provider (e.g. `binance`). See `fugazi list sources`.
        #[arg(value_name = "PROVIDER")]
        provider: String,

        /// Keep only the symbols matching this shell-style glob:
        /// `*` any run of characters, `?` one character, `[abc]` / `[a-z]` a
        /// set or range, `[!abc]` its complement, `\*` a literal `*`.
        ///
        /// Matching is case-insensitive and whole-symbol — `b*` is "starts with
        /// b", `*b*` is "contains b", and `btc` on its own means the symbol
        /// `BTC` exactly. Quote the pattern ('b*'), or the shell will try to
        /// expand it against your files first.
        #[arg(value_name = "PATTERN")]
        pattern: Option<glob::Pattern>,
    },
}

/// One YAML tag: its name, argument shape and a one-line description.
struct Entry {
    /// The tag name (without the leading `!`). Empty `args` implies the
    /// leaf/bare-word form; a non-empty `args` implies the `!tag { args }` form.
    tag: &'static str,
    args: &'static str,
    doc: &'static str,
}

/// A named group of entries, one row per category header in the output.
struct Group {
    title: &'static str,
    entries: &'static [Entry],
}

/// The full tag catalogue — one flat list of groups, kept in alphabetical
/// order of title (rendered as-is; a test asserts the order so a new group
/// lands in the right place).
const GROUPS: &[Group] = &[
    Group {
        title: "arithmetic operators",
        entries: &[
            Entry { tag: "add", args: "lhs, rhs", doc: "lhs + rhs" },
            Entry { tag: "sub", args: "lhs, rhs", doc: "lhs − rhs" },
            Entry { tag: "mul", args: "lhs, rhs", doc: "lhs × rhs" },
            Entry { tag: "div", args: "lhs, rhs", doc: "lhs / rhs (None on divide-by-zero)" },
        ],
    },
    Group {
        title: "bands (one tag per component)",
        entries: &[
            Entry { tag: "bb_upper",       args: "source, period, k",                             doc: "Bollinger upper band" },
            Entry { tag: "bb_middle",      args: "source, period, k",                             doc: "Bollinger middle band" },
            Entry { tag: "bb_lower",       args: "source, period, k",                             doc: "Bollinger lower band" },
            Entry { tag: "keltner_upper",  args: "source, ema_period, atr_period, multiplier",    doc: "Keltner upper band" },
            Entry { tag: "keltner_middle", args: "source, ema_period, atr_period, multiplier",    doc: "Keltner middle band" },
            Entry { tag: "keltner_lower",  args: "source, ema_period, atr_period, multiplier",    doc: "Keltner lower band" },
            Entry { tag: "donchian_upper", args: "high, low, period",                             doc: "Donchian upper band" },
            Entry { tag: "donchian_middle",args: "high, low, period",                             doc: "Donchian middle band" },
            Entry { tag: "donchian_lower", args: "high, low, period",                             doc: "Donchian lower band" },
        ],
    },
    Group {
        title: "bar indicators (consume the whole Candle, no source)",
        entries: &[
            Entry { tag: "atr",         args: "period", doc: "average true range" },
            Entry { tag: "mfi",         args: "period", doc: "money-flow index" },
            Entry { tag: "true_range",  args: "",       doc: "true range of the current bar" },
            Entry { tag: "obv",         args: "",       doc: "on-balance volume (cumulative)" },
            Entry { tag: "vwap",        args: "",       doc: "volume-weighted average price (cumulative)" },
            Entry { tag: "ad",          args: "",       doc: "Chaikin A/D line (cumulative)" },
        ],
    },
    Group {
        title: "basket selection rules (for `selection:` on BasketStrategySpec)",
        entries: &[
            Entry { tag: "top_bottom", args: "longs, shorts",       doc: "top `longs` symbols by score → long, bottom `shorts` → short (never overlapping)" },
            Entry { tag: "threshold",  args: "long_min, short_max", doc: "long at/above long_min; short at/below short_max" },
            Entry { tag: "quantile",   args: "long_q, short_q",     doc: "long the top long_q fraction; short the bottom short_q — counts are ceil(q * n)" },
        ],
    },
    Group {
        title: "boolean logic",
        entries: &[
            Entry { tag: "and",     args: "lhs, rhs",   doc: "lhs && rhs" },
            Entry { tag: "or",      args: "lhs, rhs",   doc: "lhs || rhs" },
            Entry { tag: "xor",     args: "lhs, rhs",   doc: "lhs ^ rhs" },
            Entry { tag: "all",     args: "[s1, ...]",  doc: "AND-fold of a list (empty ⇒ true)" },
            Entry { tag: "any",     args: "[s1, ...]",  doc: "OR-fold of a list (empty ⇒ false)" },
            Entry { tag: "not",     args: "<signal>",   doc: "logical NOT" },
            Entry { tag: "changed", args: "<signal>",   doc: "fires on any transition (0->1 or 1->0)" },
        ],
    },
    Group {
        title: "calendar (read atom.time; None on an untimed bar)",
        entries: &[
            Entry { tag: "year",         args: "", doc: "Gregorian year (e.g. 2024)" },
            Entry { tag: "month",        args: "", doc: "month of year, 1 (Jan) .. 12 (Dec)" },
            Entry { tag: "day",          args: "", doc: "day of the month, 1 .. 31" },
            Entry { tag: "hour",         args: "", doc: "hour of the day (UTC), 0 .. 23" },
            Entry { tag: "minute",       args: "", doc: "minute of the hour, 0 .. 59" },
            Entry { tag: "second",       args: "", doc: "second of the minute, 0 .. 59" },
            Entry { tag: "day_of_week",  args: "", doc: "ISO 8601 weekday, 1 (Mon) .. 7 (Sun)" },
            Entry { tag: "day_of_year",  args: "", doc: "day of the year, 1 .. 366" },
            Entry { tag: "week_of_year", args: "", doc: "ISO 8601 week of the year, 1 .. 53" },
            Entry { tag: "quarter",      args: "", doc: "calendar quarter, 1 .. 4" },
            Entry { tag: "unix_seconds", args: "", doc: "Unix seconds since the epoch" },
            Entry { tag: "unix_millis",  args: "", doc: "Unix milliseconds since the epoch" },
            Entry { tag: "time",         args: "", doc: "the raw Timestamp payload (not a scalar)" },
            Entry { tag: "is_weekday",   args: "", doc: "true on Mon–Fri (bool signal)" },
            Entry { tag: "is_weekend",   args: "", doc: "true on Sat/Sun (bool signal)" },
        ],
    },
    Group {
        title: "candle leaves",
        entries: &[
            Entry { tag: "close",   args: "", doc: "the bar's close price" },
            Entry { tag: "high",    args: "", doc: "the bar's high price" },
            Entry { tag: "low",     args: "", doc: "the bar's low price" },
            Entry { tag: "open",    args: "", doc: "the bar's open price" },
            Entry { tag: "volume",  args: "", doc: "the bar's traded volume" },
            Entry { tag: "typical", args: "", doc: "(high + low + close) / 3" },
            Entry { tag: "median",  args: "", doc: "(high + low) / 2" },
        ],
    },
    Group {
        title: "comparisons (tolerance-aware; epsilon defaults to 1e-8)",
        entries: &[
            Entry { tag: "gt", args: "lhs, rhs, epsilon?", doc: "lhs > rhs" },
            Entry { tag: "lt", args: "lhs, rhs, epsilon?", doc: "lhs < rhs" },
            Entry { tag: "ge", args: "lhs, rhs, epsilon?", doc: "lhs >= rhs" },
            Entry { tag: "le", args: "lhs, rhs, epsilon?", doc: "lhs <= rhs" },
            Entry { tag: "eq", args: "lhs, rhs, epsilon?", doc: "lhs == rhs within epsilon" },
            Entry { tag: "ne", args: "lhs, rhs, epsilon?", doc: "lhs != rhs beyond epsilon" },
        ],
    },
    Group {
        title: "conditional (three-source ternary)",
        entries: &[
            Entry {
                tag: "if_else",
                args: "cond, if_true, if_false",
                doc: "cond ? if_true : if_false — cond is a signal, both branches are real sources; None on cond ⇒ None; warm-up = max across the three (see IfElse docs)",
            },
        ],
    },
    Group {
        title: "constants",
        entries: &[
            Entry { tag: "value", args: "<n>",    doc: "a constant scalar" },
            Entry { tag: "value", args: "<str>",  doc: "a constant string — the operand of !str_eq / !str_ne (quote a numeric-looking one: !value \"70\")" },
            Entry { tag: "value", args: "<bool>", doc: "a constant boolean leaf" },
        ],
    },
    Group {
        title: "cross-timeframe composition (resample and latch, composed directly)",
        entries: &[
            Entry {
                tag: "resample",
                args: "every, inner",
                doc: "run `inner` (any Real source over Candle) on every N-bar aggregated candle; None between",
            },
            Entry {
                tag: "latch",
                args: "source",
                doc: "hold the last Some output of `source` — wrap the outermost recursive smoother of a resampled chain",
            },
        ],
    },
    Group {
        title: "crossovers (comparison + just-transitioned)",
        entries: &[
            Entry { tag: "crosses_above", args: "lhs, rhs", doc: "lhs > rhs and the comparison just flipped" },
            Entry { tag: "crosses_below", args: "lhs, rhs", doc: "lhs < rhs and the comparison just flipped" },
        ],
    },
    Group {
        title: "level comparisons (source vs. a constant)",
        entries: &[
            Entry { tag: "above", args: "source, level", doc: "source > level" },
            Entry { tag: "below", args: "source, level", doc: "source < level" },
        ],
    },
    Group {
        title: "lookback operators",
        entries: &[
            Entry { tag: "lag",   args: "source, periods", doc: "value from `periods` bars ago" },
            Entry { tag: "diff",  args: "source, periods", doc: "x[t] − x[t − periods]" },
            Entry { tag: "ratio", args: "source, periods", doc: "x[t] / x[t − periods]" },
            Entry { tag: "roc",   args: "source, periods", doc: "rate of change (100 × ratio − 100)" },
        ],
    },
    Group {
        title: "MACD (one tag per component)",
        entries: &[
            Entry { tag: "macd_line",      args: "source, fast, slow, signal", doc: "fast EMA − slow EMA" },
            Entry { tag: "macd_signal",    args: "source, fast, slow, signal", doc: "signal-EMA of the MACD line" },
            Entry { tag: "macd_histogram", args: "source, fast, slow, signal", doc: "line − signal" },
        ],
    },
    Group {
        title: "moving averages",
        entries: &[
            Entry { tag: "sma", args: "source, period", doc: "simple moving average" },
            Entry { tag: "ema", args: "source, period", doc: "exponential moving average" },
            Entry { tag: "rma", args: "source, period", doc: "Wilder's smoothed moving average" },
            Entry { tag: "wma", args: "source, period", doc: "linearly weighted moving average" },
            Entry { tag: "hma", args: "source, period", doc: "Hull moving average" },
        ],
    },
    Group {
        title: "oscillators",
        entries: &[
            Entry { tag: "rsi",        args: "source, period",                    doc: "relative strength index" },
            Entry { tag: "stddev",     args: "source, period",                    doc: "rolling standard deviation" },
            Entry { tag: "cci",        args: "source, period",                    doc: "commodity channel index" },
            Entry { tag: "stochastic", args: "source, period",                    doc: "stochastic oscillator" },
            Entry { tag: "stoch_rsi",  args: "source, rsi_period, stoch_period",  doc: "stochastic RSI" },
            Entry { tag: "williams_r", args: "period",                            doc: "Williams %R" },
        ],
    },
    Group {
        title: "placeholders (resolved before typed parsing; see `fugazi run --params`)",
        entries: &[
            Entry { tag: "param", args: "key, default?", doc: "load-time: substitute the value passed as --params key=..." },
            Entry { tag: "param", args: "<key>",         doc: "bare-string shorthand for { key: <key> }" },
            Entry { tag: "arg",   args: "key, default?", doc: "build-time: substitute a driver-supplied arg (e.g. SYM per-symbol in a basket score/sizing template)" },
            Entry { tag: "arg",   args: "<key>",         doc: "bare-string shorthand for { key: <key> }" },
            Entry { tag: "import", args: "<path>",       doc: "load-time: splice in another YAML file as this value; path is relative to the importing file (imports resolve before !param, so a fragment sees the same --params table)" },
        ],
    },
    Group {
        title: "position anchors (only inside a strategy; read the live position)",
        entries: &[
            Entry { tag: "entry",  args: "", doc: "the position's fill price (None while flat)" },
            Entry { tag: "peak",   args: "", doc: "running high since entry (long trailing-stop anchor)" },
            Entry { tag: "trough", args: "", doc: "running low since entry (short trailing-stop anchor)" },
        ],
    },
    Group {
        title: "rolling extrema",
        entries: &[
            Entry { tag: "rolling_max", args: "source, period", doc: "rolling maximum over the window" },
            Entry { tag: "rolling_min", args: "source, period", doc: "rolling minimum over the window" },
        ],
    },
    Group {
        title: "sizing helpers (for `sizing:` on SingleStrategySpec / PairsStrategySpec / BasketStrategySpec)",
        entries: &[
            Entry { tag: "equal_weight",      args: "<n_legs>",                            doc: "1/n_legs — the basket sugar for 100% gross across n_legs selected symbols" },
            Entry { tag: "vol_target",        args: "target, window, bars_per_year",       doc: "inverse realized-vol multiplier (price series)" },
            Entry { tag: "atr_risk",          args: "risk_frac, period, atr_multiple",     doc: "fixed per-trade risk sized by ATR" },
            Entry { tag: "drawdown_throttle", args: "max_drawdown",                        doc: "linear de-lever as book drawdown deepens (0..1)" },
            Entry { tag: "equity_vol_target", args: "target, window, bars_per_year",       doc: "vol targeting on the strategy's own equity returns" },
            Entry { tag: "fractional_kelly",  args: "kelly_fraction, window",              doc: "kelly_fraction * mean/variance of the last N trade returns" },
        ],
    },
    Group {
        title: "trend / directional",
        entries: &[
            Entry { tag: "adx",              args: "period",     doc: "ADX from the Adx bundle" },
            Entry { tag: "plus_di",          args: "period",     doc: "+DI from the Adx bundle" },
            Entry { tag: "minus_di",         args: "period",     doc: "-DI from the Adx bundle" },
            Entry { tag: "dmi_plus_di",      args: "period",     doc: "+DI from the standalone Dmi" },
            Entry { tag: "dmi_minus_di",     args: "period",     doc: "-DI from the standalone Dmi" },
            Entry { tag: "aroon_up",         args: "period",     doc: "Aroon Up" },
            Entry { tag: "aroon_down",       args: "period",     doc: "Aroon Down" },
            Entry { tag: "aroon_oscillator", args: "period",     doc: "Aroon Up − Aroon Down" },
            Entry { tag: "sar",              args: "step, max",  doc: "parabolic SAR" },
        ],
    },
    Group {
        title: "unstable pass-through (opt out of the readiness gate)",
        entries: &[
            Entry { tag: "unstable", args: "source", doc: "pass through `source` but report unstable_period() = 0" },
            Entry { tag: "unstable", args: "signal", doc: "pass through `signal` but report unstable_period() = 0" },
        ],
    },
];

pub fn run(cmd: ListCmd) -> Result<()> {
    // The banner goes to a human, not a pipe: the piped forms are
    // machine-friendly (one ticker per line for `grep`/`wc -l`), so the header
    // is gated on stdout being a terminal rather than a `--quiet` flag.
    if io::stdout().is_terminal() {
        let description = match &cmd {
            ListCmd::Indicators => "the strategy-YAML tag vocabulary",
            ListCmd::Sources => "the remote candle providers `get` fetches from",
            ListCmd::Tickers { .. } => "every symbol the provider exposes",
        };
        style::print_header("list", description);
    }
    let out = io::stdout();
    let mut out = out.lock();
    match cmd {
        ListCmd::Indicators => write_indicators(&mut out)?,
        ListCmd::Sources => write_sources(&mut out, KNOWN_PROVIDERS)?,
        ListCmd::Tickers { provider, pattern } => {
            write_tickers(&mut out, &provider, pattern.as_ref())?
        }
    }
    Ok(())
}

/// Fetch and print the provider's ticker list. Layout follows the `ls`
/// convention: **one symbol per line** when stdout is being piped or
/// redirected (so `| grep`, `| wc -l`, `| sort -u` keep working), and a
/// **column-major grid** sized to the terminal width when stdout is a TTY (so
/// eyeballing 1000+ symbols isn't a scrollfest). Spins up a short-lived tokio
/// runtime — like `fugazi get` — since the underlying
/// [`crate::sources::CandleSource::tickers`] method is async.
///
/// The two branches differ in one more way, and it matters: the grid elides an
/// overlong symbol at [`MAX_CELL_WIDTH`], while the piped branch **never
/// shortens anything**. Machine-read output stays exact and complete; only the
/// human-facing grid trades a few characters for density. Both branches are
/// provider-agnostic — nothing here knows a Binance ticker from a CoinGecko
/// coin id.
///
/// An optional [`glob::Pattern`] filters the list *before* either branch, so
/// `list tickers binance 'b*' | wc -l` counts what the grid would have shown.
/// Filtering happens here, over the provider's returned vocabulary, rather than
/// being pushed into the source trait: no provider's endpoint offers a
/// server-side filter, so a `pattern` parameter there would be a lie that every
/// impl re-implements identically.
fn write_tickers<W: Write>(w: &mut W, provider: &str, pattern: Option<&glob::Pattern>) -> Result<()> {
    let rt = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    let mut tickers = rt
        .block_on(tickers_of(provider))
        .with_context(|| format!("listing tickers for {provider}"))?;

    if let Some(pattern) = pattern {
        let total = tickers.len();
        tickers.retain(|t| pattern.matches(t));
        // Zero matches is a legitimate answer (an empty list to a pipe), but on
        // a terminal a silent blank screen reads like a bug — say which pattern
        // matched nothing, and out of how many. The "did you mean a substring?"
        // hint is only offered when it would actually say something different:
        // suggesting `*b**` to someone who typed `b*` is noise.
        if tickers.is_empty() && std::io::stdout().is_terminal() {
            let hint = if pattern.is_anchored() {
                format!(
                    " (matching is whole-symbol — `*{pattern}*` searches for a substring)"
                )
            } else {
                String::new()
            };
            writeln!(w, "  no symbol out of {total} matches `{pattern}`{hint}")?;
            return Ok(());
        }
    }

    if std::io::stdout().is_terminal() {
        let term_width = console::Term::stdout()
            .size_checked()
            .map(|(_, cols)| cols as usize)
            .unwrap_or(FALLBACK_WIDTH);
        write_grid(w, &tickers, term_width)?;
    } else {
        for t in &tickers {
            writeln!(w, "{t}")?;
        }
    }
    Ok(())
}

/// Render `items` as a column-major grid at most `term_width` columns wide.
///
/// Columns are sized **independently**, like `ls` — the widest item in a column
/// sets that column's width, and the layout is the largest column count whose
/// widths still fit. A uniform cell width (every column as wide as the single
/// longest item) reads the same on an even list but collapses on a skewed one:
/// CoinGecko's coin ids run from `btc` to a 72-character
/// `state-street-technology-select-sector-spdr-etf-robinhood-tokenized-stock`,
/// and one such outlier would drag ~18k short ids into a single column. This is
/// deliberately provider-agnostic — the layout is a property of the strings, not
/// of where they came from.
///
/// Widths are **display** widths, not byte lengths: Binance lists symbols like
/// `币安人生USDT`, whose 12 characters occupy 16 bytes (and 16 terminal cells,
/// CJK being double-width). `console::measure_text_width` is what the rest of
/// the CLI's styling already uses.
///
/// An item wider than the whole terminal degenerates to one column and wraps at
/// the terminal edge, which is what any downstream renderer expects.
fn write_grid<W: Write>(w: &mut W, items: &[String], term_width: usize) -> io::Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    // Elide overlong items first, then lay out the *rendered* cells — see
    // [`MAX_CELL_WIDTH`]. Only the grid elides; piping prints exact ids.
    let items: Vec<Cow<'_, str>> = items
        .iter()
        .map(|s| elide(s, MAX_CELL_WIDTH.min(term_width.max(1))))
        .collect();
    let widths: Vec<usize> = items
        .iter()
        .map(|s| console::measure_text_width(s))
        .collect();

    // Try the most columns first and take the first layout that fits. The
    // narrowest item bounds how many columns could ever fit, so this loop is
    // short even for a 20k-symbol list.
    let min_width = widths.iter().copied().min().unwrap_or(0);
    let max_cols = ((term_width + COLUMN_GAP) / (min_width + COLUMN_GAP).max(1))
        .clamp(1, items.len());

    let (cols, col_widths) = (1..=max_cols)
        .rev()
        .filter(|&cols| {
            // Skip counts whose last column would be empty (column-major packs
            // `rows` items per column, so `cols` can outrun the items). Such a
            // layout renders identically to the smaller one it degenerates
            // into — skipping keeps the chosen `cols` truthful.
            let rows = items.len().div_ceil(cols);
            rows * (cols - 1) < items.len()
        })
        .find_map(|cols| {
            let widths = column_widths(&widths, cols);
            let total: usize = widths.iter().sum::<usize>() + COLUMN_GAP * (cols - 1);
            (total <= term_width).then_some((cols, widths))
        })
        // Even one column can overflow (an item wider than the terminal); the
        // single-column layout is still the right answer.
        .unwrap_or_else(|| (1, column_widths(&widths, 1)));

    let rows = items.len().div_ceil(cols);
    for r in 0..rows {
        for (c, col_width) in col_widths.iter().enumerate() {
            let idx = c * rows + r;
            let Some(item) = items.get(idx) else { break };
            // Pad only when a further cell exists on this row; the last cell
            // in a row gets its natural width so trailing whitespace doesn't
            // trigger terminal soft-wrap on narrow terminals.
            let is_last_on_row = (c + 1) * rows + r >= items.len();
            if is_last_on_row {
                write!(w, "{item}")?;
            } else {
                let pad = col_width + COLUMN_GAP - widths[idx];
                write!(w, "{item}{:pad$}", "")?;
            }
        }
        writeln!(w)?;
    }
    Ok(())
}

/// `s` trimmed to at most `max` **display** cells, with the last cell spent on
/// an ellipsis when anything was dropped. Borrows when `s` already fits.
///
/// Width is accumulated per character (`console::measure_text_width`, which the
/// CLI's styling already uses), so a double-width CJK character is never cut in
/// half to squeeze under the cap.
fn elide(s: &str, max: usize) -> Cow<'_, str> {
    if console::measure_text_width(s) <= max {
        return Cow::Borrowed(s);
    }
    // One cell goes to the `…`; a cap of 0 or 1 leaves room for nothing else.
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = console::measure_text_width(ch.encode_utf8(&mut [0u8; 4]));
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    Cow::Owned(out)
}

/// The width of each column in a column-major layout of `widths` into `cols`
/// columns: the widest item that lands in that column.
fn column_widths(widths: &[usize], cols: usize) -> Vec<usize> {
    let rows = widths.len().div_ceil(cols);
    (0..cols)
        .map(|c| {
            widths[(c * rows).min(widths.len())..((c + 1) * rows).min(widths.len())]
                .iter()
                .copied()
                .max()
                .unwrap_or(0)
        })
        .collect()
}

/// Render the `fugazi get` provider table. Column widths track the widest
/// provider name so the descriptions line up regardless of how the list grows.
/// No title line of its own — the banner printed by [`run`] already names the
/// command.
fn write_sources<W: Write>(w: &mut W, providers: &[(&str, &str)]) -> io::Result<()> {
    writeln!(w, "  Spec grammar: <provider>:<symbol>[<freq>,...](,<symbol>[<freq>,...])*")?;
    writeln!(w)?;
    let name_width = providers.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, doc) in providers {
        writeln!(w, "    {name:<name_width$}  {doc}")?;
    }
    Ok(())
}

/// Render the full tag catalogue. [`GROUPS`] is already alphabetical, so this
/// just walks it in order. No title line of its own — the banner printed by
/// [`run`] already names the command.
fn write_indicators<W: Write>(w: &mut W) -> io::Result<()> {
    for (i, group) in GROUPS.iter().enumerate() {
        if i > 0 {
            writeln!(w)?;
        }
        writeln!(w, "  {}:", group.title)?;
        for e in group.entries {
            let sig = signature(e);
            writeln!(w, "    {sig:<52}  {}", e.doc)?;
        }
    }
    Ok(())
}

/// Render an entry's YAML surface. Parameterless leaves parse as bare strings
/// (`close`, `obv`), so they render without the `!`; everything that takes
/// arguments renders in its `!tag`-prefixed form.
fn signature(e: &Entry) -> String {
    if e.args.is_empty() {
        e.tag.to_string()
    } else if e.args.starts_with('<') || e.args.starts_with('[') {
        format!("!{} {}", e.tag, e.args)
    } else {
        format!("!{} {{ {} }}", e.tag, e.args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect every entry across every group.
    fn all_entries() -> Vec<&'static Entry> {
        GROUPS.iter().flat_map(|g| g.entries.iter()).collect()
    }

    #[test]
    fn every_entry_has_a_nonempty_tag_and_doc() {
        for e in all_entries() {
            assert!(!e.tag.is_empty(), "empty tag");
            assert!(!e.doc.is_empty(), "empty doc for `{}`", e.tag);
        }
    }

    #[test]
    fn the_output_mentions_every_group_and_representative_tags() {
        let mut buf: Vec<u8> = Vec::new();
        write_indicators(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();

        for group in GROUPS {
            assert!(
                text.contains(&format!("  {}:", group.title)),
                "missing group `{}` in output",
                group.title
            );
        }
        for tag in ["close", "!ema", "!macd_line", "!crosses_above", "!and", "!param"] {
            assert!(text.contains(tag), "missing tag `{tag}` in output");
        }
    }

    /// [`GROUPS`] is rendered as-is, so the alphabetical order lives in the
    /// source itself — this pins it so a new group lands in the right place.
    #[test]
    fn groups_are_declared_in_alphabetical_order() {
        let titles: Vec<String> = GROUPS.iter().map(|g| g.title.to_lowercase()).collect();
        let mut sorted = titles.clone();
        sorted.sort();
        assert_eq!(titles, sorted, "GROUPS is not declared in alphabetical order of title");
    }

    fn render_grid(items: &[&str], width: usize) -> String {
        let items: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        let mut buf: Vec<u8> = Vec::new();
        write_grid(&mut buf, &items, width).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn grid_lays_out_column_major_like_ls() {
        // Widest item is 2 chars, gap is 2, so each cell is 4 chars. Width 12
        // fits (12 + 2) / 4 = 3 columns. 7 items → ceil(7/3) = 3 rows.
        // Column-major means column 0 gets items[0..3], column 1 items[3..6],
        // column 2 items[6..7]. Row 0 → "11", "44", "77"; row 1 → "22", "55";
        // row 2 → "33", "66".
        let out = render_grid(
            &["11", "22", "33", "44", "55", "66", "77"],
            12,
        );
        assert_eq!(out, "11  44  77\n22  55\n33  66\n");
    }

    #[test]
    fn grid_sizes_each_column_independently_so_one_outlier_cannot_collapse_it() {
        // The `fugazi list tickers cg` pathology in miniature: mostly
        // short ids plus a long one (here still inside MAX_CELL_WIDTH, so the
        // cap isn't what's being tested). A uniform cell width — every column as
        // wide as the longest item, 20 + 2 gap = 22 — fits only one column in
        // 40. Per-column widths let the outlier widen only the column it lands
        // in: three 2-wide columns + the 20-wide one + three gaps = 32 ≤ 40, so
        // four columns fit where the old layout managed one.
        let long = "1234567890".repeat(2);
        let out = render_grid(&["aa", "bb", "cc", "dd", "ee", "ff", &long], 40);
        assert_eq!(out, format!("aa  cc  ee  {long}\nbb  dd  ff\n"));
    }

    #[test]
    fn grid_pads_by_display_width_not_byte_length() {
        // Binance really does list `币安人生USDT`: 12 display cells (CJK is
        // double-width), but 16 bytes. Padding by `str::len` would over-pad the
        // rest of its column by 4 and skew every row under it.
        let out = render_grid(&["币安人生USDT", "AB", "CD", "EF"], 20);
        let (first, second) = out.split_once('\n').unwrap();
        assert_eq!(first, "币安人生USDT  CD");
        // "AB" is padded to column 0's display width (12) + the 2-char gap.
        assert_eq!(second, "AB            EF\n");
    }

    #[test]
    fn grid_elides_an_overlong_item_at_the_cell_cap() {
        // The real CoinGecko outlier. In the grid it is elided to MAX_CELL_WIDTH
        // (23 chars + `…`); the piped branch in `write_tickers` still prints it
        // in full, which is what `| grep` and copy-paste-into-`get` consume.
        let long = "state-street-technology-select-sector-spdr-etf";
        let out = render_grid(&[long, "btc"], 80);
        let first = out.lines().next().unwrap();
        assert!(first.starts_with("state-street-technology"), "{first}");
        assert!(first.contains('…'), "{first}");
        assert_eq!(
            console::measure_text_width(first.split("  ").next().unwrap()),
            MAX_CELL_WIDTH,
        );
    }

    #[test]
    fn elide_never_splits_a_double_width_char_to_squeeze_under_the_cap() {
        // Budget 4 = 3 cells + the `…`. `币` is 2 cells wide, so exactly one
        // fits: a byte- or char-count truncation would emit two and overflow.
        assert_eq!(elide("币币币币", 4), "币…");
        assert_eq!(console::measure_text_width(elide("币币币币", 4).as_ref()), 3);
        // Anything already inside the cap is returned untouched (and borrowed).
        assert_eq!(elide("btc", 24), "btc");
        assert!(matches!(elide("btc", 24), Cow::Borrowed(_)));
    }

    #[test]
    fn grid_degrades_to_single_column_on_narrow_terminals() {
        // A 6-char terminal can't fit two cells, so we collapse to one column.
        // The cell cap is clamped to the terminal, so the overlong item is
        // elided to fit rather than soft-wrapping across lines.
        let out = render_grid(&["VERYLONG", "AB"], 6);
        assert_eq!(out, "VERYL…\nAB\n");
    }

    #[test]
    fn grid_last_cell_in_row_has_no_trailing_padding() {
        // 3 items, width 12: cell = 3+2 = 5, cols = (12+2)/5 = 2. Rows = 2.
        // First row has both cells filled → column 0 is padded to 5, column 1
        // is the final cell → no padding.
        let out = render_grid(&["AAA", "BBB", "CCC"], 12);
        assert_eq!(out, "AAA  CCC\nBBB\n");
    }

    #[test]
    fn grid_handles_empty_input() {
        let out = render_grid(&[], 80);
        assert_eq!(out, "");
    }

    #[test]
    fn sources_output_lists_every_registered_provider() {
        let mut buf: Vec<u8> = Vec::new();
        write_sources(&mut buf, KNOWN_PROVIDERS).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Spec grammar"));
        for (name, doc) in KNOWN_PROVIDERS {
            assert!(text.contains(name), "missing provider `{name}` in output");
            assert!(text.contains(doc), "missing description for `{name}` in output");
        }
    }

}
