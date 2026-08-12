//! The scalar function library available to DEFINE and MEASURES.
//!
//! DEFINE predicates are evaluated once per tentative row binding — millions of
//! times on a large partition — so these are plain Rust folds over [`Value`]
//! rather than anything that crosses a process or engine boundary.
//!
//! The set is deliberately small: functions that a row-pattern predicate actually
//! reaches for. Anything else is better expressed on either side of the function
//! call, where the host's full library is available — a row-local computation in
//! the input subquery (`(SELECT *, lower(event) AS ev FROM t)`), or a measure
//! post-processed in the outer `SELECT`.

use crate::error::{MrError, Result};
use crate::types::Ty;
use crate::value::Value;

/// Signature classes, used both to type-check at bind time and to evaluate.
enum Sig {
    /// Numeric in, same numeric family out (`abs`).
    NumericSame,
    /// Numeric in, `DOUBLE` out (`sqrt`).
    NumericDouble,
    /// Numeric in, integer out (`ceil`, `floor`, `round` with one argument).
    NumericRound,
    /// String in, string out (`lower`, `upper`, `trim`, `ltrim`, `rtrim`).
    StringSame,
    /// String in, `BIGINT` out (`length`).
    StringLength,
    /// Variadic, all arguments unified (`coalesce`, `greatest`, `least`).
    VariadicUnified,
    /// `nullif(a, b)` — type of `a`.
    NullIf,
}

fn signature(name: &str) -> Option<(Sig, std::ops::RangeInclusive<usize>)> {
    Some(match name {
        "abs" => (Sig::NumericSame, 1..=1),
        "sqrt" => (Sig::NumericDouble, 1..=1),
        "ceil" | "ceiling" | "floor" | "round" => (Sig::NumericRound, 1..=1),
        "lower" | "upper" | "trim" | "ltrim" | "rtrim" => (Sig::StringSame, 1..=1),
        "length" | "char_length" | "character_length" => (Sig::StringLength, 1..=1),
        "coalesce" | "greatest" | "least" => (Sig::VariadicUnified, 1..=64),
        "nullif" => (Sig::NullIf, 2..=2),
        _ => return None,
    })
}

/// Whether `name` (lower case) is a supported scalar function.
pub fn is_supported(name: &str) -> bool {
    signature(name).is_some()
}

/// The names we accept, for error messages.
pub fn supported_names() -> &'static [&'static str] {
    &[
        "abs",
        "ceil",
        "ceiling",
        "char_length",
        "character_length",
        "coalesce",
        "floor",
        "greatest",
        "least",
        "length",
        "lower",
        "ltrim",
        "nullif",
        "round",
        "rtrim",
        "sqrt",
        "trim",
        "upper",
    ]
}

fn arity_err(name: &str, want: &std::ops::RangeInclusive<usize>, got: usize) -> MrError {
    if want.start() == want.end() {
        MrError::Bind(format!(
            "{name}() takes {} argument(s), got {got}",
            want.start()
        ))
    } else {
        MrError::Bind(format!(
            "{name}() takes {} to {} arguments, got {got}",
            want.start(),
            want.end()
        ))
    }
}

/// Static result type of `name(args…)`, or a bind error.
pub fn result_ty(name: &str, args: &[Ty]) -> Result<Ty> {
    let (sig, arity) = signature(name).ok_or_else(|| {
        MrError::Bind(format!(
            "unknown function '{name}'; supported: {}",
            supported_names().join(", ")
        ))
    })?;
    if !arity.contains(&args.len()) {
        return Err(arity_err(name, &arity, args.len()));
    }
    let numeric = |t: &Ty| -> Result<()> {
        if *t == Ty::Null || t.is_numeric() {
            Ok(())
        } else {
            Err(MrError::Bind(format!("{name}() needs a numeric argument")))
        }
    };
    Ok(match sig {
        Sig::NumericSame => {
            numeric(&args[0])?;
            args[0].clone()
        }
        Sig::NumericDouble => {
            numeric(&args[0])?;
            Ty::Double
        }
        Sig::NumericRound => {
            numeric(&args[0])?;
            // Match DuckDB: rounding a float stays floating, an integer stays integral.
            match &args[0] {
                Ty::Double => Ty::Double,
                t => (*t).clone(),
            }
        }
        Sig::StringSame => Ty::Varchar,
        Sig::StringLength => Ty::Int64,
        Sig::VariadicUnified => {
            let mut acc = args[0].clone();
            for a in &args[1..] {
                acc = acc.unify(a).ok_or_else(|| {
                    MrError::Bind(format!("{name}() arguments have incompatible types"))
                })?;
            }
            acc
        }
        Sig::NullIf => args[0].clone(),
    })
}

