//! Tokenizer for the DEFINE / MEASURES expression language.

use crate::error::{MrError, Result};

/// An expression token.
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// An identifier or keyword (raw text; keyword-ness is decided by the parser
    /// case-insensitively).
    Ident(String),
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

/// Tokenize an expression string.
pub fn lex(src: &str) -> Result<Vec<Tok>> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => {
                toks.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                toks.push(Tok::RParen);
                i += 1;
            }
            ',' => {
                toks.push(Tok::Comma);
                i += 1;
            }
            '.' if !(i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) => {
                toks.push(Tok::Dot);
                i += 1;
            }
            '*' => {
                toks.push(Tok::Star);
                i += 1;
            }
            '+' => {
                toks.push(Tok::Plus);
                i += 1;
            }
            '-' => {
                toks.push(Tok::Minus);
                i += 1;
            }
            '/' => {
                toks.push(Tok::Slash);
                i += 1;
            }
            '%' => {
                toks.push(Tok::Percent);
                i += 1;
            }
            '=' => {
                toks.push(Tok::Eq);
                i += 1;
            }
            '|' if i + 1 < chars.len() && chars[i + 1] == '|' => {
                toks.push(Tok::Concat);
                i += 2;
            }
            ':' if i + 1 < chars.len() && chars[i + 1] == ':' => {
                toks.push(Tok::CastOp);
                i += 2;
            }
            '<' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    toks.push(Tok::Le);
                    i += 2;
                } else if i + 1 < chars.len() && chars[i + 1] == '>' {
                    toks.push(Tok::Ne);
                    i += 2;
                } else {
                    toks.push(Tok::Lt);
                    i += 1;
                }
            }
            '>' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    toks.push(Tok::Ge);
                    i += 2;
                } else {
                    toks.push(Tok::Gt);
                    i += 1;
                }
            }
            '!' if i + 1 < chars.len() && chars[i + 1] == '=' => {
                toks.push(Tok::Ne);
                i += 2;
            }
            '\'' => {
                // String literal with '' escaping.
                let mut s = String::new();
                i += 1;
                loop {
                    if i >= chars.len() {
                        return Err(MrError::Expr("unterminated string literal".into()));
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
                toks.push(Tok::Str(s));
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
                    let v = s
                        .parse::<f64>()
                        .map_err(|_| MrError::Expr(format!("invalid number '{s}'")))?;
                    toks.push(Tok::Float(v));
                } else {
                    let v = s
                        .parse::<i64>()
                        .map_err(|_| MrError::Expr(format!("integer '{s}' out of range")))?;
                    toks.push(Tok::Int(v));
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                toks.push(Tok::Ident(s));
            }
            other => {
                return Err(MrError::Expr(format!(
                    "unexpected character '{other}' in expression"
                )))
            }
        }
    }
    Ok(toks)
}
