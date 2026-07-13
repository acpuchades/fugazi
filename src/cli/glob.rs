//! Shell-style glob matching for CLI filters (`fugazi list tickers binance 'b*'`).
//!
//! Deliberately hand-rolled rather than pulling in a glob crate, or translating
//! to a regex and leaning on `regex`. The latter is the obvious move — `*` →
//! `.*`, `?` → `.`, escape the rest — but `regex` is not in the CLI's tree (the
//! `regex-syntax` that shows up in `cargo tree` is a *build*-time dep of a
//! proc-macro under `interim`), so it would mean adding `regex` +
//! `regex-automata` + `aho-corasick` at runtime. And it saves less than it
//! looks: the naive translation only holds if you drop `[…]` classes — keeping
//! them means parsing the class anyway (to map `!` → `^` and escape the body),
//! leaving just the ~25-line scan below to delete. Both approaches are linear
//! over a few thousand in-memory symbols, so there is no speed argument either.
//!
//! The alphabet is the familiar one:
//!
//! | pattern | matches |
//! |---|---|
//! | `*` | any run of characters, including none |
//! | `?` | exactly one character |
//! | `[abc]` / `[a-z]` | one character from the set/range |
//! | `[!abc]` / `[^abc]` | one character *not* in the set |
//! | `\*` | a literal `*` (same for `?`, `[`, `\`) |
//!
//! **Matching is case-insensitive and whole-string.** Case-insensitive because
//! the thing being filtered is an identifier whose casing is the provider's
//! choice, not the user's — Binance shouts `BTCUSDT` and CoinGecko whispers
//! `bitcoin`, and a user typing `b*` means the same thing to both. Whole-string
//! (rather than "contains") because that is what a glob means everywhere else a
//! shell user meets one; a substring search is spelled `*btc*`.

use std::fmt;
use std::str::FromStr;

/// One element of a parsed [`Pattern`].
#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// `*` — any run of characters, including empty.
    Any,
    /// `?` — exactly one character.
    One,
    /// A literal character (already lowercased).
    Char(char),
    /// `[…]` — one character from (or, when negated, outside) the set.
    Class { negated: bool, items: Vec<ClassItem> },
}

/// One member of a `[…]` class: a single character or an inclusive range.
#[derive(Debug, Clone, PartialEq)]
enum ClassItem {
    Char(char),
    Range(char, char),
}

impl ClassItem {
    fn contains(&self, c: char) -> bool {
        match self {
            ClassItem::Char(x) => *x == c,
            ClassItem::Range(lo, hi) => (*lo..=*hi).contains(&c),
        }
    }
}

/// A compiled glob pattern. Parse once (`"b*".parse()`), match many.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    tokens: Vec<Token>,
    /// The pattern as written — for error messages and "no matches" reports.
    source: String,
}

impl Pattern {
    /// Whether `text` matches the whole pattern, ignoring case.
    pub fn matches(&self, text: &str) -> bool {
        let text: Vec<char> = text.to_lowercase().chars().collect();
        matches_tokens(&self.tokens, &text)
    }

