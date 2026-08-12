//! `mr.match_recognize((<relation>), partition_by:=, order_by:=, pattern:=,
//! define:=, measures:=, rows:=, after:=, [step_budget:=])` — SQL:2016 row
//! pattern matching over a buffered relation.
//!
//! A `TableBufferingFunction` (Sink + Source): every input batch is buffered
//! into cross-process storage (`process`) — on disk, as Arrow IPC, under the
//! default SQLite backend; once the whole relation is present,
//! `finalize_producer` reads it back, builds the mr-core [`Plan`], and returns a
//! producer that runs the matcher **one partition at a time**, emitting a batch
//! per partition on the bind-time output schema. Pattern matching is intrinsically
//! a whole-partition operation, so buffer-all-then-compute is required (the
//! `vgi-match` idiom) — but the partition is the natural streaming unit, so the
//! whole result set never has to be live at once.

use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::{Array, RecordBatch};
use arrow_schema::{Schema, SchemaRef};
use mr_core::plan::{Plan, PlanConfig};
use vgi::arguments::Arguments;
use vgi::buffering::{BufferingParams, TableBufferingFunction};
use vgi::function::{ArgSpec, BindParams, BindResponse, FunctionMetadata};
use vgi::table_function::TableProducer;
use vgi_rpc::{OutputCollector, Result, RpcError};

use crate::arrow_in::BatchRowStore;
use crate::arrow_out::build_batch;
use crate::schema::{output_field, ArrowBindSchema};

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
    let subset_json = args.named_str("subset").unwrap_or_default();
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
    let empty = args
        .named_str("empty_matches")
        .unwrap_or_else(|| "show".to_string())
        .to_ascii_lowercase();
    let omit_empty_matches = match empty.as_str() {
        "omit" => true,
        "show" => false,
        other => {
            return Err(ve(format!(
                "empty_matches must be 'show' or 'omit', got '{other}'"
            )))
        }
    };
    let after = args
        .named_str("after")
        .unwrap_or_else(|| "past last row".to_string());
    // Absent -> None, i.e. scale the budget with each partition's row count.
    let step_budget = args.named_i64("step_budget");
    Ok(PlanConfig {
        pattern,
        define_json,
        subset_json,
        measures_json,
        partition_by,
        order_by,
        rows_all,
        omit_empty_matches,
        after,
        step_budget,
    })
}

/// Build the bound mr-core [`Plan`] from arguments + the input schema.
fn build_plan(args: &Arguments, input_schema: &SchemaRef) -> Result<Plan> {
    let cfg = plan_config(args)?;
    // Parse the pattern once to get the variable set for inference.
    let pat = mr_core::pattern::parse(&cfg.pattern).map_err(ve)?;
    // Union variables are usable wherever a pattern variable is, so the schema
    // must type-check qualifiers against both.
    let mut labels = pat.variables();
    labels.extend(mr_core::plan::subset_names(&cfg.subset_json).map_err(ve)?);
    let bind_schema = ArrowBindSchema::new(input_schema.clone(), labels);
    Plan::build(&cfg, &bind_schema).map_err(ve)
}

/// The columns of `input_schema` that `plan` actually reads, as projection
/// indices in schema order.
///
/// Everything else is dropped before the batch is buffered. Both `process` and
/// `finalize_producer` derive this from the same arguments and input schema, so
/// they always agree on the buffered layout.
fn projection(plan: &Plan, input_schema: &SchemaRef) -> Vec<usize> {
    let needed = plan.referenced_columns();
    (0..input_schema.fields().len())
        .filter(|i| {
            let name = input_schema.field(*i).name();
            needed.iter().any(|n| n.eq_ignore_ascii_case(name))
        })
        .collect()
}

/// Reject storage backends that cannot survive the sink and source running in
/// different worker processes.
///
/// The subprocess transport pools workers, so `process` and `finalize_producer`
/// are not guaranteed to share an address space. With an in-process store the
/// buffered relation is simply *absent* at finalize and the function would return
/// zero rows — silently. Durable backends (the default SQLite store, or the
/// filesystem store) are all fine.
fn check_storage_backend() -> Result<()> {
    if std::env::var("VGI_WORKER_SHARED_STORAGE")
        .unwrap_or_default()
        .eq_ignore_ascii_case("memory")
    {
        return Err(ve(
            "match_recognize: VGI_WORKER_SHARED_STORAGE=memory cannot be used with a buffering \
             function — the buffering and producing phases may run in different worker processes, \
             so an in-process store is empty at finalize and the result would be silently empty. \
             Unset the variable to use the default durable store.",
        ));
    }
    Ok(())
}

