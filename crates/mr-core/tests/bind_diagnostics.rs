//! What a user is told when a `match_recognize` call does not bind.
//!
//! Four properties, all of them about the *message* rather than the outcome:
//!
//! 1. A DEFINE predicate is type-checked at bind, so a mistake there is an error
//!    rather than a silently empty result (see `plan::check_predicate`).
//! 2. Every error out of `define`/`measures` names the key it came from — the
//!    parser sees one expression and cannot say which of a dozen it was handed.
//! 3. Parse errors quote the offending source text, never a Rust token name, and
//!    carry a caret line pointing into the string as written.
//! 4. Types are named in SQL (`VARCHAR`, `BIGINT`), not as `Ty` variants.

use std::collections::HashMap;

use mr_core::expr::parse_type_name;
use mr_core::plan::{Plan, PlanConfig};
use mr_core::types::{BindSchema, TimeUnit, Ty};

struct Sch {
    cols: HashMap<String, Ty>,
    vars: Vec<String>,
}
impl BindSchema for Sch {
    fn col_ty(&self, name: &str) -> Option<Ty> {
        self.cols
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, t)| t.clone())
    }
    fn is_variable(&self, name: &str) -> bool {
        self.vars.iter().any(|v| v.eq_ignore_ascii_case(name))
    }
}

fn sch() -> Sch {
    let mut cols = HashMap::new();
    cols.insert("price".into(), Ty::Int64);
    cols.insert("sym".into(), Ty::Varchar);
    cols.insert("ts".into(), Ty::Int64);
    cols.insert("active".into(), Ty::Boolean);
    Sch {
        cols,
        vars: vec!["A".into(), "B".into()],
    }
}

/// Bind a call, defaulting everything the individual tests do not care about.
fn bind(pattern: &str, define: &str, measures: Option<&str>) -> mr_core::Result<Plan> {
    let cfg = PlanConfig {
        pattern: pattern.into(),
        define_json: define.into(),
        measures_json: measures.map(str::to_string),
        partition_by: vec![],
        order_by: vec!["ts".into()],
        rows_all: false,
        omit_empty_matches: false,
        subset_json: String::new(),
        after: "past last row".into(),
        step_budget: None,
    };
    Plan::build(&cfg, &sch())
}

fn err(pattern: &str, define: &str, measures: Option<&str>) -> String {
    // `Plan` is deliberately not `Debug` (it owns a compiled program), so match
    // rather than `expect_err`.
    match bind(pattern, define, measures) {
        Ok(_) => panic!("expected this call not to bind"),
        Err(e) => e.to_string(),
    }
}

// ---------------------------------------------------------------------------
// 1. DEFINE predicates are type-checked at bind.
// ---------------------------------------------------------------------------

