//! The expression evaluator over a match frame.
//!
//! A [`Frame`] captures the bindings of one (partial or complete) match plus a
//! RUNNING horizon. `eval_*` resolve column refs, navigation (PREV/NEXT physical
//! vs FIRST/LAST logical), running aggregates, CLASSIFIER / MATCH_NUMBER, and
//! RUNNING/FINAL semantics against it, with full 3-valued NULL logic.

use super::aggmemo::{self, AggMemo};
use super::bindindex::BindIndex;
use super::labels::{LabelSet, VarId};
use super::rowstore::RowStore;
use super::valops;
use crate::error::{MrError, Result};
use crate::expr::ast::{AggArg, AggKind, Expr, NavKind};
use crate::value::{Interval, Value};

/// One matched row: its position on the partition tape and the variable it
/// matched.
///
/// `Copy`, and small: the matcher pushes one of these per tentative binding, so a
/// heap-allocated label here cost an allocation per VM step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bind {
    pub tape_pos: usize,
    pub var: VarId,
}

/// An evaluation frame: the store, the partition tape (sorted row indices), the
/// matched rows so far, the RUNNING horizon, and the match ordinal.
pub struct Frame<'a> {
    pub store: &'a dyn RowStore,
    pub tape: &'a [usize],
    pub binds: &'a [Bind],
    /// Number of binds visible under RUNNING semantics (current index + 1).
    pub horizon: usize,
    pub match_number: i64,
    /// The plan's labels: id <-> name, plus which variables each SUBSET covers, so
    /// `U.price` and `COUNT(U.*)` range over all of them.
    pub labels: &'a LabelSet,
    /// Where each label was last bound, so `LAST(A.x)` is a lookup rather than a
    /// backwards scan of the match. `None` falls back to the scan.
    pub label_index: Option<&'a BindIndex>,
    /// Incremental aggregate state shared by every output row of one match, so a
    /// RUNNING aggregate is folded once across the match instead of re-folded per
    /// row. `None` disables it (the matcher's frames, where the horizon jumps
    /// around under backtracking, and single-row emits that have nothing to reuse).
    pub agg_memo: Option<&'a AggMemo>,
}

impl<'a> Frame<'a> {
    /// Evaluate a measure expression. `final_default` is FINAL for ONE ROW PER
    /// MATCH, RUNNING for ALL ROWS PER MATCH (overridable per-subexpr).
    pub fn eval_measure(&self, e: &Expr, final_default: bool) -> Result<Value> {
        match self.horizon.checked_sub(1).and_then(|i| self.binds.get(i)) {
            Some(cur) => self.eval(e, cur.tape_pos, Some(cur.var), final_default),
            // An empty match binds no rows, so it has no current row: every
            // row-dependent reference is NULL (see `is_empty_match`). The tape
            // position passed here is never read.
            None => self.eval(e, 0, None, final_default),
        }
    }

    /// Whether this frame describes an **empty match** — one that bound no rows.
    ///
    /// The standard still reports empty matches (they get a match number, and
    /// `COUNT(*)` over one is 0), but nothing is bound, so there is no current row
    /// for a column reference or a physical navigation to resolve against.
    fn is_empty_match(&self) -> bool {
        self.binds.is_empty()
    }

    /// Evaluate a DEFINE predicate (always RUNNING; current = last bind).
    pub fn eval_predicate(&self, e: &Expr) -> Result<Value> {
        let cur = &self.binds[self.horizon - 1];
        self.eval(e, cur.tape_pos, Some(cur.var), false)
    }

    fn visible(&self, final_sem: bool) -> &[Bind] {
        if final_sem {
            self.binds
        } else {
            &self.binds[..self.horizon]
        }
    }

    /// The variable bound at tape position `tp`, if that row is in the match.
    ///
    /// Binary search, not a scan: a match consumes rows left to right, so `binds`
    /// is strictly increasing in `tape_pos`. This is on the hot path — every
    /// PREV/NEXT evaluation calls it — so a linear scan here made a DEFINE
    /// predicate like `x <= PREV(x) + 1` quadratic in the match length.
    fn var_at_tp(&self, tp: usize, final_sem: bool) -> Option<VarId> {
        let binds = self.visible(final_sem);
        binds
            .binary_search_by_key(&tp, |b| b.tape_pos)
            .ok()
            .map(|i| binds[i].var)
    }