/// Evaluate `name(args…)`. Follows SQL NULL propagation: every function here
/// returns NULL on a NULL argument, except `coalesce` (which is the point of it).
pub fn call(name: &str, args: &[Value]) -> Result<Value> {
    let (sig, arity) = signature(name).ok_or_else(|| {
        MrError::Eval(format!(
            "unknown function '{name}'; supported: {}",
            supported_names().join(", ")
        ))
    })?;
    if !arity.contains(&args.len()) {
        return Err(arity_err(name, &arity, args.len()));
    }
    match sig {
        Sig::VariadicUnified => return variadic(name, args),
        Sig::NullIf => {
            return Ok(
                if super::valops::compare(&args[0], &args[1]) == Some(std::cmp::Ordering::Equal) {
                    Value::Null
                } else {
                    args[0].clone()
                },
            )
        }
        _ => {}
    }
    if args.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    match sig {
        Sig::NumericSame => match &args[0] {
            Value::Int(i) => Ok(Value::Int(i.wrapping_abs())),
            Value::HugeInt(i) => Ok(Value::HugeInt(i.wrapping_abs())),
            Value::Double(d) => Ok(Value::Double(d.abs())),
            Value::Decimal(v, s) => Ok(Value::Decimal(v.wrapping_abs(), *s)),
            other => Err(MrError::Eval(format!("{name}() of non-numeric {other:?}"))),
        },
        Sig::NumericDouble => {
            let x = as_f64(name, &args[0])?;
            Ok(Value::Double(x.sqrt()))
        }
        Sig::NumericRound => {
            // Integers and decimals are already integral at scale 0; only the
            // floating and scaled-decimal cases actually move.
            match &args[0] {
                Value::Int(i) => Ok(Value::Int(*i)),
                Value::HugeInt(i) => Ok(Value::HugeInt(*i)),
                Value::Double(d) => Ok(Value::Double(match name {
                    "ceil" | "ceiling" => d.ceil(),
                    "floor" => d.floor(),
                    _ => d.round(),
                })),
                Value::Decimal(v, s) => {
                    let scale = 10i128.pow(*s as u32);
                    let q = v / scale;
                    let r = v % scale;
                    let adj = match name {
                        "ceil" | "ceiling" => (r > 0) as i128,
                        "floor" => -((r < 0) as i128),
                        _ => {
                            if r.abs() * 2 >= scale {
                                r.signum()
                            } else {
                                0
                            }
                        }
                    };
                    Ok(Value::Decimal((q + adj) * scale, *s))
                }
                other => Err(MrError::Eval(format!("{name}() of non-numeric {other:?}"))),
            }
        }
        Sig::StringSame => {
            let s = as_str(name, &args[0])?;
            Ok(Value::Str(match name {
                "lower" => s.to_lowercase(),
                "upper" => s.to_uppercase(),
                "trim" => s.trim().to_string(),
                "ltrim" => s.trim_start().to_string(),
                _ => s.trim_end().to_string(),
            }))
        }
        Sig::StringLength => {
            let s = as_str(name, &args[0])?;
            // Characters, not bytes — DuckDB's length() counts code points.
            Ok(Value::Int(s.chars().count() as i64))
        }
        Sig::VariadicUnified | Sig::NullIf => unreachable!("handled above"),
    }
}

fn variadic(name: &str, args: &[Value]) -> Result<Value> {
    match name {
        "coalesce" => Ok(args
            .iter()
            .find(|v| !v.is_null())
            .cloned()
            .unwrap_or(Value::Null)),
        // GREATEST/LEAST ignore NULLs and are NULL only when every argument is.
        _ => {
            let want_greater = name == "greatest";
            let mut best: Option<&Value> = None;
            for v in args.iter().filter(|v| !v.is_null()) {
                best = Some(match best {
                    None => v,
                    Some(b) => match super::valops::compare(v, b) {
                        Some(std::cmp::Ordering::Greater) if want_greater => v,
                        Some(std::cmp::Ordering::Less) if !want_greater => v,
                        _ => b,
                    },
                });
            }
            Ok(best.cloned().unwrap_or(Value::Null))
        }
    }
}

fn as_f64(name: &str, v: &Value) -> Result<f64> {
    v.as_f64()
        .ok_or_else(|| MrError::Eval(format!("{name}() of non-numeric {v:?}")))
}

fn as_str(name: &str, v: &Value) -> Result<String> {
    match v {
        Value::Str(s) => Ok(s.clone()),
        other => Err(MrError::Eval(format!(
            "{name}() needs a string argument, got {other:?}"
        ))),
    }
}
