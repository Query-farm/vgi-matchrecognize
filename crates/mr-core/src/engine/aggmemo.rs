//! Incremental aggregate state, so a running aggregate is linear in the match
//! rather than quadratic.
//!
//! Under ALL ROWS PER MATCH a match of `L` rows emits `L` output rows, and each one
//! re-evaluates every measure with the RUNNING horizon set to that row. An
//! aggregate re-folded over its whole visible scope each time costs `O(L)` per row,
//! so `O(L²)` per match: measured at 14 ns/row per row of match length for
//! `RUNNING SUM`, and 28 for a `FINAL` aggregate — which is worse *and* pure waste,
//! since a FINAL aggregate has the same value on every row of the match.
//!
//! The horizon only ever advances in that loop, and it advances by exactly one
//! bind, so the fold can be extended instead of redone: this holds one accumulator
//! per aggregate site and folds in the newly visible binds. `O(L)` per site per
//! match, and FINAL is folded once.
//!
//! # When it is safe
//!
//! Extending a fold is only sound if each bind's contribution does not itself
//! depend on the horizon. Most do not — `SUM(k)`, `SUM(A.value)`,
//! `array_agg(value || CLASSIFIER())` — but a nested navigation or aggregate can
//! (`SUM(x - LAST(y))` changes as the horizon grows). [`memoizable`] is the
//! conservative test; anything it rejects takes the original recompute path, so a
//! shape this cannot handle is slow rather than wrong.
//!
//! Keyed by the *address* of the `Expr::Agg` node. Node addresses are stable for
//! the life of the `Plan` and unique per node, which is exactly the identity
//! needed, and it keeps the AST free of numbering that only the evaluator uses.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::expr::ast::{AggArg, AggKind, Expr};
use crate::value::Value;

/// A partially folded aggregate.
#[derive(Debug, Clone)]
pub(crate) enum Acc {
    /// `COUNT` — of rows in scope, or of non-NULL argument values.
    Count(i64),
    /// `SUM`, left-associated exactly as the batch fold does, so the promotion
    /// sequence (Int -> HugeInt -> ...) is identical.
    Sum(Option<Value>),
    /// `AVG` keeps the running total and the count, and divides at the end.
    Avg { sum: f64, n: usize },
    /// `MIN` / `MAX`.
    Extreme(Option<Value>),
    /// `ARRAY_AGG` / `LIST` — the values in match order.
    List(Vec<Value>),
    /// `ARBITRARY` / `ANY_VALUE` — the first non-NULL value in scope order.
    First(Option<Value>),
}

/// One aggregate site's state.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    /// How many binds have been folded in. For a FINAL aggregate this is
    /// [`Entry::COMPLETE`], since FINAL sees the whole match at once.
    pub(crate) done: usize,
    pub(crate) acc: Acc,
}

impl Entry {
    /// `done` marker for a FINAL fold, which is never extended.
    pub(crate) const COMPLETE: usize = usize::MAX;
}

/// Per-match aggregate memo, shared by every output row of that match.
///
/// One per match — never reused across matches, since the accumulators describe a
/// specific bind sequence.
#[derive(Debug, Default)]
pub struct AggMemo {
    /// `(aggregate node address, FINAL semantics) -> state`.
    entries: RefCell<HashMap<(usize, bool), Entry>>,
}

impl AggMemo {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take this site's state out of the memo, if it has one.
    ///
    /// Removing rather than cloning matters: the accumulator can own a `Vec` (a
    /// growing `array_agg`), and cloning it per output row would reintroduce the
    /// quadratic this module exists to remove. The caller always puts it back.
    pub(crate) fn take(&self, key: (usize, bool)) -> Option<Entry> {
        self.entries.borrow_mut().remove(&key)
    }

    pub(crate) fn put(&self, key: (usize, bool), entry: Entry) {
        self.entries.borrow_mut().insert(key, entry);
    }
}

/// The identity of an aggregate site: the address of its `Expr` node.
pub(crate) fn site_of(e: &Expr) -> usize {
    e as *const Expr as usize
}

