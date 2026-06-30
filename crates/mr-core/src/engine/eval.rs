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
}

impl<'a> Frame<'a> {
    /// Evaluate a measure expression. `final_default` is FINAL for ONE ROW PER
    /// MATCH, RUNNING for ALL ROWS PER MATCH (overridable per-subexpr).
    pub fn eval_measure(&self, e: &Expr, final_default: bool) -> Result<Value> {
        let cur = &self.binds[self.horizon - 1];
        self.eval(e, cur.tape_pos, Some(cur.var.as_str()), final_default)
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

    fn var_at_tp(&self, tp: usize, final_sem: bool) -> Option<String> {
        self.visible(final_sem)
            .iter()
            .find(|b| b.tape_pos == tp)
            .map(|b| b.var.clone())
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
            Expr::Col(name) => self.col_value(name, cur_tp),
            Expr::Qualified(_var, col) => self.col_value(col, cur_tp),
            Expr::Classifier => Ok(cur_var
                .map(|v| Value::Str(v.to_string()))
                .unwrap_or(Value::Null)),
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
                valops::coerce(v, *ty)
            }
            Expr::Nav { kind, arg, offset } => {
                self.eval_nav(*kind, arg, *offset, cur_tp, final_sem)
            }
            Expr::Agg { kind, arg } => self.eval_agg(*kind, arg, final_sem),
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
            NavKind::Prev | NavKind::Next => {
                let target = if kind == NavKind::Prev {
                    cur_tp.checked_sub(offset)
                } else {
                    let t = cur_tp + offset;
                    if t < self.tape.len() {
                        Some(t)
                    } else {
                        None
                    }
                };
                match target {
                    Some(tp) if tp < self.tape.len() => {
                        let v = self.var_at_tp(tp, final_sem);
                        self.eval(arg, tp, v.as_deref(), final_sem)
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
                    .filter(|b| b.var.eq_ignore_ascii_case(var))
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
                Some(v) => b.var.eq_ignore_ascii_case(v),
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
