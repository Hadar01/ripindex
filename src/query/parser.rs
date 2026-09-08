//! Query syntax -> [`Node`] AST.
//!
//! Grammar (lowest precedence first; adjacency is AND):
//!
//! ```text
//! query  := and ( "OR" and )*
//! and    := unary ( "AND"? unary )+
//! unary  := "-" atom | atom
//! atom   := "(" query ")" | '"' WORD* '"' | WORD
//! ```
//!
//! * `AND` / `OR` are case-sensitive keywords; lowercase `and` / `or` are terms.
//! * Words are run through `tokenizer::atoms` (lowercased, split on punctuation,
//!   no identifier splitting — see the tokenizer docs for why). A bare WORD
//!   that yields several atoms — `foo.bar`, `std::fs::read`, `x-y` — becomes a
//!   phrase of those atoms, since that is what it was in the source. A quoted
//!   phrase with a single atom is just a term.
//! * Negation is only meaningful next to something positive, so it lives inside
//!   `And { must_not }`. A query with no positive clause — `-foo`, or
//!   `a OR -b` — is rejected with `QueryError::OnlyNegation`.
//! * `-` is negation only when it directly prefixes an operand (`-foo`,
//!   `-"a b"`, `-(a b)`). Inside a word (`foo-bar`) it is punctuation; on its
//!   own it is ignored.

use crate::error::QueryError;
use crate::tokenizer::atoms;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// Single lowercased whole-atom term.
    Term(String),
    /// Two or more terms that must appear at consecutive positions.
    Phrase(Vec<String>),
    /// Every `must` matches and no `must_not` matches. `must` is non-empty.
    And { must: Vec<Node>, must_not: Vec<Node> },
    /// At least one child matches. Every child has a positive clause.
    Or(Vec<Node>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word(String),
    Phrase(String),
    And,
    Or,
    Minus,
    LParen,
    RParen,
}

impl Tok {
    fn describe(&self) -> String {
        match self {
            Tok::Word(w) => w.clone(),
            Tok::Phrase(p) => format!("\"{p}\""),
            Tok::And => "AND".into(),
            Tok::Or => "OR".into(),
            Tok::Minus => "-".into(),
            Tok::LParen => "(".into(),
            Tok::RParen => ")".into(),
        }
    }
}

/// Tokens with their byte offsets.
fn lex(src: &str) -> Result<Vec<(usize, Tok)>, QueryError> {
    let mut toks = Vec::new();
    let mut chars = src.char_indices().peekable();
    while let Some(&(i, c)) = chars.peek() {
        match c {
            _ if c.is_whitespace() => {
                chars.next();
            }
            '(' => {
                toks.push((i, Tok::LParen));
                chars.next();
            }
            ')' => {
                toks.push((i, Tok::RParen));
                chars.next();
            }
            '"' => {
                chars.next();
                let start = i + 1;
                let end = chars
                    .by_ref()
                    .find(|&(_, d)| d == '"')
                    .map(|(j, _)| j)
                    .ok_or(QueryError::UnterminatedPhrase { at: i })?;
                toks.push((i, Tok::Phrase(src[start..end].to_string())));
            }
            '-' => {
                chars.next();
                // Negation only when something follows directly; a lone `-` is noise.
                if chars.peek().is_some_and(|&(_, d)| !d.is_whitespace()) {
                    toks.push((i, Tok::Minus));
                }
            }
            _ => {
                let start = i;
                let mut end = src.len();
                while let Some(&(j, d)) = chars.peek() {
                    if d.is_whitespace() || matches!(d, '(' | ')' | '"') {
                        end = j;
                        break;
                    }
                    chars.next();
                }
                let word = &src[start..end];
                let tok = match word {
                    "AND" => Tok::And,
                    "OR" => Tok::Or,
                    _ => Tok::Word(word.to_string()),
                };
                toks.push((start, tok));
            }
        }
    }
    Ok(toks)
}

/// Recursive-descent parser over the lexer's tokens.
struct Parser {
    toks: Vec<(usize, Tok)>,
    idx: usize,
}

impl Parser {
    fn peek(&self) -> Option<&(usize, Tok)> {
        self.toks.get(self.idx)
    }

    fn bump(&mut self) -> Option<(usize, Tok)> {
        let t = self.toks.get(self.idx).cloned();
        if t.is_some() {
            self.idx += 1;
        }
        t
    }

