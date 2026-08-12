//! PATTERN parser: tokens -> [`Pattern`] AST.
//!
//! Grammar (EBNF):
//! ```text
//! pattern     := anchor? alternation anchor?
//! alternation := concat ('|' concat)*
//! concat      := factor+
//! factor      := primary quantifier?
//! primary     := VAR | '(' alternation ')' | '^' | '$'
//! quantifier  := ('*' | '+' | '?' | '{' n '}' | '{' n ',' '}'
//!              | '{' n ',' m '}' | '{' ',' m '}') '?'?
//! ```

use super::lexer::{lex, Tok};
use crate::error::{MrError, Result};

/// A partition-edge anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    /// `^` — matches only at the start of the partition.
    Start,
    /// `$` — matches only at the end of the partition.
    End,
}

/// The compiled pattern AST (a regular expression over pattern variables).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    /// Matches zero rows.
    Empty,
    /// A single row that satisfies `DEFINE[var]` (canonical upper-case name).
    Var(String),
    /// A partition-edge anchor (consumes no rows).
    Anchor(Anchor),
    /// `a b c` — match in sequence.
    Concat(Vec<Pattern>),
    /// `a | b` — match either branch (left preferred under greedy).
    Alt(Vec<Pattern>),
    /// A quantified sub-pattern (`*`, `+`, `?`, `{n}`, `{n,}`, `{n,m}`, `{,m}`).
    Quant {
        inner: Box<Pattern>,
        min: usize,
        /// `None` means unbounded (e.g. `*`, `+`, `{n,}`).
        max: Option<usize>,
        greedy: bool,
    },
}

impl Pattern {
    /// Whether this sub-pattern can match zero rows.
    pub fn nullable(&self) -> bool {
        match self {
            Pattern::Empty | Pattern::Anchor(_) => true,
            Pattern::Var(_) => false,
            Pattern::Concat(items) => items.iter().all(Pattern::nullable),
            Pattern::Alt(branches) => branches.iter().any(Pattern::nullable),
            Pattern::Quant { inner, min, .. } => *min == 0 || inner.nullable(),
        }
    }

    /// Whether this sub-pattern matches *exactly* zero rows on every branch — it
    /// binds nothing and can never consume a row (`()`, `^`, `$`, and any
    /// combination of those).
    ///
    /// Repeating such a sub-pattern is idempotent: `()*`, `^+` and `(){5,}` add
    /// nothing beyond a single occurrence, which is what lets the parser collapse
    /// an otherwise non-terminating epsilon loop into a bounded form.
    pub fn always_empty(&self) -> bool {
        match self {
            Pattern::Empty | Pattern::Anchor(_) => true,
            Pattern::Var(_) => false,
            Pattern::Concat(items) => items.iter().all(Pattern::always_empty),
            Pattern::Alt(branches) => branches.iter().all(Pattern::always_empty),
            Pattern::Quant { inner, .. } => inner.always_empty(),
        }
    }

    /// Collect the set of pattern-variable names referenced, in first-seen order.
    pub fn variables(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.collect_vars(&mut out);
        out
    }

    fn collect_vars(&self, out: &mut Vec<String>) {
        match self {
            Pattern::Var(v) => {
                if !out.contains(v) {
                    out.push(v.clone());
                }
            }
            Pattern::Concat(items) | Pattern::Alt(items) => {
                for p in items {
                    p.collect_vars(out);
                }
            }
            Pattern::Quant { inner, .. } => inner.collect_vars(out),
            Pattern::Empty | Pattern::Anchor(_) => {}
        }
    }
}

/// Maximum grouping depth. The parser descends recursively through
/// `alternation -> concat -> factor -> primary -> '(' alternation ')'`, so deeply
/// nested input would otherwise exhaust the native stack and abort the process.
/// Real patterns nest a handful of levels; this reports a clean error instead.
const MAX_DEPTH: u32 = 128;

/// Most arguments `PERMUTE` accepts. The expansion is factorial — 6 is already 720
/// alternatives — so this is a guard against a one-line pattern becoming a huge
/// program, not a limit anyone writing SQL by hand will meet.
const MAX_PERMUTE_ARGS: usize = 6;

/// Every permutation of `order`, in lexicographic order of the original positions,
/// handed to `emit` one at a time.
///
/// Lexicographic order is the point: it is the preference order the standard gives
/// `PERMUTE`, so the first permutation that matches is the one that must win.
fn permutations(order: &[usize], at: usize, emit: &mut impl FnMut(&[usize])) {
    if at == order.len() {
        emit(order);
        return;
    }
    // Choosing the smallest remaining element first, and rebuilding rather than
    // swapping, keeps the emitted sequence lexicographic.
    for i in at..order.len() {
        let mut rotated = order.to_vec();
        let picked = rotated.remove(i);
        rotated.insert(at, picked);
        permutations(&rotated, at + 1, emit);
    }
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    depth: u32,
}

