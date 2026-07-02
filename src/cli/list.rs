//! `fugazi list` — printed catalogue of what the CLI knows about.
//!
//! Two things a user might want to enumerate:
//!
//! * `fugazi list indicators` (the default) — every `!tag` [`crate::spec`]
//!   accepts: real-valued sources, boolean signals, and the `!param`
//!   placeholder that lets `--params` substitute values. Grouped by category,
//!   mirroring the reference in CLI.md so a user does not have to leave the
//!   terminal to remember an operator's name or arguments.
//! * `fugazi list sources` — every remote candle provider the `get` subcommand
//!   can fetch from (`binance:BTCUSDT[1d]`, `yfinance:SPY[1d]`, …), rendered
//!   from the same table `get` dispatches against.

use std::io::{self, Write};

use anyhow::Result;
use clap::ValueEnum;

use super::get::KNOWN_PROVIDERS;

/// What `fugazi list` should print. Mirrors the CLI's positional argument;
/// `Indicators` is the default so `fugazi list` (with no argument) keeps the
/// original behaviour.
#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[value(rename_all = "lower")]
pub enum ListKind {
    /// The strategy-YAML tag catalogue (sources, signals, `!param`).
    #[default]
    Indicators,
    /// The remote candle providers the `get` subcommand can fetch from.
    Sources,
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

/// A top-level section (Sources / Signals / Placeholders).
struct Section {
    title: &'static str,
    subtitle: &'static str,
    groups: &'static [Group],
}

const SOURCES: Section = Section {
    title: "SOURCES",
    subtitle: "real-valued nodes (Indicator<Output = Real>)",
    groups: &[
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
            title: "constants",
            entries: &[
                Entry { tag: "value", args: "<n>", doc: "a constant scalar" },
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
            title: "MACD (one tag per component)",
            entries: &[
                Entry { tag: "macd_line",      args: "source, fast, slow, signal", doc: "fast EMA − slow EMA" },
                Entry { tag: "macd_signal",    args: "source, fast, slow, signal", doc: "signal-EMA of the MACD line" },
                Entry { tag: "macd_histogram", args: "source, fast, slow, signal", doc: "line − signal" },
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
            title: "arithmetic operators",
            entries: &[
                Entry { tag: "add", args: "lhs, rhs", doc: "lhs + rhs" },
                Entry { tag: "sub", args: "lhs, rhs", doc: "lhs − rhs" },
                Entry { tag: "mul", args: "lhs, rhs", doc: "lhs × rhs" },
                Entry { tag: "div", args: "lhs, rhs", doc: "lhs / rhs (None on divide-by-zero)" },
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
            title: "rolling extrema",
            entries: &[
                Entry { tag: "rolling_max", args: "source, period", doc: "rolling maximum over the window" },
                Entry { tag: "rolling_min", args: "source, period", doc: "rolling minimum over the window" },
            ],
        },
    ],
};

const SIGNALS: Section = Section {
    title: "SIGNALS",
    subtitle: "boolean-valued nodes (Indicator<Output = bool>)",
    groups: &[
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
            title: "level comparisons (source vs. a constant)",
            entries: &[
                Entry { tag: "above", args: "source, level", doc: "source > level" },
                Entry { tag: "below", args: "source, level", doc: "source < level" },
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
            title: "constants",
            entries: &[
                Entry { tag: "value", args: "<bool>", doc: "a constant boolean leaf" },
            ],
        },
    ],
};

const PLACEHOLDERS: Section = Section {
    title: "PLACEHOLDERS",
    subtitle: "resolved before typed parsing — see `fugazi run --params`",
    groups: &[
        Group {
            title: "param",
            entries: &[
                Entry { tag: "param", args: "key, default?", doc: "substitute the value passed as --params key=..." },
                Entry { tag: "param", args: "<key>",         doc: "bare-string shorthand for { key: <key> }" },
            ],
        },
    ],
};

pub fn run(kind: ListKind) -> Result<()> {
    let out = io::stdout();
    let mut out = out.lock();
    match kind {
        ListKind::Indicators => {
            write_all(&mut out, &SOURCES)?;
            writeln!(out)?;
            write_all(&mut out, &SIGNALS)?;
            writeln!(out)?;
            write_all(&mut out, &PLACEHOLDERS)?;
        }
        ListKind::Sources => write_sources(&mut out, KNOWN_PROVIDERS)?,
    }
    Ok(())
}

/// Render the `fugazi get` provider table. Column widths track the widest
/// provider name so the descriptions line up regardless of how the list grows.
fn write_sources<W: Write>(w: &mut W, providers: &[(&str, &str)]) -> io::Result<()> {
    writeln!(w, "SOURCES — remote candle providers (`fugazi get`)")?;
    writeln!(w)?;
    writeln!(w, "  Spec grammar: <provider>:<symbol>[<freq>,...](,<symbol>[<freq>,...])*")?;
    writeln!(w)?;
    let name_width = providers.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, doc) in providers {
        writeln!(w, "    {name:<name_width$}  {doc}")?;
    }
    Ok(())
}

fn write_all<W: Write>(w: &mut W, section: &Section) -> io::Result<()> {
    writeln!(w, "{} — {}", section.title, section.subtitle)?;
    for group in section.groups {
        writeln!(w)?;
        writeln!(w, "  {}:", group.title)?;
        for e in group.entries {
            let sig = signature(e);
            writeln!(w, "    {sig:<52}  {}", e.doc)?;
        }
    }
    Ok(())
}

/// Render an entry's YAML surface. Every entry is `!`-prefixed for a uniform
/// column even where a bare word would also parse — matching the convention in
/// CLI.md and the strategy examples.
fn signature(e: &Entry) -> String {
    if e.args.is_empty() {
        format!("!{}", e.tag)
    } else if e.args.starts_with('<') || e.args.starts_with('[') {
        format!("!{} {}", e.tag, e.args)
    } else {
        format!("!{} {{ {} }}", e.tag, e.args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect every entry across every section and group.
    fn all_entries() -> Vec<&'static Entry> {
        [&SOURCES, &SIGNALS, &PLACEHOLDERS]
            .into_iter()
            .flat_map(|s| s.groups.iter())
            .flat_map(|g| g.entries.iter())
            .collect()
    }

    #[test]
    fn every_entry_has_a_nonempty_tag_and_doc() {
        for e in all_entries() {
            assert!(!e.tag.is_empty(), "empty tag");
            assert!(!e.doc.is_empty(), "empty doc for `{}`", e.tag);
        }
    }

    #[test]
    fn the_output_mentions_every_top_level_section() {
        // Render into a buffer and spot-check the section headers plus a
        // handful of representative tags from different categories.
        let mut buf: Vec<u8> = Vec::new();
        write_all(&mut buf, &SOURCES).unwrap();
        write_all(&mut buf, &SIGNALS).unwrap();
        write_all(&mut buf, &PLACEHOLDERS).unwrap();
        let text = String::from_utf8(buf).unwrap();

        for header in ["SOURCES", "SIGNALS", "PLACEHOLDERS"] {
            assert!(text.contains(header), "missing section `{header}`");
        }
        for tag in ["close", "!ema", "!macd_line", "!crosses_above", "!and", "!param"] {
            assert!(text.contains(tag), "missing tag `{tag}` in output");
        }
    }

    #[test]
    fn sources_output_lists_every_registered_provider() {
        let mut buf: Vec<u8> = Vec::new();
        write_sources(&mut buf, KNOWN_PROVIDERS).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("SOURCES"));
        for (name, doc) in KNOWN_PROVIDERS {
            assert!(text.contains(name), "missing provider `{name}` in output");
            assert!(text.contains(doc), "missing description for `{name}` in output");
        }
    }

    #[test]
    fn list_kind_defaults_to_indicators() {
        assert_eq!(ListKind::default(), ListKind::Indicators);
    }
}