    /// Whether a row bound to `bound` is covered by the written label `label` —
    /// either the same variable, or a SUBSET that lists it. `false` when the plan
    /// does not know the label at all, which binding has already ruled out.
    fn label_covers(&self, label: &str, bound: VarId) -> bool {
        self.labels
            .id_of(label)
            .is_some_and(|id| self.labels.covers(id, bound))
    }

    /// The last visible bind covered by `label` (logical `LAST` of that label).
    ///
    /// Uses the per-label index when one is available: scanning backwards costs the
    /// distance to the label's last row, which for a variable bound once at the start
    /// of a long match is the whole match, on every evaluation.
    fn last_bind_labelled(&self, label: &str, final_sem: bool) -> Option<&Bind> {
        let visible = self.visible(final_sem);
        let id = self.labels.id_of(label)?;
        if let Some(idx) = self.label_index {
            return idx
                .last_before(id, visible.len())
                .and_then(|i| visible.get(i));
        }
        visible.iter().rev().find(|b| self.labels.covers(id, b.var))
    }

    /// Tape position of the last visible row bound to `var` (logical `LAST`).
    fn last_bind_of(&self, var: &str, final_sem: bool) -> Option<usize> {
        self.last_bind_labelled(var, final_sem).map(|b| b.tape_pos)
    }

    fn col_value(&self, name: &str, tp: usize) -> Result<Value> {
        let idx = self
            .store
            .col_index(name)
            .ok_or_else(|| MrError::Eval(format!("unknown column '{name}'")))?;
        Ok(self.store.cell(self.tape[tp], idx))
    }

