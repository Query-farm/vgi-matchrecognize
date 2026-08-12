//! The backtracking matcher: executes a compiled [`Program`] over a partition
//! tape, evaluating DEFINE predicates at each tentative assignment, with a step
//! budget (no hang) and AFTER MATCH SKIP advancement (guaranteed termination).
//!
//! Backtracking uses an explicit heap stack of pending alternatives, not host
//! recursion, so match length is bounded by the step budget rather than by the OS
//! stack: a single match may span millions of rows.

use std::collections::HashMap;

use super::eval::{Bind, Frame, SubsetMap};
use super::rowstore::RowStore;
use crate::error::{MrError, Result};
use crate::expr::ast::Expr;
use crate::pattern::compile::{Inst, Program};
use crate::value::Value;

/// AFTER MATCH SKIP mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterSkip {
    /// Resume at the row after the last matched row (default; non-overlapping).
    PastLastRow,
    /// Resume at the row after the match start (overlapping matches).
    ToNextRow,
    /// Resume at the first row bound to the named variable.
    ToFirstVar(String),
    /// Resume at the last row bound to the named variable.
    ToLastVar(String),
}

/// A successful match within a partition.
#[derive(Debug, Clone)]
pub struct Match {
    pub match_number: i64,
    /// Start tape position (inclusive).
    pub start: usize,
    /// End tape position (exclusive — one past the last matched row).
    pub end: usize,
    pub binds: Vec<Bind>,
}

/// One pending alternative on the explicit backtrack stack: resume execution at
/// `ip` and tape position `pos`, with `binds` truncated back to `binds_len`.
///
/// Truncation is a sound restore because `binds` is append-only along any single
/// path — `Char` pushes (and pops only its own entry on immediate failure) while
/// the control instructions never touch it — so the prefix `[0, binds_len)` is
/// exactly the binding state that was live when the alternative was recorded.
#[derive(Debug, Clone, Copy)]
struct Alt {
    ip: usize,
    pos: usize,
    binds_len: usize,
}

/// Per-partition matcher state.
pub struct Matcher<'a> {
    prog: &'a [Inst],
    store: &'a dyn RowStore,
    tape: &'a [usize],
    define: &'a HashMap<String, Expr>,
    subsets: &'a SubsetMap,
    after: &'a AfterSkip,
    budget: i64,
    partition_label: String,
    match_number: i64,
    /// The backtrack stack, owned by the matcher rather than by `run`, so its
    /// capacity is paid for once per partition instead of once per start position.
    stack: Vec<Alt>,
}

