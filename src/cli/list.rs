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
//! * `fugazi list tickers <provider>` — every symbol the given provider
//!   currently exposes (backed by a real HTTP call — Binance advertises its
//!   spot vocabulary through `/api/v3/exchangeInfo`; Yahoo Finance and most
//!   retail equity APIs have no such endpoint and surface an "unsupported"
//!   error).

use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result};
use clap::Subcommand;
use tokio::runtime::Builder as RuntimeBuilder;

use super::get::{KNOWN_PROVIDERS, tickers_of};
use crate::style;

/// Column separation, in spaces, between adjacent items in the TTY grid.
const COLUMN_GAP: usize = 2;
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
        title: "constants",
        entries: &[
            Entry { tag: "value", args: "<n>",    doc: "a constant scalar" },
            Entry { tag: "value", args: "<bool>", doc: "a constant boolean leaf" },
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
            Entry { tag: "param", args: "key, default?", doc: "substitute the value passed as --params key=..." },
            Entry { tag: "param", args: "<key>",         doc: "bare-string shorthand for { key: <key> }" },
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
        title: "stability gate (mask until the chain has settled)",
        entries: &[
            Entry { tag: "stable", args: "source",   doc: "None until the source's stable_period() has elapsed, then a pass-through" },
            Entry { tag: "stable", args: "<signal>", doc: "same over a signal (false meanwhile) — no trades off seed-contaminated values" },
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
        ListCmd::Tickers { provider } => write_tickers(&mut out, &provider)?,
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
fn write_tickers<W: Write>(w: &mut W, provider: &str) -> Result<()> {
    let rt = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    let tickers = rt
        .block_on(tickers_of(provider))
        .with_context(|| format!("listing tickers for {provider}"))?;

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

/// Render `items` as a column-major grid at most `term_width` characters wide.
///
/// Cells are all `max_item_width + COLUMN_GAP` chars — the same shape `ls`
/// produces — so each row lines up. If a single item is wider than the
/// terminal, we degenerate to one column (the item still wraps naturally at
/// the terminal edge, which is what any downstream renderer expects).
fn write_grid<W: Write>(w: &mut W, items: &[String], term_width: usize) -> io::Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let max_width = items.iter().map(|s| s.len()).max().unwrap_or(0);
    let cell = max_width + COLUMN_GAP;
    // N cells fit iff N*max_width + (N-1)*gap <= term_width  <=>  N <= (term_width + gap) / cell.
    let cols = ((term_width + COLUMN_GAP) / cell.max(1)).max(1);
    let rows = items.len().div_ceil(cols);

    for r in 0..rows {
        for c in 0..cols {
            let idx = c * rows + r;
            let Some(item) = items.get(idx) else { break };
            // Pad only when a further cell exists on this row; the last cell
            // in a row gets its natural width so trailing whitespace doesn't
            // trigger terminal soft-wrap on narrow terminals.
            let is_last_on_row = (c + 1) * rows + r >= items.len();
            if is_last_on_row {
                write!(w, "{item}")?;
            } else {
                write!(w, "{item:<cell$}")?;
            }
        }
        writeln!(w)?;
    }
    Ok(())
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
    fn grid_degrades_to_single_column_on_narrow_terminals() {
        // Widest item is 8 chars ("VERYLONG"), so a cell is 10 chars — a
        // 6-char terminal can't fit even one padded cell but must still
        // display something. We collapse to one column.
        let out = render_grid(&["VERYLONG", "AB"], 6);
        assert_eq!(out, "VERYLONG\nAB\n");
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