/// A predicate that is not boolean can never bind a row, so it used to produce
/// an empty result and no message at all.
#[test]
fn a_non_boolean_define_predicate_is_rejected() {
    let e = err("A B+", r#"{"B": "price"}"#, None);
    assert!(e.contains("must be BOOLEAN"), "{e}");
    assert!(
        e.contains("define['B']"),
        "the key belongs in the error: {e}"
    );
}

/// Comparing a VARCHAR column with an integer is the mistake that silently
/// matched nothing: three-valued logic made every row simply not-true.
#[test]
fn a_type_mismatch_in_a_define_predicate_is_rejected() {
    let e = err("A B+", r#"{"B": "sym > 3"}"#, None);
    assert!(e.contains("cannot compare"), "{e}");
    assert!(e.contains("define['B']"), "{e}");
}

/// The important one: an unknown column used to be a *runtime* error, so whether
/// the query failed depended on whether the matcher ever evaluated the
/// predicate. On a short partition it did not, and the query returned zero rows
/// — passing on a sample and failing in production.
#[test]
fn an_unknown_column_in_a_define_predicate_is_caught_at_bind() {
    let e = err("A B", r#"{"B": "prcie < PREV(price)"}"#, None);
    assert!(e.contains("unknown column 'prcie'"), "{e}");
    assert!(e.contains("define['B']"), "{e}");
    // Bind-time, not evaluation-time: the category is what makes it independent
    // of whether any row reaches the predicate.
    assert!(!e.contains("evaluation error"), "{e}");
}

/// The check must not reject ordinary predicates — this is the whole existing
/// corpus's shape.
#[test]
fn ordinary_predicates_still_bind() {
    for pred in [
        "price < PREV(price)",
        "price > A.price AND ts <= PREV(ts) + 1",
        "active", // a BOOLEAN column is a predicate
        "price IS NOT NULL",
        "sym IN ('a', 'b')",
        "price BETWEEN 1 AND 10",
        "NOT (price = 3)",
        "COUNT(*) < 5",
        "CLASSIFIER() = 'A'",
    ] {
        let json = format!(r#"{{"B": "{pred}"}}"#);
        assert!(
            bind("A B+", &json, None).is_ok(),
            "expected `{pred}` to bind"
        );
    }
}

/// A statically-NULL predicate is well-formed SQL — never true, but the
/// three-valued logic in `eval` handles it, so it is not an error.
#[test]
fn a_null_predicate_is_accepted() {
    assert!(bind("A B+", r#"{"B": "NULL"}"#, None).is_ok());
}

// ---------------------------------------------------------------------------
// 2. Errors name the clause and key they came from.
// ---------------------------------------------------------------------------

/// With several measures, the bare parser message ("unknown function 'lsat'")
/// gives no way to tell which one contains the typo.
#[test]
fn a_measure_error_names_the_measure() {
    let e = err(
        "A",
        "{}",
        Some(r#"{"x": "LAST(price)", "y": "LSAT(price)", "z": "COUNT(*)"}"#),
    );
    assert!(e.contains("measures['y']"), "{e}");
    assert!(e.contains("unknown function 'lsat'"), "{e}");
}

/// The array form names the measure by its `as`, not by its index: that is what
/// the user wrote and what the output column is called.
#[test]
fn the_array_form_names_the_measure_by_its_alias() {
    let e = err(
        "A",
        "{}",
        Some(r#"[{"as": "ratio", "expr": "sym + price"}]"#),
    );
    assert!(e.contains("measures['ratio']"), "{e}");
}

/// The type-override hint keeps working, and no longer repeats the name now
/// that the context prefix carries it.
#[test]
fn an_uninferable_measure_still_points_at_the_type_override() {
    let e = err("A", "{}", Some(r#"{"m": "NULL"}"#));
    assert!(e.contains("measures['m']"), "{e}");
    assert!(
        e.contains("\"type\""),
        "the escape hatch belongs in the message: {e}"
    );
    assert_eq!(
        e.matches("'m'").count(),
        1,
        "the measure name should be stated once, not twice: {e}"
    );
}

// ---------------------------------------------------------------------------
// 3. Parse errors quote source text and point at it.
// ---------------------------------------------------------------------------

/// `expected RParen` names our AST; `expected ')'` names their pattern.
#[test]
fn a_pattern_error_quotes_source_text_not_a_rust_token_name() {
    let e = err("A (B | C D", "{}", None);
    assert!(e.contains("expected ')'"), "{e}");
    for leaked in ["RParen", "LParen", "Tok::", "Some(", "None"] {
        assert!(
            !e.contains(leaked),
            "leaked the Rust token name {leaked}: {e}"
        );
    }
}

/// The caret line reproduces the pattern and marks the position.
#[test]
fn a_pattern_error_carries_a_caret_line() {
    let e = err("A (B | C D", "{}", None);
    let lines: Vec<&str> = e.lines().collect();
    assert_eq!(lines.len(), 3, "expected message + source + caret: {e}");
    assert!(lines[1].trim_end().ends_with("A (B | C D"), "{e}");
    // The caret sits one past the last character: "found end of pattern".
    let col = lines[2].find('^').unwrap();
    assert_eq!(col, lines[1].len(), "{e}");
}

/// The same for expression errors, which additionally carry their key.
#[test]
fn an_expression_error_points_into_the_predicate() {
    let e = err("A B", r#"{"B": "price < PREV(price) AND ts >"}"#, None);
    assert!(e.contains("define['B']"), "{e}");
    assert!(e.contains("found end of input"), "{e}");
    let lines: Vec<&str> = e.lines().collect();
    assert!(
        lines[1].contains("price < PREV(price) AND ts >"),
        "the predicate as written belongs above the caret: {e}"
    );
    assert!(lines[2].contains('^'), "{e}");
}

/// A mid-string error points at the offending token rather than at the end.
#[test]
fn a_caret_marks_the_offending_token_mid_expression() {
    let e = err("A B", r#"{"B": "price < * 3"}"#, None);
    let lines: Vec<&str> = e.lines().collect();
    let col = lines[2].find('^').unwrap();
    assert_eq!(
        lines[1].chars().nth(col),
        Some('*'),
        "the caret should mark the '*': {e}"
    );
}

/// Quantifier bounds are checked with the same treatment.
#[test]
fn a_bad_quantifier_reports_its_bounds_with_a_caret() {
    let e = err("A{5,2}", "{}", None);
    assert!(
        e.contains("upper bound 2 is less than lower bound 5"),
        "{e}"
    );
    assert!(e.lines().count() == 3, "expected a caret line: {e}");
}

// ---------------------------------------------------------------------------
// 4. Types are named in SQL, not as `Ty` variants.
// ---------------------------------------------------------------------------

/// `cannot compare Varchar with Int64` names our AST; a reader cannot write
/// either word in SQL.
#[test]
fn an_inference_error_names_types_in_sql() {
    let e = err("A B+", r#"{"B": "sym > 3"}"#, None);
    assert!(e.contains("cannot compare VARCHAR with BIGINT"), "{e}");
    for leaked in ["Varchar", "Int64", "HugeInt", "Ty::"] {
        assert!(
            !e.contains(leaked),
            "leaked the Rust type name {leaked}: {e}"
        );
    }
}

/// The same for the DEFINE predicate check, whose whole job is to say what the
/// predicate came out as.
#[test]
fn the_define_predicate_check_names_its_type_in_sql() {
    let e = err("A B+", r#"{"B": "price"}"#, None);
    assert!(e.contains("must be BOOLEAN, but this one is BIGINT"), "{e}");
}

/// Every spelling `Display` produces is one `parse_type_name` accepts, so a
/// type quoted back at a user is a type they can paste into the `type` override.
/// The unit-carrying temporal types are the exception worth stating: DuckDB has
/// one `TIME` spelling, and `TIMESTAMP` means microseconds.
#[test]
fn every_rendered_type_name_parses_back() {
    let cases = [
        (Ty::Boolean, Ty::Boolean),
        (Ty::Int64, Ty::Int64),
        (Ty::HugeInt, Ty::HugeInt),
        (Ty::Double, Ty::Double),
        (Ty::Decimal(18, 6), Ty::Decimal(18, 6)),
        (Ty::Varchar, Ty::Varchar),
        (Ty::Date, Ty::Date),
        (Ty::Interval, Ty::Interval),
        (
            Ty::Timestamp(TimeUnit::Micro),
            Ty::Timestamp(TimeUnit::Micro),
        ),
        // Rendered as bare TIME, which parses back at microsecond resolution.
        (Ty::Time(TimeUnit::Nano), Ty::Time(TimeUnit::Micro)),
    ];
    for (ty, expect) in cases {
        let rendered = ty.to_string();
        let parsed = parse_type_name(&rendered)
            .unwrap_or_else(|e| panic!("`{rendered}` (from {ty:?}) did not parse back: {e}"));
        assert_eq!(
            parsed, expect,
            "`{rendered}` round-tripped to the wrong type"
        );
    }
}

/// A list renders as the element type plus `[]`, the way DuckDB writes it.
#[test]
fn a_list_type_renders_as_duckdb_spells_it() {
    assert_eq!(Ty::List(Box::new(Ty::Int64)).to_string(), "BIGINT[]");
    assert_eq!(
        Ty::List(Box::new(Ty::List(Box::new(Ty::Varchar)))).to_string(),
        "VARCHAR[][]"
    );
}
