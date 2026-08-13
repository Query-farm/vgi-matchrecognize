//! Isolated timings for the compute pipeline, with no IPC or storage in the way.
//!
//! Ignored by default: it moves millions of rows, which takes ~13s in a debug build
//! and would tax every `cargo test`. Run it deliberately, in release:
//!
//! ```sh
//! cargo test --release -p mr-core --test perf_probe -- --ignored --nocapture \
//!     --test-threads=1
//! ```
//!
//! `--test-threads=1` is not optional: the probes are CPU-bound, so running them
//! concurrently inflates every number (measured: a 60% swing on one sweep) and
//! interleaves the output.
//!
//! These are measurement tools rather than assertions — the numbers depend on the
//! machine — but they are what to run before and after touching a phase, and
//! `docs/perf-baseline.md` records the numbers they printed on a known machine.
//!
//! They drive `Plan::run_buf`, which is what the worker's producer builds its output
//! batches from. `Plan::run` wraps it in a copy into a `Vec` per row for callers that
//! want that shape, and measuring *that* charges every phase for an allocation per
//! output row: on `A — one match per row` (a million output rows) the copy alone is
//! ~30% of the total.
//!
//! Three probes, because the costs have different shapes:
//!
//! - [`perf_sort_and_partition`] — per-row costs that scale with the *input*
//!   (`partitions`, `sort_tape`). DEFINE never matches, so the matcher is excluded.
//! - [`perf_matcher`] — per-row cost of the VM and of measure evaluation, i.e. what
//!   scales with the number of *matched* rows.
//! - [`perf_match_length`] — per-row cost that scales with the length of the match
//!   the row is in. `Frame::last_bind_of` and `Frame::scope` are linear in the match,
//!   so a shape that references a bound label or a running aggregate is quadratic in
//!   match length; this is the probe that shows it, and the one that has to go flat
//!   for long matches to be usable. It prints ns/row **and** ns/row-per-row-of-match,
//!   because the second number is the one that should be ~0.
use std::collections::HashMap;
use std::time::Instant;

use mr_core::engine::VecRowStore;
use mr_core::plan::{Plan, PlanConfig};
use mr_core::types::{BindSchema, Ty};
use mr_core::value::Value;

struct Sch {
    cols: HashMap<String, Ty>,
}
impl BindSchema for Sch {
    fn col_ty(&self, name: &str) -> Option<Ty> {
        self.cols
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, t)| t.clone())
    }
    fn is_variable(&self, name: &str) -> bool {
        ["A", "B", "C", "START", "DOWN", "UP"]
            .iter()
            .any(|v| name.eq_ignore_ascii_case(v))
    }
}

fn sch() -> Sch {
    Sch {
        cols: [
            ("pid".to_string(), Ty::Int64),
            ("k".to_string(), Ty::Int64),
            ("s".to_string(), Ty::Varchar),
        ]
        .into_iter()
        .collect(),
    }
}

/// A deterministic pseudo-shuffle, so the sort sees unsorted input.
fn scramble(i: usize, n: usize) -> usize {
    (i.wrapping_mul(2_654_435_761)) % n
}

fn build(n: usize, partitions: usize, varchar_key: bool) -> VecRowStore {
    let rows: Vec<Vec<Value>> = (0..n)
        .map(|i| {
            let k = scramble(i, n) as i64;
            vec![
                Value::Int((i % partitions) as i64),
                Value::Int(k),
                Value::Str(if varchar_key {
                    format!("key-{k:012}")
                } else {
                    String::new()
                }),
            ]
        })
        .collect();
    VecRowStore::new(
        vec![("pid", Ty::Int64), ("k", Ty::Int64), ("s", Ty::Varchar)],
        rows,
    )
}

