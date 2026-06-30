//! The end-to-end plan: parse + type-check the whole `match_recognize` call at
//! bind time (producing the output column layout), then run the matcher over a
//! [`RowStore`] at produce time (producing output rows).
//!
//! This is the single entry point `mr-worker` drives: [`Plan::build`] in
//! `on_bind` and [`Plan::run`] in `produce`.

use std::collections::HashMap;

use crate::engine::matcher::{AfterSkip, Matcher};
use crate::engine::{Frame, RowStore};
use crate::error::{MrError, Result};
use crate::expr::ast::Expr;
use crate::expr::parser::{parse as parse_expr, parse_type_name};
use crate::pattern::compile::{compile, Program};
use crate::pattern::parser::{parse as parse_pattern, Pattern};
use crate::types::{infer, BindSchema, Ty};
use crate::value::Value;

/// Raw, string-form arguments to `match_recognize` (as they arrive from SQL).
#[derive(Debug, Clone)]
pub struct PlanConfig {
    pub pattern: String,
    pub define_json: String,
    pub measures_json: Option<String>,
    pub partition_by: Vec<String>,
    /// Order-by specs, each `"col"`, `"col DESC"`, `"col NULLS FIRST"`, etc.
    pub order_by: Vec<String>,
    /// `true` = ALL ROWS PER MATCH, `false` = ONE ROW PER MATCH.
    pub rows_all: bool,
    /// Raw AFTER MATCH SKIP string (`"past last row"`, `"to first A"`, …).
    pub after: String,
    pub step_budget: i64,
}

/// One sort key.
#[derive(Debug, Clone)]
struct OrderKey {
    col: String,
    desc: bool,
    nulls_first: bool,
}

/// A measure: output name, expression, and the type the column will carry.
#[derive(Debug, Clone)]
pub struct Measure {
    pub name: String,
    pub expr: Expr,
    pub ty: Ty,
}

/// One output column.
#[derive(Debug, Clone)]
pub struct OutputColumn {
    pub name: String,
    pub ty: Ty,
}

/// A fully bound, executable plan.
pub struct Plan {
    program: Program,
    define: HashMap<String, Expr>,
    measures: Vec<Measure>,
    partition_by: Vec<String>,
    order_by: Vec<OrderKey>,
    rows_all: bool,
    after: AfterSkip,
    step_budget: i64,
    /// Auto `match_number` column emitted (ALL ROWS, not shadowed by a measure).
    auto_match_number: bool,
    /// Auto `classifier` column emitted (ALL ROWS, not shadowed by a measure).
    auto_classifier: bool,
    output_columns: Vec<OutputColumn>,
}

impl Plan {
    /// Bind-time: parse, type-check, and compute the output column layout.
    pub fn build(cfg: &PlanConfig, schema: &dyn BindSchema) -> Result<Plan> {
        let pattern = parse_pattern(&cfg.pattern)?;
        let program = compile(&pattern)?;
        let vars = pattern.variables();

        // DEFINE.
        let define = parse_define(&cfg.define_json, &vars)?;

        // MEASURES.
        let measures = parse_measures(cfg.measures_json.as_deref(), schema)?;

        // ORDER BY (required).
        if cfg.order_by.is_empty() {
            return Err(MrError::Bind(
                "order_by is required (at least one column)".into(),
            ));
        }
        let order_by = cfg
            .order_by
            .iter()
            .map(|s| parse_order_key(s, schema))
            .collect::<Result<Vec<_>>>()?;

        // PARTITION BY columns must exist.
        for c in &cfg.partition_by {
            if schema.col_ty(c).is_none() {
                return Err(MrError::Bind(format!(
                    "partition_by column '{c}' not found"
                )));
            }
        }

        // AFTER MATCH SKIP.
        let after = parse_after(&cfg.after, &vars)?;

        if cfg.step_budget <= 0 {
            return Err(MrError::Bind("step_budget must be positive".into()));
        }

        // Output column layout.
        let mut output_columns = Vec::new();
        for c in &cfg.partition_by {
            output_columns.push(OutputColumn {
                name: c.clone(),
                ty: schema.col_ty(c).unwrap(),
            });
        }
        let mut auto_match_number = false;
        let mut auto_classifier = false;
        if cfg.rows_all {
            for k in &order_by {
                output_columns.push(OutputColumn {
                    name: k.col.clone(),
                    ty: schema.col_ty(&k.col).unwrap(),
                });
            }
            let has_measure = |n: &str| measures.iter().any(|m| m.name.eq_ignore_ascii_case(n));
            if !has_measure("match_number") {
                auto_match_number = true;
                output_columns.push(OutputColumn {
                    name: "match_number".into(),
                    ty: Ty::Int64,
                });
            }
            if !has_measure("classifier") {
                auto_classifier = true;
                output_columns.push(OutputColumn {
                    name: "classifier".into(),
                    ty: Ty::Varchar,
                });
            }
        }
        for m in &measures {
            output_columns.push(OutputColumn {
                name: m.name.clone(),
                ty: m.ty,
            });
        }

        Ok(Plan {
            program,
            define,
            measures,
            partition_by: cfg.partition_by.clone(),
            order_by,
            rows_all: cfg.rows_all,
            after,
            step_budget: cfg.step_budget,
            auto_match_number,
            auto_classifier,
            output_columns,
        })
    }

