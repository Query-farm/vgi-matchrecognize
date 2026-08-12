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
//!   the match, so the lists are [`BindIndex::build`]-ed once and queried with a
//!   binary search bounded by the horizon.
//!
//! Lookups are by label *name* against a small universe (the pattern variables plus
//! the subset names), so `slot_of` is a short linear scan of strings rather than a
//! hash. Interning labels as integer ids would remove even that.

use super::eval::{Bind, SubsetMap};

/// Per-label bind positions for one match.
#[derive(Debug, Clone, Default)]
pub struct BindIndex {
    /// The label universe: pattern variables followed by subset names.
    labels: Vec<String>,
    /// `lists[slot]` = ascending bind indices whose row is covered by `labels[slot]`.
    lists: Vec<Vec<u32>>,
}

impl BindIndex {
    /// An empty index over `labels`.
    pub fn new(labels: &[String]) -> Self {
        BindIndex {
            labels: labels.to_vec(),
            lists: vec![Vec::new(); labels.len()],
        }
    }

    /// The index for a finished bind sequence, for the emit path.
    pub fn build(labels: &[String], binds: &[Bind], subsets: &SubsetMap) -> Self {
        let mut idx = BindIndex::new(labels);
        for (i, b) in binds.iter().enumerate() {
            idx.push(i, &b.var, subsets);
        }
        idx
    }

    /// Record that bind `bind_idx` bound the variable `var`.
    ///
    /// Appends to that variable's list and to every subset that lists it, which is
    /// what makes a union variable's `LAST` as cheap as a plain one's.
    pub fn push(&mut self, bind_idx: usize, var: &str, subsets: &SubsetMap) {
        let i = bind_idx as u32;
        for (slot, label) in self.labels.iter().enumerate() {
            let covers = label == var
                || subsets
                    .get(label)
                    .is_some_and(|ms| ms.iter().any(|m| m == var));
            if covers {
                self.lists[slot].push(i);
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

    /// The slot for `label`, if it is in the universe.
    fn slot_of(&self, label: &str) -> Option<usize> {
        self.labels.iter().position(|l| l == label)
    }

    /// The greatest recorded bind index for `label` that is below `horizon`.
    ///
    /// `None` means the label has no visible bind — which is a real answer (an
    /// unbound qualifier reads as NULL), not a lookup failure. A label outside the
    /// universe also gives `None`; callers that must distinguish the two use
    /// [`BindIndex::knows`].
    pub fn last_before(&self, label: &str, horizon: usize) -> Option<usize> {
        let slot = self.slot_of(label)?;
        let list = &self.lists[slot];
        // Ascending, so the answer is the element before the first one >= horizon.
        let cut = list.partition_point(|&i| (i as usize) < horizon);
        cut.checked_sub(1).map(|i| list[i] as usize)
    }

    /// Whether this index tracks `label` at all, so a caller can tell "no visible
    /// bind" from "not indexed" and fall back rather than answer wrongly.
    pub fn knows(&self, label: &str) -> bool {
        self.slot_of(label).is_some()
    }
}
