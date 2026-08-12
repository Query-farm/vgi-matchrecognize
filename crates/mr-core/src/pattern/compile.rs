//! Compile a [`Pattern`] AST into a backtracking VM program, plus the
//! `explain_pattern` pretty-printer.
//!
//! The VM is a classic Thompson-style instruction list executed with
//! backtracking (see [`crate::engine::matcher`]). Greedy vs reluctant
//! quantifiers differ only in the order of the two `Split` targets.

use super::parser::{Anchor, Pattern};
use crate::engine::labels::{LabelSet, VarId};
use crate::error::{MrError, Result};

/// A backtracking-VM instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inst {
    /// Consume one row that satisfies `DEFINE[var]`, binding it to `var`.
    ///
    /// The label is an id into the plan's [`LabelSet`], not a name: the VM executes
    /// this instruction once per tentative binding, and carrying a `String` here
    /// meant an allocation per step.
    Char(VarId),
    /// Try the first target; on failure, try the second.
    Split(usize, usize),
    /// Unconditional jump.
    Jmp(usize),
    /// Succeed only at partition start (tape position 0).
    AnchorStart,
    /// Succeed only at partition end (tape position == length).
    AnchorEnd,
    /// Accept — a complete match ending at the current tape position.
    Match,
}

/// A compiled pattern program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub insts: Vec<Inst>,
}

/// Ceiling on a compiled program's size.
///
/// A bounded quantifier is expanded by *copying* its body, so the instruction
/// count is the product of the repeat counts around it — and nothing bounded
/// those. `A{100000}` compiled to 100,001 instructions, and
/// `((A{1000}){1000}){1000}` to 10^9, which is an allocation failure rather
/// than an error: the process dies instead of the query. `MAX_DEPTH` guards the
/// parser's recursion and `MAX_PERMUTE_ARGS` guards the factorial expansion;
/// this is the third way a short pattern string can turn into something huge.
///
/// A million instructions is ~24 MB, far above any real pattern — `A{1000}`
/// costs 1001 — so this only ever fires on input that was going to fail anyway.
const MAX_PROGRAM_INSTS: usize = 1_000_000;

struct Compiler<'a> {
    insts: Vec<Inst>,
    labels: &'a LabelSet,
    /// The first label the set did not know, if any. Recorded rather than returned
    /// so `emit` can stay infallible and recursive.
    missing: Option<String>,
    /// Set once the program outgrew [`MAX_PROGRAM_INSTS`]. Recorded rather than
    /// returned for the same reason as `missing`, and checked at the top of
    /// `emit` so an expansion already in flight stops immediately.
    too_big: bool,
}

