//! Temporal arithmetic: timestamp differences, interval shifts, and the ranges
//! where the i64 intermediates used to overflow.
//!
//! These run under `cargo test`, i.e. the dev profile with overflow checks on,
//! so a regression aborts the test rather than quietly returning a wrapped
//! number. `[profile.release]` sets no `overflow-checks`, which is exactly why
//! the original bug was invisible in the shipped binary: `TIMESTAMP
//! '9999-12-31' - epoch` returned a *negative* interval with no error at all.

use std::collections::HashMap;

use mr_core::engine::VecRowStore;
use mr_core::error::MrError;
use mr_core::plan::{Plan, PlanConfig};
use mr_core::types::{BindSchema, TimeUnit, Ty};
use mr_core::value::{Interval, Value};

/// Microsecond ticks for 9999-12-31 00:00:00, the top of the DuckDB TIMESTAMP
/// range. Times 1000 to reach nanoseconds it is 2.5e20, against an i64 ceiling
/// of 9.2e18.
const YEAR_9999_MICROS: i64 = 253_402_214_400_000_000;

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
        name == "A"
    }
}

fn sch() -> Sch {
    Sch {
        cols: [
            ("id".to_string(), Ty::Int64),
            ("ts".to_string(), Ty::Timestamp(TimeUnit::Micro)),
            ("d".to_string(), Ty::Date),
            ("t".to_string(), Ty::Time(TimeUnit::Micro)),
        ]
        .into_iter()
        .collect(),
    }
}