    /// Whether the next token can begin an operand.
    fn starts_operand(&self) -> bool {
        matches!(self.peek(), Some((_, Tok::Word(_) | Tok::Phrase(_) | Tok::Minus | Tok::LParen)))
    }

    fn parse_or(&mut self) -> Result<Node, QueryError> {
        let mut children = vec![self.parse_and()?];
        while matches!(self.peek(), Some((_, Tok::Or))) {
            let (at, _) = self.bump().unwrap();
            if !self.starts_operand() {
                return Err(QueryError::Unexpected { found: "OR".into(), at });
            }
            children.push(self.parse_and()?);
        }
        Ok(if children.len() == 1 { children.pop().unwrap() } else { Node::Or(children) })
    }

    fn parse_and(&mut self) -> Result<Node, QueryError> {
        let mut must = Vec::new();
        let mut must_not = Vec::new();
        loop {
            match self.peek() {
                None | Some((_, Tok::Or | Tok::RParen)) => break,
                Some((_, Tok::And)) => {
                    let (at, _) = self.bump().unwrap();
                    let leading = must.is_empty() && must_not.is_empty();
                    if leading || !self.starts_operand() {
                        return Err(QueryError::Unexpected { found: "AND".into(), at });
                    }
                }
                Some(_) => {
                    if let Some((node, negated)) = self.parse_unary()? {
                        if negated {
                            must_not.push(node);
                        } else {
                            must.push(node);
                        }
                    }
                }
            }
        }
        if must.is_empty() {
            if !must_not.is_empty() {
                return Err(QueryError::OnlyNegation);
            }
            // Nothing at all: `()`, `foo OR )`, or words with no atoms (`!!!`).
            return Err(match self.peek() {
                Some((at, tok)) => QueryError::Unexpected { found: tok.describe(), at: *at },
                None => QueryError::Empty,
            });
        }
        Ok(if must.len() == 1 && must_not.is_empty() {
            must.pop().unwrap()
        } else {
            Node::And { must, must_not }
        })
    }

    /// `Ok(None)` for an operand that produced no terms (punctuation-only word,
    /// empty phrase); callers skip it.
    fn parse_unary(&mut self) -> Result<Option<(Node, bool)>, QueryError> {
        let negated = matches!(self.peek(), Some((_, Tok::Minus)));
        if negated {
            self.bump();
        }
        Ok(self.parse_atom()?.map(|n| (n, negated)))
    }

    fn parse_atom(&mut self) -> Result<Option<Node>, QueryError> {
        let (at, tok) = self.bump().ok_or(QueryError::Empty)?;
        match tok {
            Tok::LParen => {
                let node = self.parse_or()?;
                match self.bump() {
                    Some((_, Tok::RParen)) => Ok(Some(node)),
                    _ => Err(QueryError::UnbalancedParen { at }),
                }
            }
            Tok::Word(w) => Ok(text_to_node(&w)),
            Tok::Phrase(p) => Ok(text_to_node(&p)),
            other => Err(QueryError::Unexpected { found: other.describe(), at }),
        }
    }
}

/// Parse a query string. Empty / whitespace-only input is `QueryError::Empty`.
pub fn parse(input: &str) -> Result<Node, QueryError> {
    let toks = lex(input)?;
    if toks.is_empty() {
        return Err(QueryError::Empty);
    }
    let mut p = Parser { toks, idx: 0 };
    let node = p.parse_or()?;
    if let Some((at, tok)) = p.peek() {
        // parse_or only stops early on a `)` it did not open.
        return Err(match tok {
            Tok::RParen => QueryError::UnbalancedParen { at: *at },
            other => QueryError::Unexpected { found: other.describe(), at: *at },
        });
    }
    Ok(node)
}

/// Atoms of a word or quoted phrase: one → `Term`, several → `Phrase`,
/// none → `None`.
fn text_to_node(text: &str) -> Option<Node> {
    let mut terms: Vec<String> = atoms(text).map(|t| t.term.into_owned()).collect();
    match terms.len() {
        0 => None,
        1 => Some(Node::Term(terms.pop().unwrap())),
        _ => Some(Node::Phrase(terms)),
    }
}

#[cfg(test)]
mod tests {
    use super::Node::*;
    use super::*;

