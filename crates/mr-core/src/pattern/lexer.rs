//! Tokenizer for the PATTERN clause (a regular expression over variables).

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

/// Tokenize a pattern string. Whitespace separates and is otherwise ignored.
pub fn lex(src: &str) -> Result<Vec<Tok>> {
    let mut toks = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => {
                i += 1;
            }
            '|' => {
                toks.push(Tok::Pipe);
                i += 1;
            }
            '(' => {
                toks.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                toks.push(Tok::RParen);
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
            '?' => {
                toks.push(Tok::Question);
                i += 1;
            }
            '{' => {
                toks.push(Tok::LBrace);
                i += 1;
            }
            '}' => {
                toks.push(Tok::RBrace);
                i += 1;
            }
            ',' => {
                toks.push(Tok::Comma);
                i += 1;
            }
            '^' => {
                toks.push(Tok::Caret);
                i += 1;
            }
            '$' => {
                toks.push(Tok::Dollar);
                i += 1;
            }
            '-' if i + 1 < chars.len() && chars[i + 1] == '}' => {
                return Err(MrError::Pattern(
                    "exclusion syntax `{- ... -}` is not supported in v1 (v1.1 non-goal)".into(),
                ));
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                let n = s.parse::<usize>().map_err(|_| {
                    MrError::Pattern(format!("quantifier count '{s}' is out of range"))
                })?;
                toks.push(Tok::Num(n));
            }
            '"' => {
                // A double-quoted label is case-sensitive: `"b"` is a different
                // variable from the unquoted `b` (which canonicalizes to `B`).
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= chars.len() {
                        return Err(MrError::Pattern(
                            "unterminated double-quoted pattern variable".into(),
                        ));
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
                    return Err(MrError::Pattern("empty pattern variable name".into()));
                }
                toks.push(Tok::Var(s));
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                toks.push(Tok::Var(s.to_ascii_uppercase()));
            }
            other => {
                return Err(MrError::Pattern(format!(
                    "unexpected character '{other}' in pattern"
                )));
            }
        }
    }
    Ok(toks)
}
