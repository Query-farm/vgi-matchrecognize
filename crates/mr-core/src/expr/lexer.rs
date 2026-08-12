//! Tokenizer for the DEFINE / MEASURES expression language.

use std::fmt;

use crate::diag::point_at;
use crate::error::{MrError, Result};

/// An expression token.
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// An identifier or keyword (raw text; keyword-ness is decided by the parser
    /// case-insensitively).
    Ident(String),
    /// A double-quoted identifier — never a keyword, and **case-sensitive**, so
    /// `"b"` is a different pattern variable from the unquoted `b` (which
    /// canonicalizes to `B`).
    QuotedIdent(String),
    Int(i64),
    Float(f64),
    Str(String),
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `,`
    Comma,
    /// `.`
    Dot,
    /// `*`
    Star,
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `/`
    Slash,
    /// `%`
    Percent,
    /// `||`
    Concat,
    /// `::`
    CastOp,
    /// `=`
    Eq,
    /// `<>` or `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// How a token reads back to the user: the source text it was written as.
/// Never the Rust variant name — `expected RParen` asks the reader to know our
/// AST rather than their own expression.
impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tok::Ident(s) => write!(f, "{s}"),
            // Quoted forms keep their quotes: that is how they were written, and
            // `"b"` really is a different name from `b`.
            Tok::QuotedIdent(s) => write!(f, "\"{s}\""),
            Tok::Int(v) => write!(f, "{v}"),
            Tok::Float(v) => write!(f, "{v}"),
            Tok::Str(s) => write!(f, "'{s}'"),
            Tok::LParen => f.write_str("("),
            Tok::RParen => f.write_str(")"),
            Tok::Comma => f.write_str(","),
            Tok::Dot => f.write_str("."),
            Tok::Star => f.write_str("*"),
            Tok::Plus => f.write_str("+"),
            Tok::Minus => f.write_str("-"),
            Tok::Slash => f.write_str("/"),
            Tok::Percent => f.write_str("%"),
            Tok::Concat => f.write_str("||"),
            Tok::CastOp => f.write_str("::"),
            Tok::Eq => f.write_str("="),
            Tok::Ne => f.write_str("<>"),
            Tok::Lt => f.write_str("<"),
            Tok::Le => f.write_str("<="),
            Tok::Gt => f.write_str(">"),
            Tok::Ge => f.write_str(">="),
        }
    }
}

/// Tokenize an expression string, discarding the source positions.
pub fn lex(src: &str) -> Result<Vec<Tok>> {
    lex_spanned(src).map(|(toks, _)| toks)
}

/// Tokenize an expression string, returning each token's starting **character**
/// index alongside it, so the parser can point a caret at what it rejected.
///
/// The two vectors are always the same length and are indexed together.
pub fn lex_spanned(src: &str) -> Result<(Vec<Tok>, Vec<usize>)> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut spans = Vec::new();
    let mut i = 0;
    // Every `push` below records the position the token started at, so a new
    // token form cannot silently arrive without one.
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
            c if c.is_whitespace() => i += 1,
            '(' => {
                push!(Tok::LParen, at);
                i += 1;
            }
            ')' => {
                push!(Tok::RParen, at);
                i += 1;
            }
            ',' => {
                push!(Tok::Comma, at);
                i += 1;
            }
            '.' if !(i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) => {
                push!(Tok::Dot, at);
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
            '-' => {
                push!(Tok::Minus, at);
                i += 1;
            }
            '/' => {
                push!(Tok::Slash, at);
                i += 1;
            }
            '%' => {
                push!(Tok::Percent, at);
                i += 1;
            }
            '=' => {
                push!(Tok::Eq, at);
                i += 1;
            }
            '|' if i + 1 < chars.len() && chars[i + 1] == '|' => {
                push!(Tok::Concat, at);
                i += 2;
            }
            ':' if i + 1 < chars.len() && chars[i + 1] == ':' => {
                push!(Tok::CastOp, at);
                i += 2;
            }
            '<' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    push!(Tok::Le, at);
                    i += 2;
                } else if i + 1 < chars.len() && chars[i + 1] == '>' {
                    push!(Tok::Ne, at);
                    i += 2;
                } else {
                    push!(Tok::Lt, at);
                    i += 1;
                }
            }
            '>' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    push!(Tok::Ge, at);
                    i += 2;
                } else {
                    push!(Tok::Gt, at);
                    i += 1;
                }
            }
            '!' if i + 1 < chars.len() && chars[i + 1] == '=' => {
                push!(Tok::Ne, at);
                i += 2;
            }
            '\'' => {
                // String literal with '' escaping.
                let mut s = String::new();
                i += 1;
                loop {
                    if i >= chars.len() {
                        return Err(MrError::Expr(format!(
                            "unterminated string literal{}",
                            point_at(src, at)
                        )));
                    }
                    let ch = chars[i];
                    if ch == '\'' {
                        if i + 1 < chars.len() && chars[i + 1] == '\'' {
                            s.push('\'');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        s.push(ch);
                        i += 1;
                    }
                }
                push!(Tok::Str(s), at);
            }
            c if c.is_ascii_digit() || c == '.' => {
                let start = i;
                let mut is_float = false;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    if chars[i] == '.' {
                        is_float = true;
                    }
                    i += 1;
                }
                // Exponent.
                if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
                    is_float = true;
                    i += 1;
                    if i < chars.len() && (chars[i] == '+' || chars[i] == '-') {
                        i += 1;
                    }
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let s: String = chars[start..i].iter().collect();
                if is_float {
                    let v = s.parse::<f64>().map_err(|_| {
                        MrError::Expr(format!("invalid number '{s}'{}", point_at(src, at)))
                    })?;
                    push!(Tok::Float(v), at);
                } else {
                    let v = s.parse::<i64>().map_err(|_| {
                        MrError::Expr(format!("integer '{s}' out of range{}", point_at(src, at)))
                    })?;
                    push!(Tok::Int(v), at);
                }
            }
            '"' => {
                // A double-quoted identifier; `""` escapes a literal quote.
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= chars.len() {
                        return Err(MrError::Expr(format!(
                            "unterminated double-quoted identifier{}",
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
                push!(Tok::QuotedIdent(s), at);
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                push!(Tok::Ident(s), at);
            }
            other => {
                return Err(MrError::Expr(format!(
                    "unexpected character '{other}' in expression{}",
                    point_at(src, at)
                )))
            }
        }
    }
    Ok((toks, spans))
}