    fn t(s: &str) -> Node {
        Term(s.into())
    }
    fn ph(words: &[&str]) -> Node {
        Phrase(words.iter().map(|w| w.to_string()).collect())
    }
    fn and(must: Vec<Node>, must_not: Vec<Node>) -> Node {
        And { must, must_not }
    }

    #[test]
    fn single_term_is_lowercased() {
        assert_eq!(parse("foo").unwrap(), t("foo"));
        assert_eq!(parse("  Foo  ").unwrap(), t("foo"));
        assert_eq!(parse("Café").unwrap(), t("café"));
    }

    #[test]
    fn adjacency_and_explicit_and_are_the_same() {
        let expected = and(vec![t("foo"), t("bar")], vec![]);
        assert_eq!(parse("foo bar").unwrap(), expected);
        assert_eq!(parse("foo AND bar").unwrap(), expected);
        assert_eq!(parse("foo AND bar baz").unwrap(), and(vec![t("foo"), t("bar"), t("baz")], vec![]));
    }

    #[test]
    fn lowercase_keywords_are_terms() {
        assert_eq!(parse("foo and bar").unwrap(), and(vec![t("foo"), t("and"), t("bar")], vec![]));
        assert_eq!(parse("foo or bar").unwrap(), and(vec![t("foo"), t("or"), t("bar")], vec![]));
    }

    #[test]
    fn or_binds_looser_than_and() {
        assert_eq!(parse("foo OR bar").unwrap(), Or(vec![t("foo"), t("bar")]));
        assert_eq!(
            parse("a b OR c d OR e").unwrap(),
            Or(vec![and(vec![t("a"), t("b")], vec![]), and(vec![t("c"), t("d")], vec![]), t("e")])
        );
    }

    #[test]
    fn phrases() {
        assert_eq!(parse("\"hello world\"").unwrap(), ph(&["hello", "world"]));
        assert_eq!(parse("\"Hello, World!\"").unwrap(), ph(&["hello", "world"]));
        assert_eq!(parse("\"hello\"").unwrap(), t("hello"));
        assert_eq!(parse("\"hello world\" foo").unwrap(), and(vec![ph(&["hello", "world"]), t("foo")], vec![]));
        assert_eq!(parse("a\"b c\"d").unwrap(), and(vec![t("a"), ph(&["b", "c"]), t("d")], vec![]));
    }

    #[test]
    fn empty_phrase_is_skipped() {
        assert_eq!(parse("foo \"\" bar").unwrap(), and(vec![t("foo"), t("bar")], vec![]));
        assert_eq!(parse("\"\""), Err(QueryError::Empty));
        assert_eq!(parse("\"...\""), Err(QueryError::Empty));
    }

    #[test]
    fn punctuated_words_become_phrases_of_atoms() {
        assert_eq!(parse("std::fs::read").unwrap(), ph(&["std", "fs", "read"]));
        assert_eq!(parse("foo.bar").unwrap(), ph(&["foo", "bar"]));
        assert_eq!(parse("foo-bar").unwrap(), ph(&["foo", "bar"]));
        assert_eq!(parse("a@b.com").unwrap(), ph(&["a", "b", "com"]));
    }

    #[test]
    fn identifiers_are_single_atoms_on_the_query_side() {
        // Same segmentation as the index side, no part splitting.
        assert_eq!(parse("snake_case").unwrap(), t("snake_case"));
        assert_eq!(parse("parseHttpResponse").unwrap(), t("parsehttpresponse"));
        assert_eq!(parse("__init__").unwrap(), t("__init__"));
        assert_eq!(parse("\"parse http\"").unwrap(), ph(&["parse", "http"]));
    }

    #[test]
    fn negation() {
        assert_eq!(parse("foo -bar").unwrap(), and(vec![t("foo")], vec![t("bar")]));
        assert_eq!(parse("foo -bar -baz").unwrap(), and(vec![t("foo")], vec![t("bar"), t("baz")]));
        assert_eq!(parse("-bar foo").unwrap(), and(vec![t("foo")], vec![t("bar")]));
        assert_eq!(parse("foo -\"a b\"").unwrap(), and(vec![t("foo")], vec![ph(&["a", "b"])]));
        assert_eq!(parse("foo -(a OR b)").unwrap(), and(vec![t("foo")], vec![Or(vec![t("a"), t("b")])]));
        assert_eq!(parse("foo -bar-baz").unwrap(), and(vec![t("foo")], vec![ph(&["bar", "baz"])]));
        assert_eq!(parse("foo AND -bar").unwrap(), and(vec![t("foo")], vec![t("bar")]));
    }

