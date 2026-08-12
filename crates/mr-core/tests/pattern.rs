//! PATTERN grammar tests: every production, precedence, quantifiers (greedy +
//! reluctant), grouping, anchors, and clean errors on malformed input.

use mr_core::pattern::{compile, explain, parse, Anchor, Pattern};

fn p(s: &str) -> Pattern {
    parse(s).unwrap()
}

#[test]
fn single_variable() {
    assert_eq!(p("A"), Pattern::Var("A".into()));
    // Case-insensitive: canonicalized upper.
    assert_eq!(p("down"), Pattern::Var("DOWN".into()));
}

#[test]
fn concatenation() {
    match p("A B C") {
        Pattern::Concat(items) => assert_eq!(items.len(), 3),
        other => panic!("expected concat, got {other:?}"),
    }
}

#[test]
fn alternation_binds_looser_than_concat() {
    // A B | C D  ==  (A B) | (C D)
    match p("A B | C D") {
        Pattern::Alt(branches) => {
            assert_eq!(branches.len(), 2);
            assert!(matches!(branches[0], Pattern::Concat(_)));
            assert!(matches!(branches[1], Pattern::Concat(_)));
        }
        other => panic!("expected alt, got {other:?}"),
    }
}

#[test]
fn quantifiers() {
    assert!(matches!(
        p("A*"),
        Pattern::Quant {
            min: 0,
            max: None,
            greedy: true,
            ..
        }
    ));
    assert!(matches!(
        p("A+"),
        Pattern::Quant {
            min: 1,
            max: None,
            greedy: true,
            ..
        }
    ));
    assert!(matches!(
        p("A?"),
        Pattern::Quant {
            min: 0,
            max: Some(1),
            greedy: true,
            ..
        }
    ));
    assert!(matches!(
        p("A{3}"),
        Pattern::Quant {
            min: 3,
            max: Some(3),
            ..
        }
    ));
    assert!(matches!(
        p("A{2,}"),
        Pattern::Quant {
            min: 2,
            max: None,
            ..
        }
    ));
    assert!(matches!(
        p("A{2,5}"),
        Pattern::Quant {
            min: 2,
            max: Some(5),
            ..
        }
    ));
    assert!(matches!(
        p("A{,4}"),
        Pattern::Quant {
            min: 0,
            max: Some(4),
            ..
        }
    ));
}

#[test]
fn reluctant_quantifiers() {
    assert!(matches!(
        p("A+?"),
        Pattern::Quant {
            greedy: false,
            min: 1,
            ..
        }
    ));
    assert!(matches!(
        p("A*?"),
        Pattern::Quant {
            greedy: false,
            min: 0,
            ..
        }
    ));
    assert!(matches!(
        p("A{2,}?"),
        Pattern::Quant {
            greedy: false,
            min: 2,
            max: None,
            ..
        }
    ));
}

#[test]
fn grouping() {
    match p("(A B)+") {
        Pattern::Quant {
            inner,
            min: 1,
            max: None,
            ..
        } => {
            assert!(matches!(*inner, Pattern::Concat(_)));
        }
        other => panic!("expected quantified group, got {other:?}"),
    }
}

#[test]
fn anchors() {
    match p("^ A $") {
        Pattern::Concat(items) => {
            assert_eq!(items[0], Pattern::Anchor(Anchor::Start));
            assert_eq!(items[2], Pattern::Anchor(Anchor::End));
        }
        other => panic!("expected concat with anchors, got {other:?}"),
    }
}

#[test]
fn variables_collected_in_order() {
    assert_eq!(
        p("START DOWN+ UP+").variables(),
        vec!["START", "DOWN", "UP"]
    );
}

#[test]
fn malformed_patterns_error() {
    assert!(parse("").is_err());
    assert!(parse("(A").is_err()); // unbalanced
    assert!(parse("A)").is_err()); // trailing
    assert!(parse("A{5,2}").is_err()); // hi < lo
    assert!(parse("*A").is_err()); // quantifier with no primary
    assert!(parse("{- A -}").is_err()); // exclusion (v1.1)
}

#[test]
fn unbounded_quantifier_over_nullable_rejected() {
    // (A?)* would loop forever; rejected at compile.
    assert!(parse("(A?)*").is_err());
    assert!(parse("(A*)+").is_err());
    // But a bounded repetition of a nullable body is fine.
    assert!(parse("(A?){3}").is_ok());
}

#[test]
fn compiles_to_program_with_match_terminator() {
    let pat = p("A B");
    // Compiling resolves each pattern variable to its id in the plan's label set,
    // so the VM never carries label strings.
    let labels = mr_core::engine::LabelSet::new(pat.variables(), &[]);
    let prog = compile(&pat, &labels).unwrap();
    assert!(matches!(
        prog.insts.last(),
        Some(mr_core::pattern::compile::Inst::Match)
    ));
    // `A` and `B` are the first two ids, in declaration order.
    assert_eq!(
        prog.insts.first(),
        Some(&mr_core::pattern::compile::Inst::Char(0))
    );
    assert!(prog
        .insts
        .contains(&mr_core::pattern::compile::Inst::Char(1)));
}

#[test]
fn explain_renders() {
    let s = explain(&p("START DOWN+ UP+"));
    assert!(s.contains("START"));
    assert!(s.contains("greedy"));
}
