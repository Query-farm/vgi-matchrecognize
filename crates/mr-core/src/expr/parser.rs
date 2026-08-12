//! A Pratt (precedence-climbing) parser for the DEFINE / MEASURES expression
//! language. Produces the [`Expr`] AST.

use super::ast::{AggArg, AggKind, BinOp, Expr, NavKind};
use super::lexer::{lex, Tok};
use crate::error::{MrError, Result};
use crate::types::{TimeUnit, Ty};

// Binding powers (higher = tighter).
const BP_OR: u8 = 1;
const BP_AND: u8 = 2;
const BP_CMP: u8 = 3;
const BP_CONCAT: u8 = 4;
const BP_ADD: u8 = 5;
const BP_MUL: u8 = 6;
const BP_UNARY: u8 = 7;

/// Maximum expression nesting depth. `parse_expr` recurses through parenthesized
/// sub-expressions and call arguments, so deeply nested input would otherwise
/// exhaust the native stack and abort the process. Real DEFINE/MEASURES
/// expressions nest a handful of levels; this reports a clean error instead.
const MAX_DEPTH: u32 = 128;

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    depth: u32,
}

impl Parser {
    /// Enter one nesting level, refusing input deep enough to blow the stack.
    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(MrError::Expr(format!(
                "expression nests deeper than the {MAX_DEPTH}-level limit"
            )));
        }
        Ok(())
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn peek_at(&self, n: usize) -> Option<&Tok> {
        self.toks.get(self.pos + n)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, t: &Tok) -> Result<()> {
        match self.next() {
            Some(ref got) if got == t => Ok(()),
            Some(got) => Err(MrError::Expr(format!("expected {t:?}, found {got:?}"))),
            None => Err(MrError::Expr(format!("expected {t:?}, found end of input"))),
        }
    }

    /// Uppercased keyword text if the next token is an identifier.
    fn peek_kw(&self) -> Option<String> {
        match self.peek() {
            Some(Tok::Ident(s)) => Some(s.to_ascii_uppercase()),
            _ => None,
        }
    }

    fn parse_expr(&mut self, min_bp: u8) -> Result<Expr> {
        self.enter()?;
        let mut lhs = self.parse_prefix()?;
        while let Some((lbp, _)) = self.infix_bp() {
            if lbp < min_bp {
                break;
            }
            lhs = self.parse_infix(lhs, lbp)?;
        }
        // Only decremented on success; an error aborts the whole parse.
        self.depth -= 1;
        Ok(lhs)
    }

    /// Left binding power of the upcoming infix/postfix operator, if any.
    fn infix_bp(&self) -> Option<(u8, ())> {
        match self.peek()? {
            Tok::Eq | Tok::Ne | Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge => Some((BP_CMP, ())),
            Tok::Concat => Some((BP_CONCAT, ())),
            Tok::Plus | Tok::Minus => Some((BP_ADD, ())),
            Tok::Star | Tok::Slash | Tok::Percent => Some((BP_MUL, ())),
            Tok::Ident(s) => match s.to_ascii_uppercase().as_str() {
                "OR" => Some((BP_OR, ())),
                "AND" => Some((BP_AND, ())),
                "IS" | "BETWEEN" | "IN" | "LIKE" => Some((BP_CMP, ())),
                "NOT" => {
                    // Only as `NOT BETWEEN` / `NOT IN`.
                    match self.peek_at(1) {
                        Some(Tok::Ident(s2)) => {
                            let k = s2.to_ascii_uppercase();
                            if k == "BETWEEN" || k == "IN" {
                                Some((BP_CMP, ()))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn parse_infix(&mut self, lhs: Expr, lbp: u8) -> Result<Expr> {
        // Arithmetic / comparison / concat operators.
        let bin = |op: BinOp| -> Option<BinOp> { Some(op) };
        let op = match self.peek().cloned() {
            Some(Tok::Eq) => bin(BinOp::Eq),
            Some(Tok::Ne) => bin(BinOp::Ne),
            Some(Tok::Lt) => bin(BinOp::Lt),
            Some(Tok::Le) => bin(BinOp::Le),
            Some(Tok::Gt) => bin(BinOp::Gt),
            Some(Tok::Ge) => bin(BinOp::Ge),
            Some(Tok::Concat) => bin(BinOp::Concat),
            Some(Tok::Plus) => bin(BinOp::Add),
            Some(Tok::Minus) => bin(BinOp::Sub),
            Some(Tok::Star) => bin(BinOp::Mul),
            Some(Tok::Slash) => bin(BinOp::Div),
            Some(Tok::Percent) => bin(BinOp::Mod),
            _ => None,
        };
        if let Some(op) = op {
            self.next();
            let rhs = self.parse_expr(lbp + 1)?;
            return Ok(Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            });
        }
        // Keyword infix/postfix.
        let kw = self.peek_kw().unwrap_or_default();
        match kw.as_str() {
            "AND" => {
                self.next();
                let rhs = self.parse_expr(BP_AND + 1)?;
                Ok(Expr::Binary {
                    op: BinOp::And,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                })
            }
            "OR" => {
                self.next();
                let rhs = self.parse_expr(BP_OR + 1)?;
                Ok(Expr::Binary {
                    op: BinOp::Or,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                })
            }
            "IS" => {
                self.next();
                let negated = if self.peek_kw().as_deref() == Some("NOT") {
                    self.next();
                    true
                } else {
                    false
                };
                match self.peek_kw().as_deref() {
                    Some("NULL") => {
                        self.next();
                        Ok(Expr::IsNull {
                            expr: Box::new(lhs),
                            negated,
                        })
                    }
                    other => Err(MrError::Expr(format!(
                        "expected NULL after IS [NOT], found {other:?}"
                    ))),
                }
            }
            "NOT" | "BETWEEN" | "IN" => {
                let negated = if kw == "NOT" {
                    self.next();
                    true
                } else {
                    false
                };
                let which = self.peek_kw().unwrap_or_default();
                self.next(); // consume BETWEEN / IN
                match which.as_str() {
                    "BETWEEN" => {
                        let lo = self.parse_expr(BP_ADD)?;
                        match self.peek_kw().as_deref() {
                            Some("AND") => {
                                self.next();
                            }
                            other => {
                                return Err(MrError::Expr(format!(
                                    "expected AND in BETWEEN, found {other:?}"
                                )))
                            }
                        }
                        let hi = self.parse_expr(BP_ADD)?;
                        Ok(Expr::Between {
                            expr: Box::new(lhs),
                            lo: Box::new(lo),
                            hi: Box::new(hi),
                            negated,
                        })
                    }
                    "IN" => {
                        self.expect(&Tok::LParen)?;
                        let mut list = Vec::new();
                        if !matches!(self.peek(), Some(Tok::RParen)) {
                            loop {
                                list.push(self.parse_expr(0)?);
                                if matches!(self.peek(), Some(Tok::Comma)) {
                                    self.next();
                                } else {
                                    break;
                                }
                            }
                        }
                        self.expect(&Tok::RParen)?;
                        Ok(Expr::In {
                            expr: Box::new(lhs),
                            list,
                            negated,
                        })
                    }
                    other => Err(MrError::Expr(format!("unexpected operator '{other}'"))),
                }
            }
            other => Err(MrError::Expr(format!("unexpected operator '{other}'"))),
        }
    }

    fn parse_prefix(&mut self) -> Result<Expr> {
        let mut e = self.parse_primary()?;
        // Postfix `::T` cast.
        while matches!(self.peek(), Some(Tok::CastOp)) {
            self.next();
            let ty = self.parse_ty()?;
            e = Expr::Cast {
                expr: Box::new(e),
                ty,
            };
        }
        Ok(e)
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        match self.peek().cloned() {
            Some(Tok::Minus) => {
                self.next();
                let e = self.parse_expr(BP_UNARY)?;
                Ok(Expr::Neg(Box::new(e)))
            }
            Some(Tok::Plus) => {
                self.next();
                self.parse_expr(BP_UNARY)
            }
            Some(Tok::LParen) => {
                self.next();
                let e = self.parse_expr(0)?;
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            Some(Tok::Int(v)) => {
                self.next();
                Ok(Expr::Int(v))
            }
            Some(Tok::Float(v)) => {
                self.next();
                Ok(Expr::Double(v))
            }
            Some(Tok::Str(s)) => {
                self.next();
                Ok(Expr::Str(s))
            }
            Some(Tok::Ident(name)) => self.parse_ident(name),
            Some(Tok::QuotedIdent(name)) => self.parse_quoted_ident(name),
            other => Err(MrError::Expr(format!(
                "unexpected token {other:?} where an expression was expected"
            ))),
        }
    }

    fn parse_ident(&mut self, raw: String) -> Result<Expr> {
        let kw = raw.to_ascii_uppercase();
        match kw.as_str() {
            "TRUE" => {
                self.next();
                Ok(Expr::Bool(true))
            }
            "FALSE" => {
                self.next();
                Ok(Expr::Bool(false))
            }
            "NULL" => {
                self.next();
                Ok(Expr::Null)
            }
            "NOT" => {
                self.next();
                let e = self.parse_expr(BP_UNARY)?;
                Ok(Expr::Not(Box::new(e)))
            }
            "INTERVAL" => {
                self.next();
                self.parse_interval()
            }
            "CAST" => {
                self.next();
                self.expect(&Tok::LParen)?;
                let e = self.parse_expr(0)?;
                match self.peek_kw().as_deref() {
                    Some("AS") => {
                        self.next();
                    }
                    other => {
                        return Err(MrError::Expr(format!(
                            "expected AS in CAST, found {other:?}"
                        )))
                    }
                }
                let ty = self.parse_ty()?;
                self.expect(&Tok::RParen)?;
                Ok(Expr::Cast {
                    expr: Box::new(e),
                    ty,
                })
            }
            "RUNNING" | "FINAL" => {
                self.next();
                let final_ = kw == "FINAL";
                let inner = self.parse_expr(BP_UNARY)?;
                Ok(Expr::RunningFinal {
                    final_,
                    inner: Box::new(inner),
                })
            }
            // `LAG`/`LEAD` are how Snowflake spells physical navigation inside
            // MATCH_RECOGNIZE; SQL:2016 (and Trino, and Oracle) call it PREV/NEXT.
            // Accepting both means a query can be moved between them unedited.
            "PREV" | "LAG" | "NEXT" | "LEAD" | "FIRST" | "LAST" => {
                self.next();
                let nav = match kw.as_str() {
                    "PREV" | "LAG" => NavKind::Prev,
                    "NEXT" | "LEAD" => NavKind::Next,
                    "FIRST" => NavKind::First,
                    _ => NavKind::Last,
                };
                self.parse_nav(nav)
            }
            "SUM" | "COUNT" | "AVG" | "MIN" | "MAX" | "ARRAY_AGG" | "LIST" | "ARBITRARY"
            | "ANY_VALUE" => {
                self.next();
                let agg = match kw.as_str() {
                    "SUM" => AggKind::Sum,
                    "COUNT" => AggKind::Count,
                    "AVG" => AggKind::Avg,
                    "MIN" => AggKind::Min,
                    "MAX" => AggKind::Max,
                    // DuckDB spells array_agg `list`; Trino spells any_value
                    // `arbitrary`. Accept both spellings of each.
                    "ARRAY_AGG" | "LIST" => AggKind::ArrayAgg,
                    _ => AggKind::Arbitrary,
                };
                self.parse_agg(agg)
            }
            "CLASSIFIER" => {
                self.next();
                self.expect(&Tok::LParen)?;
                // `CLASSIFIER(label)` restricts the reference to rows bound to a
                // pattern variable or SUBSET; bare `CLASSIFIER()` is unrestricted.
                let label = match self.peek().cloned() {
                    Some(Tok::Ident(v)) => {
                        self.next();
                        Some(v.to_ascii_uppercase())
                    }
                    Some(Tok::QuotedIdent(v)) => {
                        self.next();
                        Some(v)
                    }
                    _ => None,
                };
                self.expect(&Tok::RParen)?;
                Ok(Expr::Classifier(label))
            }
            "MATCH_NUMBER" => {
                self.next();
                self.expect(&Tok::LParen)?;
                self.expect(&Tok::RParen)?;
                Ok(Expr::MatchNumber)
            }
            // Snowflake's `MATCH_SEQUENCE_NUMBER()`: the 1-based position of this row
            // within its match. That is exactly a RUNNING `COUNT(*)`, so it is sugar
            // rather than a new capability — but it is what a Snowflake query says.
            "MATCH_SEQUENCE_NUMBER" => {
                self.next();
                self.expect(&Tok::LParen)?;
                self.expect(&Tok::RParen)?;
                Ok(Expr::RunningFinal {
                    final_: false,
                    inner: Box::new(Expr::Agg {
                        kind: AggKind::Count,
                        arg: AggArg::Star,
                    }),
                })
            }
            _ if matches!(self.peek_at(1), Some(Tok::LParen)) => {
                // Not a keyword, but applied to an argument list: a scalar
                // function call such as `abs(x)` or `coalesce(a, b)`.
                self.next();
                self.expect(&Tok::LParen)?;
                let mut args = Vec::new();
                if !matches!(self.peek(), Some(Tok::RParen)) {
                    loop {
                        args.push(self.parse_expr(0)?);
                        if matches!(self.peek(), Some(Tok::Comma)) {
                            self.next();
                            continue;
                        }
                        break;
                    }
                }
                self.expect(&Tok::RParen)?;
                Ok(Expr::Call {
                    name: raw.to_ascii_lowercase(),
                    args,
                })
            }
            _ => {
                // A column reference, possibly qualified `VAR.col`.
                self.next();
                if matches!(self.peek(), Some(Tok::Dot)) {
                    self.next();
                    match self.next() {
                        Some(Tok::Ident(col)) => Ok(Expr::Qualified(raw.to_ascii_uppercase(), col)),
                        Some(Tok::QuotedIdent(col)) => {
                            Ok(Expr::Qualified(raw.to_ascii_uppercase(), col))
                        }
                        Some(Tok::Star) => Err(MrError::Expr(
                            "`A.*` is only valid as the argument to COUNT(...)".into(),
                        )),
                        other => Err(MrError::Expr(format!(
                            "expected a column name after '{raw}.', found {other:?}"
                        ))),
                    }
                } else {
                    Ok(Expr::Col(raw))
                }
            }
        }
    }

    /// A double-quoted identifier: never a keyword, and case-sensitive. It names
    /// either a column (`"My Col"`) or a pattern variable (`"b".value`); the
    /// variable name keeps its exact spelling, unlike the unquoted form.
    fn parse_quoted_ident(&mut self, raw: String) -> Result<Expr> {
        self.next();
        if matches!(self.peek(), Some(Tok::Dot)) {
            self.next();
            match self.next() {
                Some(Tok::Ident(col)) | Some(Tok::QuotedIdent(col)) => {
                    Ok(Expr::Qualified(raw, col))
                }
                Some(Tok::Star) => Err(MrError::Expr(
                    "`A.*` is only valid as the argument to COUNT(...)".into(),
                )),
                other => Err(MrError::Expr(format!(
                    "expected a column name after '\"{raw}\".', found {other:?}"
                ))),
            }
        } else {
            Ok(Expr::Col(raw))
        }
    }

    fn parse_nav(&mut self, kind: NavKind) -> Result<Expr> {
        self.expect(&Tok::LParen)?;
        let arg = self.parse_expr(0)?;
        let offset = if matches!(self.peek(), Some(Tok::Comma)) {
            self.next();
            match self.next() {
                Some(Tok::Int(n)) if n >= 0 => n as usize,
                other => {
                    return Err(MrError::Expr(format!(
                        "navigation offset must be a non-negative integer, found {other:?}"
                    )))
                }
            }
        } else {
            // PREV/NEXT default to one physical row; FIRST/LAST default to the
            // first/last row itself (offset 0).
            match kind {
                NavKind::Prev | NavKind::Next => 1,
                NavKind::First | NavKind::Last => 0,
            }
        };
        self.expect(&Tok::RParen)?;
        Ok(Expr::Nav {
            kind,
            arg: Box::new(arg),
            offset,
        })
    }

    fn parse_agg(&mut self, kind: AggKind) -> Result<Expr> {
        self.expect(&Tok::LParen)?;
        // COUNT(*) / COUNT(A.*) special forms.
        let arg = if matches!(self.peek(), Some(Tok::RParen)) {
            // `count()` is the same as `count(*)`, as SQL:2016 and Trino allow.
            if kind != AggKind::Count {
                return Err(MrError::Expr(format!(
                    "{kind:?}() needs an argument; only COUNT() may be empty"
                )));
            }
            AggArg::Star
        } else if matches!(self.peek(), Some(Tok::Star)) {
            self.next();
            if kind != AggKind::Count {
                return Err(MrError::Expr(format!(
                    "{kind:?}(*) is not valid; only COUNT(*) is allowed"
                )));
            }
            AggArg::Star
        } else if matches!(self.peek(), Some(Tok::Ident(_)) | Some(Tok::QuotedIdent(_)))
            && matches!(self.peek_at(1), Some(Tok::Dot))
            && matches!(self.peek_at(2), Some(Tok::Star))
        {
            let var = match self.next() {
                Some(Tok::Ident(v)) => v.to_ascii_uppercase(),
                // A quoted label keeps its exact spelling.
                Some(Tok::QuotedIdent(v)) => v,
                _ => unreachable!(),
            };
            self.next(); // dot
            self.next(); // star
            if kind != AggKind::Count {
                return Err(MrError::Expr(format!(
                    "{kind:?}({var}.*) is not valid; only COUNT(A.*) is allowed"
                )));
            }
            AggArg::QualStar(var)
        } else {
            AggArg::Expr(Box::new(self.parse_expr(0)?))
        };
        self.expect(&Tok::RParen)?;
        Ok(Expr::Agg { kind, arg })
    }

    fn parse_interval(&mut self) -> Result<Expr> {
        let n = match self.next() {
            Some(Tok::Int(n)) => n,
            Some(Tok::Str(s)) => s
                .trim()
                .parse::<i64>()
                .map_err(|_| MrError::Expr(format!("invalid INTERVAL count '{s}'")))?,
            other => {
                return Err(MrError::Expr(format!(
                    "expected a count after INTERVAL, found {other:?}"
                )))
            }
        };
        let unit = match self.next() {
            Some(Tok::Ident(u)) => u.to_ascii_uppercase(),
            other => {
                return Err(MrError::Expr(format!(
                    "expected an INTERVAL unit, found {other:?}"
                )))
            }
        };
        let (months, days, nanos): (i32, i32, i64) = match unit.trim_end_matches('S') {
            "YEAR" => ((n * 12) as i32, 0, 0),
            "MONTH" => (n as i32, 0, 0),
            "WEEK" => (0, (n * 7) as i32, 0),
            "DAY" => (0, n as i32, 0),
            "HOUR" => (0, 0, n * 3_600_000_000_000),
            "MINUTE" => (0, 0, n * 60_000_000_000),
            "SECOND" => (0, 0, n * 1_000_000_000),
            "MILLISECOND" => (0, 0, n * 1_000_000),
            "MICROSECOND" => (0, 0, n * 1_000),
            other => {
                return Err(MrError::Expr(format!(
                    "unsupported INTERVAL unit '{other}'"
                )))
            }
        };
        Ok(Expr::Interval {
            months,
            days,
            nanos,
        })
    }

    /// Parse a SQL type name into a [`Ty`].
    fn parse_ty(&mut self) -> Result<Ty> {
        let name = match self.next() {
            Some(Tok::Ident(s)) => s.to_ascii_uppercase(),
            other => {
                return Err(MrError::Expr(format!(
                    "expected a type name, found {other:?}"
                )))
            }
        };
        match name.as_str() {
            "BIGINT" | "INT" | "INTEGER" | "INT8" | "INT4" | "INT2" | "SMALLINT" | "TINYINT" => {
                Ok(Ty::Int64)
            }
            "HUGEINT" => Ok(Ty::HugeInt),
            "DOUBLE" | "FLOAT" | "FLOAT8" | "FLOAT4" | "REAL" => Ok(Ty::Double),
            "DECIMAL" | "NUMERIC" => {
                if matches!(self.peek(), Some(Tok::LParen)) {
                    self.next();
                    let p = self.expect_int()? as u8;
                    let s = if matches!(self.peek(), Some(Tok::Comma)) {
                        self.next();
                        self.expect_int()? as i8
                    } else {
                        0
                    };
                    self.expect(&Tok::RParen)?;
                    Ok(Ty::Decimal(p, s))
                } else {
                    Ok(Ty::Decimal(18, 3))
                }
            }
            "VARCHAR" | "TEXT" | "STRING" | "CHAR" => {
                // `VARCHAR(n)` / `CHAR(n)`: accept and ignore the length, as DuckDB
                // does — the declared width is not part of our type model.
                if matches!(self.peek(), Some(Tok::LParen)) {
                    self.next();
                    self.expect_int()?;
                    self.expect(&Tok::RParen)?;
                }
                Ok(Ty::Varchar)
            }
            "BOOLEAN" | "BOOL" => Ok(Ty::Boolean),
            "DATE" => Ok(Ty::Date),
            "TIMESTAMP" | "DATETIME" => Ok(Ty::Timestamp(TimeUnit::Micro)),
            "TIME" => Ok(Ty::Time(TimeUnit::Micro)),
            "INTERVAL" => Ok(Ty::Interval),
            other => Err(MrError::Expr(format!("unknown type name '{other}'"))),
        }
    }

    fn expect_int(&mut self) -> Result<i64> {
        match self.next() {
            Some(Tok::Int(n)) => Ok(n),
            other => Err(MrError::Expr(format!(
                "expected an integer, found {other:?}"
            ))),
        }
    }
}

/// Parse a DEFINE / MEASURES expression string into an [`Expr`] AST.
pub fn parse(src: &str) -> Result<Expr> {
    let toks = lex(src)?;
    if toks.is_empty() {
        return Err(MrError::Expr("empty expression".into()));
    }
    let mut p = Parser {
        toks,
        pos: 0,
        depth: 0,
    };
    let e = p.parse_expr(0)?;
    if p.pos != p.toks.len() {
        return Err(MrError::Expr(format!(
            "unexpected trailing tokens starting at {:?}",
            p.toks[p.pos]
        )));
    }
    Ok(e)
}

/// Parse a standalone SQL type name (used for the `type` override on measures).
pub fn parse_type_name(src: &str) -> Result<Ty> {
    let toks = lex(src)?;
    let mut p = Parser {
        toks,
        pos: 0,
        depth: 0,
    };
    let ty = p.parse_ty()?;
    if p.pos != p.toks.len() {
        return Err(MrError::Expr(format!("trailing tokens in type '{src}'")));
    }
    Ok(ty)
}
