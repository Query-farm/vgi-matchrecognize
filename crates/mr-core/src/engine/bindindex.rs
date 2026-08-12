//! Where each pattern label was last bound, without scanning the match.
//!
//! `LAST(A.x)` — which is what a bare `A.x` means — asks for the most recent visible
//! row bound to `A`. Answering that by scanning `binds` backwards costs the distance
//! to that row, and in the shape that matters it is the whole match: a predicate like
//! `DEFINE B: k >= A.k` over `A B*` binds `A` once at the start and then evaluates the
//! predicate at every subsequent row, so each evaluation scans the entire prefix.
//! Measured at 1.9 ns/row per row of match length — i.e. quadratic in the match.
//!
//! This keeps, per label, the ascending list of bind indices where a covering row was
//! bound. "Covering" is subset-aware: a row bound to `A` is covered by `A` and by any
//! SUBSET listing `A`, so it appears in both lists.
//!
//! Two access patterns, both supported:
//!
//! - **While matching**, `binds` grows one row at a time and shrinks on backtracking,
//!   so the index is maintained incrementally: [`BindIndex::push`] appends and
//!   [`BindIndex::truncate`] pops the tails. The horizon there is always the whole
//!   prefix, so the answer is the last element of a list.
//! - **While emitting**, the binds are already final and the horizon ascends through
//!   the match, so the lists are [`BindIndex::refill`]-ed once and queried with a
//!   binary search bounded by the horizon.
//!
//! Lookups are by [`VarId`], so finding a label's list is an array index.

use super::eval::Bind;
use super::labels::{LabelSet, VarId};

/// Per-label bind positions for one match.
#[derive(Debug, Clone, Default)]
pub struct BindIndex {
    /// `lists[label]` = ascending bind indices whose row that label covers.
    /// Indexed by [`VarId`], so a lookup is an array index.
    lists: Vec<Vec<u32>>,
}

impl BindIndex {
    /// An empty index over a plan's labels.
    pub fn new(labels: &LabelSet) -> Self {
        BindIndex {
            lists: vec![Vec::new(); labels.len()],
        }
    }

    /// Refill for a finished bind sequence, keeping the allocated lists.
    ///
    /// The emit path calls this once per match and reuses one index across the whole
    /// partition — building a fresh one per match allocated a `Vec` per label per
    /// match, which cost 36% on a partition of many tiny matches.
    pub fn refill(&mut self, binds: &[Bind], labels: &LabelSet) {
        self.clear();
        for (i, b) in binds.iter().enumerate() {
            self.push(i, b.var, labels);
        }
    }

    /// Record that bind `bind_idx` bound the variable `var`.
    ///
    /// Appends to that variable's list and to every subset that lists it, which is
    /// what makes a union variable's `LAST` as cheap as a plain one's.
    pub fn push(&mut self, bind_idx: usize, var: VarId, labels: &LabelSet) {
        let i = bind_idx as u32;
        for label in 0..self.lists.len() as VarId {
            if labels.covers(label, var) {
                self.lists[label as usize].push(i);
            }
        }
    }

    /// Drop every recorded position at or beyond `binds_len`, mirroring
    /// `binds.truncate(binds_len)` when the matcher restores an alternative.
    pub fn truncate(&mut self, binds_len: usize) {
        let limit = binds_len as u32;
        for list in &mut self.lists {
            while list.last().is_some_and(|&i| i >= limit) {
                list.pop();
            }
        }
    }

    /// Forget everything, keeping the allocated lists for the next start position.
    pub fn clear(&mut self) {
        for list in &mut self.lists {
            list.clear();
        }
    }

    /// The greatest recorded bind index for `label` that is below `horizon`.
    ///
    /// `None` means the label has no visible bind — which is a real answer (an
    /// unbound qualifier reads as NULL), not a lookup failure. A label outside the
    /// universe also gives `None`; callers that must distinguish the two use
    /// [`BindIndex::knows`].
    pub fn last_before(&self, label: VarId, horizon: usize) -> Option<usize> {
        let list = self.lists.get(label as usize)?;
        // Ascending, so the answer is the element before the first one >= horizon.
        let cut = list.partition_point(|&i| (i as usize) < horizon);
        cut.checked_sub(1).map(|i| list[i] as usize)
    }
}
