//! Unicode-aware tokenizer with code-identifier splitting.
//!
//! **Atoms and positions.** Text is segmented with UAX #29 word boundaries
//! (`unicode-segmentation`). UAX #29 keeps "mid" punctuation inside a word
//! (`std.fs.read`, `don't`, `a:b`) — wrong for code — so each word is further
//! cut on punctuation. Combining marks and format characters are *not* cut
//! points, so `é` written as `e` + U+0301 stays one atom. The resulting runs
//! that contain at least one alphanumeric character are *atoms*; one position
//! is assigned per atom, in source order.
//!
//! **Identifier splitting (index side only).** An atom is additionally split on
//! `_` and on camelCase boundaries (`lower→Upper`, and `UPPER→Upper+lower` for
//! acronyms: `parseHTTPResponse` → `parse`, `HTTP`, `Response`). Digits stay
//! attached to the run they follow (`sha256`; `utf8Decode` → `utf8`, `decode`).
//! Parts are emitted **at the same position as the whole atom**.
//!
//! **The asymmetry, deliberately.** Parts exist for *single-term* matching:
//! the query `http` finds `parse_http_response` and `HttpResponse`. Phrases
//! operate on *atoms*: `"fs read"` matches `std.fs.read` because those are
//! consecutive atoms, and `"response bar"` matches `HttpResponse bar` because
//! the part shares its atom's position — but `"parse http"` does **not** match
//! `parse_http`, since both parts sit at one position. The query side uses
//! [`atoms`] (same segmentation, no splitting), so `parse_http` typed as a
//! query is one atom and matches the whole identifier exactly. `parse_http`
//! and `"parse http"` are therefore different queries; that is the price of
//! phrases that span an identifier and its neighbour, which is the common
//! case in code search.
//!
//! **Folding.** Terms are Unicode-lowercased. No stemming, no stop words.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::ops::Range;

use unicode_segmentation::{UWordBoundIndices, UnicodeSegmentation};

/// A term occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token<'a> {
    /// Lowercased term. Borrowed when the source atom was already lowercase.
    pub term: Cow<'a, str>,
    /// 0-based atom index within the document.
    pub position: u32,
    /// Byte range of the source *atom* in the input (the whole atom, even for
    /// an identifier part). Used by `snippet` for highlighting.
    pub span: Range<usize>,
}

/// Iterator returned by [`tokenize`] / [`atoms`].
pub struct Tokens<'a> {
    words: UWordBoundIndices<'a>,
    /// Unconsumed remainder of the current UAX #29 word: (absolute byte offset, text).
    word: Option<(usize, &'a str)>,
    /// Identifier parts of the current atom still to emit.
    pending: VecDeque<Token<'a>>,
    next_position: u32,
    split_identifiers: bool,
}

impl<'a> Tokens<'a> {
    fn new(text: &'a str, split_identifiers: bool) -> Self {
        Self {
            words: text.split_word_bound_indices(),
            word: None,
            pending: VecDeque::new(),
            next_position: 0,
            split_identifiers,
        }
    }

    /// Next atom as `(absolute byte offset, text)`.
    fn next_atom(&mut self) -> Option<(usize, &'a str)> {
        loop {
            if let Some((base, rest)) = self.word.take() {
                // Skip leading cut characters.
                let start = match rest.char_indices().find(|&(_, c)| !is_cut(c)) {
                    Some((i, _)) => i,
                    None => continue, // word exhausted; fetch the next one
                };
                let run = &rest[start..];
                let end = run
                    .char_indices()
                    .find(|&(_, c)| is_cut(c))
                    .map(|(i, _)| i)
                    .unwrap_or(run.len());
                let (atom, remainder) = run.split_at(end);
                if !remainder.is_empty() {
                    self.word = Some((base + start + end, remainder));
                }
                if atom.chars().any(char::is_alphanumeric) {
                    return Some((base + start, atom));
                }
                continue;
            }
            let (off, w) = self.words.next()?;
            self.word = Some((off, w));
        }
    }
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        if let Some(t) = self.pending.pop_front() {
            return Some(t);
        }
        let (start, atom) = self.next_atom()?;
        let position = self.next_position;
        self.next_position += 1;
        let span = start..start + atom.len();
        let whole = fold(atom);
        if self.split_identifiers {
            for part in identifier_parts(atom) {
                let term = fold(part);
                if term != whole && !self.pending.iter().any(|t| t.term == term) {
                    self.pending.push_back(Token { term, position, span: span.clone() });
                }
            }
        }
        Some(Token { term: whole, position, span })
    }
}