impl<'a> Matcher<'a> {
    /// Build a matcher for one partition.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        program: &'a Program,
        store: &'a dyn RowStore,
        tape: &'a [usize],
        define: &'a HashMap<String, Expr>,
        subsets: &'a SubsetMap,
        after: &'a AfterSkip,
        step_budget: i64,
        partition_label: impl Into<String>,
        first_match_number: i64,
    ) -> Self {
        Matcher {
            prog: &program.insts,
            store,
            tape,
            define,
            subsets,
            after,
            budget: step_budget,
            partition_label: partition_label.into(),
            match_number: first_match_number,
            stack: Vec::new(),
        }
    }

    /// The match number assigned to the next partition (so a global counter can
    /// continue across partitions if desired).
    pub fn next_match_number(&self) -> i64 {
        self.match_number
    }

    /// Find every match in the partition, advancing per AFTER MATCH SKIP. The
    /// outer loop advances the tape cursor by at least one row each iteration,
    /// so termination is guaranteed independently of the step budget.
    ///
    /// **Empty matches** (a pattern that matches zero rows at some position, e.g.
    /// `B*` where the row cannot bind `B`) are real matches: they are reported and
    /// they consume a match number, per SQL:2016. Since they span no rows, the
    /// cursor advances by one so the scan still terminates. Whether they reach the
    /// output is the caller's decision (SHOW / OMIT EMPTY MATCHES).
    pub fn find_all(&mut self) -> Result<Vec<Match>> {
        let mut matches = Vec::new();
        let mut i = 0usize;
        // One buffer for the whole scan. Every failed attempt ends with `run`
        // truncating it back to empty, so reusing it keeps the grown capacity
        // instead of re-paying for it at each of the tape's start positions.
        let mut binds: Vec<Bind> = Vec::new();
        while i < self.tape.len() {
            binds.clear();
            let res = self.run(i, &mut binds)?;
            match res {
                Some(end) => {
                    let m = Match {
                        match_number: self.match_number,
                        start: i,
                        end,
                        // Move the binds out rather than cloning them: `binds` is
                        // cleared at the top of the next iteration either way, so a
                        // clone would allocate a second Vec and deep-copy every
                        // bind only to drop the original.
                        binds: std::mem::take(&mut binds),
                    };
                    self.match_number += 1;
                    let next = self.skip_target(i, &m)?;
                    matches.push(m);
                    i = if next > i { next } else { i + 1 };
                }
                None => i += 1,
            }
        }
        Ok(matches)
    }

    /// Whether a row bound to `bound` is covered by `label` — the same variable,
    /// or a SUBSET that lists it.
    fn label_covers(&self, label: &str, bound: &str) -> bool {
        label == bound
            || self
                .subsets
                .get(label)
                .is_some_and(|ms| ms.iter().any(|m| m == bound))
    }

    fn skip_target(&self, start: usize, m: &Match) -> Result<usize> {
        let pos = match self.after {
            AfterSkip::PastLastRow => m.end,
            AfterSkip::ToNextRow => start + 1,
            AfterSkip::ToFirstVar(v) => m
                .binds
                .iter()
                .find(|b| self.label_covers(v, &b.var))
                .map(|b| b.tape_pos)
                .unwrap_or(m.end),
            AfterSkip::ToLastVar(v) => m
                .binds
                .iter()
                .rev()
                .find(|b| self.label_covers(v, &b.var))
                .map(|b| b.tape_pos)
                .unwrap_or(m.end),
        };
        Ok(pos)
    }

    /// The backtracking VM executor: a depth-first search driven by an explicit
    /// **heap** backtrack stack. Returns the end tape position on accept, leaving
    /// `binds` holding the accepted match; on failure returns `Ok(None)` with
    /// `binds` restored to its entry state.
    ///
    /// The pending alternatives live on the heap rather than the call stack, so a
    /// single match may span millions of rows — match length is bounded only by
    /// the step budget, never by the OS stack.
    fn run(&mut self, start_pos: usize, binds: &mut Vec<Bind>) -> Result<Option<usize>> {
        // A copy of the shared program reference, so indexing it does not borrow
        // `self` and the loop body can still call `&mut self` / `&self` methods.
        let prog = self.prog;
        let base = binds.len();
        // The backtrack stack lives on `self` so its capacity survives across start
        // positions; `Alt` is `Copy`, so clearing is free and keeps the allocation.
        self.stack.clear();
        let mut ip = 0usize;
        let mut pos = start_pos;
        loop {
            self.budget -= 1;
            if self.budget <= 0 {
                return Err(MrError::StepBudget(format!(
                    "step budget exhausted in partition {} (pattern may be ambiguous; raise \
                     step_budget or rewrite the pattern)",
                    self.partition_label
                )));
            }
            // `false` = this instruction failed; fall through to backtracking.
            let advanced = match &prog[ip] {
                Inst::Match => return Ok(Some(pos)),
                Inst::Jmp(t) => {
                    ip = *t;
                    true
                }
                Inst::Split(a, b) => {
                    // Take the first target now and keep the second for later.
                    // Popping LIFO reproduces exactly the preference order of the
                    // former recursive executor, so greedy vs reluctant semantics
                    // (which differ only in `Split` branch order) are unchanged.
                    self.stack.push(Alt {
                        ip: *b,
                        pos,
                        binds_len: binds.len(),
                    });
                    ip = *a;
                    true
                }
                Inst::AnchorStart => {
                    if pos == 0 {
                        ip += 1;
                        true
                    } else {
                        false
                    }
                }
                Inst::AnchorEnd => {
                    if pos == self.tape.len() {
                        ip += 1;
                        true
                    } else {
                        false
                    }
                }
                Inst::Char(var) => {
                    if pos >= self.tape.len() {
                        false
                    } else {
                        binds.push(Bind {
                            tape_pos: pos,
                            var: var.clone(),
                        });
                        if self.predicate_holds(var, binds)? {
                            ip += 1;
                            pos += 1;
                            true
                        } else {
                            binds.pop();
                            false
                        }
                    }
                }
            };
            if !advanced {
                match self.stack.pop() {
                    Some(alt) => {
                        ip = alt.ip;
                        pos = alt.pos;
                        binds.truncate(alt.binds_len);
                    }
                    None => {
                        binds.truncate(base);
                        return Ok(None);
                    }
                }
            }
        }
    }

    fn predicate_holds(&self, var: &str, binds: &[Bind]) -> Result<bool> {
        let expr = match self.define.get(var) {
            Some(e) => e,
            // Undefined variables default to "always true" (SQL standard).
            None => return Ok(true),
        };
        let frame = Frame {
            store: self.store,
            tape: self.tape,
            binds,
            horizon: binds.len(),
            match_number: self.match_number,
            subsets: self.subsets,
        };
        let v = frame.eval_predicate(expr)?;
        Ok(matches!(v, Value::Bool(true)))
    }
}
