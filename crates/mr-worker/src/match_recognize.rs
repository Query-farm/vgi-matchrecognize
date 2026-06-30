//! `mr.match_recognize((<relation>), partition_by:=, order_by:=, pattern:=,
//! define:=, measures:=, rows:=, after:=, [step_budget:=])` — SQL:2016 row
//! pattern matching over a buffered relation.
//!
//! A `TableBufferingFunction` (Sink + Source): every input batch is buffered
//! into cross-process storage (`process`); once the whole relation is present,
//! `finalize_producer` concatenates it, builds the mr-core [`Plan`], runs the
//! matcher per partition, and emits the result rows on the bind-time output
//! schema. Pattern matching is intrinsically a whole-partition operation, so
//! buffer-all-then-compute is required (the `vgi-match` idiom).

use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::{Array, RecordBatch};
use arrow_schema::{Schema, SchemaRef};
use arrow_select::concat::concat_batches;
use mr_core::plan::{Plan, PlanConfig};
use vgi::arguments::Arguments;
use vgi::buffering::{BufferingParams, TableBufferingFunction};
use vgi::function::{ArgSpec, BindParams, BindResponse, FunctionMetadata};
use vgi::ipc;
use vgi::table_function::TableProducer;
use vgi_rpc::{OutputCollector, Result, RpcError};

use crate::arrow_in::BatchRowStore;
use crate::arrow_out::build_batch;
use crate::schema::{output_field, ArrowBindSchema};

const NS: &[u8] = b"match_recognize";
const DEFAULT_STEP_BUDGET: i64 = 5_000_000;

pub struct MatchRecognize;

fn ve(e: impl std::fmt::Display) -> RpcError {
    RpcError::value_error(e.to_string())
}

/// Read a named VARCHAR[] argument into a `Vec<String>` (empty if absent/NULL).
fn named_str_list(args: &Arguments, name: &str) -> Result<Vec<String>> {
    let Some(arr) = args.named(name) else {
        return Ok(Vec::new());
    };
    let list = arr
        .as_any()
        .downcast_ref::<arrow_array::ListArray>()
        .ok_or_else(|| ve(format!("argument '{name}' must be a VARCHAR[] list")))?;
    if list.is_empty() || list.is_null(0) {
        return Ok(Vec::new());
    }
    let inner = list.value(0);
    let mut out = Vec::new();
    if let Some(s) = inner.as_string_opt::<i32>() {
        for i in 0..s.len() {
            if s.is_null(i) {
                return Err(ve(format!("argument '{name}' must not contain NULL")));
            }
            out.push(s.value(i).to_string());
        }
    } else if let Some(s) = inner.as_string_opt::<i64>() {
        for i in 0..s.len() {
            out.push(s.value(i).to_string());
        }
    } else {
        return Err(ve(format!("argument '{name}' must be a VARCHAR[] list")));
    }
    Ok(out)
}

/// Assemble the [`PlanConfig`] from the call arguments.
fn plan_config(args: &Arguments) -> Result<PlanConfig> {
    let pattern = args
        .named_str("pattern")
        .ok_or_else(|| ve("match_recognize: 'pattern' is required"))?;
    let define_json = args.named_str("define").unwrap_or_default();
    let measures_json = args.named_str("measures");
    let partition_by = named_str_list(args, "partition_by")?;
    let order_by = named_str_list(args, "order_by")?;
    let rows = args
        .named_str("rows")
        .unwrap_or_else(|| "one".to_string())
        .to_ascii_lowercase();
    let rows_all = match rows.as_str() {
        "all" => true,
        "one" => false,
        other => return Err(ve(format!("rows must be 'one' or 'all', got '{other}'"))),
    };
    let after = args
        .named_str("after")
        .unwrap_or_else(|| "past last row".to_string());
    let step_budget = args.named_i64("step_budget").unwrap_or(DEFAULT_STEP_BUDGET);
    Ok(PlanConfig {
        pattern,
        define_json,
        measures_json,
        partition_by,
        order_by,
        rows_all,
        after,
        step_budget,
    })
}

/// Build the bound mr-core [`Plan`] from arguments + the input schema.
fn build_plan(args: &Arguments, input_schema: &SchemaRef) -> Result<Plan> {
    let cfg = plan_config(args)?;
    // Parse the pattern once to get the variable set for inference.
    let pat = mr_core::pattern::parse(&cfg.pattern).map_err(ve)?;
    let bind_schema = ArrowBindSchema::new(input_schema.clone(), pat.variables());
    Plan::build(&cfg, &bind_schema).map_err(ve)
}

/// The output Arrow schema derived from a bound plan.
fn output_schema(plan: &Plan) -> SchemaRef {
    let fields = plan
        .output_columns()
        .iter()
        .map(|c| output_field(&c.name, c.ty))
        .collect::<Vec<_>>();
    Arc::new(Schema::new(fields))
}

impl TableBufferingFunction for MatchRecognize {
    fn name(&self) -> &str {
        "match_recognize"
    }