    /// The output schema (column name + type), fixed at bind time.
    pub fn output_columns(&self) -> &[OutputColumn] {
        &self.output_columns
    }

    /// The measures (for diagnostics / tests).
    pub fn measures(&self) -> &[Measure] {
        &self.measures
    }

    /// Produce-time: group into partitions, sort each, match, and emit output
    /// rows in `output_columns` order.
    pub fn run(&self, store: &dyn RowStore) -> Result<Vec<Vec<Value>>> {
        let mut out = Vec::new();
        let mut match_number = 1i64;
        let partitions = self.partitions(store)?;
        for (label, mut tape) in partitions {
            self.sort_tape(store, &mut tape)?;
            let mut matcher = Matcher::new(
                &self.program,
                store,
                &tape,
                &self.define,
                &self.after,
                self.step_budget,
                label,
                match_number,
            );
            let matches = matcher.find_all()?;
            match_number = matcher.next_match_number();
            for m in &matches {
                // Empty matches (zero bound rows) emit no output, per the spec's
                // "omit empty matches" behavior — for both ONE ROW and ALL ROWS.
                if m.binds.is_empty() {
                    continue;
                }
                if self.rows_all {
                    for k in 0..m.binds.len() {
                        out.push(self.emit_all_row(store, &tape, m, k)?);
                    }
                } else {
                    out.push(self.emit_one_row(store, &tape, m)?);
                }
            }
        }
        Ok(out)
    }

    fn emit_one_row(
        &self,
        store: &dyn RowStore,
        tape: &[usize],
        m: &crate::engine::Match,
    ) -> Result<Vec<Value>> {
        let frame = Frame {
            store,
            tape,
            binds: &m.binds,
            horizon: m.binds.len(),
            match_number: m.match_number,
        };
        let mut row = Vec::with_capacity(self.output_columns.len());
        let anchor_tape = m.binds.first().map(|b| b.tape_pos).unwrap_or(m.start);
        for c in &self.partition_by {
            row.push(self.col_at(store, tape, anchor_tape, c)?);
        }
        for meas in &self.measures {
            let v = frame.eval_measure(&meas.expr, true)?;
            row.push(crate::engine::valops::coerce(v, meas.ty)?);
        }
        Ok(row)
    }

    fn emit_all_row(
        &self,
        store: &dyn RowStore,
        tape: &[usize],
        m: &crate::engine::Match,
        k: usize,
    ) -> Result<Vec<Value>> {
        let frame = Frame {
            store,
            tape,
            binds: &m.binds,
            horizon: k + 1,
            match_number: m.match_number,
        };
        let row_tape = m.binds[k].tape_pos;
        let mut row = Vec::with_capacity(self.output_columns.len());
        for c in &self.partition_by {
            row.push(self.col_at(store, tape, row_tape, c)?);
        }
        for key in &self.order_by {
            row.push(self.col_at(store, tape, row_tape, &key.col)?);
        }
        if self.auto_match_number {
            row.push(Value::Int(m.match_number));
        }
        if self.auto_classifier {
            row.push(Value::Str(m.binds[k].var.clone()));
        }
        for meas in &self.measures {
            let v = frame.eval_measure(&meas.expr, false)?;
            row.push(crate::engine::valops::coerce(v, meas.ty)?);
        }
        Ok(row)
    }

    fn col_at(&self, store: &dyn RowStore, tape: &[usize], tp: usize, name: &str) -> Result<Value> {
        let idx = store
            .col_index(name)
            .ok_or_else(|| MrError::Eval(format!("unknown column '{name}'")))?;
        Ok(store.cell(tape[tp], idx))
    }

