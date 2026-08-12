//! Isolated timings for partitioning and sorting, with no IPC or storage in the way.
//!
//! Ignored by default: it moves millions of rows, which takes ~13s in a debug build
//! and would tax every `cargo test`. Run it deliberately, in release:
//!
//! ```sh
//! cargo test --release -p mr-core --test perf_probe -- --ignored --nocapture
//! ```
//!
//! It is a measurement tool rather than an assertion — the numbers depend on the
//! machine — but it is the thing to run before and after touching `sort_tape` or
//! `partitions`, which are the two per-row costs in the pipeline.
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
        name.eq_ignore_ascii_case("A")
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
            plan.run(&store).unwrap();
        });
        eprintln!("{:>62.0} ns/row", ms * 1e6 / n as f64);
    }

    eprintln!("\n=== one partition, sort only (VARCHAR key) ===");
    for n in [100_000usize, 400_000, 1_600_000] {
        let store = build(n, 1, true);
        let plan = plan_for("s", false);
        let ms = time(&format!("  {n:>9} rows, order_by s"), || {
            plan.run(&store).unwrap();
        });
        eprintln!("{:>62.0} ns/row", ms * 1e6 / n as f64);
    }

    eprintln!("\n=== partitioning cost (BIGINT key, many small partitions) ===");
    for n in [100_000usize, 400_000, 1_600_000] {
        let store = build(n, n / 10, false);
        let plan = plan_for("k", true);
        let ms = time(&format!("  {n:>9} rows, {} partitions", n / 10), || {
            plan.run(&store).unwrap();
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
            plan.run(&store).unwrap();
        });
    }
}
