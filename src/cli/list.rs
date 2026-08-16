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

use super::get::{KNOWN_PROVIDERS, ProviderInfo, tickers_of};
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

// The `indicators` catalogue is rendered from the grammar descriptor
// (`spec::grammar::spec_grammar`) grouped by its `category` taxonomy
// (`spec::grammar::CATEGORIES`) — one authority for the tag set, its prose, and
// its curated grouping, shared with every other consumer. Nothing tag-specific
// is hand-maintained here.

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
/// [`crate::sources::SeriesSource::tickers`] method is async.
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
fn write_sources<W: Write>(w: &mut W, providers: &[ProviderInfo]) -> io::Result<()> {
    writeln!(w, "  Spec grammar: <provider>:<symbol>[<freq>,...](,<symbol>[<freq>,...])*")?;
    writeln!(w)?;
    let name_width = providers.iter().map(|p| p.name.len()).max().unwrap_or(0);
    for p in providers {
        let (name, doc) = (p.name, p.description);
        writeln!(w, "    {name:<name_width$}  {doc}")?;
    }
    Ok(())
}

/// Render the full tag catalogue from the grammar descriptor, grouped by its
/// [`CATEGORIES`](crate::spec::grammar::CATEGORIES) taxonomy. The taxonomy is
/// already alphabetical and covers every tag exactly once (both test-pinned in
/// `spec::grammar`), so this walks it in order; the signature and prose for each
/// tag come straight from the descriptor record, so nothing is re-encoded here.
/// No title line of its own — the banner printed by [`run`] already names it.
fn write_indicators<W: Write>(w: &mut W) -> io::Result<()> {
    use std::collections::HashMap;

    use crate::spec::grammar::{CATEGORIES, spec_grammar};

    let grammar = spec_grammar();
    let by_name: HashMap<&str, &_> = grammar.iter().map(|t| (t.name.as_str(), t)).collect();

    for (i, (label, tags)) in CATEGORIES.iter().enumerate() {
        if i > 0 {
            writeln!(w)?;
        }
        writeln!(w, "  {label}:")?;
        for name in *tags {
            // Present by construction — `categories_cover_every_tag_once` pins it.
            let tag = by_name[name];
            let sig = signature(tag);
            let doc = tag.doc.as_deref().unwrap_or("");
            writeln!(w, "    {sig:<52}  {doc}")?;
        }
    }
    Ok(())
}

/// Render a tag's YAML surface from its descriptor record. Parameterless leaves
/// (a `unit` tag, or a `map` whose keys are all omissible) parse as bare strings
/// and render without the `!`; everything else renders in its `!tag`-prefixed
/// form, with an optional `map` key marked by a trailing `?`.
fn signature(tag: &crate::spec::grammar::GrammarTag) -> String {
    match tag.shape.as_str() {
        // A candle leaf whose only key is the blessed `source:` selector
        // (`!close`, `!high`, …) reads best as the bare word — that's how it's
        // written 99% of the time. Anything with a real parameter shows its
        // body, optional keys marked `?`.
        "map" if tag.fields.iter().all(|f| f.name == "source") => tag.name.clone(),
        "map" => {
            let body: Vec<String> = tag
                .fields
                .iter()
                .map(|f| if f.required { f.name.clone() } else { format!("{}?", f.name) })
                .collect();
            format!("!{} {{ {} }}", tag.name, body.join(", "))
        }
        "newtype" => format!("!{} {}", tag.name, payload_form(tag.payload.as_deref())),
        "seq" => format!("!{} {}", tag.name, seq_form(tag.payload.as_deref())),
        // `unit` and anything unforeseen: the bare word.
        _ => tag.name.clone(),
    }
}

/// The placeholder for a `newtype` tag's single positional value, by payload type.
fn payload_form(payload: Option<&str>) -> &'static str {
    match payload {
        Some("node") => "<source>",
        Some("literal") => "<value>",
        Some("uint") => "<n>",
        Some("number") => "<x>",
        Some("str") | Some("str_operand") => "<name>",
        _ => "<…>",
    }
}

/// The placeholder for a `seq` tag's list body, by element/list payload type.
fn seq_form(payload: Option<&str>) -> &'static str {
    match payload {
        Some("str_list") => "[ SYM, … ]",
        Some("number_list") => "[ w0, … ]",
        _ => "[ … ]",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalogue is a projection of the descriptor now, so tag coverage,
    /// no-bogus-tags, and section order are all pinned once in `spec::grammar`
    /// (`categories_cover_every_tag_once` / `categories_are_alphabetical`). What
    /// remains list-specific is that the *rendering* drops nothing: every tag in
    /// the taxonomy must appear as a line, under its section header.
    #[test]
    fn the_output_renders_every_category_and_tag() {
        use crate::spec::grammar::{CATEGORIES, spec_grammar};

        let mut buf: Vec<u8> = Vec::new();
        write_indicators(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();

        for (label, _) in CATEGORIES {
            assert!(text.contains(&format!("  {label}:")), "missing section `{label}`");
        }
        // Every descriptor tag's name shows up somewhere in the rendered body.
        for tag in spec_grammar() {
            assert!(
                text.contains(tag.name.as_str()),
                "tag `{}` never rendered in `list indicators`",
                tag.name
            );
        }
    }

    /// The signature renderer's shape rules: a candle leaf is bare, an indicator
    /// shows its (optional-marked) params, and the load-time / seq forms carry a
    /// placeholder body.
    #[test]
    fn signatures_render_from_the_descriptor() {
        use crate::spec::grammar::spec_grammar;

        let grammar = spec_grammar();
        let sig = |name: &str| {
            signature(grammar.iter().find(|t| t.name == name).expect(name))
        };

        // `!close { source? }` collapses to the bare leaf; `!sma` shows params.
        assert_eq!(sig("close"), "close");
        assert_eq!(sig("sma"), "!sma { source?, period }");
        // All-defaulted params still render (not collapsed to bare).
        assert!(sig("bb_upper").starts_with("!bb_upper { "), "{}", sig("bb_upper"));
        // newtype / seq placeholders.
        assert_eq!(sig("import"), "!import <name>");
        assert_eq!(sig("fixed"), "!fixed [ w0, … ]");
        assert_eq!(sig("all_of"), "!all_of [ SYM, … ]");
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
        for p in KNOWN_PROVIDERS {
            let (name, doc) = (p.name, p.description);
            assert!(text.contains(name), "missing provider `{name}` in output");
            assert!(text.contains(doc), "missing description for `{name}` in output");
        }
    }

}
