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

struct Compiler<'a> {
    insts: Vec<Inst>,
    labels: &'a LabelSet,
    /// The first label the set did not know, if any. Recorded rather than returned
    /// so `emit` can stay infallible and recursive.
    missing: Option<String>,
}

impl Compiler<'_> {
    fn here(&self) -> usize {
        self.insts.len()
    }

    fn emit(&mut self, node: &Pattern) {
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
        // `min` mandatory copies.
        for _ in 0..min {
            self.emit(inner);
        }
        match max {
            None => self.emit_star(inner, greedy),
            Some(m) => {
                for _ in 0..(m - min) {
                    self.emit_opt(inner, greedy);
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
    };
    c.emit(pat);
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