    fn eval(
        &self,
        e: &Expr,
        cur_tp: usize,
        cur_var: Option<VarId>,
        final_sem: bool,
    ) -> Result<Value> {
        match e {
            Expr::Null => Ok(Value::Null),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Int(i) => Ok(Value::Int(*i)),
            Expr::Double(d) => Ok(Value::Double(*d)),
            Expr::Str(s) => Ok(Value::Str(s.clone())),
            Expr::Interval {
                months,
                days,
                nanos,
            } => Ok(Value::Interval(Interval {
                months: *months,
                days: *days,
                nanos: *nanos,
            })),
            Expr::Col(name) if self.is_empty_match() => {
                // No current row to read from, so an empty match yields NULL for
                // any column reference. Resolve the name anyway, so a typo is
                // still an error rather than a silent NULL.
                self.store
                    .col_index(name)
                    .ok_or_else(|| MrError::Eval(format!("unknown column '{name}'")))?;
                Ok(Value::Null)
            }
            Expr::Col(name) => self.col_value(name, cur_tp),
            // A bare `VAR.col` used outside a navigation/aggregate call is
            // shorthand for `LAST(VAR.col)` under the prevailing RUNNING/FINAL
            // semantics — NULL when no row is bound to `VAR` yet. When the row
            // being evaluated is itself bound to `VAR` the read is direct; that
            // is how FIRST/LAST/aggregate scopes and qualified PREV/NEXT pin
            // their row, and it also makes `B.col` inside `DEFINE[B]` mean the
            // candidate row.
            Expr::Qualified(var, col) => {
                if cur_var.is_some_and(|cv| self.label_covers(var, cv)) {
                    self.col_value(col, cur_tp)
                } else {
                    match self.last_bind_of(var, final_sem) {
                        Some(tp) => self.col_value(col, tp),
                        None => Ok(Value::Null),
                    }
                }
            }
            // The classifier of the row this reference resolves to. Reading it
            // from the tape rather than `cur_var` keeps it correct when a
            // navigation has pinned `cur_var` to a qualifier.
            Expr::Classifier(None) => Ok(self
                .var_at_tp(cur_tp, final_sem)
                .or(cur_var)
                .map(|id| Value::Str(self.labels.name(id).to_string()))
                .unwrap_or(Value::Null)),
            // `CLASSIFIER(label)`: the last visible row covered by that label.
            Expr::Classifier(Some(label)) => {
                // Inside a FIRST/LAST/aggregate scope walk the row is already
                // pinned to a member of the label, so report that row.
                if let Some(v) = self.var_at_tp(cur_tp, final_sem) {
                    if self.label_covers(label, v) {
                        return Ok(Value::Str(self.labels.name(v).to_string()));
                    }
                }
                Ok(self
                    .last_bind_labelled(label, final_sem)
                    .map(|b| Value::Str(self.labels.name(b.var).to_string()))
                    .unwrap_or(Value::Null))
            }
            Expr::MatchNumber => Ok(Value::Int(self.match_number)),
            Expr::RunningFinal { final_, inner } => self.eval(inner, cur_tp, cur_var, *final_),
            Expr::Neg(x) => valops::negate(&self.eval(x, cur_tp, cur_var, final_sem)?),
            Expr::Not(x) => {
                let v = self.eval(x, cur_tp, cur_var, final_sem)?;
                Ok(match v.as_bool() {
                    Some(b) => Value::Bool(!b),
                    None => Value::Null,
                })
            }
            Expr::Binary { op, lhs, rhs } => {
                let l = self.eval(lhs, cur_tp, cur_var, final_sem)?;
                let r = self.eval(rhs, cur_tp, cur_var, final_sem)?;
                valops::binary(*op, &l, &r)
            }
            Expr::IsNull { expr, negated } => {
                let v = self.eval(expr, cur_tp, cur_var, final_sem)?;
                Ok(Value::Bool(v.is_null() != *negated))
            }
            Expr::Between {
                expr,
                lo,
                hi,
                negated,
            } => {
                let v = self.eval(expr, cur_tp, cur_var, final_sem)?;
                let lo = self.eval(lo, cur_tp, cur_var, final_sem)?;
                let hi = self.eval(hi, cur_tp, cur_var, final_sem)?;
                let ge = valops::binary(crate::expr::ast::BinOp::Ge, &v, &lo)?;
                let le = valops::binary(crate::expr::ast::BinOp::Le, &v, &hi)?;
                let res = valops::binary(crate::expr::ast::BinOp::And, &ge, &le)?;
                Ok(match (res.as_bool(), negated) {
                    (Some(b), true) => Value::Bool(!b),
                    (Some(b), false) => Value::Bool(b),
                    (None, _) => Value::Null,
                })
            }
            Expr::In {
                expr,
                list,
                negated,
            } => {
                let v = self.eval(expr, cur_tp, cur_var, final_sem)?;
                if v.is_null() {
                    return Ok(Value::Null);
                }
                let mut saw_null = false;
                for item in list {
                    let iv = self.eval(item, cur_tp, cur_var, final_sem)?;
                    if iv.is_null() {
                        saw_null = true;
                        continue;
                    }
                    if valops::compare(&v, &iv) == Some(std::cmp::Ordering::Equal) {
                        return Ok(Value::Bool(!*negated));
                    }
                }
                if saw_null {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Bool(*negated))
                }
            }
            Expr::Cast { expr, ty } => {
                let v = self.eval(expr, cur_tp, cur_var, final_sem)?;
                valops::coerce(v, ty)
            }
            Expr::Call { name, args } => {
                let vals = args
                    .iter()
                    .map(|a| self.eval(a, cur_tp, cur_var, final_sem))
                    .collect::<Result<Vec<_>>>()?;
                super::scalar::call(name, &vals)
            }
            Expr::Nav { kind, arg, offset } => {
                self.eval_nav(*kind, arg, *offset, cur_tp, final_sem)
            }
            Expr::Agg { kind, arg } => {
                // Try the incremental path first; it declines (returning None) for
                // shapes whose per-row contribution depends on the horizon.
                match self.eval_agg_memoized(e, *kind, arg, final_sem)? {
                    Some(v) => Ok(v),
                    None => self.eval_agg(*kind, arg, final_sem),
                }
            }
        }
    }

    /// Where a physical navigation (`PREV`/`NEXT`) starts, what to read there, and
    /// under which RUNNING/FINAL semantics — `None` when no such row exists.
    ///
    /// Navigation **nests**: `PREV(LAST(x), n)` means "find the row `LAST(x)`
    /// designates, then step back `n` physical rows and read `x` there". So the
    /// inner *logical* navigation picks the anchor and the outer *physical* offset
    /// applies to it. Resolving the argument as a whole instead would silently
    /// discard the offset, because `LAST` re-derives its own row from the frame.
    ///
    /// A bare `PREV(x, n)` anchors on the current row, and a bare qualifier
    /// (`PREV(A.x, n)`) anchors on A's last row — the standard's shorthand for
    /// `PREV(LAST(A.x), n)`.
    fn nav_anchor<'e>(
        &self,
        arg: &'e Expr,
        cur_tp: usize,
        final_sem: bool,
    ) -> Option<(usize, &'e Expr, bool)> {
        match arg {
            Expr::RunningFinal { final_, inner } => self.nav_anchor(inner, cur_tp, *final_),
            Expr::Nav {
                kind: kind @ (NavKind::First | NavKind::Last),
                arg: inner,
                offset,
            } => {
                let qual = dominant_qualifier(inner);
                let from_end = matches!(kind, NavKind::Last);
                self.scope_nth(qual.as_deref(), final_sem, *offset, from_end)
                    .map(|b| (b.tape_pos, &**inner, final_sem))
            }
            // Nested physical navigation, e.g. `PREV(NEXT(x))`: resolve the inner
            // one to a row, then let the caller apply the outer offset.
            Expr::Nav {
                kind,
                arg: inner,
                offset,
            } => {
                let (anchor, read, sem) = self.nav_anchor(inner, cur_tp, final_sem)?;
                let tp = if *kind == NavKind::Prev {
                    anchor.checked_sub(*offset)?
                } else {
                    anchor + offset
                };
                (tp < self.tape.len()).then_some((tp, read, sem))
            }
            _ => match dominant_qualifier(arg) {
                Some(v) => self
                    .last_bind_of(&v, final_sem)
                    .map(|tp| (tp, arg, final_sem)),
                None => Some((cur_tp, arg, final_sem)),
            },
        }
    }

    fn eval_nav(
        &self,
        kind: NavKind,
        arg: &Expr,
        offset: usize,
        cur_tp: usize,
        final_sem: bool,
    ) -> Result<Value> {
        match kind {
            // Physical navigation from an empty match has no row to start from.
            NavKind::Prev | NavKind::Next if self.is_empty_match() => Ok(Value::Null),
            NavKind::Prev | NavKind::Next => {
                // Physical navigation applies its offset to an *anchor* row, then
                // reads there. See `nav_anchor` for how the anchor is chosen.
                let Some((anchor, read, sem)) = self.nav_anchor(arg, cur_tp, final_sem) else {
                    return Ok(Value::Null);
                };
                let target = if kind == NavKind::Prev {
                    anchor.checked_sub(offset)
                } else {
                    Some(anchor + offset)
                };
                match target {
                    Some(tp) if tp < self.tape.len() => {
                        // Pin `cur_var` to the qualifier so reading a qualified
                        // column at the navigated row is a physical read, rather
                        // than resolving to LAST all over again.
                        // The qualifier written in the expression wins, so a
                        // qualified read at the navigated row stays a physical read;
                        // otherwise use whatever that row is actually bound to.
                        let v = dominant_qualifier(read)
                            .and_then(|q| self.labels.id_of(&q))
                            .or_else(|| self.var_at_tp(tp, sem));
                        self.eval(read, tp, v, sem)
                    }
                    _ => Ok(Value::Null),
                }
            }
            NavKind::First | NavKind::Last => {
                // `FIRST(x, n)` is the n-th row of the scope from the start,
                // `LAST(x, n)` the n-th from the end; off the end is NULL.
                let qual = dominant_qualifier(arg);
                let from_end = matches!(kind, NavKind::Last);
                match self.scope_nth(qual.as_deref(), final_sem, offset, from_end) {
                    Some(b) => {
                        let (tp, var) = (b.tape_pos, b.var);
                        self.eval(arg, tp, Some(var), final_sem)
                    }
                    None => Ok(Value::Null),
                }
            }
        }
    }

    /// Extend this aggregate site's fold to the current horizon, if the site can be
    /// folded incrementally and a memo is available. Returns `None` when the caller
    /// should just recompute from scratch.
    ///
    /// The horizon advances monotonically through an ALL ROWS emit loop, so the work
    /// per output row is the *newly visible* binds rather than all of them. A
    /// non-monotonic call (a smaller horizon than already folded) falls back rather
    /// than trying to un-fold.
    fn eval_agg_memoized(
        &self,
        node: &Expr,
        kind: AggKind,
        arg: &AggArg,
        final_sem: bool,
    ) -> Result<Option<Value>> {
        let Some(memo) = self.agg_memo else {
            return Ok(None);
        };
        // The dominant qualifier is a property of the expression, not of the row, so
        // resolve it once: it decides both the scope filter and whether the shape can
        // be folded incrementally at all.
        let qual = match arg {
            AggArg::Expr(e) => dominant_qualifier(e),
            _ => None,
        };
        if !aggmemo::memoizable(kind, arg, qual.as_deref()) {
            return Ok(None);
        }
        let key = (aggmemo::site_of(node), final_sem);
        // FINAL sees the whole match, so it is folded once and then reused for every
        // remaining output row of the match.
        let target = if final_sem {
            aggmemo::Entry::COMPLETE
        } else {
            self.horizon
        };
        let mut entry = match memo.take(key) {
            // Already complete: read it and put it straight back. `take` removes the
            // entry, so returning early without re-inserting would make every later
            // row re-fold the whole match — the quadratic, restored by accident.
            Some(e) if e.done == aggmemo::Entry::COMPLETE => {
                let out = aggmemo::finish(&e.acc);
                memo.put(key, e);
                return Ok(Some(out));
            }
            Some(e) if e.done <= self.horizon => e,
            // Either nothing folded yet, or the horizon moved backwards.
            _ => aggmemo::Entry {
                done: 0,
                acc: aggmemo::empty_acc(kind),
            },
        };
        let from = entry.done;
        let upto = if final_sem {
            self.binds.len()
        } else {
            self.horizon
        };
        let min = matches!(kind, AggKind::Min);
        for b in &self.binds[from..upto] {
            match arg {
                // `COUNT(*)` counts rows; `COUNT(U.*)` counts the rows the union
                // variable covers. There is no value to fold in either case.
                AggArg::Star => aggmemo::count_row(&mut entry.acc),
                AggArg::QualStar(v) => {
                    if self.label_covers(v, b.var) {
                        aggmemo::count_row(&mut entry.acc);
                    }
                }
                AggArg::Expr(e) => {
                    // The same scope filter `scope()` applies: an aggregate ranges
                    // over the rows its dominant qualifier covers.
                    let in_scope = match &qual {
                        Some(v) => self.label_covers(v, b.var),
                        None => true,
                    };
                    if in_scope {
                        let v = self.eval(e, b.tape_pos, Some(b.var), final_sem)?;
                        // Every aggregate skips NULLs, and COUNT(expr) counts the
                        // non-NULL values.
                        if !v.is_null() {
                            aggmemo::accumulate(&mut entry.acc, &v, min)?;
                        }
                    }
                }
            }
        }
        entry.done = target;
        let out = aggmemo::finish(&entry.acc);
        memo.put(key, entry);
        Ok(Some(out))
    }

    fn eval_agg(&self, kind: AggKind, arg: &AggArg, final_sem: bool) -> Result<Value> {
        let visible = self.visible(final_sem);
        match (kind, arg) {
            (AggKind::Count, AggArg::Star) => Ok(Value::Int(visible.len() as i64)),
            (AggKind::Count, AggArg::QualStar(var)) => {
                let n = visible
                    .iter()
                    .filter(|b| self.label_covers(var, b.var))
                    .count();
                Ok(Value::Int(n as i64))
            }
            (_, AggArg::Star) | (_, AggArg::QualStar(_)) => {
                Err(MrError::Eval("only COUNT accepts '*'".into()))
            }
            (kind, AggArg::Expr(e)) => {
                // The recompute path, for shapes `aggmemo` declines. Still no
                // materialized scope: only the surviving values are collected.
                let qual = dominant_qualifier(e);
                let rows: Vec<Bind> = self
                    .scope_iter(qual.as_deref(), final_sem)
                    .copied()
                    .collect();
                let mut vals: Vec<Value> = Vec::new();
                for b in &rows {
                    let v = self.eval(e, b.tape_pos, Some(b.var), final_sem)?;
                    if !v.is_null() {
                        vals.push(v);
                    }
                }
                match kind {
                    AggKind::Count => Ok(Value::Int(vals.len() as i64)),
                    AggKind::Sum => fold_sum(&vals),
                    AggKind::Avg => fold_avg(&vals),
                    AggKind::Min => fold_extreme(&vals, true),
                    AggKind::Max => fold_extreme(&vals, false),
                    // The matched values in match order. Unlike the other
                    // aggregates this is empty rather than NULL over no rows,
                    // which is what makes a RUNNING array_agg grow row by row.
                    AggKind::ArrayAgg => Ok(Value::List(vals)),
                    AggKind::Arbitrary => Ok(vals.into_iter().next().unwrap_or(Value::Null)),
                }
            }
        }
    }

    /// The visible rows an aggregate / FIRST / LAST ranges over: the visible prefix,
    /// filtered by the dominant variable qualifier (the whole prefix when
    /// unqualified).
    ///
    /// An iterator, not a `Vec`: this used to clone every bind in scope — one heap
    /// `String` per matched row — on every call, which made a single `LAST(x)` under
    /// ALL ROWS cost O(match length) per output row.
    fn scope_iter<'s>(
        &'s self,
        qual: Option<&'s str>,
        final_sem: bool,
    ) -> impl Iterator<Item = &'s Bind> + 's {
        self.visible(final_sem).iter().filter(move |b| match qual {
            Some(v) => self.label_covers(v, b.var),
            None => true,
        })
    }

    /// The `n`-th row of that scope, counted from the start or from the end.
    ///
    /// Unqualified scopes are index arithmetic on the visible prefix — O(1), which is
    /// what makes `LAST(x)` and `FIRST(x)` flat in match length. A qualified scope
    /// walks from the chosen end and stops at the row it wants, so it costs the
    /// distance to that row rather than the whole match.
    fn scope_nth(
        &self,
        qual: Option<&str>,
        final_sem: bool,
        n: usize,
        from_end: bool,
    ) -> Option<&Bind> {
        let visible = self.visible(final_sem);
        match qual {
            None if from_end => visible
                .len()
                .checked_sub(1 + n)
                .and_then(|i| visible.get(i)),
            None => visible.get(n),
            Some(v) if from_end => visible
                .iter()
                .rev()
                .filter(|b| self.label_covers(v, b.var))
                .nth(n),
            Some(v) => visible
                .iter()
                .filter(|b| self.label_covers(v, b.var))
                .nth(n),
        }
    }
}