fn plan_for(order_col: &str, partitioned: bool) -> Plan {
    let cfg = PlanConfig {
        include: Vec::new(),
        pattern: "A".into(),
        define_json: r#"{"A":"k < 0"}"#.into(), // never matches: isolates partition+sort
        subset_json: String::new(),
        measures_json: Some(r#"{"n":"COUNT(*)"}"#.into()),
        partition_by: if partitioned {
            vec!["pid".into()]
        } else {
            vec![]
        },
        order_by: vec![order_col.into()],
        rows_all: false,
        omit_empty_matches: false,
        after: "past last row".into(),
        step_budget: None,
    };
    Plan::build(&cfg, &sch()).unwrap()
}

/// A plan over `pattern`/`define`/`measures`, ordered by `k`, one partition.
fn plan_of(pattern: &str, define: &str, measures: &str, rows_all: bool) -> Plan {
    let cfg = PlanConfig {
        include: Vec::new(),
        pattern: pattern.into(),
        define_json: define.into(),
        subset_json: String::new(),
        measures_json: Some(measures.into()),
        partition_by: vec![],
        order_by: vec!["k".into()],
        rows_all,
        omit_empty_matches: false,
        after: "past last row".into(),
        step_budget: None,
    };
    Plan::build(&cfg, &sch()).unwrap()
}

/// `n` rows in one partition with `k` **already ascending**, so the sort is nearly
/// free and what is left is the matcher and the measures.
fn ordered_store(n: usize) -> VecRowStore {
    VecRowStore::new(
        vec![("pid", Ty::Int64), ("k", Ty::Int64), ("s", Ty::Varchar)],
        (0..n as i64)
            .map(|k| vec![Value::Int(0), Value::Int(k), Value::Str(String::new())])
            .collect(),
    )
}

fn time(label: &str, f: impl FnOnce()) -> f64 {
    let t = Instant::now();
    f();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    eprintln!("{label:<52} {ms:>9.1} ms");
    ms
}

#[test]
#[ignore = "perf measurement; run with --release --ignored"]
fn perf_sort_and_partition() {
    eprintln!("\n=== one partition, sort only (BIGINT key) ===");
    for n in [100_000usize, 400_000, 1_600_000] {
        let store = build(n, 1, false);
        let plan = plan_for("k", false);
        let ms = time(&format!("  {n:>9} rows, order_by k"), || {
            plan.run_buf(&store).unwrap();
        });
        eprintln!("{:>62.0} ns/row", ms * 1e6 / n as f64);
    }

    eprintln!("\n=== one partition, sort only (VARCHAR key) ===");
    for n in [100_000usize, 400_000, 1_600_000] {
        let store = build(n, 1, true);
        let plan = plan_for("s", false);
        let ms = time(&format!("  {n:>9} rows, order_by s"), || {
            plan.run_buf(&store).unwrap();
        });
        eprintln!("{:>62.0} ns/row", ms * 1e6 / n as f64);
    }

    eprintln!("\n=== partitioning cost (BIGINT key, many small partitions) ===");
    for n in [100_000usize, 400_000, 1_600_000] {
        let store = build(n, n / 10, false);
        let plan = plan_for("k", true);
        let ms = time(&format!("  {n:>9} rows, {} partitions", n / 10), || {
            plan.run_buf(&store).unwrap();
        });
        eprintln!("{:>62.0} ns/row", ms * 1e6 / n as f64);
    }

    eprintln!("\n=== already-sorted vs reverse-sorted input (1.6M rows) ===");
    for (label, rows) in [
        ("ascending", (0..1_600_000i64).collect::<Vec<_>>()),
        ("descending", (0..1_600_000i64).rev().collect::<Vec<_>>()),
    ] {
        let store = VecRowStore::new(
            vec![("pid", Ty::Int64), ("k", Ty::Int64), ("s", Ty::Varchar)],
            rows.into_iter()
                .map(|k| vec![Value::Int(0), Value::Int(k), Value::Str(String::new())])
                .collect(),
        );
        let plan = plan_for("k", false);
        time(&format!("  {label}"), || {
            plan.run_buf(&store).unwrap();
        });
    }
}

/// Matcher + measure cost per row, on input that is already ordered so the sort is
/// nearly free. The interesting comparison is across the four rows of output: a VM
/// that never binds, a VM that binds every row, and then what each additional
/// measure costs on top.
#[test]
#[ignore = "perf measurement; run with --release --ignored"]
fn perf_matcher() {
    const N: usize = 1_000_000;
    let store = ordered_store(N);

    eprintln!("\n=== matcher + measures, {N} rows, 1 partition, pre-ordered ===");
    for (label, plan) in [
        (
            "never matches (VM floor)",
            plan_of("A", r#"{"A":"k < 0"}"#, r#"{"n":"COUNT(*)"}"#, false),
        ),
        (
            "A  — one match per row",
            plan_of("A", r#"{"A":"k >= 0"}"#, r#"{"n":"COUNT(*)"}"#, false),
        ),
        (
            "A+ — one match, ONE ROW, count(*)",
            plan_of("A+", r#"{"A":"k >= 0"}"#, r#"{"n":"COUNT(*)"}"#, false),
        ),
        (
            "A+ — one match, ALL ROWS, count(*)",
            plan_of(
                "A+",
                r#"{"A":"k >= 0"}"#,
                r#"{"n":"RUNNING COUNT(*)"}"#,
                true,
            ),
        ),
        (
            "v-shape, 3 vars, PREV predicates",
            plan_of(
                "START DOWN+ UP+",
                r#"{"DOWN":"k < PREV(k)","UP":"k > PREV(k)"}"#,
                r#"{"n":"COUNT(*)"}"#,
                false,
            ),
        ),
    ] {
        let ms = time(&format!("  {label:<44}"), || {
            plan.run_buf(&store).unwrap();
        });
        eprintln!("{:>62.0} ns/row", ms * 1e6 / N as f64);
    }
}

/// The quadratic probe: cost per row as a function of the length of the match the
/// row belongs to.
///
/// Each case is a single match covering the whole partition (`A B*` over ascending
/// `k`), so the match length *is* the row count and doubling the rows doubles the
/// per-row work if a phase is linear in the match. The control cases reference no
/// bound label and evaluate no aggregate, so they should stay flat; the others go
/// through `Frame::last_bind_of` or `Frame::scope`.
///
/// Read the second column, not the first: `ns/row/L` flat and non-zero means
/// quadratic, and driving it to ~0 is the goal.
#[test]
#[ignore = "perf measurement; run with --release --ignored"]
fn perf_match_length() {
    // `sum` at L=32k is ~15s on its own, so the aggregate cases stop at 16k.
    let cases: [(&str, &str, &str, bool, &[usize]); 7] = [
        // Control: predicate reads the row only.
        (
            "control:  B: k >= 0            (ONE ROW)",
            "{\"B\":\"k >= 0\"}",
            r#"{"n":"COUNT(*)"}"#,
            false,
            &[1_000, 4_000, 16_000, 32_000],
        ),
        // Quadratic #1: a qualified reference means LAST(A.k) per evaluation.
        (
            "last_bind_of:  B: k >= A.k     (ONE ROW)",
            "{\"B\":\"k >= A.k\"}",
            r#"{"n":"COUNT(*)"}"#,
            false,
            &[1_000, 4_000, 16_000, 32_000],
        ),
        // Control for the emit loop: one output row per matched row, no scope walk.
        (
            "control:  RUNNING COUNT(*)     (ALL ROWS)",
            "{\"B\":\"k >= 0\"}",
            r#"{"n":"RUNNING COUNT(*)"}"#,
            true,
            &[1_000, 4_000, 16_000],
        ),
        // Quadratic #2: a running fold over the whole visible scope, per output row.
        (
            "scope:  RUNNING SUM(k)         (ALL ROWS)",
            "{\"B\":\"k >= 0\"}",
            r#"{"n":"RUNNING SUM(k)"}"#,
            true,
            &[1_000, 4_000, 16_000],
        ),
        (
            "scope:  LAST(k)                (ALL ROWS)",
            "{\"B\":\"k >= 0\"}",
            r#"{"n":"LAST(k)"}"#,
            true,
            &[1_000, 4_000, 16_000],
        ),
        // A FINAL aggregate is constant across the match's output rows, yet is
        // recomputed for every one of them.
        (
            "scope:  FINAL SUM(k)           (ALL ROWS)",
            "{\"B\":\"k >= 0\"}",
            r#"{"n":"FINAL SUM(k)"}"#,
            true,
            &[1_000, 4_000, 16_000],
        ),
        // An aggregate inside DEFINE is evaluated once per VM step, during matching,
        // so it is the matcher's own quadratic (and it runs under backtracking, where
        // an extend-only fold has to be invalidated rather than extended).
        (
            "agg in DEFINE:  B: SUM(k) >= 0 (ONE ROW)",
            "{\"B\":\"SUM(k) >= 0\"}",
            r#"{"n":"COUNT(*)"}"#,
            false,
            &[1_000, 4_000, 16_000],
        ),
    ];

    eprintln!("\n=== cost per row vs match length (one match of L rows) ===");
    for (label, define, measures, rows_all, lengths) in cases {
        eprintln!("\n  {label}");
        for &l in lengths {
            let store = ordered_store(l);
            let plan = plan_of("A B*", define, measures, rows_all);
            let ms = time(&format!("    L = {l:>6}"), || {
                plan.run_buf(&store).unwrap();
            });
            let ns_per_row = ms * 1e6 / l as f64;
            eprintln!(
                "{ns_per_row:>52.0} ns/row {:>8.2} ns/row/L",
                ns_per_row / l as f64
            );
        }
    }
}
