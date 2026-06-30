//! The backtracking matcher: executes a compiled [`Program`] over a partition
//! tape, evaluating DEFINE predicates at each tentative assignment, with a step
//! budget (no hang) and AFTER MATCH SKIP advancement (guaranteed termination).

use std::collections::HashMap;

use super::eval::{Bind, Frame};
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

/// Recursion-depth cap, a backstop against stack overflow on deep grouping;
/// the step budget is the primary guard.
const DEPTH_CAP: usize = 100_000;

/// Per-partition matcher state.
pub struct Matcher<'a> {
    prog: &'a [Inst],
    store: &'a dyn RowStore,
    tape: &'a [usize],
    define: &'a HashMap<String, Expr>,
    after: &'a AfterSkip,
    budget: i64,
    partition_label: String,
    match_number: i64,
}

impl<'a> Matcher<'a> {
    /// Build a matcher for one partition.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        program: &'a Program,
        store: &'a dyn RowStore,
        tape: &'a [usize],
        define: &'a HashMap<String, Expr>,
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
            after,
            budget: step_budget,
            partition_label: partition_label.into(),
            match_number: first_match_number,
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
    pub fn find_all(&mut self) -> Result<Vec<Match>> {
        let mut matches = Vec::new();
        let mut i = 0usize;
        while i < self.tape.len() {
            let mut binds: Vec<Bind> = Vec::new();
            let res = self.run(0, i, &mut binds, 0)?;
            match res {
                // Empty matches (zero bound rows) are not recorded or numbered
                // (the spec omits them); the cursor still advances by one row so
                // the loop always terminates.
                Some(_) if binds.is_empty() => i += 1,
                Some(end) => {
                    let m = Match {
                        match_number: self.match_number,
                        start: i,
                        end,
                        binds: binds.clone(),
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

    fn skip_target(&self, start: usize, m: &Match) -> Result<usize> {
        let pos = match self.after {
            AfterSkip::PastLastRow => m.end,
            AfterSkip::ToNextRow => start + 1,
            AfterSkip::ToFirstVar(v) => m
                .binds
                .iter()
                .find(|b| b.var.eq_ignore_ascii_case(v))
                .map(|b| b.tape_pos)
                .unwrap_or(m.end),
            AfterSkip::ToLastVar(v) => m
                .binds
                .iter()
                .rev()
                .find(|b| b.var.eq_ignore_ascii_case(v))
                .map(|b| b.tape_pos)
                .unwrap_or(m.end),
        };
        Ok(pos)
    }

    /// The backtracking VM executor. Returns the end tape position on accept.
    /// Leaves `binds` unchanged when it returns `Ok(None)`.
    fn run(
        &mut self,
        ip: usize,
        pos: usize,
        binds: &mut Vec<Bind>,
        depth: usize,
    ) -> Result<Option<usize>> {
        self.budget -= 1;
        if self.budget <= 0 {
            return Err(MrError::StepBudget(format!(
                "step budget exhausted in partition {} (pattern may be ambiguous; raise \
                 step_budget or rewrite the pattern)",
                self.partition_label
            )));
        }
        if depth > DEPTH_CAP {
            return Err(MrError::StepBudget(format!(
                "recursion depth cap reached in partition {} (pattern too deeply nested)",
                self.partition_label
            )));
        }
        match self.prog[ip].clone() {
            Inst::Match => Ok(Some(pos)),
            Inst::Jmp(t) => self.run(t, pos, binds, depth + 1),
            Inst::Split(a, b) => {
                if let Some(e) = self.run(a, pos, binds, depth + 1)? {
                    return Ok(Some(e));
                }
                self.run(b, pos, binds, depth + 1)
            }
            Inst::AnchorStart => {
                if pos == 0 {
                    self.run(ip + 1, pos, binds, depth + 1)
                } else {
                    Ok(None)
                }
            }
            Inst::AnchorEnd => {
                if pos == self.tape.len() {
                    self.run(ip + 1, pos, binds, depth + 1)
                } else {
                    Ok(None)
                }
            }
            Inst::Char(var) => {
                if pos >= self.tape.len() {
                    return Ok(None);
                }
                binds.push(Bind {
                    tape_pos: pos,
                    var: var.clone(),
                });
                let holds = self.predicate_holds(&var, binds)?;
                if holds {
                    if let Some(e) = self.run(ip + 1, pos + 1, binds, depth + 1)? {
                        return Ok(Some(e));
                    }
                }
                binds.pop();
                Ok(None)
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
        };
        let v = frame.eval_predicate(expr)?;
        Ok(matches!(v, Value::Bool(true)))
    }
}
