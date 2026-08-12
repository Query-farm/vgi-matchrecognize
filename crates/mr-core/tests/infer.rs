//! Bind-time type-inference table (spec §C). `(expr, col types) -> Ty`.

use std::collections::HashMap;

use mr_core::expr::parse;
use mr_core::types::{infer, BindSchema, TimeUnit, Ty};

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
    cols.insert("amount".into(), Ty::Double);
    cols.insert("qty".into(), Ty::Int64);
    cols.insert("name".into(), Ty::Varchar);
    cols.insert("ts".into(), Ty::Timestamp(TimeUnit::Micro));
    cols.insert("d".into(), Ty::Date);
    cols.insert("rate".into(), Ty::Decimal(10, 4));
    Sch {
        cols,
        vars: vec!["A".into(), "B".into(), "START".into(), "DOWN".into()],
    }
}

fn ty(s: &str) -> Ty {
    infer(&parse(s).unwrap(), &sch()).unwrap()
}

#[test]
fn leaves() {
    assert_eq!(ty("price"), Ty::Int64);
    assert_eq!(ty("A.price"), Ty::Int64); // qualifier doesn't change type
    assert_eq!(ty("42"), Ty::Int64);
    assert_eq!(ty("1.5"), Ty::Double);
    assert_eq!(ty("'x'"), Ty::Varchar);
    assert_eq!(ty("TRUE"), Ty::Boolean);
    assert_eq!(ty("INTERVAL 5 SECOND"), Ty::Interval);
}

#[test]
fn functions() {
    assert_eq!(ty("MATCH_NUMBER()"), Ty::Int64);
    assert_eq!(ty("CLASSIFIER()"), Ty::Varchar);
    assert_eq!(ty("COUNT(*)"), Ty::Int64);
    assert_eq!(ty("COUNT(A.*)"), Ty::Int64);
    assert_eq!(ty("COUNT(price)"), Ty::Int64);
    assert_eq!(ty("FIRST(A.price)"), Ty::Int64);
    assert_eq!(ty("LAST(amount)"), Ty::Double);
    assert_eq!(ty("PREV(price)"), Ty::Int64);
    assert_eq!(ty("MIN(price)"), Ty::Int64);
    assert_eq!(ty("MAX(amount)"), Ty::Double);
}

#[test]
fn sum_widening() {
    assert_eq!(ty("SUM(qty)"), Ty::HugeInt); // integer -> HUGEINT
    assert_eq!(ty("SUM(amount)"), Ty::Double); // float -> DOUBLE
    assert_eq!(ty("SUM(rate)"), Ty::Decimal(38, 4)); // decimal -> DECIMAL(38,s)
}

#[test]
fn avg_widening() {
    assert_eq!(ty("AVG(qty)"), Ty::Double);
    assert_eq!(ty("AVG(amount)"), Ty::Double);
}

#[test]
fn arithmetic_promotion() {
    assert_eq!(ty("price + qty"), Ty::Int64);
    assert_eq!(ty("price + amount"), Ty::Double);
    assert_eq!(ty("price - qty"), Ty::Int64);
    assert_eq!(ty("price * 2"), Ty::Int64);
    assert_eq!(ty("price / qty"), Ty::Double); // division widens to DOUBLE
    assert_eq!(ty("FIRST(START.price) - LAST(DOWN.price)"), Ty::Int64);
}

#[test]
fn temporal_arithmetic() {
    assert_eq!(ty("ts + INTERVAL 1 HOUR"), Ty::Timestamp(TimeUnit::Micro));
    assert_eq!(ty("d + INTERVAL 1 DAY"), Ty::Date);
    assert_eq!(ty("ts - ts"), Ty::Interval);
}

#[test]
fn comparison_and_logical_are_boolean() {
    assert_eq!(ty("price < PREV(price)"), Ty::Boolean);
    assert_eq!(ty("price > 5 AND qty < 3"), Ty::Boolean);
    assert_eq!(ty("NOT (price = 1)"), Ty::Boolean);
    assert_eq!(ty("price IS NULL"), Ty::Boolean);
    assert_eq!(ty("price BETWEEN 1 AND 9"), Ty::Boolean);
    assert_eq!(ty("name IN ('a', 'b')"), Ty::Boolean);
}

#[test]
fn concat_is_varchar() {
    assert_eq!(ty("name || 'x'"), Ty::Varchar);
}

#[test]
fn explicit_cast_wins() {
    assert_eq!(ty("(price / qty)::DECIMAL(18,6)"), Ty::Decimal(18, 6));
    assert_eq!(ty("CAST(price AS DOUBLE)"), Ty::Double);
}

#[test]
fn uninferable_and_errors() {
    // Bare NULL -> the unknown type (the worker turns this into an override hint).
    assert_eq!(ty("NULL"), Ty::Null);
    // Unknown column.
    assert!(infer(&parse("nosuch").unwrap(), &sch()).is_err());
    // Mismatched comparison.
    assert!(infer(&parse("name < price").unwrap(), &sch()).is_err());
    // VARCHAR + BIGINT.
    assert!(infer(&parse("name + price").unwrap(), &sch()).is_err());
    // qualifier that isn't a pattern variable.
    assert!(infer(&parse("Z.price").unwrap(), &sch()).is_err());
}
