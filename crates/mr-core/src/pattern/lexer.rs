//! Tokenizer for the PATTERN clause (a regular expression over variables).

use std::fmt;

use crate::diag::point_at;
use crate::error::{MrError, Result};

/// A PATTERN token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    /// A pattern-variable identifier (canonicalized to upper case).
    Var(String),
    /// `|`
    Pipe,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `*`
    Star,
    /// `+`
    Plus,
    /// `?`
    Question,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `,`
    Comma,
    /// A non-negative integer literal (inside `{}` quantifiers).
    Num(usize),
    /// `^`
    Caret,
    /// `$`
    Dollar,
}

/// How a token reads back to the user: the source text it was written as, with
/// no surrounding quotes (callers add them). Never the Rust variant name — a
/// message saying `expected RParen` asks the reader to know our AST.
impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tok::Var(v) => write!(f, "{v}"),
            Tok::Num(n) => write!(f, "{n}"),
            Tok::Pipe => f.write_str("|"),
            Tok::LParen => f.write_str("("),
            Tok::RParen => f.write_str(")"),
            Tok::Star => f.write_str("*"),
            Tok::Plus => f.write_str("+"),
            Tok::Question => f.write_str("?"),
            Tok::LBrace => f.write_str("{"),
            Tok::RBrace => f.write_str("}"),
            Tok::Comma => f.write_str(","),
            Tok::Caret => f.write_str("^"),
            Tok::Dollar => f.write_str("$"),
        }
    }
}

/// Tokenize a pattern string, discarding the source positions.
pub fn lex(src: &str) -> Result<Vec<Tok>> {
    lex_spanned(src).map(|(toks, _)| toks)
}

/// Tokenize a pattern string, returning each token's starting **character**
/// index alongside it, so the parser can point a caret at what it rejected.
///
/// Whitespace separates and is otherwise ignored. The two vectors are always
/// the same length and are indexed together.
pub fn lex_spanned(src: &str) -> Result<(Vec<Tok>, Vec<usize>)> {
    let mut toks = Vec::new();
    let mut spans = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    // Every `push` below records the position the token started at. Keeping it
    // in one closure means a new token form cannot silently arrive without one.
    macro_rules! push {
        ($tok:expr, $at:expr) => {{
            toks.push($tok);
            spans.push($at);
        }};
    }
    while i < chars.len() {
        let c = chars[i];
        let at = i;
        match c {
            c if c.is_whitespace() => {
                i += 1;
            }
            '|' => {
                push!(Tok::Pipe, at);
                i += 1;
            }
            '(' => {
                push!(Tok::LParen, at);
                i += 1;
            }
            ')' => {
                push!(Tok::RParen, at);
                i += 1;
            }
            '*' => {
                push!(Tok::Star, at);
                i += 1;
            }
            '+' => {
                push!(Tok::Plus, at);
                i += 1;
            }
            '?' => {
                push!(Tok::Question, at);
                i += 1;
            }
            '{' => {
                push!(Tok::LBrace, at);
                i += 1;
            }
            '}' => {
                push!(Tok::RBrace, at);
                i += 1;
            }
            ',' => {
                push!(Tok::Comma, at);
                i += 1;
            }
            '^' => {
                push!(Tok::Caret, at);
                i += 1;
            }
            '$' => {
                push!(Tok::Dollar, at);
                i += 1;
            }
            '-' if i + 1 < chars.len() && chars[i + 1] == '}' => {
                return Err(MrError::Pattern(format!(
                    "exclusion syntax `{{- ... -}}` is not supported in v1 (v1.1 non-goal){}",
                    point_at(src, at)
                )));
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                let n = s.parse::<usize>().map_err(|_| {
                    MrError::Pattern(format!(
                        "quantifier count '{s}' is out of range{}",
                        point_at(src, at)
                    ))
                })?;
                push!(Tok::Num(n), at);
            }
            '"' => {
                // A double-quoted label is case-sensitive: `"b"` is a different
                // variable from the unquoted `b` (which canonicalizes to `B`).
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= chars.len() {
                        return Err(MrError::Pattern(format!(
                            "unterminated double-quoted pattern variable{}",
                            point_at(src, at)
                        )));
                    }
                    if chars[i] == '"' {
                        if i + 1 < chars.len() && chars[i + 1] == '"' {
                            s.push('"');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    s.push(chars[i]);
                    i += 1;
                }
                if s.is_empty() {
                    return Err(MrError::Pattern(format!(
                        "empty pattern variable name{}",
                        point_at(src, at)
                    )));
                }
                push!(Tok::Var(s), at);
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                push!(Tok::Var(s.to_ascii_uppercase()), at);
            }
            other => {
                return Err(MrError::Pattern(format!(
                    "unexpected character '{other}' in pattern{}",
                    point_at(src, at)
                )));
            }
        }
    }
    Ok((toks, spans))
}