/// Whether an aggregate over `arg` can be folded incrementally — i.e. whether each
/// bind's contribution is independent of where the RUNNING horizon sits.
///
/// Rejects navigation (`PREV`/`NEXT`/`FIRST`/`LAST`), nested aggregates, explicit
/// RUNNING/FINAL switches and `CLASSIFIER(label)`, all of which resolve against the
/// visible prefix.
///
/// A qualified reference is subtler. `SUM(A.value)` is fine: the scope is filtered to
/// rows `A` covers, so `A.value` reads the row in hand. But `SUM(A.value + B.value)`
/// is scoped by its *dominant* qualifier `A`, and at an `A` row `B.value` means
/// `LAST(B)` — which moves as the horizon grows, so an earlier row's contribution
/// would be frozen at a stale value. Hence a qualified reference is accepted only
/// when its qualifier is the dominant one, which is what makes the read direct.
/// `dominant` is that qualifier, already computed by the caller.
pub(crate) fn memoizable(kind: AggKind, arg: &AggArg, dominant: Option<&str>) -> bool {
    match arg {
        // `COUNT(*)` is the visible row count — already O(1), so memoizing it would
        // only add two hash lookups per output row.
        AggArg::Star => false,
        // `COUNT(U.*)` filters the prefix, so it is worth folding. Only COUNT accepts
        // a `*` form at all; anything else is an error the recompute path must raise.
        AggArg::QualStar(_) => matches!(kind, AggKind::Count),
        AggArg::Expr(e) => horizon_independent(e, dominant),
    }
}

/// Count one row in scope, for the `COUNT(*)` / `COUNT(U.*)` forms where the row
/// itself is the contribution and there is no value to fold.
pub(crate) fn count_row(acc: &mut Acc) {
    if let Acc::Count(n) = acc {
        *n += 1;
    }
}

fn horizon_independent(e: &Expr, dominant: Option<&str>) -> bool {
    match e {
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Double(_)
        | Expr::Str(_)
        | Expr::Interval { .. }
        | Expr::MatchNumber
        | Expr::Col(_)
        // CLASSIFIER() names the row being evaluated, not the prefix.
        | Expr::Classifier(None) => true,
        // Direct read only when this is the qualifier the scope filtered on.
        Expr::Qualified(v, _) => dominant == Some(v.as_str()),
        Expr::Neg(x) | Expr::Not(x) | Expr::Cast { expr: x, .. } => {
            horizon_independent(x, dominant)
        }
        Expr::Binary { lhs, rhs, .. } => {
            horizon_independent(lhs, dominant) && horizon_independent(rhs, dominant)
        }
        Expr::Call { args, .. } => args.iter().all(|a| horizon_independent(a, dominant)),
        // Everything else resolves against the visible prefix.
        _ => false,
    }
}

/// Fold one more value into `acc`.
pub(crate) fn accumulate(acc: &mut Acc, v: &Value, min: bool) -> crate::error::Result<()> {
    use crate::error::MrError;
    match acc {
        Acc::Count(n) => *n += 1,
        Acc::Sum(cur) => {
            *cur = Some(match cur.take() {
                None => v.clone(),
                Some(prev) => {
                    crate::engine::valops::binary(crate::expr::ast::BinOp::Add, &prev, v)?
                }
            });
        }
        Acc::Avg { sum, n } => {
            *sum += v
                .as_f64()
                .ok_or_else(|| MrError::Eval(format!("AVG of non-numeric {v:?}")))?;
            *n += 1;
        }
        Acc::Extreme(best) => {
            let take = match best.as_ref() {
                None => true,
                Some(b) => valops_takes(v, b, min),
            };
            if take {
                *best = Some(v.clone());
            }
        }
        Acc::List(vals) => vals.push(v.clone()),
        Acc::First(first) => {
            if first.is_none() {
                *first = Some(v.clone());
            }
        }
    }
    Ok(())
}

fn valops_takes(candidate: &Value, best: &Value, min: bool) -> bool {
    match crate::engine::valops::compare(candidate, best) {
        Some(std::cmp::Ordering::Less) => min,
        Some(std::cmp::Ordering::Greater) => !min,
        _ => false,
    }
}

/// The accumulator an aggregate kind starts from.
pub(crate) fn empty_acc(kind: AggKind) -> Acc {
    match kind {
        AggKind::Count => Acc::Count(0),
        AggKind::Sum => Acc::Sum(None),
        AggKind::Avg => Acc::Avg { sum: 0.0, n: 0 },
        AggKind::Min | AggKind::Max => Acc::Extreme(None),
        AggKind::ArrayAgg => Acc::List(Vec::new()),
        AggKind::Arbitrary => Acc::First(None),
    }
}

/// The aggregate's value given its accumulator. Matches the batch folds exactly,
/// including "NULL over no rows" for everything except `ARRAY_AGG`, which is the
/// empty list — that difference is what makes a RUNNING `array_agg` grow row by row
/// instead of jumping from NULL.
pub(crate) fn finish(acc: &Acc) -> Value {
    match acc {
        Acc::Count(n) => Value::Int(*n),
        Acc::Sum(v) | Acc::Extreme(v) | Acc::First(v) => v.clone().unwrap_or(Value::Null),
        Acc::Avg { sum, n } => {
            if *n == 0 {
                Value::Null
            } else {
                Value::Double(sum / *n as f64)
            }
        }
        Acc::List(vals) => Value::List(vals.clone()),
    }
}