    #[test]
    fn negation_needs_a_positive_clause() {
        assert_eq!(parse("-foo"), Err(QueryError::OnlyNegation));
        assert_eq!(parse("-foo -bar"), Err(QueryError::OnlyNegation));
        assert_eq!(parse("foo OR -bar"), Err(QueryError::OnlyNegation));
        assert_eq!(parse("(-foo) bar"), Err(QueryError::OnlyNegation));
        // ...but a negation inside a group that has its own positive is fine.
        assert_eq!(
            parse("(a -b) OR c").unwrap(),
            Or(vec![and(vec![t("a")], vec![t("b")]), t("c")])
        );
    }

    #[test]
    fn lone_minus_is_noise_and_double_minus_is_an_error() {
        assert_eq!(parse("foo - bar").unwrap(), and(vec![t("foo"), t("bar")], vec![]));
        assert_eq!(parse("foo -").unwrap(), t("foo"));
        assert_eq!(parse("foo --bar"), Err(QueryError::Unexpected { found: "-".into(), at: 5 }));
    }

    #[test]
    fn parentheses() {
        assert_eq!(parse("(foo)").unwrap(), t("foo"));
        assert_eq!(parse("(a OR b) c").unwrap(), and(vec![Or(vec![t("a"), t("b")]), t("c")], vec![]));
        assert_eq!(parse("a (b OR c)").unwrap(), and(vec![t("a"), Or(vec![t("b"), t("c")])], vec![]));
        assert_eq!(parse("a(b)c").unwrap(), and(vec![t("a"), t("b"), t("c")], vec![]));
        assert_eq!(
            parse("((a OR b) AND (c OR d)) OR e").unwrap(),
            Or(vec![
                and(vec![Or(vec![t("a"), t("b")]), Or(vec![t("c"), t("d")])], vec![]),
                t("e"),
            ])
        );
    }

    #[test]
    fn paren_errors() {
        assert_eq!(parse("(foo"), Err(QueryError::UnbalancedParen { at: 0 }));
        assert_eq!(parse("foo)"), Err(QueryError::UnbalancedParen { at: 3 }));
        assert_eq!(parse("(a (b) c"), Err(QueryError::UnbalancedParen { at: 0 }));
        assert_eq!(parse("()"), Err(QueryError::Unexpected { found: ")".into(), at: 1 }));
        assert_eq!(parse("foo ()"), Err(QueryError::Unexpected { found: ")".into(), at: 5 }));
    }

    #[test]
    fn empty_and_punctuation_only() {
        assert_eq!(parse(""), Err(QueryError::Empty));
        assert_eq!(parse("   \t "), Err(QueryError::Empty));
        assert_eq!(parse("!!! ..."), Err(QueryError::Empty));
        assert_eq!(parse("foo !!! bar").unwrap(), and(vec![t("foo"), t("bar")], vec![]));
    }

    #[test]
    fn keyword_position_errors() {
        assert_eq!(parse("AND foo"), Err(QueryError::Unexpected { found: "AND".into(), at: 0 }));
        assert_eq!(parse("foo AND"), Err(QueryError::Unexpected { found: "AND".into(), at: 4 }));
        assert_eq!(parse("foo OR"), Err(QueryError::Unexpected { found: "OR".into(), at: 4 }));
        assert_eq!(parse("OR foo"), Err(QueryError::Unexpected { found: "OR".into(), at: 0 }));
        assert_eq!(parse("foo AND OR bar"), Err(QueryError::Unexpected { found: "AND".into(), at: 4 }));
        assert_eq!(parse("foo OR AND bar"), Err(QueryError::Unexpected { found: "OR".into(), at: 4 }));
        assert_eq!(parse("foo -AND"), Err(QueryError::Unexpected { found: "AND".into(), at: 5 }));
    }

    #[test]
    fn unterminated_phrase() {
        assert_eq!(parse("foo \"bar baz"), Err(QueryError::UnterminatedPhrase { at: 4 }));
    }

    #[test]
    fn offsets_are_bytes_in_the_original_input() {
        // "Café " is 6 bytes; the paren error should point past it.
        assert_eq!(parse("Café )"), Err(QueryError::UnbalancedParen { at: 6 }));
    }
}