/// Index-side tokenization: every atom plus its identifier parts.
pub fn tokenize(text: &str) -> Tokens<'_> {
    Tokens::new(text, true)
}

/// Query-side tokenization: whole atoms only, no identifier splitting.
/// Atom boundaries and positions are identical to [`tokenize`]'s.
pub fn atoms(text: &str) -> Tokens<'_> {
    Tokens::new(text, false)
}

/// Unicode-lowercase, borrowing when nothing changes.
fn fold(s: &str) -> Cow<'_, str> {
    if s.chars().any(char::is_uppercase) {
        Cow::Owned(s.to_lowercase())
    } else {
        Cow::Borrowed(s)
    }
}

/// Characters that split a UAX #29 word into atoms: punctuation that UAX #29
/// leaves inside words (its MidLetter / MidNum / MidNumLet / quote classes),
/// plus all other ASCII punctuation except `_`. Never letters, digits,
/// combining marks or format characters.
fn is_cut(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_punctuation() && c != '_';
    }
    matches!(
        c,
        // MidLetter
        '\u{00B7}' | '\u{0387}' | '\u{055F}' | '\u{05F4}' | '\u{2027}' | '\u{FE13}' | '\u{FE55}' | '\u{FF1A}'
        // MidNumLet
        | '\u{2018}' | '\u{2019}' | '\u{2024}' | '\u{FE52}' | '\u{FF07}' | '\u{FF0E}'
        // MidNum
        | '\u{037E}' | '\u{0589}' | '\u{060C}' | '\u{060D}' | '\u{066C}' | '\u{07F8}' | '\u{2044}'
        | '\u{FE10}' | '\u{FE14}' | '\u{FE50}' | '\u{FE54}' | '\u{FF0C}' | '\u{FF1B}'
    )
}

/// Split one atom into identifier parts (original case, not lowercased).
///
/// Returns an empty Vec when there is nothing to add: the atom is a single
/// part identical to itself (`sha256`, `Foo`, `HTTP`). `snake_case` →
/// `["snake", "case"]`; `parseHTTPResponse` → `["parse", "HTTP", "Response"]`;
/// `utf8Decode` → `["utf8", "Decode"]`; `__init__` → `["init"]`.
pub fn identifier_parts(atom: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = Vec::new();
    for seg in atom.split('_').filter(|s| !s.is_empty()) {
        split_camel(seg, &mut parts);
    }
    if parts.len() == 1 && parts[0].len() == atom.len() {
        parts.clear();
    }
    parts
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Upper,
    Lower,
    Digit,
    /// Combining marks, format chars — continue whatever run we are in.
    Other,
}

fn class(c: char) -> Class {
    if c.is_uppercase() {
        Class::Upper
    } else if c.is_numeric() {
        Class::Digit
    } else if c.is_alphabetic() {
        // Lowercase, or caseless scripts (Han, Thai, ...): never a boundary.
        Class::Lower
    } else {
        Class::Other
    }
}

