//! Deeply nested `pattern` / `define` / `measures` input must be rejected with a
//! clean error rather than aborting the process.
//!
//! Both parsers descend recursively through grouping, so ~1000 nested parentheses
//! used to exhaust the native stack — reachable straight from a SQL string
//! literal, which made it a crash any caller could trigger.

use mr_core::expr::parser as expr_parser;
use mr_core::pattern::parser as pattern_parser;

/// Nesting a realistic pattern a few levels deep keeps working.
#[test]
fn moderate_pattern_nesting_is_accepted() {
    for n in [1usize, 5, 20, 100] {
        let src = format!("{}A B{}", "(".repeat(n), ")".repeat(n));
        assert!(
            pattern_parser::parse(&src).is_ok(),
            "{n} levels of pattern nesting should parse"
        );
    }
}

#[test]
fn moderate_expression_nesting_is_accepted() {
    for n in [1usize, 5, 20, 100] {
        let src = format!("{}price + 1{}", "(".repeat(n), ")".repeat(n));
        assert!(
            expr_parser::parse(&src).is_ok(),
            "{n} levels of expression nesting should parse"
        );
    }
}

/// Past the limit: a clean error, and — critically — the process survives to
/// make the assertion.
#[test]
fn pathological_pattern_nesting_errors_cleanly() {
    for n in [200usize, 1_000, 50_000] {
        let src = format!("{}A{}", "(".repeat(n), ")".repeat(n));
        let err = pattern_parser::parse(&src)
            .expect_err("deeply nested pattern must be refused, not crash");
        assert!(
            err.to_string().contains("nests deeper"),
            "expected a nesting-limit error at {n} levels, got: {err}"
        );
    }
}

#[test]
fn pathological_expression_nesting_errors_cleanly() {
    for n in [200usize, 1_000, 50_000] {
        let src = format!("{}1{}", "(".repeat(n), ")".repeat(n));
        let err = expr_parser::parse(&src)
            .expect_err("deeply nested expression must be refused, not crash");
        assert!(
            err.to_string().contains("nests deeper"),
            "expected a nesting-limit error at {n} levels, got: {err}"
        );
    }
}

/// Unbalanced deep input (no closing parens) must also be refused, not crash.
#[test]
fn unbalanced_deep_nesting_errors_cleanly() {
    let src = "(".repeat(100_000);
    assert!(pattern_parser::parse(&src).is_err());
    assert!(expr_parser::parse(&src).is_err());
}

/// Deep nesting reached through function-call arguments rather than parentheses.
#[test]
fn deep_call_nesting_errors_cleanly() {
    let src = format!("{}price{}", "PREV(".repeat(1_000), ")".repeat(1_000));
    assert!(
        expr_parser::parse(&src).is_err(),
        "deeply nested calls must be refused, not crash"
    );
}
