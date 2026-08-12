//! The expression evaluator over a match frame.
//!
//! A [`Frame`] captures the bindings of one (partial or complete) match plus a
//! RUNNING horizon. `eval_*` resolve column refs, navigation (PREV/NEXT physical
//! vs FIRST/LAST logical), running aggregates, CLASSIFIER / MATCH_NUMBER, and
//! RUNNING/FINAL semantics against it, with full 3-valued NULL logic.

use super::rowstore::RowStore;
use super::valops;
use crate::error::{MrError, Result};
use crate::expr::ast::{AggArg, AggKind, Expr, NavKind};
use crate::value::{Interval, Value};

/// One matched row: its position on the partition tape and the variable it
/// matched.
#[derive(Debug, Clone)]
pub struct Bind {
    pub tape_pos: usize,
    pub var: String,
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
    /// SUBSET name -> member variables. A qualifier naming a subset matches any
    /// of its members, so `U.price` and `COUNT(U.*)` range over all of them.
    pub subsets: &'a SubsetMap,
}

/// SUBSET declarations: union-variable name -> the pattern variables it covers.
pub type SubsetMap = std::collections::HashMap<String, Vec<String>>;

impl<'a> Frame<'a> {
    /// Evaluate a measure expression. `final_default` is FINAL for ONE ROW PER
    /// MATCH, RUNNING for ALL ROWS PER MATCH (overridable per-subexpr).
    pub fn eval_measure(&self, e: &Expr, final_default: bool) -> Result<Value> {
        match self.horizon.checked_sub(1).and_then(|i| self.binds.get(i)) {
            Some(cur) => self.eval(e, cur.tape_pos, Some(cur.var.as_str()), final_default),
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
        self.eval(e, cur.tape_pos, Some(cur.var.as_str()), false)
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
    fn var_at_tp(&self, tp: usize, final_sem: bool) -> Option<String> {
        let binds = self.visible(final_sem);
        binds
            .binary_search_by_key(&tp, |b| b.tape_pos)
            .ok()
            .map(|i| binds[i].var.clone())
    }

    /// Whether a row bound to `bound` is covered by the label `label` — either the
    /// same variable, or a SUBSET that lists it.
    fn label_covers(&self, label: &str, bound: &str) -> bool {
        if label == bound {
            return true;
        }
        self.subsets
            .get(label)
            .is_some_and(|members| members.iter().any(|m| m == bound))
    }

    /// The last visible bind covered by `label` (logical `LAST` of that label).
    fn last_bind_labelled(&self, label: &str, final_sem: bool) -> Option<&Bind> {
        self.visible(final_sem)
            .iter()
            .rev()
            .find(|b| self.label_covers(label, &b.var))
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
        cur_var: Option<&str>,
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
                .or_else(|| cur_var.map(|v| v.to_string()))
                .map(Value::Str)
                .unwrap_or(Value::Null)),
            // `CLASSIFIER(label)`: the last visible row covered by that label.
            Expr::Classifier(Some(label)) => {
                // Inside a FIRST/LAST/aggregate scope walk the row is already
                // pinned to a member of the label, so report that row.
                if let Some(v) = self.var_at_tp(cur_tp, final_sem) {
                    if self.label_covers(label, &v) {
                        return Ok(Value::Str(v));
                    }
                }
                Ok(self
                    .last_bind_labelled(label, final_sem)
                    .map(|b| Value::Str(b.var.clone()))
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
            Expr::Agg { kind, arg } => self.eval_agg(*kind, arg, final_sem),
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
                let scope = self.scope(inner, final_sem);
                let idx = match kind {
                    NavKind::First => *offset,
                    _ => scope.len().checked_sub(1 + *offset)?,
                };
                scope.get(idx).map(|b| (b.tape_pos, &**inner, final_sem))
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
                        let v = dominant_qualifier(read).or_else(|| self.var_at_tp(tp, sem));
                        self.eval(read, tp, v.as_deref(), sem)
                    }
                    _ => Ok(Value::Null),
                }
            }
            NavKind::First | NavKind::Last => {
                let scope = self.scope(arg, final_sem);
                if scope.is_empty() {
                    return Ok(Value::Null);
                }
                let idx = match kind {
                    NavKind::First => offset,
                    _ => {
                        if offset >= scope.len() {
                            return Ok(Value::Null);
                        }
                        scope.len() - 1 - offset
                    }
                };
                match scope.get(idx) {
                    Some(b) => self.eval(arg, b.tape_pos, Some(b.var.as_str()), final_sem),
                    None => Ok(Value::Null),
                }
            }
        }
    }

    fn eval_agg(&self, kind: AggKind, arg: &AggArg, final_sem: bool) -> Result<Value> {
        let visible = self.visible(final_sem);
        match (kind, arg) {
            (AggKind::Count, AggArg::Star) => Ok(Value::Int(visible.len() as i64)),
            (AggKind::Count, AggArg::QualStar(var)) => {
                let n = visible
                    .iter()
                    .filter(|b| self.label_covers(var, &b.var))
                    .count();
                Ok(Value::Int(n as i64))
            }
            (_, AggArg::Star) | (_, AggArg::QualStar(_)) => {
                Err(MrError::Eval("only COUNT accepts '*'".into()))
            }
            (kind, AggArg::Expr(e)) => {
                let scope = self.scope(e, final_sem);
                let mut vals: Vec<Value> = Vec::new();
                for b in &scope {
                    let v = self.eval(e, b.tape_pos, Some(b.var.as_str()), final_sem)?;
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

    /// The visible rows an aggregate / FIRST / LAST ranges over, filtered by the
    /// dominant variable qualifier of `arg` (whole match when unqualified).
    fn scope(&self, arg: &Expr, final_sem: bool) -> Vec<Bind> {
        let qual = dominant_qualifier(arg);
        self.visible(final_sem)
            .iter()
            .filter(|b| match &qual {
                Some(v) => self.label_covers(v, &b.var),
                None => true,
            })
            .cloned()
            .collect()
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