/// camelCase split of an underscore-free segment, appending to `out`.
fn split_camel<'a>(seg: &'a str, out: &mut Vec<&'a str>) {
    let chars: Vec<(usize, char)> = seg.char_indices().collect();
    let mut start = 0;
    for i in 1..chars.len() {
        let (off, c) = chars[i];
        if class(c) != Class::Upper {
            continue;
        }
        let prev = class(chars[i - 1].1);
        // lower→Upper, digit→Upper: `parseHttp`, `utf8Decode`.
        let after_non_upper = prev != Class::Upper && prev != Class::Other;
        // UPPER→Upper+lower: the last capital of an acronym starts the next word.
        let acronym_end = prev == Class::Upper
            && chars.get(i + 1).is_some_and(|&(_, n)| class(n) == Class::Lower);
        if after_non_upper || acronym_end {
            out.push(&seg[start..off]);
            start = off;
        }
    }
    out.push(&seg[start..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(text: &str) -> Vec<(String, u32)> {
        tokenize(text).map(|t| (t.term.into_owned(), t.position)).collect()
    }

    fn just_terms(text: &str) -> Vec<String> {
        tokenize(text).map(|t| t.term.into_owned()).collect()
    }

    #[test]
    fn plain_words_get_consecutive_positions() {
        assert_eq!(
            terms("the quick brown fox"),
            vec![("the".into(), 0), ("quick".into(), 1), ("brown".into(), 2), ("fox".into(), 3)]
        );
    }

    #[test]
    fn lowercases() {
        assert_eq!(just_terms("Hello WORLD Mixed"), vec!["hello", "world", "mixed"]);
    }

    #[test]
    fn snake_case_emits_whole_then_parts_at_same_position() {
        assert_eq!(
            terms("snake_case_name"),
            vec![("snake_case_name".into(), 0), ("snake".into(), 0), ("case".into(), 0), ("name".into(), 0)]
        );
    }

    #[test]
    fn camel_case() {
        assert_eq!(just_terms("parseHttpResponse"), vec!["parsehttpresponse", "parse", "http", "response"]);
        assert_eq!(just_terms("XMLHttpRequest"), vec!["xmlhttprequest", "xml", "http", "request"]);
        assert_eq!(just_terms("getX"), vec!["getx", "get", "x"]);
        assert_eq!(just_terms("iOS"), vec!["ios", "i", "os"]);
    }

    #[test]
    fn acronyms_and_digits() {
        assert_eq!(just_terms("parseHTTPResponse"), vec!["parsehttpresponse", "parse", "http", "response"]);
        assert_eq!(just_terms("utf8Decode"), vec!["utf8decode", "utf8", "decode"]);
        assert_eq!(just_terms("parseHTTP2Response"), vec!["parsehttp2response", "parse", "http2", "response"]);
        assert_eq!(just_terms("sha256"), vec!["sha256"]);
        assert_eq!(just_terms("base64url"), vec!["base64url"]);
    }

    #[test]
    fn single_part_identifiers_are_not_duplicated() {
        assert_eq!(just_terms("Foo"), vec!["foo"]);
        assert_eq!(just_terms("HTTP"), vec!["http"]);
        assert_eq!(just_terms("foo"), vec!["foo"]);
    }

    #[test]
    fn leading_and_trailing_underscores() {
        assert_eq!(just_terms("__init__"), vec!["__init__", "init"]);
        assert_eq!(just_terms("_private"), vec!["_private", "private"]);
        assert_eq!(just_terms("___"), Vec::<String>::new());
    }

    #[test]
    fn repeated_parts_are_emitted_once() {
        assert_eq!(just_terms("a_a"), vec!["a_a", "a"]);
        assert_eq!(just_terms("Foo_foo"), vec!["foo_foo", "foo"]);
    }

    #[test]
    fn mixed_snake_and_camel() {
        assert_eq!(
            just_terms("read_HttpHeader_v2"),
            vec!["read_httpheader_v2", "read", "http", "header", "v2"]
        );
    }

    #[test]
    fn punctuation_splits_atoms_and_advances_positions() {
        assert_eq!(
            terms("std::fs::read_to_string(path)"),
            vec![
                ("std".into(), 0),
                ("fs".into(), 1),
                ("read_to_string".into(), 2),
                ("read".into(), 2),
                ("to".into(), 2),
                ("string".into(), 2),
                ("path".into(), 3),
            ]
        );
        assert_eq!(just_terms("a.b.c"), vec!["a", "b", "c"]);
        assert_eq!(just_terms("don't"), vec!["don", "t"]);
        assert_eq!(just_terms("x-y"), vec!["x", "y"]);
        assert_eq!(just_terms("foo@bar.com"), vec!["foo", "bar", "com"]);
        assert_eq!(just_terms("3.14"), vec!["3", "14"]);
    }

    #[test]
    fn pure_punctuation_produces_nothing() {
        assert_eq!(just_terms("... --- !!! ;;; ()[]{}"), Vec::<String>::new());
        assert_eq!(just_terms(""), Vec::<String>::new());
        assert_eq!(just_terms("   \n\t "), Vec::<String>::new());
    }

    #[test]
    fn code_line() {
        assert_eq!(
            just_terms("let x = Vec::<u8>::with_capacity(16); // comment"),
            vec!["let", "x", "vec", "u8", "with_capacity", "with", "capacity", "16", "comment"]
        );
    }

    #[test]
    fn unicode_letters_and_folding() {
        assert_eq!(just_terms("naïve café Ünïcödé"), vec!["naïve", "café", "ünïcödé"]);
        assert_eq!(just_terms("ΑΒΓ Straße"), vec!["αβγ", "straße"]);
        assert_eq!(just_terms("Привет_мир"), vec!["привет_мир", "привет", "мир"]);
    }

    #[test]
    fn unicode_camel_case() {
        assert_eq!(just_terms("überFoo"), vec!["überfoo", "über", "foo"]);
        assert_eq!(just_terms("ПриветМир"), vec!["приветмир", "привет", "мир"]);
    }

    #[test]
    fn combining_marks_stay_attached() {
        // "e" + COMBINING ACUTE ACCENT — not a cut point, not a case boundary.
        assert_eq!(just_terms("cafe\u{301} ok"), vec!["cafe\u{301}", "ok"]);
    }

    #[test]
    fn cjk_and_mixed_scripts() {
        // UAX #29 treats each Han character as its own word.
        assert_eq!(just_terms("日本語 text"), vec!["日", "本", "語", "text"]);
        // Hangul syllables join into one word.
        assert_eq!(just_terms("한국어"), vec!["한국어"]);
    }

    #[test]
    fn emoji_and_symbols_are_skipped() {
        assert_eq!(just_terms("hello 👋 world ✓"), vec!["hello", "world"]);
    }

    #[test]
    fn spans_cover_the_whole_atom_for_parts() {
        let toks: Vec<Token> = tokenize("  fooBar baz").collect();
        assert_eq!(toks[0].term, "foobar");
        assert_eq!(toks[0].span, 2..8);
        assert_eq!(toks[1].term, "foo");
        assert_eq!(toks[1].span, 2..8);
        assert_eq!(toks[2].term, "bar");
        assert_eq!(toks[2].span, 2..8);
        assert_eq!(toks[3].term, "baz");
        assert_eq!(toks[3].span, 9..12);
    }

    #[test]
    fn spans_are_correct_with_multibyte_prefix() {
        let text = "日本 Ünï_x";
        let toks: Vec<Token> = tokenize(text).collect();
        let atom = &toks[2];
        assert_eq!(atom.term, "ünï_x");
        assert_eq!(&text[atom.span.clone()], "Ünï_x");
    }

    #[test]
    fn borrowed_when_already_lowercase() {
        let toks: Vec<Token> = tokenize("lower Upper").collect();
        assert!(matches!(toks[0].term, Cow::Borrowed(_)));
        assert!(matches!(toks[1].term, Cow::Owned(_)));
    }

    #[test]
    fn atoms_matches_tokenize_minus_parts() {
        // The query path must segment exactly like the index path.
        let inputs = [
            "snake_case parseHttp foo.bar std::fs::read",
            "naïve_café ÜberFoo__x 3.14 a-b-c",
            "日本語 한국어 Привет_мир cafe\u{301}",
            "__init__ ___ !!! x",
        ];
        for text in inputs {
            let from_atoms: Vec<(String, u32, Range<usize>)> =
                atoms(text).map(|t| (t.term.into_owned(), t.position, t.span)).collect();
            // Whole-atom tokens from `tokenize` are exactly the first token at each position.
            let mut seen = std::collections::HashSet::new();
            let wholes: Vec<(String, u32, Range<usize>)> = tokenize(text)
                .filter(|t| seen.insert(t.position))
                .map(|t| (t.term.into_owned(), t.position, t.span))
                .collect();
            assert_eq!(from_atoms, wholes, "input: {text:?}");
        }
        let q: Vec<String> = atoms("snake_case").map(|t| t.term.into_owned()).collect();
        assert_eq!(q, vec!["snake_case"]);
    }

    #[test]
    fn identifier_parts_direct() {
        assert_eq!(identifier_parts("snake_case"), vec!["snake", "case"]);
        assert_eq!(identifier_parts("parseHTTPResponse"), vec!["parse", "HTTP", "Response"]);
        assert_eq!(identifier_parts("utf8Decode"), vec!["utf8", "Decode"]);
        assert_eq!(identifier_parts("__init__"), vec!["init"]);
        assert_eq!(identifier_parts("sha256"), Vec::<&str>::new());
        assert_eq!(identifier_parts("Foo"), Vec::<&str>::new());
        assert_eq!(identifier_parts("HTTP"), Vec::<&str>::new());
    }
}