/// The output Arrow schema derived from a bound plan.
fn output_schema(plan: &Plan) -> SchemaRef {
    let fields = plan
        .output_columns()
        .iter()
        .map(|c| output_field(&c.name, &c.ty))
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
                "Column names to partition by (like SQL `PARTITION BY`). Each partition is matched \
                 independently. Omit for a single global partition.",
            ),
            ArgSpec::const_typed(
                "order_by",
                -1,
                varchar_list,
                "Column names defining the intra-partition row order (like SQL `ORDER BY`). \
                 Required. An element may carry a ' DESC' and/or ' NULLS FIRST/LAST' suffix, e.g. \
                 'ts DESC'.",
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
                "A JSON object mapping each pattern variable to a predicate over the current row \
                 that decides whether the row can match that variable, e.g. '{\"DOWN\":\"price < \
                 PREV(price)\"}'. Variables not listed default to always-match. Predicates may use \
                 column refs, PREV/NEXT/FIRST/LAST, running aggregates, and \
                 arithmetic/comparison/logical operators.",
            ),
            ArgSpec::const_arg(
                "subset",
                -1,
                "varchar",
                "SQL:2016 SUBSET: a JSON object declaring union variables. Each key names a new \
                 union variable and its value is a JSON array naming the pattern variables that \
                 union covers — for instance a key U whose array names the variables A and B. A \
                 union variable stands for any of its members wherever a pattern variable may \
                 appear, such as in a qualified column reference or an aggregate. Giving a union \
                 variable its own predicate in define is an error. This argument is free-form \
                 JSON, not a fixed vocabulary of keywords.",
            ),
            ArgSpec::const_arg(
                "measures",
                -1,
                "varchar",
                "A JSON object mapping each output column name to a measure expression over the \
                 match — for example a single-key object like {\"n\": \"COUNT(*)\"}. An \
                 alternative array form lets each measure additionally pin an explicit output \
                 type via an object carrying as / expr / type keys. Expression output types are \
                 otherwise inferred from the input schema at bind time. This argument is \
                 free-form JSON, not a fixed vocabulary of keywords.",
            ),
            ArgSpec::const_arg(
                "rows",
                -1,
                "varchar",
                "Output cardinality. Accepts exactly two lowercase string values: 'one' (the \
                 default) for one summary row per match (SQL:2016 ONE ROW PER MATCH), or 'all' for \
                 one row per matched row, each tagged with its match_number and classifier \
                 (SQL:2016 ALL ROWS PER MATCH). The SQL:2016 phrases themselves are not accepted — \
                 pass 'one' or 'all'.",
            )
            .with_choices(["one", "all"]),
            ArgSpec::const_arg(
                "after",
                -1,
                "varchar",
                "AFTER MATCH SKIP mode: chooses where the search for the next match resumes after \
                 a successful match. It defaults to skipping past the last row of the matched \
                 span. It may instead resume at the row after the match start; or at the first / \
                 last row that was bound to a named pattern variable (naming any variable from \
                 define). Browse the mr.main.after_match_skip_modes view for the full set.",
            )
            // Two of the four forms name a pattern variable, so the value set is
            // not closed; a pattern is the machine-readable constraint that fits.
            .with_pattern(r"(?i)^(past last row|to next row|to (first|last) .+)$"),
            ArgSpec::const_arg(
                "empty_matches",
                -1,
                "varchar",
                "Whether a match that binds no rows contributes an output row. Accepts exactly two \
                 lowercase string values: 'show' (the default, SQL:2016 SHOW EMPTY MATCHES) or \
                 'omit' (OMIT EMPTY MATCHES). It applies only when rows is 'all'; with rows 'one' \
                 an empty match always reports a row, carrying NULL measures. Empty matches \
                 consume a match number either way. The SQL:2016 phrases themselves are not \
                 accepted — pass 'show' or 'omit'.",
            )
            .with_choices(["show", "omit"]),
            ArgSpec::const_arg(
                "step_budget",
                -1,
                "int64",
                "Maximum matcher steps per partition before a clean catastrophic-backtracking \
                 error is raised. Omit it to scale the budget with each partition's row count, \
                 which keeps ordinary long matches working at any size; pass a number to pin it \
                 instead. The matcher never hangs.",
            )
            .with_ge(1.0),
        ]
    }

    fn on_bind(&self, params: &BindParams) -> Result<BindResponse> {
        let input = params
            .input_schema
            .as_ref()
            .ok_or_else(|| ve("match_recognize: requires an input relation"))?;
        check_storage_backend()?;
        let plan = build_plan(&params.arguments, input)?;
        Ok(BindResponse {
            output_schema: output_schema(&plan),
            opaque_data: Vec::new(),
        })
    }

    fn process(&self, params: &BufferingParams, batch: &RecordBatch) -> Result<Vec<u8>> {
        // Buffer only the columns the pattern reads. Volume through the store
        // dominates runtime, so carrying unused columns is the most expensive
        // thing we could do here.
        let input_schema = params
            .input_schema
            .clone()
            .ok_or_else(|| ve("match_recognize: missing input schema while buffering"))?;
        let plan = build_plan(&params.arguments, &input_schema)?;
        let projected = batch
            .project(&projection(&plan, &input_schema))
            .map_err(|e| RpcError::runtime_error(e.to_string()))?;
        crate::buffer::append_batch(&params.storage, &params.execution_id, &projected)?;
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

        // Read the buffered relation back, verifying nothing was lost in transit.
        let batches = crate::buffer::read_batches(&params.storage, &finalize_state_id)?;

        // The store is built over the *projected* schema, matching what `process`
        // wrote; the batches are kept as they arrived rather than concatenated,
        // since the engine reads cells by row index and a merged copy would only
        // double peak memory.
        let projected_schema = std::sync::Arc::new(
            input_schema
                .project(&projection(&plan, &input_schema))
                .map_err(|e| RpcError::runtime_error(e.to_string()))?,
        );
        let store = BatchRowStore::new(projected_schema, batches);
        let tapes = plan.partition_tapes(&store).map_err(ve)?;
        Ok(Box::new(PartitionStream {
            plan,
            store,
            tapes: tapes.into_iter(),
            output_schema: params.output_schema.clone(),
        }))
    }
}

