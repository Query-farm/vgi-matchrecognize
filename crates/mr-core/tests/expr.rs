//! Expression Pratt-parser tests: precedence, associativity, qualified refs,
//! navigation/aggregate calls, INTERVAL, casts, BETWEEN/IN/IS NULL.

use mr_core::expr::ast::{AggArg, AggKind, BinOp, Expr, NavKind};
use mr_core::expr::parse;

fn e(s: &str) -> Expr {
    parse(s).unwrap()
}

#[test]
fn precedence_mul_over_add() {
    // a + b * c == a + (b * c)
    match e("a + b * c") {
        Expr::Binary {
            op: BinOp::Add,
            rhs,
            ..
        } => {
            assert!(matches!(*rhs, Expr::Binary { op: BinOp::Mul, .. }));
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn precedence_comparison_over_and() {
    // a < b AND c > d  ==  (a<b) AND (c>d)
    match e("a < b AND c > d") {
        Expr::Binary {
            op: BinOp::And,
            lhs,
            rhs,
        } => {
            assert!(matches!(*lhs, Expr::Binary { op: BinOp::Lt, .. }));
            assert!(matches!(*rhs, Expr::Binary { op: BinOp::Gt, .. }));
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn qualified_ref() {
    assert_eq!(e("A.price"), Expr::Qualified("A".into(), "price".into()));
}

#[test]
fn navigation_calls() {
    match e("PREV(price)") {
        Expr::Nav {
            kind: NavKind::Prev,
            offset: 1,
            ..
        } => {}
        other => panic!("got {other:?}"),
    }
    match e("PREV(price, 2)") {
        Expr::Nav {
            kind: NavKind::Prev,
            offset: 2,
            ..
        } => {}
        other => panic!("got {other:?}"),
    }
    // FIRST/LAST default to offset 0.
    match e("FIRST(A.ts)") {
        Expr::Nav {
            kind: NavKind::First,
            offset: 0,
            ..
        } => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn aggregates() {
    assert!(matches!(
        e("COUNT(*)"),
        Expr::Agg {
            kind: AggKind::Count,
            arg: AggArg::Star
        }
    ));
    assert!(matches!(
        e("COUNT(A.*)"),
        Expr::Agg {
            kind: AggKind::Count,
            arg: AggArg::QualStar(_)
        }
    ));
    assert!(matches!(
        e("SUM(qty)"),
        Expr::Agg {
            kind: AggKind::Sum,
            ..
        }
    ));
    // SUM(*) is rejected.
    assert!(parse("SUM(*)").is_err());
}

#[test]
fn classifier_and_match_number() {
    assert_eq!(e("CLASSIFIER()"), Expr::Classifier(None));
    assert_eq!(e("MATCH_NUMBER()"), Expr::MatchNumber);
}

#[test]
fn running_final_sugar() {
    // FINAL COUNT(FAIL.*) parses as FINAL(COUNT(FAIL.*)).
    match e("FINAL COUNT(FAIL.*)") {
        Expr::RunningFinal {
            final_: true,
            inner,
        } => {
            assert!(matches!(
                *inner,
                Expr::Agg {
                    kind: AggKind::Count,
                    ..
                }
            ));
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn interval_literal() {
    match e("INTERVAL 1800 SECOND") {
        Expr::Interval {
            months: 0,
            days: 0,
            nanos,
        } => {
            assert_eq!(nanos, 1800 * 1_000_000_000)
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn cast_forms() {
    assert!(matches!(e("price::DOUBLE"), Expr::Cast { .. }));
    assert!(matches!(e("CAST(price AS BIGINT)"), Expr::Cast { .. }));
}

#[test]
fn between_in_isnull() {
    assert!(matches!(
        e("x BETWEEN 1 AND 10"),
        Expr::Between { negated: false, .. }
    ));
    assert!(matches!(
        e("x NOT BETWEEN 1 AND 10"),
        Expr::Between { negated: true, .. }
    ));
    assert!(matches!(
        e("x IN (1, 2, 3)"),
        Expr::In { negated: false, .. }
    ));
    assert!(matches!(
        e("x IS NULL"),
        Expr::IsNull { negated: false, .. }
    ));
    assert!(matches!(
        e("x IS NOT NULL"),
        Expr::IsNull { negated: true, .. }
    ));
}

#[test]
fn string_concat_and_literals() {
    assert!(matches!(
        e("a || b"),
        Expr::Binary {
            op: BinOp::Concat,
            ..
        }
    ));
    assert_eq!(e("'it''s'"), Expr::Str("it's".into()));
    assert_eq!(e("42"), Expr::Int(42));
    assert_eq!(e("1.5"), Expr::Double(1.5));
}

#[test]
fn malformed_expr_errors() {
    assert!(parse("a +").is_err());
    assert!(parse("(a").is_err());
    assert!(parse("A.").is_err());
}

// --- prefix NOT precedence -------------------------------------------------
//
// SQL orders these OR < AND < NOT < comparison, so `NOT` swallows the whole
// comparison to its right but stops at AND/OR. Parsing it at the unary level
// instead inverted `NOT x IS NULL` into `x IS NULL`.

#[test]
fn not_is_looser_than_comparison() {
    // NOT a = b  ==  NOT (a = b), not (NOT a) = b.
    match e("NOT a = b") {
        Expr::Not(inner) => assert!(matches!(*inner, Expr::Binary { op: BinOp::Eq, .. })),
        other => panic!("got {other:?}"),
    }
    match e("NOT p > 10") {
        Expr::Not(inner) => assert!(matches!(*inner, Expr::Binary { op: BinOp::Gt, .. })),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn not_is_looser_than_is_null() {
    // The shape that inverted the end-to-end answer: `Not(IsNull(a))`, never
    // `IsNull(Not(a))`.
    match e("NOT a IS NULL") {
        Expr::Not(inner) => assert!(matches!(*inner, Expr::IsNull { negated: false, .. })),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn not_is_tighter_than_and_and_or() {
    // NOT a AND b  ==  (NOT a) AND b.
    match e("NOT a AND b") {
        Expr::Binary {
            op: BinOp::And,
            lhs,
            rhs,
        } => {
            assert!(matches!(*lhs, Expr::Not(_)));
            assert_eq!(*rhs, Expr::Col("b".into()));
        }
        other => panic!("got {other:?}"),
    }
    match e("NOT a OR b") {
        Expr::Binary {
            op: BinOp::Or, lhs, ..
        } => assert!(matches!(*lhs, Expr::Not(_))),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn not_over_between_and_in() {
    // Prefix NOT absorbs a comparison-level operator, giving the un-negated
    // node wrapped in `Not` — semantically the same as the infix `negated: true`
    // form that `between_in_isnull` pins.
    match e("NOT x BETWEEN 1 AND 10") {
        Expr::Not(inner) => assert!(matches!(*inner, Expr::Between { negated: false, .. })),
        other => panic!("got {other:?}"),
    }
    match e("NOT x IN (1, 2)") {
        Expr::Not(inner) => assert!(matches!(*inner, Expr::In { negated: false, .. })),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn double_negation() {
    match e("NOT NOT a") {
        Expr::Not(inner) => assert!(matches!(*inner, Expr::Not(_))),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn unary_minus_still_binds_tighter_than_mul() {
    // Renumbering the ladder for BP_NOT must not move unary minus.
    match e("-a * b") {
        Expr::Binary {
            op: BinOp::Mul,
            lhs,
            ..
        } => assert!(matches!(*lhs, Expr::Neg(_))),
        other => panic!("got {other:?}"),
    }
    match e("-a + b") {
        Expr::Binary {
            op: BinOp::Add,
            lhs,
            ..
        } => assert!(matches!(*lhs, Expr::Neg(_))),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn running_final_still_binds_tightly() {
    // FINAL LAST(x) > 3  ==  (FINAL LAST(x)) > 3.
    match e("FINAL LAST(x) > 3") {
        Expr::Binary {
            op: BinOp::Gt, lhs, ..
        } => assert!(matches!(*lhs, Expr::RunningFinal { final_: true, .. })),
        other => panic!("got {other:?}"),
    }
}
