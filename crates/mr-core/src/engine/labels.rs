//! Pattern labels as small integers.
//!
//! A label — a pattern variable like `A`, or a `SUBSET` union variable like `U` — is
//! written as a string, but at run time it is only ever *compared*. Carrying the
//! string into the engine made every VM step allocate: `Bind { var: String }` was
//! cloned once per tentative row binding (not once per matched row), which a CPU
//! profile put at the top of the matcher's self time, with `malloc`/`free` at ~47%.
//! Looking a predicate up by name cost a `SipHash` of the label on the same path.
//!
//! So the label universe is fixed at bind time and everything downstream carries a
//! [`VarId`] index into it: `Inst::Char`, `Bind`, `AFTER MATCH SKIP`, the DEFINE
//! table (a `Vec` indexed by id rather than a `HashMap`), and the per-label bind
//! index. `Bind` becomes `Copy`, so binding a row is a push of two integers.
//!
//! Expressions keep their written labels. Resolving one is an exact comparison
//! against a handful of short strings — labels are canonical by the time they get
//! here (unquoted upper-cased, double-quoted verbatim), so no case folding is
//! needed — which is cheaper than hashing and happens once per expression node
//! evaluated, not once per VM step.

use std::sync::Arc;

/// Index of a label in the [`LabelSet`]. `u16` because a pattern with 65,536
/// distinct variables is not a thing, and it keeps [`crate::engine::Bind`] small.
pub type VarId = u16;

/// The labels one plan can name: its pattern variables, then its SUBSET names.
///
/// Ordering is stable and meaningful — variables first, in the order the pattern
/// declares them — so ids are reproducible for a given query.
#[derive(Debug, Clone)]
pub struct LabelSet {
    names: Arc<[String]>,
    /// `members[id]` — the ids a SUBSET covers. Empty for a plain variable, which
    /// covers only itself.
    members: Vec<Vec<VarId>>,
}

impl LabelSet {
    /// Build from the canonical label list plus each subset's member names.
    ///
    /// `subset_members` maps a subset name to its member variable names; both are
    /// already canonical (see `plan::resolve_var`). A name that is not in `names`
    /// is ignored rather than rejected — binding has already validated them.
    pub fn new(names: Vec<String>, subset_members: &[(String, Vec<String>)]) -> Self {
        let mut set = LabelSet {
            names: names.into(),
            members: Vec::new(),
        };
        set.members = vec![Vec::new(); set.names.len()];
        for (subset, member_names) in subset_members {
            if let Some(sid) = set.id_of(subset) {
                let ids: Vec<VarId> = member_names.iter().filter_map(|m| set.id_of(m)).collect();
                set.members[sid as usize] = ids;
            }
        }
        set
    }

    /// The id of a written label, if this plan knows it.
    ///
    /// Exact comparison: labels are canonicalised at bind time (unquoted upper-cased,
    /// double-quoted kept verbatim), and `"b"` is deliberately a *different* label
    /// from `B`.
    pub fn id_of(&self, name: &str) -> Option<VarId> {
        self.names
            .iter()
            .position(|n| n == name)
            .map(|i| i as VarId)
    }

    /// The written form of an id — for `CLASSIFIER`, the `classifier` output column,
    /// and error messages.
    pub fn name(&self, id: VarId) -> &str {
        &self.names[id as usize]
    }

    /// How many labels this plan has.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Whether a row bound to `bound` is covered by `label` — the same variable, or
    /// a SUBSET that lists it.
    pub fn covers(&self, label: VarId, bound: VarId) -> bool {
        label == bound || self.members[label as usize].contains(&bound)
    }
}