    /// Group rows into partitions (first-seen order preserved).
    fn partitions(&self, store: &dyn RowStore) -> Result<Vec<(String, Vec<usize>)>> {
        let n = store.num_rows();
        if self.partition_by.is_empty() {
            return Ok(vec![("(all)".to_string(), (0..n).collect())]);
        }
        let idxs: Vec<usize> = self
            .partition_by
            .iter()
            .map(|c| {
                store
                    .col_index(c)
                    .ok_or_else(|| MrError::Eval(format!("unknown partition column '{c}'")))
            })
            .collect::<Result<_>>()?;
        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
        for r in 0..n {
            let key = idxs
                .iter()
                .map(|&ci| key_part(&store.cell(r, ci)))
                .collect::<Vec<_>>()
                .join("\u{1}");
            groups.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                Vec::new()
            });
            groups.get_mut(&key).unwrap().push(r);
        }
        Ok(order
            .into_iter()
            .map(|k| {
                let rows = groups.remove(&k).unwrap();
                (k, rows)
            })
            .collect())
    }

    /// Stable-sort a partition's row indices by the order-by keys.
    fn sort_tape(&self, store: &dyn RowStore, tape: &mut [usize]) -> Result<()> {
        let keys: Vec<(usize, bool, bool)> = self
            .order_by
            .iter()
            .map(|k| {
                store
                    .col_index(&k.col)
                    .ok_or_else(|| MrError::Eval(format!("unknown order column '{}'", k.col)))
                    .map(|ci| (ci, k.desc, k.nulls_first))
            })
            .collect::<Result<_>>()?;
        // Stable sort by successive keys.
        tape.sort_by(|&a, &b| {
            for &(ci, desc, nulls_first) in &keys {
                let va = store.cell(a, ci);
                let vb = store.cell(b, ci);
                let ord = cmp_with_nulls(&va, &vb, nulls_first);
                let ord = if desc { ord.reverse() } else { ord };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
        Ok(())
    }
}

fn cmp_with_nulls(a: &Value, b: &Value, nulls_first: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a.is_null(), b.is_null()) {
        (true, true) => Equal,
        (true, false) => {
            if nulls_first {
                Less
            } else {
                Greater
            }
        }
        (false, true) => {
            if nulls_first {
                Greater
            } else {
                Less
            }
        }
        (false, false) => crate::engine::valops::compare(a, b).unwrap_or(Equal),
    }
}

/// A stable key fragment for a partition value.
fn key_part(v: &Value) -> String {
    match v {
        Value::Null => "\u{0}NULL".to_string(),
        other => crate::engine::valops::to_string(other),
    }
}

fn parse_order_key(spec: &str, schema: &dyn BindSchema) -> Result<OrderKey> {
    let s = spec.trim();
    let upper = s.to_ascii_uppercase();
    let mut desc = false;
    let mut nulls_first = None;
    let mut col_part = s;
    // Strip a trailing NULLS FIRST / NULLS LAST.
    if let Some(pos) = upper.find("NULLS FIRST") {
        nulls_first = Some(true);
        col_part = &s[..pos];
    } else if let Some(pos) = upper.find("NULLS LAST") {
        nulls_first = Some(false);
        col_part = &s[..pos];
    }
    let cu = col_part.trim().to_ascii_uppercase();
    let col = if let Some(rest) = cu.strip_suffix(" DESC") {
        desc = true;
        col_part.trim()[..rest.len()].trim().to_string()
    } else if let Some(rest) = cu.strip_suffix(" ASC") {
        col_part.trim()[..rest.len()].trim().to_string()
    } else {
        col_part.trim().to_string()
    };
    if schema.col_ty(&col).is_none() {
        return Err(MrError::Bind(format!("order_by column '{col}' not found")));
    }
    // Default NULLS ordering follows DuckDB: NULLS LAST for ASC, FIRST for DESC.
    let nulls_first = nulls_first.unwrap_or(desc);
    Ok(OrderKey {
        col,
        desc,
        nulls_first,
    })
}

fn parse_after(s: &str, vars: &[String]) -> Result<AfterSkip> {
    let t = s.trim().to_ascii_lowercase();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t == "past last row" {
        return Ok(AfterSkip::PastLastRow);
    }
    if t == "to next row" {
        return Ok(AfterSkip::ToNextRow);
    }
    let resolve = |var: &str| -> Result<String> {
        let v = var.to_ascii_uppercase();
        if vars.iter().any(|x| x.eq_ignore_ascii_case(&v)) {
            Ok(v)
        } else {
            Err(MrError::Bind(format!(
                "after: '{var}' is not a pattern variable"
            )))
        }
    };
    if let Some(rest) = t.strip_prefix("to first ") {
        return Ok(AfterSkip::ToFirstVar(resolve(rest.trim())?));
    }
    if let Some(rest) = t.strip_prefix("to last ") {
        return Ok(AfterSkip::ToLastVar(resolve(rest.trim())?));
    }
    Err(MrError::Bind(format!(
        "unknown AFTER MATCH SKIP mode '{s}' (expected 'past last row', 'to next row', \
         'to first <VAR>', or 'to last <VAR>')"
    )))
}

fn parse_define(json: &str, vars: &[String]) -> Result<HashMap<String, Expr>> {
    let trimmed = json.trim();
    let map: serde_json::Map<String, serde_json::Value> = if trimmed.is_empty() {
        serde_json::Map::new()
    } else {
        serde_json::from_str(trimmed)
            .map_err(|e| MrError::Bind(format!("define is not a JSON object: {e}")))?
    };
    let mut out = HashMap::new();
    for (k, v) in map {
        let pred = v
            .as_str()
            .ok_or_else(|| MrError::Bind(format!("define['{k}'] must be a string predicate")))?;
        let var = k.to_ascii_uppercase();
        let expr = parse_expr(pred)?;
        out.insert(var, expr);
    }
    // Warn-style validation: every DEFINE var should be in the pattern. We make
    // it an error to catch typos early.
    for k in out.keys() {
        if !vars.iter().any(|x| x.eq_ignore_ascii_case(k)) {
            return Err(MrError::Bind(format!(
                "define variable '{k}' does not appear in the pattern"
            )));
        }
    }
    Ok(out)
}

fn parse_measures(json: Option<&str>, schema: &dyn BindSchema) -> Result<Vec<Measure>> {
    let json = match json {
        Some(s) if !s.trim().is_empty() => s,
        _ => return Ok(Vec::new()),
    };
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| MrError::Bind(format!("measures is not valid JSON: {e}")))?;
    let mut measures = Vec::new();
    match value {
        // Array form: [{ "as"/"name", "expr", "type"? }, ...]
        serde_json::Value::Array(items) => {
            for (i, item) in items.into_iter().enumerate() {
                let obj = item
                    .as_object()
                    .ok_or_else(|| MrError::Bind(format!("measures[{i}] must be an object")))?;
                let name = obj
                    .get("as")
                    .or_else(|| obj.get("name"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| MrError::Bind(format!("measures[{i}] needs an 'as'/'name'")))?
                    .to_string();
                let expr_s = obj
                    .get("expr")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| MrError::Bind(format!("measures[{i}] needs an 'expr'")))?;
                let expr = parse_expr(expr_s)?;
                let ty = match obj.get("type").and_then(|v| v.as_str()) {
                    Some(t) => parse_type_name(t)?,
                    None => resolve_measure_ty(&expr, schema, &name)?,
                };
                measures.push(Measure { name, expr, ty });
            }
        }
        // Object form: { "<name>": "<expr>", ... } — inference, no override.
        serde_json::Value::Object(map) => {
            for (name, v) in map {
                let expr_s = v.as_str().ok_or_else(|| {
                    MrError::Bind(format!("measures['{name}'] must be a string expression"))
                })?;
                let expr = parse_expr(expr_s)?;
                let ty = resolve_measure_ty(&expr, schema, &name)?;
                measures.push(Measure { name, expr, ty });
            }
        }
        _ => {
            return Err(MrError::Bind(
                "measures must be a JSON object or array".into(),
            ))
        }
    }
    Ok(measures)
}

fn resolve_measure_ty(expr: &Expr, schema: &dyn BindSchema, name: &str) -> Result<Ty> {
    let ty = infer(expr, schema)?;
    if ty == Ty::Null {
        return Err(MrError::Infer(format!(
            "could not infer a type for measure '{name}' (it resolves to NULL); supply an \
             explicit type via the array form, e.g. {{\"as\":\"{name}\",\"expr\":\"…\",\
             \"type\":\"DOUBLE\"}}"
        )));
    }
    Ok(ty)
}

impl Pattern {
    /// Render this pattern for `explain_pattern` (re-exported convenience).
    pub fn explain(&self) -> String {
        crate::pattern::compile::explain(self)
    }
}