    /// Whether the pattern is pinned at both ends — i.e. neither begins nor
    /// ends with `*`, so it can't match a symbol with characters on either
    /// side of it. Callers use this to decide whether "did you mean the
    /// substring form `*p*`?" is worth saying: to someone who already typed
    /// `b*`, it isn't.
    pub fn is_anchored(&self) -> bool {
        self.tokens.first() != Some(&Token::Any) && self.tokens.last() != Some(&Token::Any)
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

impl FromStr for Pattern {
    type Err = String;

    /// Parse the glob. The only way to fail is an unterminated `[`, which is
    /// almost always a forgotten `]` rather than an intended literal — saying so
    /// beats silently matching nothing.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Lowercase up front so both the pattern's literals and (in `matches`)
        // the candidate are compared in one case.
        let chars: Vec<char> = s.to_lowercase().chars().collect();
        let mut tokens = Vec::new();
        let mut i = 0;

        while i < chars.len() {
            match chars[i] {
                '*' => {
                    // Collapse `**` — it means exactly what `*` does, and the
                    // matcher's backtracking is simpler with no empty runs.
                    if tokens.last() != Some(&Token::Any) {
                        tokens.push(Token::Any);
                    }
                    i += 1;
                }
                '?' => {
                    tokens.push(Token::One);
                    i += 1;
                }
                '\\' if i + 1 < chars.len() => {
                    tokens.push(Token::Char(chars[i + 1]));
                    i += 2;
                }
                '[' => {
                    let (token, next) = parse_class(&chars, i, s)?;
                    tokens.push(token);
                    i = next;
                }
                c => {
                    tokens.push(Token::Char(c));
                    i += 1;
                }
            }
        }

        Ok(Pattern {
            tokens,
            source: s.to_string(),
        })
    }
}

/// Parse a `[…]` class starting at `open` (which indexes the `[`). Returns the
/// token and the index just past the closing `]`.
fn parse_class(chars: &[char], open: usize, source: &str) -> Result<(Token, usize), String> {
    let mut i = open + 1;
    let negated = matches!(chars.get(i), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let mut items = Vec::new();
    // A `]` in first position is a literal `]`, per the shell convention.
    if chars.get(i) == Some(&']') {
        items.push(ClassItem::Char(']'));
        i += 1;
    }

    while let Some(&c) = chars.get(i) {
        if c == ']' {
            return Ok((Token::Class { negated, items }, i + 1));
        }
        // `a-z`, but a trailing `-` (as in `[a-]`) is a literal `-`.
        if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|&e| e != ']') {
            let end = chars[i + 2];
            if end < c {
                return Err(format!(
                    "invalid pattern `{source}`: range `{c}-{end}` runs backwards"
                ));
            }
            items.push(ClassItem::Range(c, end));
            i += 3;
        } else {
            items.push(ClassItem::Char(c));
            i += 1;
        }
    }

    Err(format!(
        "invalid pattern `{source}`: unterminated `[` (add a closing `]`, or \
         write `\\[` for a literal bracket)"
    ))
}

/// Whether `text` matches `tokens` in full.
///
/// The classic linear two-pointer scan: walk both, and on a mismatch fall back
/// to the most recent `*` and let it swallow one more character. `*` is the only
/// token that can backtrack, so one saved position is enough — no recursion, and
/// no pathological blowup on a pattern like `*a*a*a*`.
fn matches_tokens(tokens: &[Token], text: &[char]) -> bool {
    let (mut ti, mut si) = (0usize, 0usize);
    // (token index of the last `*`, the text position it is currently absorbing up to)
    let mut star: Option<(usize, usize)> = None;

    while si < text.len() {
        match tokens.get(ti) {
            Some(Token::Any) => {
                star = Some((ti, si));
                ti += 1;
            }
            Some(token) if matches_one(token, text[si]) => {
                ti += 1;
                si += 1;
            }
            // Mismatch (or pattern exhausted with text left over): give the last
            // `*` one more character, or fail if there was none.
            _ => match star {
                Some((star_ti, star_si)) => {
                    star = Some((star_ti, star_si + 1));
                    ti = star_ti + 1;
                    si = star_si + 1;
                }
                None => return false,
            },
        }
    }

    // Text exhausted: any trailing `*`s may match empty, but nothing else can.
    tokens[ti..].iter().all(|t| *t == Token::Any)
}

/// Whether a single non-`*` token matches one character.
fn matches_one(token: &Token, c: char) -> bool {
    match token {
        Token::One => true,
        Token::Char(x) => *x == c,
        Token::Class { negated, items } => {
            let hit = items.iter().any(|item| item.contains(c));
            hit != *negated
        }
        Token::Any => unreachable!("`*` is handled by the caller's backtracking"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pattern: &str) -> Pattern {
        pattern.parse().unwrap()
    }

    #[test]
    fn star_matches_a_prefix_a_suffix_and_a_substring() {
        assert!(p("b*").matches("BTCUSDT"));
        assert!(!p("b*").matches("ETHUSDT"));

        assert!(p("*usdt").matches("BTCUSDT"));
        assert!(!p("*usdt").matches("BTCUSDC"));

        assert!(p("*b*").matches("ETHBTC"));
        assert!(p("*b*").matches("BTCUSDT"));
        assert!(!p("*b*").matches("ETHUSDT"));
    }

    #[test]
    fn matching_is_case_insensitive_both_ways() {
        // The provider picks the casing (Binance shouts, CoinGecko whispers);
        // the user shouldn't have to care.
        assert!(p("b*").matches("BTCUSDT"));
        assert!(p("B*").matches("bitcoin"));
        assert!(p("*USD*").matches("btcusdt"));
    }

    #[test]
    fn matching_is_whole_string_not_substring() {
        // A bare word is an exact match — `*btc*` is how you spell "contains".
        assert!(p("btc").matches("BTC"));
        assert!(!p("btc").matches("BTCUSDT"));
    }

    #[test]
    fn question_mark_matches_exactly_one_character() {
        assert!(p("?tc*").matches("BTCUSDT"));
        assert!(!p("?tc").matches("BTCUSDT"));
        assert!(p("btc???t").matches("BTCUSDT"));
        assert!(!p("btc??t").matches("BTCUSDT"));
    }

    #[test]
    fn character_classes_match_sets_ranges_and_negations() {
        assert!(p("[be]*").matches("BTCUSDT"));
        assert!(p("[be]*").matches("ETHUSDT"));
        assert!(!p("[be]*").matches("SOLUSDT"));

        assert!(p("[a-c]*").matches("BTCUSDT"));
        assert!(!p("[a-c]*").matches("ETHUSDT"));

        assert!(p("[!b]*").matches("ETHUSDT"));
        assert!(!p("[!b]*").matches("BTCUSDT"));
        assert!(p("[^b]*").matches("ETHUSDT"));
    }

    #[test]
    fn a_backslash_escapes_a_metacharacter() {
        assert!(p(r"a\*b").matches("a*b"));
        assert!(!p(r"a\*b").matches("axxb"));
    }

    #[test]
    fn interior_stars_match_in_sequence() {
        // `foo*bar*baz`: the literals must appear in order, with anything (or
        // nothing) between them.
        let pattern = p("foo*bar*baz");
        assert!(pattern.matches("fooXbarYbaz"));
        assert!(pattern.matches("foobarbaz")); // each `*` may absorb nothing
        assert!(pattern.matches("foo-----bar-----baz"));
        assert!(!pattern.matches("foobaz")); // `bar` is missing
        assert!(!pattern.matches("foobazbar")); // out of order
        assert!(!pattern.matches("fooXbarYbazZ")); // `baz` must end the string
        assert!(p("foo*bar*baz*").matches("fooXbarYbazZ")); // …unless a `*` follows

        // The backtracking case that a greedy first-match would get wrong: the
        // first `bar` is a dead end, and the matcher has to find the second.
        assert!(p("*bar*baz").matches("bar-then-bar-and-baz"));

        // A real one: every Binance perp of a coin whose name has `usd` inside.
        assert!(p("*usd*t").matches("BFUSDUSDT"));
    }

    #[test]
    fn star_backtracks_without_blowing_up() {
        // The shape that kills a naive recursive matcher.
        let pattern = p("*a*a*a*a*a*a*b");
        assert!(!pattern.matches(&"a".repeat(64)));
        assert!(pattern.matches(&format!("{}b", "a".repeat(64))));
    }

    #[test]
    fn a_bare_star_matches_everything_including_empty() {
        assert!(p("*").matches(""));
        assert!(p("*").matches("BTCUSDT"));
        assert!(p("**").matches("BTCUSDT"));
        assert!(p("").matches(""));
        assert!(!p("").matches("BTC"));
    }

    #[test]
    fn anchoredness_reports_whether_a_substring_hint_would_help() {
        assert!(p("btc").is_anchored());
        assert!(p("btc?usdt").is_anchored());
        assert!(!p("b*").is_anchored());
        assert!(!p("*btc").is_anchored());
        assert!(!p("*btc*").is_anchored());
    }

    #[test]
    fn unterminated_class_is_a_parse_error_that_says_so() {
        let err = "[abc".parse::<Pattern>().unwrap_err();
        assert!(err.contains("unterminated"), "{err}");
    }

    #[test]
    fn backwards_range_is_a_parse_error() {
        let err = "[z-a]*".parse::<Pattern>().unwrap_err();
        assert!(err.contains("backwards"), "{err}");
    }

    #[test]
    fn non_ascii_tickers_match_by_character_not_byte() {
        // Binance really lists this one; `?` counts characters, not bytes.
        assert!(p("币*").matches("币安人生USDT"));
        assert!(p("????usdt").matches("币安人生USDT"));
    }
}