/// The first variable qualifier appearing in `e` (the aggregate/navigation
/// scope selector), if any.
fn dominant_qualifier(e: &Expr) -> Option<String> {
    match e {
        Expr::Qualified(v, _) => Some(v.clone()),
        Expr::Neg(x) | Expr::Not(x) | Expr::Cast { expr: x, .. } => dominant_qualifier(x),
        Expr::RunningFinal { inner, .. } => dominant_qualifier(inner),
        Expr::Binary { lhs, rhs, .. } => {
            dominant_qualifier(lhs).or_else(|| dominant_qualifier(rhs))
        }
        Expr::Nav { arg, .. } => dominant_qualifier(arg),
        Expr::Call { args, .. } => args.iter().find_map(dominant_qualifier),
        Expr::Classifier(label) => label.clone(),
        Expr::Agg {
            arg: AggArg::Expr(x),
            ..
        } => dominant_qualifier(x),
        Expr::Agg {
            arg: AggArg::QualStar(v),
            ..
        } => Some(v.clone()),
        _ => None,
    }
}

fn fold_sum(vals: &[Value]) -> Result<Value> {
    if vals.is_empty() {
        return Ok(Value::Null);
    }
    let mut acc = vals[0].clone();
    for v in &vals[1..] {
        acc = valops::binary(crate::expr::ast::BinOp::Add, &acc, v)?;
    }
    Ok(acc)
}

fn fold_avg(vals: &[Value]) -> Result<Value> {
    if vals.is_empty() {
        return Ok(Value::Null);
    }
    let mut sum = 0.0f64;
    for v in vals {
        sum += v
            .as_f64()
            .ok_or_else(|| MrError::Eval(format!("AVG of non-numeric {v:?}")))?;
    }
    Ok(Value::Double(sum / vals.len() as f64))
}

fn fold_extreme(vals: &[Value], min: bool) -> Result<Value> {
    if vals.is_empty() {
        return Ok(Value::Null);
    }
    let mut best = vals[0].clone();
    for v in &vals[1..] {
        if let Some(ord) = valops::compare(v, &best) {
            let take = if min {
                ord == std::cmp::Ordering::Less
            } else {
                ord == std::cmp::Ordering::Greater
            };
            if take {
                best = v.clone();
            }
        }
    }
    Ok(best)
}