/// One `A+` match over every row, with `measures := {"m": <expr>}`.
fn measure(expr: &str, rows: Vec<Vec<Value>>) -> mr_core::error::Result<Value> {
    let cfg = PlanConfig {
        include: Vec::new(),
        pattern: "A+".into(),
        define_json: "{}".into(),
        subset_json: String::new(),
        measures_json: Some(format!(r#"{{"m":{}}}"#, serde_json_string(expr))),
        partition_by: vec![],
        order_by: vec!["id".into()],
        rows_all: false,
        omit_empty_matches: false,
        after: "past last row".into(),
        step_budget: Some(1_000_000),
    };
    let store = VecRowStore::new(
        vec![
            ("id", Ty::Int64),
            ("ts", Ty::Timestamp(TimeUnit::Micro)),
            ("d", Ty::Date),
            ("t", Ty::Time(TimeUnit::Micro)),
        ],
        rows,
    );
    let out = Plan::build(&cfg, &sch())?.run(&store)?;
    // Columns: m
    Ok(out[0][0].clone())
}

/// Minimal JSON string escaping — the measure expressions here contain no
/// quotes, but the value has to be a JSON string either way.
fn serde_json_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn row(id: i64, ts: i64, date: i32, time: i64) -> Vec<Value> {
    vec![
        Value::Int(id),
        Value::Timestamp(ts, TimeUnit::Micro),
        Value::Date(date),
        Value::Time(time, TimeUnit::Micro),
    ]
}

fn interval(v: &Value) -> Interval {
    match v {
        Value::Interval(i) => *i,
        other => panic!("expected an INTERVAL, got {other:?}"),
    }
}

// --- timestamp differences -------------------------------------------------

/// The headline case. Rescaling microsecond ticks to nanoseconds overflows i64
/// past ~2262, so this panicked under test and wrapped to a negative interval
/// in release. The days spill keeps it exact.
#[test]
fn timestamp_difference_past_2262_does_not_overflow() {
    let got = measure(
        "LAST(ts) - FIRST(ts)",
        vec![row(1, 0, 0, 0), row(2, YEAR_9999_MICROS, 0, 0)],
    )
    .unwrap();
    let i = interval(&got);
    assert_eq!(i.months, 0);
    // 253_402_214_400 seconds / 86_400 = 2_932_896 whole days, no remainder.
    assert_eq!(i.days, 2_932_896);
    assert_eq!(i.nanos, 0);
}

/// A difference that already fitted keeps its exact previous shape — all of it
/// in `nanos`, nothing spilled — so only the inputs that used to break change.
#[test]
fn timestamp_difference_in_range_keeps_its_nanosecond_shape() {
    let got = measure(
        "LAST(ts) - FIRST(ts)",
        vec![row(1, 0, 0, 0), row(2, 1_000_000, 0, 0)],
    )
    .unwrap();
    assert_eq!(
        interval(&got),
        Interval {
            months: 0,
            days: 0,
            nanos: 1_000_000_000,
        }
    );
}

/// Reversing the operands negates the whole quantity: `%` keeps the dividend's
/// sign, so `days` and `nanos` never disagree about direction.
#[test]
fn timestamp_difference_is_signed_consistently() {
    let got = measure(
        "FIRST(ts) - LAST(ts)",
        vec![row(1, 0, 0, 0), row(2, YEAR_9999_MICROS + 1, 0, 0)],
    )
    .unwrap();
    let i = interval(&got);
    assert!(i.days < 0, "days should be negative, got {}", i.days);
    assert!(
        i.nanos <= 0,
        "nanos should not oppose days, got {}",
        i.nanos
    );
}

/// The spill is lossless because a day is exactly 86_400e9 ns everywhere, so a
/// difference and its negation have the same magnitude in total nanoseconds.
#[test]
fn the_days_spill_is_exact() {
    let got = measure(
        "LAST(ts) - FIRST(ts)",
        vec![row(1, 0, 0, 0), row(2, YEAR_9999_MICROS + 1_500_000, 0, 0)],
    )
    .unwrap();
    let i = interval(&got);
    let total = i.days as i128 * 86_400 * 1_000_000_000 + i.nanos as i128;
    assert_eq!(total, (YEAR_9999_MICROS as i128 + 1_500_000) * 1_000);
}

// --- interval shifts -------------------------------------------------------

#[test]
fn timestamp_plus_small_interval_is_unchanged() {
    let got = measure("LAST(ts) + INTERVAL 1 HOUR", vec![row(1, 0, 0, 0)]).unwrap();
    assert_eq!(got, Value::Timestamp(3_600_000_000, TimeUnit::Micro));
}

/// `months * 30 * 86_400e9` overflowed i64 past ~113 months, so even a
/// ten-year interval was in range of the bug.
#[test]
fn timestamp_plus_decade_interval_does_not_overflow() {
    let got = measure("LAST(ts) + INTERVAL 10 YEAR", vec![row(1, 0, 0, 0)]).unwrap();
    // 120 months x 30 days, in microseconds.
    assert_eq!(
        got,
        Value::Timestamp(120 * 30 * 86_400 * 1_000_000, TimeUnit::Micro)
    );
}

/// Past the *representable* range — i64 microsecond ticks run out around
/// 1.07e8 days from the epoch — the answer is a clean error, not a wrap. (The
/// calendar range DuckDB accepts is narrower; nothing here enforces that, and
/// the ticks are what this function is responsible for.)
#[test]
fn timestamp_plus_out_of_range_interval_is_a_clean_eval_error() {
    let err = measure(
        "LAST(ts) + INTERVAL 200000000 DAY",
        vec![row(1, YEAR_9999_MICROS, 0, 0)],
    )
    .unwrap_err();
    assert!(
        matches!(err, MrError::Eval(ref m) if m.contains("out of range")),
        "expected a clean out-of-range eval error, got {err:?}"
    );
}

/// A shift that is merely large, but still representable, must go through
/// rather than being rejected — the guard is on the i64 range, not on a guess.
#[test]
fn a_large_but_representable_shift_still_works() {
    let got = measure(
        "LAST(ts) + INTERVAL 3000000 DAY",
        vec![row(1, YEAR_9999_MICROS, 0, 0)],
    )
    .unwrap();
    assert_eq!(
        got,
        Value::Timestamp(
            YEAR_9999_MICROS + 3_000_000 * 86_400 * 1_000_000,
            TimeUnit::Micro
        )
    );
}

#[test]
fn date_plus_small_interval_is_unchanged() {
    let got = measure("LAST(d) + INTERVAL 1 DAY", vec![row(1, 0, 100, 0)]).unwrap();
    assert_eq!(got, Value::Date(101));
}

/// `i.months * 30` was i32 arithmetic and wrapped rather than erroring.
#[test]
fn date_plus_month_interval_does_not_truncate() {
    let err = measure("LAST(d) + INTERVAL 100000000 MONTH", vec![row(1, 0, 0, 0)]).unwrap_err();
    assert!(
        matches!(err, MrError::Eval(ref m) if m.contains("out of range")),
        "expected a clean out-of-range eval error, got {err:?}"
    );
}

// --- the arms that were missing entirely -----------------------------------

/// `infer` types `DATE - DATE` as INTERVAL, but `temporal_arith` had no arm for
/// it, so evaluation fell through to the numeric path and died with
/// "non-numeric operand Date(..)" — a bind-time-valid expression that could
/// never produce a row.
#[test]
fn date_minus_date_yields_an_interval() {
    let got = measure(
        "LAST(d) - FIRST(d)",
        vec![row(1, 0, 100, 0), row(2, 0, 130, 0)],
    )
    .unwrap();
    assert_eq!(
        interval(&got),
        Interval {
            months: 0,
            days: 30,
            nanos: 0,
        }
    );
}

/// The same hole for TIME.
#[test]
fn time_minus_time_yields_an_interval() {
    let got = measure(
        "LAST(t) - FIRST(t)",
        vec![row(1, 0, 0, 0), row(2, 0, 0, 5_000_000)],
    )
    .unwrap();
    assert_eq!(
        interval(&got),
        Interval {
            months: 0,
            days: 0,
            nanos: 5_000_000_000,
        }
    );
}
