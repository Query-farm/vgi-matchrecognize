//! The end-to-end plan: parse + type-check the whole `match_recognize` call at
//! bind time (producing the output column layout), then run the matcher over a
//! [`RowStore`] at produce time (producing output rows).
//!
//! This is the single entry point `mr-worker` drives: [`Plan::build`] in
//! `on_bind` and [`Plan::run`] in `produce`.

use std::collections::HashMap;

use crate::engine::labels::{LabelSet, VarId};
use crate::engine::matcher::{AfterSkip, Matcher};
use crate::engine::{AggMemo, BindIndex, Frame, Match, RowStore};
use crate::error::{MrError, Result};
use crate::expr::ast::{AggArg, Expr};
use crate::expr::parser::{parse as parse_expr, parse_type_name};
use crate::pattern::compile::{compile, Program};
use crate::pattern::parser::{parse as parse_pattern, Pattern};
use crate::rows::RowBuf;
use crate::types::{infer, BindSchema, Ty};
use crate::value::Value;

/// Floor for the automatic step budget — enough to catch catastrophic
/// backtracking on a small partition, where a per-row allowance would be tiny.
const MIN_AUTO_STEP_BUDGET: i64 = 5_000_000;

/// Steps allowed per row in the automatic budget. A linear scan costs a handful
/// of steps per row (one per VM instruction touched: roughly `Split`/`Char`/`Jmp`
/// for a simple quantifier), so this leaves ample headroom for elaborate patterns
/// while still being blown through quickly by anything super-linear.
const AUTO_STEPS_PER_ROW: i64 = 128;

/// The default per-partition step budget for a partition of `rows` rows.
///
/// The budget exists to stop *catastrophic backtracking*, which is super-linear in
/// the partition size — so a fixed constant is the wrong shape: it lets a
/// pathological pattern run a long time on a small partition while cutting off a
/// perfectly ordinary linear scan of a large one. Scaling per row keeps legitimate
/// long matches (a sessionization whose partition is one long session) working at
/// any size, and still trips on quadratic-or-worse behaviour.
pub fn auto_step_budget(rows: usize) -> i64 {
    (rows as i64)
        .saturating_mul(AUTO_STEPS_PER_ROW)
        .max(MIN_AUTO_STEP_BUDGET)
}

/// Raw, string-form arguments to `match_recognize` (as they arrive from SQL).
#[derive(Debug, Clone)]
pub struct PlanConfig {
    pub pattern: String,
    pub define_json: String,
    pub measures_json: Option<String>,
    pub partition_by: Vec<String>,
    /// Input columns to carry through to the output unchanged, alongside the
    /// partition keys. Empty means none.
    pub include: Vec<String>,
    /// Order-by specs, each `"col"`, `"col DESC"`, `"col NULLS FIRST"`, etc.
    pub order_by: Vec<String>,
    /// `true` = ALL ROWS PER MATCH, `false` = ONE ROW PER MATCH.
    pub rows_all: bool,
    /// ALL ROWS PER MATCH **OMIT EMPTY MATCHES**: drop the row an empty match
    /// would contribute. Ignored for ONE ROW PER MATCH, which always reports
    /// empty matches. Default `false` (SQL:2016 SHOW EMPTY MATCHES).
    pub omit_empty_matches: bool,
    /// SUBSET declarations as a JSON object mapping a union-variable name to its
    /// member pattern variables, e.g. `{"U": ["A", "B"]}`. Empty means none.
    pub subset_json: String,
    /// Raw AFTER MATCH SKIP string (`"past last row"`, `"to first A"`, …).
    pub after: String,
    /// Per-partition matcher step budget. `None` scales it with the partition's
    /// row count (see [`auto_step_budget`]); `Some(n)` pins it to `n`.
    pub step_budget: Option<i64>,
}

/// One sort key.
#[derive(Debug, Clone)]
struct OrderKey {
    col: String,
    desc: bool,
    nulls_first: bool,
}

/// A measure: output name, expression, and the type the column will carry.
#[derive(Debug, Clone)]
pub struct Measure {
    pub name: String,
    pub expr: Expr,
    pub ty: Ty,
}

/// One output column.
#[derive(Debug, Clone)]
pub struct OutputColumn {
    pub name: String,
    pub ty: Ty,
}

/// A fully bound, executable plan.
pub struct Plan {
    program: Program,
    /// DEFINE predicates indexed by label id. `None` = undefined, i.e. always true.
    define: Vec<Option<Expr>>,
    measures: Vec<Measure>,
    partition_by: Vec<String>,
    /// Passthrough columns, in call order, with anything already emitted as a
    /// partition or order key removed.
    include: Vec<String>,
    order_by: Vec<OrderKey>,
    rows_all: bool,
    omit_empty_matches: bool,
    after: AfterSkip,
    step_budget: Option<i64>,
    /// Auto `match_number` column emitted (ALL ROWS, not shadowed by a measure).
    auto_match_number: bool,
    /// Auto `classifier` column emitted (ALL ROWS, not shadowed by a measure).
    auto_classifier: bool,
    output_columns: Vec<OutputColumn>,
    /// The labels a qualifier may name — pattern variables then subset names — with
    /// their ids and subset membership. Everything downstream carries ids.
    labels: LabelSet,
}

impl Plan {
    /// Bind-time: parse, type-check, and compute the output column layout.
    pub fn build(cfg: &PlanConfig, schema: &dyn BindSchema) -> Result<Plan> {
        let pattern = parse_pattern(&cfg.pattern)?;
        let vars = pattern.variables();

        // SUBSET. Union variables are usable anywhere a pattern variable is, so
        // the label universe that DEFINE/MEASURES resolve against is the pattern
        // variables plus the subset names.
        let subsets = parse_subsets(&cfg.subset_json, &vars)?;
        let mut labels: Vec<String> = vars.clone();
        labels.extend(subsets.keys().cloned());
        // Ids are assigned here, once, and everything downstream uses them.
        let mut subset_members: Vec<(String, Vec<String>)> = subsets.into_iter().collect();
        subset_members.sort_by(|a, b| a.0.cmp(&b.0));
        let label_set = LabelSet::new(labels.clone(), &subset_members);
        let program = compile(&pattern, &label_set)?;

        // DEFINE. Only real pattern variables may be defined, but a predicate may
        // reference subsets.
        let define_map = parse_define(&cfg.define_json, &vars, &labels, schema)?;
        let mut define: Vec<Option<Expr>> = vec![None; label_set.len()];
        for (name, expr) in define_map {
            if let Some(id) = label_set.id_of(&name) {
                define[id as usize] = Some(expr);
            }
        }

        // MEASURES.
        let measures = parse_measures(cfg.measures_json.as_deref(), schema, &labels)?;

        // ORDER BY (required).
        if cfg.order_by.is_empty() {
            return Err(MrError::Bind(
                "order_by is required (at least one column)".into(),
            ));
        }
        let order_by = cfg
            .order_by
            .iter()
            .map(|s| parse_order_key(s, schema))
            .collect::<Result<Vec<_>>>()?;

        // PARTITION BY columns must exist.
        for c in &cfg.partition_by {
            if schema.col_ty(c).is_none() {
                return Err(MrError::Bind(format!(
                    "partition_by column '{c}' not found"
                )));
            }
        }

        // AFTER MATCH SKIP.
        let after = parse_after(&cfg.after, &labels, &label_set)?;

        if cfg.step_budget.is_some_and(|b| b <= 0) {
            return Err(MrError::Bind("step_budget must be positive".into()));
        }

        // Passthrough columns. A column already emitted as a partition key (or, under
        // ALL ROWS, as an order key) is dropped rather than rejected: naming it is
        // redundant, not wrong, and emitting it twice would give the result two
        // columns of the same name.
        let mut include: Vec<String> = Vec::new();
        for c in &cfg.include {
            if schema.col_ty(c).is_none() {
                return Err(MrError::Bind(format!(
                    "include column '{c}' not found in the input"
                )));
            }
            let already = cfg.partition_by.iter().any(|p| p.eq_ignore_ascii_case(c))
                || (cfg.rows_all && order_by.iter().any(|k| k.col.eq_ignore_ascii_case(c)))
                || include.iter().any(|p| p.eq_ignore_ascii_case(c));
            if !already {
                include.push(c.clone());
            }
        }

        // Output column layout.
        let mut output_columns = Vec::new();
        for c in &cfg.partition_by {
            output_columns.push(OutputColumn {
                name: c.clone(),
                ty: schema.col_ty(c).unwrap(),
            });
        }
        for c in &include {
            output_columns.push(OutputColumn {
                name: c.clone(),
                ty: schema.col_ty(c).unwrap(),
            });
        }
        let mut auto_match_number = false;
        let mut auto_classifier = false;
        if cfg.rows_all {
            for k in &order_by {
                output_columns.push(OutputColumn {
                    name: k.col.clone(),
                    ty: schema.col_ty(&k.col).unwrap(),
                });
            }
            let has_measure = |n: &str| measures.iter().any(|m| m.name.eq_ignore_ascii_case(n));
            if !has_measure("match_number") {
                auto_match_number = true;
                output_columns.push(OutputColumn {
                    name: "match_number".into(),
                    ty: Ty::Int64,
                });
            }
            if !has_measure("classifier") {
                auto_classifier = true;
                output_columns.push(OutputColumn {
                    name: "classifier".into(),
                    ty: Ty::Varchar,
                });
            }
        }
        for m in &measures {
            output_columns.push(OutputColumn {
                name: m.name.clone(),
                ty: m.ty.clone(),
            });
        }

        Ok(Plan {
            program,
            labels: label_set,
            define,
            measures,
            partition_by: cfg.partition_by.clone(),
            include,
            order_by,
            rows_all: cfg.rows_all,
            omit_empty_matches: cfg.omit_empty_matches,
            after,
            step_budget: cfg.step_budget,
            auto_match_number,
            auto_classifier,
            output_columns,
        })
    }