impl Compiler<'_> {
    fn here(&self) -> usize {
        self.insts.len()
    }

    /// Whether `n` more instructions would exceed the ceiling; records the
    /// failure if so, so callers can simply stop.
    fn would_overflow(&mut self, n: usize) -> bool {
        if self.too_big {
            return true;
        }
        if self.insts.len().saturating_add(n) > MAX_PROGRAM_INSTS {
            self.too_big = true;
        }
        self.too_big
    }

    fn emit(&mut self, node: &Pattern) {
        // Stop the moment the budget is gone: an outer quantifier may be part
        // way through copying a body, and every nested level is still looping.
        if self.would_overflow(1) {
            return;
        }
        match node {
            Pattern::Empty => {}
            Pattern::Anchor(Anchor::Start) => self.insts.push(Inst::AnchorStart),
            Pattern::Anchor(Anchor::End) => self.insts.push(Inst::AnchorEnd),
            Pattern::Var(v) => match self.labels.id_of(v) {
                Some(id) => self.insts.push(Inst::Char(id)),
                None => {
                    if self.missing.is_none() {
                        self.missing = Some(v.clone());
                    }
                }
            },
            Pattern::Concat(items) => {
                for it in items {
                    self.emit(it);
                }
            }
            Pattern::Alt(branches) => self.emit_alt(branches),
            Pattern::Quant {
                inner,
                min,
                max,
                greedy,
            } => self.emit_quant(inner, *min, *max, *greedy),
        }
    }

    fn emit_alt(&mut self, branches: &[Pattern]) {
        // Emit b0 guarded by a Split to the rest; each branch jumps to a common
        // end. The last branch needs no split.
        let mut jmp_fixups = Vec::new();
        for (i, branch) in branches.iter().enumerate() {
            let last = i == branches.len() - 1;
            if last {
                self.emit(branch);
            } else {
                let split_at = self.here();
                self.insts.push(Inst::Split(0, 0)); // placeholder
                let branch_start = self.here();
                self.emit(branch);
                let jmp_at = self.here();
                self.insts.push(Inst::Jmp(0)); // to end
                jmp_fixups.push(jmp_at);
                let rest_start = self.here();
                self.insts[split_at] = Inst::Split(branch_start, rest_start);
            }
        }
        let end = self.here();
        for j in jmp_fixups {
            self.insts[j] = Inst::Jmp(end);
        }
    }

    fn emit_quant(&mut self, inner: &Pattern, min: usize, max: Option<usize>, greedy: bool) {
        // Reject the count itself before looping over it. The per-`emit` check
        // would stop the *output* growing, but `A{4000000000}` would still spin
        // through four billion no-op iterations to get there — each copy of the
        // body is at least one instruction, so the count alone is a lower bound
        // on the program size.
        let copies = max.unwrap_or(min).max(min);
        if self.would_overflow(copies) {
            return;
        }
        // `min` mandatory copies.
        for _ in 0..min {
            self.emit(inner);
        }
        match max {
            None => self.emit_star(inner, greedy),
            Some(m) => {
                for _ in 0..(m - min) {
                    self.emit_opt(inner, greedy);
                    if self.too_big {
                        return;
                    }
                }
            }
        }
    }

    /// `inner*` — a Kleene loop.
    fn emit_star(&mut self, inner: &Pattern, greedy: bool) {
        let l1 = self.here();
        self.insts.push(Inst::Split(0, 0)); // placeholder
        let l2 = self.here();
        self.emit(inner);
        self.insts.push(Inst::Jmp(l1));
        let exit = self.here();
        self.insts[l1] = if greedy {
            Inst::Split(l2, exit)
        } else {
            Inst::Split(exit, l2)
        };
    }

    /// `inner?` — optional.
    fn emit_opt(&mut self, inner: &Pattern, greedy: bool) {
        let l1 = self.here();
        self.insts.push(Inst::Split(0, 0)); // placeholder
        let l2 = self.here();
        self.emit(inner);
        let exit = self.here();
        self.insts[l1] = if greedy {
            Inst::Split(l2, exit)
        } else {
            Inst::Split(exit, l2)
        };
    }
}

/// Compile a [`Pattern`] AST into a [`Program`] (terminated by `Match`).
pub fn compile(pat: &Pattern, labels: &LabelSet) -> Result<Program> {
    let mut c = Compiler {
        insts: Vec::new(),
        labels,
        missing: None,
        too_big: false,
    };
    c.emit(pat);
    if c.too_big {
        return Err(MrError::Pattern(format!(
            "pattern compiles to more than the {MAX_PROGRAM_INSTS}-instruction limit; a bounded \
             quantifier is expanded by copying its body, so the repeat counts multiply — use \
             smaller bounds, or an unbounded '*'/'+' where the exact count does not matter"
        )));
    }
    if let Some(name) = c.missing {
        return Err(MrError::Bind(format!(
            "internal: pattern variable '{name}' is missing from the label set"
        )));
    }
    c.insts.push(Inst::Match);
    Ok(Program { insts: c.insts })
}

/// A human-readable rendering of the parsed pattern (the `explain_pattern`
/// developer aid). Renders the AST, not the VM, so it stays compact.
pub fn explain(pat: &Pattern) -> String {
    fn go(p: &Pattern, out: &mut String, top: bool) {
        match p {
            Pattern::Empty => out.push_str("(empty)"),
            Pattern::Var(v) => out.push_str(v),
            Pattern::Anchor(Anchor::Start) => out.push('^'),
            Pattern::Anchor(Anchor::End) => out.push('$'),
            Pattern::Concat(items) => {
                if !top {
                    out.push('(');
                }
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(" · ");
                    }
                    go(it, out, false);
                }
                if !top {
                    out.push(')');
                }
            }
            Pattern::Alt(branches) => {
                out.push('(');
                for (i, b) in branches.iter().enumerate() {
                    if i > 0 {
                        out.push_str(" | ");
                    }
                    go(b, out, false);
                }
                out.push(')');
            }
            Pattern::Quant {
                inner,
                min,
                max,
                greedy,
            } => {
                out.push('(');
                go(inner, out, false);
                out.push(')');
                let q = match (min, max) {
                    (0, None) => "*".to_string(),
                    (1, None) => "+".to_string(),
                    (0, Some(1)) => "?".to_string(),
                    (n, None) => format!("{{{n},}}"),
                    (n, Some(m)) if n == m => format!("{{{n}}}"),
                    (n, Some(m)) => format!("{{{n},{m}}}"),
                };
                out.push_str(&q);
                out.push_str(if *greedy { "greedy" } else { "reluctant" });
            }
        }
    }
    let mut out = String::new();
    go(pat, &mut out, true);
    out
}