/// Rows to accumulate before emitting an output batch. Matching is per-partition,
/// so the partition is the unit of work — but partitions are often tiny (one per
/// user, say), and emitting a batch each would pay IPC framing per handful of
/// rows. Coalescing to a target keeps batches a sensible size while still bounding
/// how many output rows are live at once.
const TARGET_BATCH_ROWS: usize = 8192;

/// Streams the result, running the matcher one partition at a time and emitting
/// batches of roughly [`TARGET_BATCH_ROWS`] rows.
///
/// Only the rows of the partitions accumulated so far are ever materialized, not
/// the whole result set. Each partition is numbered independently, so batching
/// several into one output batch cannot affect MATCH_NUMBER.
struct PartitionStream {
    plan: Plan,
    store: BatchRowStore,
    tapes: std::vec::IntoIter<(String, Vec<usize>)>,
    output_schema: SchemaRef,
}

impl TableProducer for PartitionStream {
    fn next_batch(&mut self, _out: &mut OutputCollector) -> Result<Option<RecordBatch>> {
        let mut rows: Vec<Vec<mr_core::value::Value>> = Vec::new();
        // Consume partitions until the batch is full enough (a single partition may
        // overshoot the target — matches are never split across batches) or the
        // input is drained. Partitions yielding no matches simply add nothing.
        for (label, mut tape) in self.tapes.by_ref() {
            self.plan
                .run_partition(&self.store, &label, &mut tape, &mut rows)
                .map_err(ve)?;
            if rows.len() >= TARGET_BATCH_ROWS {
                break;
            }
        }
        if rows.is_empty() {
            // Every remaining partition matched nothing: end of stream.
            return Ok(None);
        }
        build_batch(
            self.output_schema.clone(),
            self.plan.output_columns(),
            &rows,
        )
        .map(Some)
    }
}