    /// The output schema (column name + type), fixed at bind time.
    pub fn output_columns(&self) -> &[OutputColumn] {
        &self.output_columns
    }

    /// The measures (for diagnostics / tests).
    pub fn measures(&self) -> &[Measure] {
        &self.measures
    }

    /// Every input column this plan reads: the partition and order keys plus every
    /// column referenced by a DEFINE predicate or a MEASURES expression.
    ///
    /// The worker projects buffered batches down to these before writing them to
    /// storage. Buffering volume dominates runtime — a single unused 200-byte
    /// column measured 2.8x on a 2M-row query — so carrying columns the pattern
    /// never reads is the most expensive thing this function can do.
    ///
    /// Names are returned as written in the input schema's terms (matching is
    /// case-insensitive, as elsewhere), de-duplicated, order not significant.
    ///
    /// (The `partition_by` accessor below is exposed so the worker can shard buffered
    /// rows on exactly the key this plan will group by — the two must not be able to
    /// disagree.)
    pub fn partition_by_columns(&self) -> &[String] {
        &self.partition_by
    }

    /// The columns this plan reads, for projection pushdown.
    pub fn referenced_columns(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let add = |name: &str, out: &mut Vec<String>| {
            if !out.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                out.push(name.to_string());
            }
        };
        for c in &self.partition_by {
            add(c, &mut out);
        }
        for c in &self.include {
            add(c, &mut out);
        }
        for k in &self.order_by {
            add(&k.col, &mut out);
        }
        for e in self.define.iter().flatten() {
            collect_columns(e, &mut out);
        }
        for m in &self.measures {
            collect_columns(&m.expr, &mut out);
        }
        out
    }

    /// Produce-time: group into partitions, sort each, match, and emit every
    /// output row.
    ///
    /// This materializes the whole result, so it is for tests and for callers that
    /// were going to hold everything anyway. The worker streams instead —
    /// [`Plan::partitions`], then [`Plan::match_partition`] and [`Plan::emit_rows`]
    /// with a row limit — which keeps one output batch live rather than one
    /// partition's worth of rows.
    pub fn run(&self, store: &dyn RowStore) -> Result<Vec<Vec<Value>>> {
        Ok(self.run_buf(store)?.to_rows())
    }

    /// [`Plan::run`], but handing back the buffer the emit path fills rather than a
    /// `Vec` per row — what a caller that is about to build an Arrow batch wants.
    pub fn run_buf(&self, store: &dyn RowStore) -> Result<RowBuf> {
        let mut parts = self.partitions(store)?;
        let mut out = RowBuf::new(self.output_columns.len());
        for p in 0..parts.len() {
            let mut run = {
                let (label, tape) = parts.part_mut(p);
                self.match_partition(store, label, tape)?
            };
            self.emit_rows(store, parts.tape(p), &mut run, usize::MAX, &mut out)?;
        }
        Ok(out)
    }

    /// Sort, match, and emit one whole partition into `out`.
    ///
    /// Partitions are independent: `MATCH_NUMBER()` counts matches *within* a
    /// partition, so every partition starts again at 1 (SQL:2016).
    pub fn run_partition(
        &self,
        store: &dyn RowStore,
        label: &str,
        tape: &mut [usize],
        out: &mut RowBuf,
    ) -> Result<()> {
        let mut run = self.match_partition(store, label, tape)?;
        self.emit_rows(store, tape, &mut run, usize::MAX, out)
    }

    /// Sort `tape` and find every match in it, without emitting any output row.
    ///
    /// This is the half of the work that has to happen all at once — a match may
    /// span the partition — and the returned [`PartitionRun`] owns its result, so a
    /// caller can hold several matched partitions (matched concurrently, say) while
    /// emitting their rows on its own schedule.
    pub fn match_partition(
        &self,
        store: &dyn RowStore,
        label: &str,
        tape: &mut [usize],
    ) -> Result<PartitionRun> {
        self.sort_tape(store, tape)?;
        let mut matcher = Matcher::new(
            &self.program,
            store,
            tape,
            &self.define,
            &self.labels,
            &self.after,
            self.step_budget
                .unwrap_or_else(|| auto_step_budget(tape.len())),
            label,
            1,
        );
        let matches = matcher.find_all()?;
        Ok(PartitionRun {
            matches,
            mi: 0,
            ri: 0,
            loaded: false,
            // One index and one memo for the whole partition, refilled per match:
            // the lists keep their capacity, so a partition of many short matches
            // pays no per-match allocation.
            index: BindIndex::new(&self.labels),
            memo: AggMemo::new(),
        })
    }

    /// Emit output rows from `run` until it holds `limit` rows or the partition is
    /// exhausted; `tape` must be the same (now sorted) tape that was matched.
    ///
    /// The cursor is `(match, row within match)`, so a batch boundary can fall
    /// *inside* a match — which is the point: under ALL ROWS PER MATCH a single
    /// match can be as long as the partition, and materializing its rows was the
    /// one term in this engine that no memory budget bounded. Nothing downstream
    /// depends on a match arriving whole: rows carry their own `match_number`, and
    /// they are emitted in the same order either way. The RUNNING horizon is `k`,
    /// which is stored in the cursor, so a resumed match evaluates exactly as an
    /// uninterrupted one does.
    pub fn emit_rows(
        &self,
        store: &dyn RowStore,
        tape: &[usize],
        run: &mut PartitionRun,
        limit: usize,
        out: &mut RowBuf,
    ) -> Result<()> {
        let PartitionRun {
            matches,
            mi,
            ri,
            loaded,
            index,
            memo,
        } = run;
        while *mi < matches.len() && out.len() < limit {
            let m = &matches[*mi];
            if m.binds.is_empty() {
                // An empty match produces exactly one output row (SQL:2016 SHOW
                // EMPTY MATCHES, the default), positioned at the row where it
                // occurred, with every measure evaluated over an empty match.
                // ALL ROWS PER MATCH OMIT EMPTY MATCHES drops it; ONE ROW PER
                // MATCH always reports it.
                if !(self.rows_all && self.omit_empty_matches) {
                    emit_row(out, |o| self.emit_empty_row(store, tape, m, o))?;
                }
                *mi += 1;
                continue;
            }
            if !*loaded {
                // Both emit paths resolve `LAST(label)` repeatedly, so index this
                // match's binds by label once instead of scanning it per lookup.
                index.refill(&m.binds, &self.labels);
                // The accumulators describe one bind sequence, so they cannot carry
                // over from the previous match.
                memo.clear();
                *loaded = true;
            }
            if self.rows_all {
                while *ri < m.binds.len() && out.len() < limit {
                    let k = *ri;
                    emit_row(out, |o| {
                        self.emit_all_row(store, tape, m, k, index, memo, o)
                    })?;
                    *ri += 1;
                }
                if *ri < m.binds.len() {
                    // Stopped mid-match: keep the cursor, the index and the memo.
                    return Ok(());
                }
            } else {
                emit_row(out, |o| self.emit_one_row(store, tape, m, index, o))?;
            }
            *mi += 1;
            *ri = 0;
            *loaded = false;
        }
        Ok(())
    }

    fn emit_one_row(
        &self,
        store: &dyn RowStore,
        tape: &[usize],
        m: &Match,
        index: &BindIndex,
        out: &mut RowBuf,
    ) -> Result<()> {
        let frame = Frame {
            store,
            tape,
            binds: &m.binds,
            horizon: m.binds.len(),
            match_number: m.match_number,
            labels: &self.labels,
            label_index: Some(index),
            // A single output row has no later rows to share a fold with.
            agg_memo: None,
        };
        let anchor_tape = m.binds.first().map(|b| b.tape_pos).unwrap_or(m.start);
        for c in &self.partition_by {
            out.push(self.col_at(store, tape, anchor_tape, c)?);
        }
        for c in &self.include {
            out.push(self.col_at(store, tape, anchor_tape, c)?);
        }
        for meas in &self.measures {
            let v = frame.eval_measure(&meas.expr, true)?;
            out.push(crate::engine::valops::coerce(v, &meas.ty)?);
        }
        Ok(())
    }

    /// One output row for an **empty match**: the universal columns come from the
    /// row the match sits on, while every measure sees a match with no rows bound
    /// (so `CLASSIFIER()` and navigation are NULL, `COUNT(*)` is 0, and
    /// `MATCH_NUMBER()` still reports the number the empty match consumed).
    fn emit_empty_row(
        &self,
        store: &dyn RowStore,
        tape: &[usize],
        m: &Match,
        out: &mut RowBuf,
    ) -> Result<()> {
        let frame = Frame {
            store,
            tape,
            binds: &m.binds,
            horizon: 0,
            match_number: m.match_number,
            labels: &self.labels,
            // Nothing is bound, so there is nothing to index.
            label_index: None,
            agg_memo: None,
        };
        for c in &self.partition_by {
            out.push(self.col_at(store, tape, m.start, c)?);
        }
        for c in &self.include {
            out.push(self.col_at(store, tape, m.start, c)?);
        }
        if self.rows_all {
            for key in &self.order_by {
                out.push(self.col_at(store, tape, m.start, &key.col)?);
            }
            if self.auto_match_number {
                out.push(Value::Int(m.match_number));
            }
            if self.auto_classifier {
                // No row is bound, so there is no classifier.
                out.push(Value::Null);
            }
        }
        for meas in &self.measures {
            // FINAL for ONE ROW PER MATCH, RUNNING for ALL ROWS — the same default
            // the non-empty paths use. Over an empty match the two coincide.
            let v = frame.eval_measure(&meas.expr, !self.rows_all)?;
            out.push(crate::engine::valops::coerce(v, &meas.ty)?);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_all_row(
        &self,
        store: &dyn RowStore,
        tape: &[usize],
        m: &Match,
        k: usize,
        index: &BindIndex,
        memo: &AggMemo,
        out: &mut RowBuf,
    ) -> Result<()> {
        let frame = Frame {
            store,
            tape,
            binds: &m.binds,
            horizon: k + 1,
            match_number: m.match_number,
            labels: &self.labels,
            label_index: Some(index),
            // `k` ascends through the match, so the horizon only ever advances and
            // each aggregate can extend its fold by the one newly visible bind.
            agg_memo: Some(memo),
        };
        let row_tape = m.binds[k].tape_pos;
        for c in &self.partition_by {
            out.push(self.col_at(store, tape, row_tape, c)?);
        }
        for c in &self.include {
            out.push(self.col_at(store, tape, row_tape, c)?);
        }
        for key in &self.order_by {
            out.push(self.col_at(store, tape, row_tape, &key.col)?);
        }
        if self.auto_match_number {
            out.push(Value::Int(m.match_number));
        }
        if self.auto_classifier {
            out.push(Value::Str(self.labels.name(m.binds[k].var).to_string()));
        }
        for meas in &self.measures {
            let v = frame.eval_measure(&meas.expr, false)?;
            out.push(crate::engine::valops::coerce(v, &meas.ty)?);
        }
        Ok(())
    }

    fn col_at(&self, store: &dyn RowStore, tape: &[usize], tp: usize, name: &str) -> Result<Value> {
        let idx = store
            .col_index(name)
            .ok_or_else(|| MrError::Eval(format!("unknown column '{name}'")))?;
        Ok(store.cell(tape[tp], idx))
    }

    /// Group rows into partitions (first-seen order preserved).
    pub fn partitions(&self, store: &dyn RowStore) -> Result<Partitions> {
        let n = store.num_rows();
        if self.partition_by.is_empty() {
            return Ok(Partitions::single("(all)".to_string(), n));
        }
        let idxs: Vec<usize> = self
            .partition_by
            .iter()
            .map(|c| {
                store
                    .col_index(c)
                    .ok_or_else(|| MrError::Eval(format!("unknown partition column '{c}'")))
            })
            .collect::<Result<_>>()?;
        // The common shape — one integer-family key, e.g. `partition_by := ['uid']` —
        // groups on the raw integer, so no key string is built at all.
        if let Some((ids, labels)) = self.group_ids_by_int(store, &idxs) {
            return Ok(Partitions::from_ids(&ids, labels));
        }
        let mut labels: Vec<String> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut ids = GroupIds::with_capacity(n);
        // One reused buffer, one hash lookup, and an allocation only when a new
        // group appears. Building a `Vec<String>` and joining it per row — then
        // cloning the key twice and looking it up twice — cost about four
        // allocations and two lookups on every row of the input.
        let mut key = String::new();
        for r in 0..n {
            key.clear();
            for (j, &ci) in idxs.iter().enumerate() {
                if j > 0 {
                    key.push('\u{1}');
                }
                write_key_part(&mut key, &store.cell(r, ci));
            }
            match seen.get(&key) {
                Some(&g) => ids.push(g),
                None => {
                    let g = labels.len();
                    seen.insert(key.clone(), g);
                    labels.push(key.clone());
                    ids.push(g);
                }
            }
        }
        Ok(Partitions::from_ids(&ids, labels))
    }

    /// Group on a single integer-family partition key, without rendering a key
    /// string per row.
    ///
    /// The generic path builds a `String` for every row — a throwaway allocation per
    /// key cell plus a string hash — which is most of what partitioning costs. Here
    /// the key is the raw `i64` (or `None` for NULL, which groups with itself), and
    /// the label a partition carries is rendered once per group instead of once per
    /// row. Returns `None` when the shape does not apply, including a value that
    /// disagrees with the column's declared type, so the generic path stays the
    /// authority on how keys are formed.
    fn group_ids_by_int(
        &self,
        store: &dyn RowStore,
        idxs: &[usize],
    ) -> Option<(GroupIds, Vec<String>)> {
        let [ci] = *idxs else { return None };
        let ty = store.col_ty(ci);
        if !matches!(
            ty,
            Ty::Int64 | Ty::UInt64 | Ty::Date | Ty::Timestamp(_) | Ty::Time(_)
        ) {
            return None;
        }
        let n = store.num_rows();
        let mut first_seen: HashMap<Option<i64>, usize> = HashMap::new();
        let mut samples: Vec<Value> = Vec::new();
        let mut ids = GroupIds::with_capacity(n);
        for r in 0..n {
            let cell = store.cell(r, ci);
            let key = match (&cell, &ty) {
                (Value::Null, _) => None,
                (Value::Int(v), Ty::Int64) => Some(*v),
                // Grouping needs equality, not ordering, and `as i64` is a
                // bijection on the bit pattern — so a u64 hashes and compares
                // exactly right here even though it would *sort* wrong. Sound
                // only because the arm is gated on the column's declared type,
                // so Int and UInt keys can never share the map; the label and
                // the output value both come from the sampled `Value` below,
                // never from this key.
                (Value::UInt(v), Ty::UInt64) => Some(*v as i64),
                (Value::Date(v), Ty::Date) => Some(*v as i64),
                (Value::Timestamp(v, u), Ty::Timestamp(want)) if u == want => Some(*v),
                (Value::Time(v, u), Ty::Time(want)) if u == want => Some(*v),
                // Not uniformly the declared type: let the generic path handle it.
                _ => return None,
            };
            match first_seen.get(&key) {
                Some(&g) => ids.push(g),
                None => {
                    first_seen.insert(key, samples.len());
                    ids.push(samples.len());
                    samples.push(cell);
                }
            }
        }
        Some((
            ids,
            samples
                .iter()
                .map(|sample| {
                    // Same rendering the generic path would produce, so a step-budget
                    // error names the partition the same way either way.
                    let mut label = String::new();
                    write_key_part(&mut label, sample);
                    label
                })
                .collect(),
        ))
    }

    /// Stable-sort a partition's row indices by the order-by keys.
    fn sort_tape(&self, store: &dyn RowStore, tape: &mut [usize]) -> Result<()> {
        let keys: Vec<(usize, bool, bool)> = self
            .order_by
            .iter()
            .map(|k| {
                store
                    .col_index(&k.col)
                    .ok_or_else(|| MrError::Eval(format!("unknown order column '{}'", k.col)))
                    .map(|ci| (ci, k.desc, k.nulls_first))
            })
            .collect::<Result<_>>()?;
        // A sort does ~n·log n comparisons over n rows, so anything paid per
        // comparison is paid log n times per row. `cmp_cells` is a virtual call that
        // re-locates the row in the store every time (the Arrow store binary-searches
        // its batch offsets, twice per comparison). When every key is fixed-width,
        // reading each key *once* into a flat vector and sorting on that is the same
        // ordering for a fraction of the work — and it is exactly the common case,
        // ordering by a timestamp or an id.
        //
        // Variable-width keys (VARCHAR, LIST) are deliberately left on `cmp_cells`:
        // materialising them would clone every string, trading the store's in-place
        // buffer comparison for a copy of the column.
        // Reading the keys out costs a few allocations, so it only pays on a
        // partition big enough to amortise them. A ten-row partition sorts in a
        // handful of comparisons either way, and a query can have 160,000 of them.
        // The upper bound is not a tuning knob: the permutation is `u32` and its top
        // bit marks a cycle as applied, so above it the positions would not fit.
        // `n as u32` used to wrap silently there.
        if (MIN_ROWS_FOR_KEY_SORT..=MAX_ROWS_FOR_KEY_SORT).contains(&tape.len())
            && keys
                .iter()
                .all(|&(ci, _, _)| fixed_width(&store.col_ty(ci)))
        {
            self.sort_tape_on_keys(store, tape, &keys);
            return Ok(());
        }
        tape.sort_by(|&a, &b| {
            for &(ci, desc, nulls_first) in &keys {
                let ord = store.cmp_cells(a, b, ci, desc, nulls_first);
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
        Ok(())
    }

    /// Sort by keys read once into memory, rather than fetched per comparison.
    ///
    /// Sorts a permutation of the tape's *positions* so the keys stay addressable by
    /// their original position, then applies the permutation. Stable, and the
    /// comparator is [`valops::cmp_for_sort`] — the same total order `cmp_cells`
    /// implements, which `mr-worker/tests/sort_agreement.rs` pins the two together on.
    fn sort_tape_on_keys(
        &self,
        store: &dyn RowStore,
        tape: &mut [usize],
        keys: &[(usize, bool, bool)],
    ) {
        let n = tape.len();
        // One column of keys per order-by term, indexed by tape position.
        let cols: Vec<KeyCol> = keys
            .iter()
            .map(|&(ci, _, _)| KeyCol::read(store, tape, ci))
            .collect();
        let mut perm: Vec<u32> = (0..n as u32).collect();
        perm.sort_by(|&a, &b| {
            for (col, &(_, desc, nulls_first)) in cols.iter().zip(keys) {
                let ord = col.cmp_at(a as usize, b as usize, desc, nulls_first);
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
        apply_permutation(tape, &mut perm);
    }
}

/// The partitions of a relation: every partition's row indices, in one buffer.
///
/// Partition `p` owns `rows[bounds[p]..bounds[p + 1]]` and is named `labels[p]`,
/// with partitions in first-seen order and rows ascending within each.
///
/// The previous shape was a `Vec<(String, Vec<usize>)>` — one growable `Vec` per
/// partition, filled by pushing. That costs a header and a separate allocation per
/// partition, and, because the sizes are not known in advance, up to twice the row
/// indices in doubling slack that is never returned. Counting the groups first and
/// filling exact-size ranges gives one allocation for the whole relation, which on
/// a large input is the difference between 8 and up to 16 bytes per row of resident
/// memory, and takes the per-partition allocation out of a query with hundreds of
/// thousands of tiny partitions.
///
/// The indices stay `usize` rather than `u32`. Halving them would cap the relation
/// at 4.29B rows, and — since the matcher's tape is `usize` — would need every
/// partition expanded into a scratch buffer before it could be matched, which costs
/// *more* on the one shape that hurts most: a single partition holding everything.
pub struct Partitions {
    rows: Vec<usize>,
    /// `len() + 1` entries: the start of each partition, then the total.
    bounds: Vec<usize>,
    labels: Vec<String>,
}

impl Partitions {
    /// The whole relation as one partition (no `partition_by`).
    fn single(label: String, n: usize) -> Partitions {
        Partitions {
            rows: (0..n).collect(),
            bounds: vec![0, n],
            labels: vec![label],
        }
    }

    /// Group `ids` (one group id per row, in row order) into contiguous ranges.
    fn from_ids(ids: &GroupIds, labels: Vec<String>) -> Partitions {
        let n = ids.len();
        let ngroups = labels.len();
        // Count, prefix-sum, then place each row at its group's cursor: one pass to
        // size every range exactly and one to fill them, with rows landing in
        // ascending order within a partition because `r` ascends.
        let mut bounds = vec![0usize; ngroups + 1];
        for r in 0..n {
            bounds[ids.get(r) + 1] += 1;
        }
        for g in 0..ngroups {
            bounds[g + 1] += bounds[g];
        }
        let mut cursor = bounds[..ngroups].to_vec();
        let mut rows = vec![0usize; n];
        for r in 0..n {
            let g = ids.get(r);
            rows[cursor[g]] = r;
            cursor[g] += 1;
        }
        Partitions {
            rows,
            bounds,
            labels,
        }
    }

    /// How many partitions.
    pub fn len(&self) -> usize {
        self.labels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.labels.is_empty()
    }

    /// Rows across every partition.
    pub fn total_rows(&self) -> usize {
        self.rows.len()
    }

    /// Rows in partition `p`.
    pub fn rows_in(&self, p: usize) -> usize {
        self.bounds[p + 1] - self.bounds[p]
    }

    /// Partition `p`'s name (the rendered partition key).
    pub fn label(&self, p: usize) -> &str {
        &self.labels[p]
    }

    /// Partition `p`'s tape — sorted, once it has been through
    /// [`Plan::match_partition`].
    pub fn tape(&self, p: usize) -> &[usize] {
        &self.rows[self.bounds[p]..self.bounds[p + 1]]
    }

    /// Partition `p`'s name and its tape, mutably (the matcher sorts in place).
    pub fn part_mut(&mut self, p: usize) -> (&str, &mut [usize]) {
        let (lo, hi) = (self.bounds[p], self.bounds[p + 1]);
        (&self.labels[p], &mut self.rows[lo..hi])
    }

    /// Partitions `from..to`, each with its own mutable tape.
    ///
    /// Disjoint slices of one buffer, so a caller can hand each partition to a
    /// different thread and every tape is still sorted in place.
    pub fn chunk_mut(&mut self, from: usize, to: usize) -> Vec<(&str, &mut [usize])> {
        let (lo, hi) = (self.bounds[from], self.bounds[to]);
        let mut rest = &mut self.rows[lo..hi];
        let mut out = Vec::with_capacity(to - from);
        for p in from..to {
            let (head, tail) = rest.split_at_mut(self.bounds[p + 1] - self.bounds[p]);
            out.push((self.labels[p].as_str(), head));
            rest = tail;
        }
        out
    }
}

/// One group id per row, in row order.
///
/// `u32` while the relation fits it, which is the only case that will ever run;
/// the `usize` arm keeps a relation of more than 4.29B rows correct rather than
/// silently wrapping into the wrong partition.
enum GroupIds {
    Small(Vec<u32>),
    Large(Vec<usize>),
}

impl GroupIds {
    fn with_capacity(n: usize) -> GroupIds {
        if n <= u32::MAX as usize {
            GroupIds::Small(Vec::with_capacity(n))
        } else {
            GroupIds::Large(Vec::with_capacity(n))
        }
    }

    fn push(&mut self, g: usize) {
        match self {
            GroupIds::Small(v) => v.push(g as u32),
            GroupIds::Large(v) => v.push(g),
        }
    }

    fn get(&self, r: usize) -> usize {
        match self {
            GroupIds::Small(v) => v[r] as usize,
            GroupIds::Large(v) => v[r],
        }
    }

    fn len(&self) -> usize {
        match self {
            GroupIds::Small(v) => v.len(),
            GroupIds::Large(v) => v.len(),
        }
    }
}

/// One partition's matches, plus a cursor over the output rows they produce.
///
/// Produced by [`Plan::match_partition`] and drained by [`Plan::emit_rows`]. It
/// owns everything it needs except the tape, so it can be moved out of the thread
/// that matched it and emitted later, a batch at a time.
pub struct PartitionRun {
    matches: Vec<Match>,
    /// Next match to emit from.
    mi: usize,
    /// Next bind within `matches[mi]` (ALL ROWS PER MATCH only).
    ri: usize,
    /// Whether `index` and `memo` describe `matches[mi]` yet.
    loaded: bool,
    index: BindIndex,
    memo: AggMemo,
}

impl PartitionRun {
    /// Whether every output row has been emitted.
    pub fn is_done(&self) -> bool {
        self.mi >= self.matches.len()
    }

    /// Matches found in this partition.
    pub fn matches(&self) -> usize {
        self.matches.len()
    }
}

/// Emit one output row, discarding a half-written one if a measure fails.
///
/// The buffer is row-major with no per-row structure, so a partial row would
/// silently shift every column of every row after it. The query is over either
/// way, but a producer may hold the buffer while it reports the error.
fn emit_row(out: &mut RowBuf, f: impl FnOnce(&mut RowBuf) -> Result<()>) -> Result<()> {
    let mark = out.cells();
    match f(out) {
        Ok(()) => {
            out.end_row();
            Ok(())
        }
        Err(e) => {
            out.truncate_cells(mark);
            Err(e)
        }
    }
}

/// Marks a permutation slot whose cycle has been applied. The positions
/// themselves are bounded by [`MAX_ROWS_FOR_KEY_SORT`], so the top bit is free.
const PERM_DONE: u32 = 1 << 31;

/// Largest partition the key-sort path handles, from the `u32` permutation minus
/// its [`PERM_DONE`] bit. Anything larger sorts through `cmp_cells` instead.
const MAX_ROWS_FOR_KEY_SORT: usize = (PERM_DONE - 1) as usize;

/// Rearrange `tape` so that `tape[i]` becomes the element `perm[i]` named, in
/// place. `perm` is consumed as scratch (its slots are marked as they are applied).
///
/// The obvious version copies the tape and scatters through the copy, which costs
/// another 8 bytes per row of the partition — on the one shape that has no other
/// relief, a single huge partition, that copy was the largest allocation in the
/// sort after the keys themselves. Following each cycle instead writes every slot
/// exactly once with no second buffer.
fn apply_permutation(tape: &mut [usize], perm: &mut [u32]) {
    debug_assert_eq!(tape.len(), perm.len());
    for start in 0..tape.len() {
        if perm[start] & PERM_DONE != 0 {
            continue;
        }
        // `held` is the element that belongs to whichever slot gathers from
        // `start`; it is placed when the cycle closes back onto it.
        let held = tape[start];
        let mut slot = start;
        loop {
            let from = (perm[slot] & !PERM_DONE) as usize;
            perm[slot] |= PERM_DONE;
            if from == start {
                tape[slot] = held;
                break;
            }
            // Sound because `from` is written only on the next turn of this loop,
            // so it still holds its pre-permutation element.
            tape[slot] = tape[from];
            slot = from;
        }
    }
}

/// A stable key fragment for a partition value.
/// Partition size at which reading sort keys into memory starts to pay for its
/// allocations. Below this the per-comparison saving cannot cover them.
const MIN_ROWS_FOR_KEY_SORT: usize = 256;

/// A sort key column, read out of the store once.
///
/// The integer-family types get a packed representation — 8 bytes and a null flag per
/// row instead of a 32-byte `Value` — which both halves the memory a large partition's
/// sort needs and makes each comparison an `i64` compare rather than a match over an
/// enum. That covers `BIGINT`, `DATE`, `TIMESTAMP` and `TIME`, which is what
/// `ORDER BY` almost always names. Everything else fixed-width keeps its `Value`.
enum KeyCol {
    Ints {
        vals: Vec<i64>,
        null: Vec<bool>,
    },
    /// `UBIGINT`. Its own buffer rather than a bias-encoded `i64`: the encoding
    /// would be sound (`vals` is only ever compared, never read back), but
    /// sorting is where this project's bugs have lived, and an invisible
    /// transform between the column and the comparator is the wrong trade for
    /// zero bytes saved.
    UInts {
        vals: Vec<u64>,
        null: Vec<bool>,
    },
    Vals(Vec<Value>),
}

/// NULL placement for a packed column, shared by every packed variant.
///
/// `None` means both rows are present and the caller should compare values.
/// Placement is **absolute**: `nulls_first` says where NULLs land in the final
/// order, so `DESC` must not flip them — which is why this is factored out
/// rather than copied per variant.
fn null_ord(a_null: bool, b_null: bool, nulls_first: bool) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering::*;
    match (a_null, b_null) {
        (true, true) => Some(Equal),
        (true, false) => Some(if nulls_first { Less } else { Greater }),
        (false, true) => Some(if nulls_first { Greater } else { Less }),
        (false, false) => None,
    }
}

impl KeyCol {
    fn read(store: &dyn RowStore, tape: &[usize], ci: usize) -> KeyCol {
        let ty = store.col_ty(ci);
        if let Some(packed) = KeyCol::read_ints(store, tape, ci, &ty) {
            return packed;
        }
        KeyCol::Vals(tape.iter().map(|&r| store.cell(r, ci)).collect())
    }

    /// Try to read the column as packed `i64`s.
    ///
    /// `None` if the column is not integer-family, or if any value disagrees with the
    /// declared type — including a temporal value in a different unit, which cannot be
    /// compared as a raw integer (that is what made `TIMESTAMP '9999-12-31'` sort
    /// before 2020 when the rescale overflowed). Those columns fall back to the
    /// `Value` comparator, which handles the mixed case exactly.
    fn read_ints(store: &dyn RowStore, tape: &[usize], ci: usize, ty: &Ty) -> Option<KeyCol> {
        // UBIGINT gets its own reader: u64 ordering is not i64 ordering above
        // 2^63, so it cannot share this buffer.
        if matches!(ty, Ty::UInt64) {
            return KeyCol::read_uints(store, tape, ci);
        }
        if !matches!(ty, Ty::Int64 | Ty::Date | Ty::Timestamp(_) | Ty::Time(_)) {
            return None;
        }
        let mut vals = Vec::with_capacity(tape.len());
        let mut null = Vec::with_capacity(tape.len());
        for &r in tape {
            let raw = match (store.cell(r, ci), ty) {
                (Value::Null, _) => None,
                (Value::Int(v), Ty::Int64) => Some(v),
                (Value::Date(v), Ty::Date) => Some(v as i64),
                (Value::Timestamp(v, u), Ty::Timestamp(want)) if u == *want => Some(v),
                (Value::Time(v, u), Ty::Time(want)) if u == *want => Some(v),
                // Anything else: the column is not uniformly what it claims to be.
                _ => return None,
            };
            match raw {
                Some(v) => {
                    vals.push(v);
                    null.push(false);
                }
                None => {
                    // The placeholder is never compared — `null` gates it.
                    vals.push(0);
                    null.push(true);
                }
            }
        }
        Some(KeyCol::Ints { vals, null })
    }

    /// Read a `UBIGINT` column as packed `u64`s, on the same terms as
    /// [`KeyCol::read_ints`]: `None` if any value disagrees with the declared
    /// type, so a column that is not uniformly what it claims drops to the
    /// `Value` comparator.
    fn read_uints(store: &dyn RowStore, tape: &[usize], ci: usize) -> Option<KeyCol> {
        let mut vals = Vec::with_capacity(tape.len());
        let mut null = Vec::with_capacity(tape.len());
        for &r in tape {
            match store.cell(r, ci) {
                Value::UInt(v) => {
                    vals.push(v);
                    null.push(false);
                }
                Value::Null => {
                    // The placeholder is never compared — `null` gates it.
                    vals.push(0);
                    null.push(true);
                }
                _ => return None,
            }
        }
        Some(KeyCol::UInts { vals, null })
    }

    /// Order two rows of this column. Identical to
    /// [`valops::cmp_for_sort`](crate::engine::valops::cmp_for_sort), including the
    /// rule that NULL placement is *absolute*: `nulls_first` says where NULLs land in
    /// the final order, so `DESC` must not flip them.
    fn cmp_at(&self, a: usize, b: usize, desc: bool, nulls_first: bool) -> std::cmp::Ordering {
        let packed = match self {
            KeyCol::Ints { vals, null } => match null_ord(null[a], null[b], nulls_first) {
                Some(ord) => return ord,
                None => vals[a].cmp(&vals[b]),
            },
            KeyCol::UInts { vals, null } => match null_ord(null[a], null[b], nulls_first) {
                Some(ord) => return ord,
                None => vals[a].cmp(&vals[b]),
            },
            KeyCol::Vals(vals) => {
                return crate::engine::valops::cmp_for_sort(&vals[a], &vals[b], desc, nulls_first)
            }
        };
        if desc {
            packed.reverse()
        } else {
            packed
        }
    }
}

/// Whether a sort key of this type occupies a fixed number of bytes, so reading a
/// column of them into memory copies no heap data.
fn fixed_width(ty: &Ty) -> bool {
    match ty {
        Ty::Boolean
        | Ty::Int64
        | Ty::UInt64
        | Ty::HugeInt
        | Ty::Double
        | Ty::Decimal(_, _)
        | Ty::Date
        | Ty::Timestamp(_)
        | Ty::Time(_)
        | Ty::Interval
        | Ty::Null => true,
        // A copy of these would be a copy of the column's heap data.
        Ty::Varchar | Ty::List(_) => false,
    }
}

fn write_key_part(out: &mut String, v: &Value) {
    match v {
        // A sentinel that no rendered value can produce, so NULL groups with NULL
        // and with nothing else.
        Value::Null => out.push_str("\u{0}NULL"),
        other => out.push_str(&crate::engine::valops::to_string(other)),
    }
}

fn parse_order_key(spec: &str, schema: &dyn BindSchema) -> Result<OrderKey> {
    let s = spec.trim();
    let upper = s.to_ascii_uppercase();
    let mut desc = false;
    let mut nulls_first = None;
    let mut col_part = s;
    // Strip a trailing NULLS FIRST / NULLS LAST.
    if let Some(pos) = upper.find("NULLS FIRST") {
        nulls_first = Some(true);
        col_part = &s[..pos];
    } else if let Some(pos) = upper.find("NULLS LAST") {
        nulls_first = Some(false);
        col_part = &s[..pos];
    }
    let cu = col_part.trim().to_ascii_uppercase();
    let col = if let Some(rest) = cu.strip_suffix(" DESC") {
        desc = true;
        col_part.trim()[..rest.len()].trim().to_string()
    } else if let Some(rest) = cu.strip_suffix(" ASC") {
        col_part.trim()[..rest.len()].trim().to_string()
    } else {
        col_part.trim().to_string()
    };
    if schema.col_ty(&col).is_none() {
        return Err(MrError::Bind(format!("order_by column '{col}' not found")));
    }
    // Default NULLS ordering follows DuckDB, which sorts NULLs last in *both*
    // directions (verified: `ORDER BY k DESC` yields [2, 1, NULL]).
    let nulls_first = nulls_first.unwrap_or(false);
    Ok(OrderKey {
        col,
        desc,
        nulls_first,
    })
}

fn parse_after(s: &str, vars: &[String], labels: &LabelSet) -> Result<AfterSkip> {
    let t = s.trim().to_ascii_lowercase();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t == "past last row" {
        return Ok(AfterSkip::PastLastRow);
    }
    if t == "to next row" {
        return Ok(AfterSkip::ToNextRow);
    }
    let resolve = |var: &str| -> Result<VarId> {
        let v = resolve_var(var, vars).unwrap_or_else(|| var.to_ascii_uppercase());
        labels
            .id_of(&v)
            .filter(|_| vars.iter().any(|x| x.eq_ignore_ascii_case(&v)))
            .ok_or_else(|| MrError::Bind(format!("after: '{var}' is not a pattern variable")))
    };
    if let Some(rest) = t.strip_prefix("to first ") {
        return Ok(AfterSkip::ToFirstVar(resolve(rest.trim())?));
    }
    if let Some(rest) = t.strip_prefix("to last ") {
        return Ok(AfterSkip::ToLastVar(resolve(rest.trim())?));
    }
    Err(MrError::Bind(format!(
        "unknown AFTER MATCH SKIP mode '{s}' (expected 'past last row', 'to next row', \
         'to first <VAR>', or 'to last <VAR>')"
    )))
}

fn parse_define(
    json: &str,
    vars: &[String],
    labels: &[String],
    schema: &dyn BindSchema,
) -> Result<HashMap<String, Expr>> {
    let trimmed = json.trim();
    let map: serde_json::Map<String, serde_json::Value> = if trimmed.is_empty() {
        serde_json::Map::new()
    } else {
        serde_json::from_str(trimmed)
            .map_err(|e| MrError::Bind(format!("define is not a JSON object: {e}")))?
    };
    let mut out = HashMap::new();
    for (k, v) in map {
        let pred = v
            .as_str()
            .ok_or_else(|| MrError::Bind(format!("define['{k}'] must be a string predicate")))?;
        // Catch typos early: a DEFINE key must name a variable the pattern declares.
        let var = resolve_var(&k, vars).ok_or_else(|| {
            MrError::Bind(format!(
                "define variable '{k}' does not appear in the pattern"
            ))
        })?;
        // Everything from here on names the key it came from, since the parser
        // and the inferencer see one predicate and cannot say which it was.
        let ctx = format!("define['{k}']");
        let expr = (|| -> Result<Expr> {
            let mut expr = parse_expr(pred)?;
            canonicalize_vars(&mut expr, labels)?;
            check_predicate(&expr, schema)?;
            Ok(expr)
        })()
        .map_err(|e| e.with_context(&ctx))?;
        out.insert(var, expr);
    }
    Ok(out)
}

/// Type-check a DEFINE predicate at bind time.
///
/// MEASURES have always been inferred (their types *are* the output schema), but
/// DEFINE was only parsed — so a predicate that could never be true was a silent
/// empty result rather than an error. All three of these used to return zero rows
/// and no message:
///
/// ```text
/// define := {"B": "price"}                 -- not a predicate at all
/// define := {"B": "sym > 3"}               -- VARCHAR compared with INTEGER
/// define := {"B": "prcie < PREV(price)"}   -- typo; only ever an error if evaluated
/// ```
///
/// The third is the reason this is a bind check and not a runtime one: an unknown
/// column raised `MrError::Eval` from inside the matcher, so whether the query
/// failed at all depended on whether the predicate was ever reached — it would
/// pass on a small sample and fail in production, or quietly match nothing.
///
/// `Ty::Null` is accepted: a predicate that is statically NULL (`NULL`, or a
/// comparison against it) is never true, but it is well-formed SQL and the
/// three-valued logic in `eval` handles it.
fn check_predicate(expr: &Expr, schema: &dyn BindSchema) -> Result<()> {
    let ty = infer(expr, schema)?;
    if ty == Ty::Boolean || ty == Ty::Null {
        Ok(())
    } else {
        Err(MrError::Infer(format!(
            "a DEFINE predicate must be BOOLEAN, but this one is {ty}; it can never bind a row \
             (write a comparison, e.g. `price < PREV(price)`)"
        )))
    }
}

/// Collect every column name `e` reads, appending to `out` without duplicates.
fn collect_columns(e: &Expr, out: &mut Vec<String>) {
    let push = |name: &str, out: &mut Vec<String>| {
        if !out.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            out.push(name.to_string());
        }
    };
    match e {
        Expr::Col(name) => push(name, out),
        Expr::Qualified(_, col) => push(col, out),
        Expr::Nav { arg, .. } => collect_columns(arg, out),
        Expr::RunningFinal { inner, .. } => collect_columns(inner, out),
        Expr::Neg(x) | Expr::Not(x) | Expr::Cast { expr: x, .. } => collect_columns(x, out),
        Expr::IsNull { expr, .. } => collect_columns(expr, out),
        Expr::Binary { lhs, rhs, .. } => {
            collect_columns(lhs, out);
            collect_columns(rhs, out);
        }
        Expr::Between { expr, lo, hi, .. } => {
            collect_columns(expr, out);
            collect_columns(lo, out);
            collect_columns(hi, out);
        }
        Expr::In { expr, list, .. } => {
            collect_columns(expr, out);
            for it in list {
                collect_columns(it, out);
            }
        }
        Expr::Call { args, .. } => {
            for a in args {
                collect_columns(a, out);
            }
        }
        Expr::Agg { arg, .. } => match arg {
            AggArg::Expr(x) => collect_columns(x, out),
            // `COUNT(*)` / `COUNT(A.*)` read no column values, only bindings.
            AggArg::Star | AggArg::QualStar(_) => {}
        },
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Double(_)
        | Expr::Str(_)
        | Expr::Interval { .. }
        | Expr::Classifier(_)
        | Expr::MatchNumber => {}
    }
}

/// Resolve a written pattern-variable name to its canonical spelling.
///
/// An unquoted label canonicalizes to upper case; a double-quoted one keeps its
/// exact spelling. So a written name is matched **exactly first**, then as an
/// unquoted (upper-cased) name — which makes `define {"b": …}` mean the quoted
/// label `b` when the pattern declares one, and the ordinary label `B` otherwise.
fn resolve_var(written: &str, vars: &[String]) -> Option<String> {
    if vars.iter().any(|v| v == written) {
        return Some(written.to_string());
    }
    let upper = written.to_ascii_uppercase();
    vars.iter().find(|v| **v == upper).cloned()
}

/// Rewrite every pattern-variable reference in `e` to its canonical spelling, so
/// the evaluator can compare names exactly (quoted labels are case-sensitive).
fn canonicalize_vars(e: &mut Expr, vars: &[String]) -> Result<()> {
    let fix = |name: &mut String| -> Result<()> {
        match resolve_var(name, vars) {
            Some(c) => {
                *name = c;
                Ok(())
            }
            None => Err(MrError::Bind(format!(
                "'{name}' is not a pattern variable (declared: {})",
                vars.join(", ")
            ))),
        }
    };
    match e {
        Expr::Qualified(var, _) => fix(var)?,
        Expr::Agg {
            arg: AggArg::QualStar(var),
            ..
        } => fix(var)?,
        Expr::Agg {
            arg: AggArg::Expr(x),
            ..
        } => canonicalize_vars(x, vars)?,
        Expr::Nav { arg, .. } => canonicalize_vars(arg, vars)?,
        Expr::RunningFinal { inner, .. } => canonicalize_vars(inner, vars)?,
        Expr::Neg(x) | Expr::Not(x) | Expr::Cast { expr: x, .. } => canonicalize_vars(x, vars)?,
        Expr::IsNull { expr, .. } => canonicalize_vars(expr, vars)?,
        Expr::Binary { lhs, rhs, .. } => {
            canonicalize_vars(lhs, vars)?;
            canonicalize_vars(rhs, vars)?;
        }
        Expr::Between { expr, lo, hi, .. } => {
            canonicalize_vars(expr, vars)?;
            canonicalize_vars(lo, vars)?;
            canonicalize_vars(hi, vars)?;
        }
        Expr::In { expr, list, .. } => {
            canonicalize_vars(expr, vars)?;
            for it in list {
                canonicalize_vars(it, vars)?;
            }
        }
        Expr::Call { args, .. } => {
            for a in args {
                canonicalize_vars(a, vars)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The SUBSET names declared in `subset_json`, canonicalized — the worker adds
/// these to the label set its bind schema type-checks qualifiers against.
pub fn subset_names(subset_json: &str) -> Result<Vec<String>> {
    let trimmed = subset_json.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(trimmed)
        .map_err(|e| MrError::Bind(format!("subset is not a JSON object: {e}")))?;
    Ok(map.keys().map(|k| k.to_ascii_uppercase()).collect())
}

/// Parse the `subset` argument: `{"U": ["A", "B"], …}`.
fn parse_subsets(json: &str, vars: &[String]) -> Result<HashMap<String, Vec<String>>> {
    let trimmed = json.trim();
    if trimmed.is_empty() {
        return Ok(HashMap::new());
    }
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(trimmed)
        .map_err(|e| MrError::Bind(format!("subset is not a JSON object: {e}")))?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (name, v) in map {
        // A subset name canonicalizes like any label, and must not collide with a
        // pattern variable (it would be ambiguous in a qualifier).
        let canon = if vars.contains(&name) {
            return Err(MrError::Bind(format!(
                "subset '{name}' collides with a pattern variable of the same name"
            )));
        } else {
            name.to_ascii_uppercase()
        };
        let arr = v.as_array().ok_or_else(|| {
            MrError::Bind(format!(
                "subset['{name}'] must be an array of pattern variable names"
            ))
        })?;
        if arr.is_empty() {
            return Err(MrError::Bind(format!("subset['{name}'] is empty")));
        }
        let mut members = Vec::new();
        for m in arr {
            let raw = m.as_str().ok_or_else(|| {
                MrError::Bind(format!("subset['{name}'] members must be strings"))
            })?;
            let member = resolve_var(raw, vars).ok_or_else(|| {
                MrError::Bind(format!(
                    "subset['{name}'] member '{raw}' does not appear in the pattern"
                ))
            })?;
            if !members.contains(&member) {
                members.push(member);
            }
        }
        out.insert(canon, members);
    }
    Ok(out)
}

fn parse_measures(
    json: Option<&str>,
    schema: &dyn BindSchema,
    vars: &[String],
) -> Result<Vec<Measure>> {
    let json = match json {
        Some(s) if !s.trim().is_empty() => s,
        _ => return Ok(Vec::new()),
    };
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| MrError::Bind(format!("measures is not valid JSON: {e}")))?;
    let mut measures = Vec::new();
    match value {
        // Array form: [{ "as"/"name", "expr", "type"? }, ...]
        serde_json::Value::Array(items) => {
            for (i, item) in items.into_iter().enumerate() {
                let obj = item
                    .as_object()
                    .ok_or_else(|| MrError::Bind(format!("measures[{i}] must be an object")))?;
                let name = obj
                    .get("as")
                    .or_else(|| obj.get("name"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| MrError::Bind(format!("measures[{i}] needs an 'as'/'name'")))?
                    .to_string();
                let expr_s = obj
                    .get("expr")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| MrError::Bind(format!("measures[{i}] needs an 'expr'")))?;
                // Named by its `as`, not by its index: that is what the user has
                // in front of them, and what the output column will be called.
                let ctx = format!("measures['{name}']");
                let (expr, ty) = (|| -> Result<(Expr, Ty)> {
                    let mut expr = parse_expr(expr_s)?;
                    canonicalize_vars(&mut expr, vars)?;
                    let ty = match obj.get("type").and_then(|v| v.as_str()) {
                        Some(t) => parse_type_name(t)?,
                        None => resolve_measure_ty(&expr, schema, &name)?,
                    };
                    Ok((expr, ty))
                })()
                .map_err(|e| e.with_context(&ctx))?;
                measures.push(Measure { name, expr, ty });
            }
        }
        // Object form: { "<name>": "<expr>", ... } — inference, no override.
        serde_json::Value::Object(map) => {
            for (name, v) in map {
                let expr_s = v.as_str().ok_or_else(|| {
                    MrError::Bind(format!("measures['{name}'] must be a string expression"))
                })?;
                let ctx = format!("measures['{name}']");
                let (expr, ty) = (|| -> Result<(Expr, Ty)> {
                    let mut expr = parse_expr(expr_s)?;
                    canonicalize_vars(&mut expr, vars)?;
                    let ty = resolve_measure_ty(&expr, schema, &name)?;
                    Ok((expr, ty))
                })()
                .map_err(|e| e.with_context(&ctx))?;
                measures.push(Measure { name, expr, ty });
            }
        }
        _ => {
            return Err(MrError::Bind(
                "measures must be a JSON object or array".into(),
            ))
        }
    }
    Ok(measures)
}

/// The static type of a measure, or a bind error pointing at the type override.
///
/// The caller adds the `measures['name']` prefix (see [`MrError::with_context`]),
/// so the message here does not repeat the name — only the escape hatch, which
/// has to spell out the array form because that is the only place a `type` may
/// be written.
fn resolve_measure_ty(expr: &Expr, schema: &dyn BindSchema, name: &str) -> Result<Ty> {
    let ty = infer(expr, schema)?;
    if ty == Ty::Null {
        return Err(MrError::Infer(format!(
            "could not infer a type (it resolves to NULL); supply an explicit type via the array \
             form, e.g. {{\"as\":\"{name}\",\"expr\":\"…\",\"type\":\"DOUBLE\"}}"
        )));
    }
    Ok(ty)
}

impl Pattern {
    /// Render this pattern for `explain_pattern` (re-exported convenience).
    pub fn explain(&self) -> String {
        crate::pattern::compile::explain(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference the in-place version has to match: `out[i] = tape[perm[i]]`.
    fn scattered(tape: &[usize], perm: &[u32]) -> Vec<usize> {
        perm.iter().map(|&p| tape[p as usize]).collect()
    }

    #[test]
    fn a_permutation_is_applied_in_place() {
        // A rotation, a reversal, the identity, a fixed point beside a swap, and one
        // long cycle — the shapes the cycle walk has to close correctly.
        let cases: Vec<Vec<u32>> = vec![
            vec![],
            vec![0],
            vec![1, 0],
            vec![0, 1, 2, 3],
            vec![3, 2, 1, 0],
            vec![1, 2, 3, 0],
            vec![0, 2, 1, 3],
            vec![4, 0, 3, 1, 2],
        ];
        for perm in cases {
            let tape: Vec<usize> = (0..perm.len()).map(|i| i * 10 + 7).collect();
            let want = scattered(&tape, &perm);
            let mut got = tape.clone();
            let mut scratch = perm.clone();
            apply_permutation(&mut got, &mut scratch);
            assert_eq!(got, want, "permutation {perm:?}");
        }
    }

    /// Every permutation of eight elements, so no cycle decomposition is missed.
    #[test]
    fn every_small_permutation_is_applied_correctly() {
        let mut perm: Vec<u32> = (0..8).collect();
        let mut count = 0;
        loop {
            let tape: Vec<usize> = (0..perm.len()).map(|i| i * 3 + 1).collect();
            let want = scattered(&tape, &perm);
            let mut got = tape.clone();
            let mut scratch = perm.clone();
            apply_permutation(&mut got, &mut scratch);
            assert_eq!(got, want, "permutation {perm:?}");
            count += 1;
            if !next_permutation(&mut perm) {
                break;
            }
        }
        assert_eq!(count, 40_320, "8! permutations");
    }

    /// Next permutation in lexicographic order; `false` once the last is reached.
    fn next_permutation(v: &mut [u32]) -> bool {
        let Some(i) = (0..v.len().saturating_sub(1)).rfind(|&i| v[i] < v[i + 1]) else {
            return false;
        };
        let j = (i + 1..v.len())
            .rfind(|&j| v[j] > v[i])
            .expect("a successor");
        v.swap(i, j);
        v[i + 1..].reverse();
        true
    }
}