impl Parser {
    /// Enter one grouping level, refusing input deep enough to blow the stack.
    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(MrError::Pattern(format!(
                "pattern nests deeper than the {MAX_DEPTH}-level limit"
            )));
        }
        Ok(())
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, t: &Tok) -> Result<()> {
        match self.next() {
            Some(ref got) if got == t => Ok(()),
            Some(got) => Err(MrError::Pattern(format!("expected {t:?}, found {got:?}"))),
            None => Err(MrError::Pattern(format!(
                "expected {t:?}, found end of pattern"
            ))),
        }
    }

    /// alternation := concat ('|' concat)*
    fn alternation(&mut self) -> Result<Pattern> {
        self.enter()?;
        let mut branches = vec![self.concat()?];
        while matches!(self.peek(), Some(Tok::Pipe)) {
            self.next();
            branches.push(self.concat()?);
        }
        // Only decremented on success; an error aborts the whole parse.
        self.depth -= 1;
        if branches.len() == 1 {
            Ok(branches.pop().unwrap())
        } else {
            Ok(Pattern::Alt(branches))
        }
    }

    /// concat := factor+
    fn concat(&mut self) -> Result<Pattern> {
        let mut items = Vec::new();
        while self.starts_factor() {
            items.push(self.factor()?);
        }
        match items.len() {
            0 => Ok(Pattern::Empty),
            1 => Ok(items.pop().unwrap()),
            _ => Ok(Pattern::Concat(items)),
        }
    }

    fn starts_factor(&self) -> bool {
        matches!(
            self.peek(),
            Some(Tok::Var(_)) | Some(Tok::LParen) | Some(Tok::Caret) | Some(Tok::Dollar)
        )
    }

    /// factor := primary quantifier?
    fn factor(&mut self) -> Result<Pattern> {
        let prim = self.primary()?;
        if let Some((min, max)) = self.try_quantifier()? {
            // A bare reluctant marker `?` may follow.
            let greedy = if matches!(self.peek(), Some(Tok::Question)) {
                self.next();
                false
            } else {
                true
            };
            // A sub-pattern that always matches exactly zero rows is idempotent
            // under repetition, so collapse the quantifier to a bounded, equivalent
            // form rather than emitting a loop that could never make progress.
            // This is what makes `()*`, `^+`, `(){5,}` and `(B ()*)*` work.
            if prim.always_empty() {
                return Ok(if min == 0 {
                    // Zero occurrences (empty) or one — `()?`, `^?`.
                    Pattern::Quant {
                        inner: Box::new(prim),
                        min: 0,
                        max: Some(1),
                        greedy,
                    }
                } else {
                    // At least one occurrence, and repeats add nothing.
                    prim
                });
            }
            if max.is_none() && prim.nullable() {
                return Err(MrError::Pattern(
                    "an unbounded quantifier (* + {n,}) over a possibly-empty sub-pattern is not \
                     supported in v1 (it would not terminate); rewrite the pattern"
                        .into(),
                ));
            }
            Ok(Pattern::Quant {
                inner: Box::new(prim),
                min,
                max,
                greedy,
            })
        } else {
            Ok(prim)
        }
    }

    /// Parse an optional quantifier, returning `(min, max)` if present.
    fn try_quantifier(&mut self) -> Result<Option<(usize, Option<usize>)>> {
        match self.peek() {
            Some(Tok::Star) => {
                self.next();
                Ok(Some((0, None)))
            }
            Some(Tok::Plus) => {
                self.next();
                Ok(Some((1, None)))
            }
            Some(Tok::Question) => {
                self.next();
                Ok(Some((0, Some(1))))
            }
            Some(Tok::LBrace) => {
                self.next();
                self.brace_quantifier().map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Parse the body of a `{...}` quantifier (the `{` already consumed).
    fn brace_quantifier(&mut self) -> Result<(usize, Option<usize>)> {
        // Forms: {n} | {n,} | {n,m} | {,m} | {,}
        let result = match self.peek() {
            Some(Tok::Comma) => {
                self.next();
                match self.peek() {
                    // `{,}` — no bound at either end, i.e. the same as `*`.
                    Some(Tok::RBrace) => (0, None),
                    // {,m}
                    _ => {
                        let m = self.expect_num()?;
                        (0, Some(m))
                    }
                }
            }
            Some(Tok::Num(_)) => {
                let n = self.expect_num()?;
                match self.peek() {
                    Some(Tok::Comma) => {
                        self.next();
                        match self.peek() {
                            // {n,}
                            Some(Tok::RBrace) => (n, None),
                            // {n,m}
                            _ => {
                                let m = self.expect_num()?;
                                (n, Some(m))
                            }
                        }
                    }
                    // {n}
                    _ => (n, Some(n)),
                }
            }
            other => {
                return Err(MrError::Pattern(format!(
                    "malformed quantifier, expected a number or ',' after '{{', found {other:?}"
                )))
            }
        };
        self.expect(&Tok::RBrace)?;
        if let (lo, Some(hi)) = result {
            if hi < lo {
                return Err(MrError::Pattern(format!(
                    "quantifier upper bound {hi} is less than lower bound {lo}"
                )));
            }
        }
        Ok(result)
    }

    fn expect_num(&mut self) -> Result<usize> {
        match self.next() {
            Some(Tok::Num(n)) => Ok(n),
            other => Err(MrError::Pattern(format!(
                "expected a number in quantifier, found {other:?}"
            ))),
        }
    }

    /// primary := VAR | '(' alternation ')' | '^' | '$'
    /// `PERMUTE(p1, p2, …)` — matches its arguments in any order.
    ///
    /// Desugared here into the alternation of every permutation, which is what the
    /// standard defines it as. The branch order matters and is not arbitrary: SQL:2016
    /// says the permutations are tried in lexicographic order of the *argument
    /// positions*, so `PERMUTE(A, B)` prefers `A B` over `B A`, and that preference is
    /// what decides which match wins. Emitting them in that order makes the existing
    /// alternation machinery produce the right answer with no matcher changes.
    ///
    /// Capped at [`MAX_PERMUTE_ARGS`] arguments, because the expansion is factorial:
    /// 6 arguments are already 720 branches, and a cap gives a clear error instead of
    /// a pattern that compiles into something enormous.
    fn permute(&mut self) -> Result<Pattern> {
        self.next(); // PERMUTE
        self.expect(&Tok::LParen)?;
        self.enter()?;
        let mut args = Vec::new();
        loop {
            args.push(self.alternation()?);
            match self.peek() {
                Some(Tok::Comma) => {
                    self.next();
                }
                _ => break,
            }
        }
        self.expect(&Tok::RParen)?;
        self.depth -= 1;
        if args.len() < 2 {
            return Err(MrError::Pattern(
                "PERMUTE needs at least two arguments".into(),
            ));
        }
        if args.len() > MAX_PERMUTE_ARGS {
            return Err(MrError::Pattern(format!(
                "PERMUTE takes at most {MAX_PERMUTE_ARGS} arguments (it expands to every \
                 permutation of them, so {} would be {} alternatives)",
                args.len(),
                (1..=args.len()).product::<usize>()
            )));
        }
        let mut branches: Vec<Pattern> = Vec::new();
        let order: Vec<usize> = (0..args.len()).collect();
        permutations(&order, 0, &mut |perm| {
            branches.push(Pattern::Concat(
                perm.iter().map(|&i| args[i].clone()).collect(),
            ));
        });
        Ok(Pattern::Alt(branches))
    }

    fn primary(&mut self) -> Result<Pattern> {
        // `PERMUTE(A, B, ...)` before the plain-variable case, since it lexes as a
        // variable followed by `(`.
        if let Some(Tok::Var(v)) = self.peek() {
            if v.eq_ignore_ascii_case("PERMUTE")
                && self.toks.get(self.pos + 1) == Some(&Tok::LParen)
            {
                return self.permute();
            }
        }
        match self.next() {
            Some(Tok::Var(v)) => Ok(Pattern::Var(v)),
            Some(Tok::Caret) => Ok(Pattern::Anchor(Anchor::Start)),
            Some(Tok::Dollar) => Ok(Pattern::Anchor(Anchor::End)),
            Some(Tok::LParen) => {
                let inner = self.alternation()?;
                self.expect(&Tok::RParen)?;
                Ok(inner)
            }
            other => Err(MrError::Pattern(format!(
                "expected a variable, '(', '^', or '$', found {other:?}"
            ))),
        }
    }
}

/// Parse a PATTERN string into a [`Pattern`] AST.
pub fn parse(src: &str) -> Result<Pattern> {
    let toks = lex(src)?;
    if toks.is_empty() {
        return Err(MrError::Pattern("pattern is empty".into()));
    }
    let mut p = Parser {
        toks,
        pos: 0,
        depth: 0,
    };
    let pat = p.alternation()?;
    if p.pos != p.toks.len() {
        return Err(MrError::Pattern(format!(
            "unexpected trailing tokens starting at {:?}",
            p.toks[p.pos]
        )));
    }
    Ok(pat)
}