    fn metadata(&self) -> FunctionMetadata {
        crate::catalog::match_recognize_metadata()
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        use arrow_schema::{DataType, Field};
        let varchar_list = DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)));
        vec![
            ArgSpec::column(
                "data",
                0,
                "table",
                "The input relation to match over, supplied as a subquery (e.g. `(SELECT symbol, \
                 ts, price FROM ticks)`). The whole relation is buffered, partitioned, sorted, and \
                 scanned for pattern matches.",
            ),
            ArgSpec::const_typed(
                "partition_by",
                -1,
                varchar_list.clone(),
                "Column names to partition by (like SQL `PARTITION BY`), as a VARCHAR list. Each \
                 partition is matched independently. Omit for a single global partition.",
            ),
            ArgSpec::const_typed(
                "order_by",
                -1,
                varchar_list,
                "Column names defining the intra-partition row order (like SQL `ORDER BY`), as a \
                 VARCHAR list. Required. An element may carry a ' DESC' and/or ' NULLS FIRST/LAST' \
                 suffix, e.g. 'ts DESC'.",
            ),
            ArgSpec::const_arg(
                "pattern",
                -1,
                "varchar",
                "The row pattern: a regular expression over pattern variables, e.g. 'START DOWN+ \
                 UP+'. Supports concatenation, alternation '|', quantifiers '* + ? {n} {n,} {n,m}' \
                 (greedy, or reluctant with a trailing '?'), grouping '()', and partition-edge \
                 anchors '^' and '$'.",
            ),
            ArgSpec::const_arg(
                "define",
                -1,
                "varchar",
                "A JSON object mapping each pattern variable to a boolean predicate over the \
                 current row, e.g. '{\"DOWN\":\"price < PREV(price)\"}'. Variables not listed \
                 default to always-true. Predicates may use column refs, PREV/NEXT/FIRST/LAST, \
                 running aggregates, and arithmetic/comparison/logical operators.",
            ),
            ArgSpec::const_arg(
                "measures",
                -1,
                "varchar",
                "A JSON object (or array, for explicit type overrides) defining the output \
                 measure columns, e.g. '{\"n\":\"COUNT(*)\",\"first_ts\":\"FIRST(A.ts)\"}'. The \
                 array form '[{\"as\":\"r\",\"expr\":\"…\",\"type\":\"DOUBLE\"}]' overrides the \
                 inferred output type. Measure types are inferred from the input schema at bind \
                 time.",
            ),
            ArgSpec::const_arg(
                "rows",
                -1,
                "varchar",
                "Output cardinality: 'one' for ONE ROW PER MATCH (one summary row per match; the \
                 default) or 'all' for ALL ROWS PER MATCH (one row per matched row, tagged with \
                 its match_number and classifier).",
            ),
            ArgSpec::const_arg(
                "after",
                -1,
                "varchar",
                "AFTER MATCH SKIP mode controlling where the next match search resumes: 'past last \
                 row' (default), 'to next row', 'to first <VAR>', or 'to last <VAR>'.",
            ),
            ArgSpec::const_arg(
                "step_budget",
                -1,
                "int64",
                "Maximum matcher steps per partition before a clean catastrophic-backtracking \
                 error is raised (default 5000000). The matcher never hangs; raise this for very \
                 large or ambiguous patterns.",
            ),
        ]
    }

    fn on_bind(&self, params: &BindParams) -> Result<BindResponse> {
        let input = params
            .input_schema
            .as_ref()
            .ok_or_else(|| ve("match_recognize: requires an input relation"))?;
        let plan = build_plan(&params.arguments, input)?;
        Ok(BindResponse {
            output_schema: output_schema(&plan),
            opaque_data: Vec::new(),
        })
    }

    fn process(&self, params: &BufferingParams, batch: &RecordBatch) -> Result<Vec<u8>> {
        params
            .storage
            .append(&params.execution_id, NS, b"", ipc::write_batch(batch)?);
        Ok(params.execution_id.clone())
    }

    fn combine(&self, params: &BufferingParams, _state_ids: &[Vec<u8>]) -> Result<Vec<Vec<u8>>> {
        Ok(vec![params.execution_id.clone()])
    }

    fn finalize_producer(
        &self,
        params: &BufferingParams,
        finalize_state_id: Vec<u8>,
    ) -> Result<Box<dyn TableProducer>> {
        let input_schema = params
            .input_schema
            .clone()
            .ok_or_else(|| ve("match_recognize: missing input schema at finalize"))?;
        let plan = build_plan(&params.arguments, &input_schema)?;

        // Drain all buffered batches.
        let mut batches: Vec<RecordBatch> = Vec::new();
        let mut after_id = 0i64;
        loop {
            let rows = params
                .storage
                .scan(&finalize_state_id, NS, b"", after_id, 256);
            if rows.is_empty() {
                break;
            }
            for (id, bytes) in rows {
                after_id = id;
                batches.push(ipc::read_batch(&bytes)?);
            }
        }

        let combined = if batches.is_empty() {
            RecordBatch::new_empty(input_schema.clone())
        } else {
            concat_batches(&input_schema, &batches)
                .map_err(|e| RpcError::runtime_error(e.to_string()))?
        };

        let store = BatchRowStore::new(combined);
        let rows = plan.run(&store).map_err(ve)?;
        let out = build_batch(params.output_schema.clone(), plan.output_columns(), &rows)?;
        Ok(Box::new(OneShot { batch: Some(out) }))
    }
}

/// Emits a single precomputed batch, then EOF.
struct OneShot {
    batch: Option<RecordBatch>,
}

impl TableProducer for OneShot {
    fn next_batch(&mut self, _out: &mut OutputCollector) -> Result<Option<RecordBatch>> {
        Ok(self.batch.take())
    }
}
