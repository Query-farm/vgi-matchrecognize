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

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
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
pub(crate) fn named_str_list(args: &Arguments, name: &str) -> Result<Vec<String>> {
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
    // Both string widths, read identically. They used to differ: the `i32` arm
    // rejected a NULL element while the `i64` arm silently turned it into `""`,
    // so `partition_by := ['a', NULL]` was an error or a phantom column name
    // depending only on how wide DuckDB happened to make the list.
    let elems: Vec<Option<String>> = if let Some(s) = inner.as_string_opt::<i32>() {
        (0..s.len())
            .map(|i| (!s.is_null(i)).then(|| s.value(i).to_string()))
            .collect()
    } else if let Some(s) = inner.as_string_opt::<i64>() {
        (0..s.len())
            .map(|i| (!s.is_null(i)).then(|| s.value(i).to_string()))
            .collect()
    } else {
        return Err(ve(format!("argument '{name}' must be a VARCHAR[] list")));
    };
    elems
        .into_iter()
        .map(|e| e.ok_or_else(|| ve(format!("argument '{name}' must not contain NULL"))))
        .collect()
}

/// Assemble the [`PlanConfig`] from the call arguments.
fn plan_config(args: &Arguments) -> Result<PlanConfig> {
    let pattern = args
        .named_str("pattern")
        .ok_or_else(|| ve("match_recognize: 'pattern' is required"))?;
    // These three are maps from a name to something, so they accept a DuckDB MAP or STRUCT
    // as well as a JSON string; `crate::args` normalises the shape to JSON text.
    let define_json = crate::args::structured_arg(args, "define")?.unwrap_or_default();
    let subset_json = crate::args::structured_arg(args, "subset")?.unwrap_or_default();
    let measures_json = crate::args::structured_arg(args, "measures")?;
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

thread_local! {
    /// Buffered-column projections, keyed by execution, for the sink path.
    ///
    /// `process` is called once per input batch and was rebuilding the whole plan each
    /// time — parsing the pattern, parsing every DEFINE and MEASURES expression, and
    /// re-running type inference. That is 5.3 us per batch, so ~26 ms per 10M rows, for
    /// an answer that cannot change within an execution: the arguments and the input
    /// schema are fixed at bind time.
    ///
    /// Thread-local rather than shared, so concurrent sinks never contend on a lock —
    /// the cost is one build per thread per execution instead of one per batch. Bounded
    /// and cleared wholesale when it grows, since an execution's entry is dead the
    /// moment its query ends and nothing tells the worker when that was.
    static SINK_PROJECTIONS: RefCell<HashMap<Vec<u8>, Rc<Vec<usize>>>> =
        RefCell::new(HashMap::new());
}

/// Most executions to keep projections for. Small: a worker serves a handful of
/// concurrent queries, and rebuilding after a flush costs microseconds.
const SINK_PLAN_CACHE_MAX: usize = 16;

/// The columns this execution buffers, computed once per thread.
///
/// The plan itself is not kept: `process` needs it only to work out which columns the
/// pattern reads, and that answer is what gets reused.
fn sink_projection(
    execution_id: &[u8],
    args: &Arguments,
    input_schema: &SchemaRef,
) -> Result<Rc<Vec<usize>>> {
    if let Some(hit) = SINK_PROJECTIONS.with(|c| c.borrow().get(execution_id).map(Rc::clone)) {
        return Ok(hit);
    }
    let plan = build_plan(args, input_schema)?;
    let entry = Rc::new(projection(&plan, input_schema));
    SINK_PROJECTIONS.with(|c| {
        let mut cache = c.borrow_mut();
        if cache.len() >= SINK_PLAN_CACHE_MAX {
            cache.clear();
        }
        cache.insert(execution_id.to_vec(), Rc::clone(&entry));
    });
    Ok(entry)
}

/// The schema the sinks wrote: the input schema projected to the columns the plan
/// reads. Derived the same way in every phase, so they agree on the buffered layout.
fn projected_schema(plan: &Plan, input_schema: &SchemaRef) -> Result<SchemaRef> {
    Ok(Arc::new(
        input_schema
            .project(&projection(plan, input_schema))
            .map_err(|e| RpcError::runtime_error(e.to_string()))?,
    ))
}

/// Reject storage backends that cannot survive the sink and source running in
/// different worker processes.
///
/// The subprocess transport pools workers, so `process` and `finalize_producer`
/// are not guaranteed to share an address space. With an in-process store the
/// buffered relation is simply *absent* at finalize and the function would return
/// zero rows — silently. Durable backends (the default SQLite store, or the
/// filesystem store) are all fine.
///
/// `multi_process` is what makes `memory` unsound, so it is a parameter rather than
/// an assumption: the wasm build serves every phase from **one** module instance
/// (the SAB ring hands each slot a serve *thread*, not a process) and has no
/// filesystem to be durable on, so there `memory` is the only workable backend and
/// is perfectly safe. See `crates/mr-wasm/src/lib.rs`, which selects it.
pub(crate) fn storage_backend_ok(configured: Option<&str>, multi_process: bool) -> Result<()> {
    let is_memory = configured.is_some_and(|v| v.eq_ignore_ascii_case("memory"));
    // A local spool carries the buffered batches across processes on its own, so the
    // SDK store only has to hold the small control records — but those matter too
    // (the schemas the finalize phase replays), so `memory` stays refused when the
    // phases can land in different processes.
    if is_memory && multi_process {
        return Err(ve(
            "match_recognize: VGI_WORKER_SHARED_STORAGE=memory cannot be used with a buffering \
             function — the buffering and producing phases may run in different worker processes, \
             so an in-process store is empty at finalize and the result would be silently empty. \
             Unset the variable to use the default durable store.",
        ));
    }
    Ok(())
}

/// [`storage_backend_ok`] against the ambient configuration.
fn check_storage_backend() -> Result<()> {
    // One address space under wasm32; a pool of worker processes everywhere else.
    let multi_process = !cfg!(target_arch = "wasm32");
    storage_backend_ok(
        std::env::var("VGI_WORKER_SHARED_STORAGE").ok().as_deref(),
        multi_process,
    )
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
                "any",
                "Maps each pattern variable to a predicate over the current row that decides \
                 whether the row can match that variable. Write it as a MAP, as in a single-entry \
                 map from the key DOWN to the predicate price < PREV(price), or as the equivalent \
                 JSON object in a string. A STRUCT is also accepted. Variables not listed default \
                 to always-match. Predicates may use column refs, PREV/NEXT/FIRST/LAST, running \
                 aggregates, and arithmetic/comparison/logical operators. Dollar-quoting a \
                 predicate avoids doubling the quotes inside it.",
            ),
            ArgSpec::const_arg(
                "subset",
                -1,
                "any",
                "SQL:2016 SUBSET: declares union variables, as a MAP (or the equivalent JSON \
                 object in a string). Each key names a new union variable and its value is a list \
                 naming the pattern variables that union covers — for instance a key U whose list \
                 names the variables A and B. A union variable then matches rows bound to its \
                 members, wherever a pattern variable may be written, such as in a qualified \
                 column reference or an aggregate. Giving a union variable its own predicate in \
                 define is an error. This argument is free-form JSON, not a fixed vocabulary of \
                 keywords.",
            ),
            ArgSpec::const_arg(
                "measures",
                -1,
                "any",
                "Maps each output column name to a measure expression over the match. Write it as \
                 a MAP, as in a single-entry map from the key n to the expression COUNT(*), or as \
                 the equivalent JSON object in a string; a STRUCT is also accepted, and key order \
                 is the output column order in every form. An alternative list form lets each \
                 measure additionally pin an explicit output type, as entries carrying as / expr \
                 / type keys. Expression output types are otherwise inferred from the input \
                 schema at bind time. This argument is free-form, not a fixed vocabulary of \
                 keywords.",
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
        let columns = sink_projection(&params.execution_id, &params.arguments, &input_schema)?;
        let projected = batch
            .project(&columns)
            .map_err(|e| RpcError::runtime_error(e.to_string()))?;
        crate::buffer::append_batch(
            &params.storage,
            &params.execution_id,
            &projected,
            params.batch_index,
        )?;
        Ok(params.execution_id.clone())
    }

    /// Decide the shape of the finalize phase, now that the input size is known.
    ///
    /// This is the only point where the whole relation is known to be present and a
    /// single process is looking at it, so it is where the split decision belongs: if
    /// the spooled input is bigger than the finalize memory budget, rewrite it into
    /// shards by partition key and return one finalize state per shard, so each
    /// producer holds a shard rather than the relation. Otherwise return one state and
    /// nothing is rewritten.
    fn combine(&self, params: &BufferingParams, state_ids: &[Vec<u8>]) -> Result<Vec<Vec<u8>>> {
        // Carry the sink count to finalize outside the store, so finalize can tell
        // "no input rows" (legitimately empty) from "cannot see the sinks' state".
        let count = u32::try_from(state_ids.len()).unwrap_or(u32::MAX);
        let scope = &params.execution_id;

        let spooled = crate::spool::sink_uncompressed_bytes(scope);
        let plan = params
            .input_schema
            .as_ref()
            .and_then(|schema| build_plan(&params.arguments, schema).ok());
        let partitioned = plan
            .as_ref()
            .is_some_and(|p| !p.partition_by_columns().is_empty());
        let shards = crate::shard::shard_count(spooled, partitioned);

        // Sharding covers spooled rows only. If any sink fell back to the SDK store
        // its rows are not in the spool, so splitting would silently drop them —
        // stay unsharded rather than risk that.
        let all_spooled = crate::buffer::sdk_log_is_empty(&params.storage, scope) && spooled > 0;
        if shards > 1 && all_spooled {
            if let (Some(plan), Some(input_schema)) = (plan.as_ref(), params.input_schema.as_ref())
            {
                let projected = projected_schema(plan, input_schema)?;
                let rows = crate::shard::split(scope, plan, &projected, shards)?;
                params.log(format!(
                    "match_recognize: split {spooled} buffered bytes into {shards} shards \
                     (finalize memory budget {} bytes)",
                    crate::shard::budget_bytes()
                ));
                return Ok((0..shards)
                    .map(|s| {
                        crate::buffer::FinalizeState::encode(
                            scope,
                            count,
                            s as u32,
                            shards as u32,
                            Some(rows[s]),
                        )
                    })
                    .collect());
            }
        }
        Ok(vec![crate::buffer::FinalizeState::encode(
            scope, count, 0, 1, None,
        )])
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
        let state = crate::buffer::FinalizeState::decode(&finalize_state_id)?;
        let batches = crate::buffer::read_batches(&params.storage, &state)?;

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
        // The buffered relation is now entirely in `store`, and nothing reads the spool
        // again — so this is the point to delete it. Waiting for end-of-stream or Drop
        // is not reliable: a consumer that stops asking (or the SDK holding the producer
        // until its *best-effort* destructor RPC) leaves the files behind, which is
        // exactly what the e2e suite was doing, one directory per query.
        //
        // If a finalize stream were ever re-initialised for the same state id it would
        // now read nothing — and be told so: the sink-count and per-shard row-count
        // guards turn that into an error, never a short answer.
        match state.shards {
            0 | 1 => crate::spool::discard(&state.scope),
            _ => crate::spool::discard_shard(&state.scope, state.shard as usize),
        }
        let tapes = plan.partition_tapes(&store).map_err(ve)?;
        Ok(Box::new(PartitionStream {
            plan,
            store,
            tapes: tapes.into_iter(),
            output_schema: params.output_schema.clone(),
            ready: std::collections::VecDeque::new(),
            threads: match_threads(),
        }))
    }
}

/// Rows to accumulate before emitting an output batch. Matching is per-partition,
/// so the partition is the unit of work — but partitions are often tiny (one per
/// user, say), and emitting a batch each would pay IPC framing per handful of
/// rows. Coalescing to a target keeps batches a sensible size while still bounding
/// how many output rows are live at once.
const TARGET_BATCH_ROWS: usize = 8192;

/// Input rows to match ahead of what the consumer has asked for.
///
/// Matching runs in parallel over a *chunk* of partitions, so the chunk has to be
/// worth the threads: too small and thread setup dominates, too large and more output
/// rows are live at once than necessary. Sized in input rows because that is what is
/// known before matching (output rows can be anywhere from zero to one per input row).
const LOOKAHEAD_ROWS: usize = 8 * TARGET_BATCH_ROWS;

/// Most partitions to match in one chunk, whatever their size — a bound on how many
/// tapes are in flight when partitions are tiny.
const MAX_CHUNK_PARTITIONS: usize = 4096;

/// How many threads to match partitions on.
///
/// `VGI_MR_MATCH_THREADS` overrides it; 1 forces the serial path, which is what the
/// determinism tests compare against. Defaults to the machine's parallelism, capped —
/// the worker is one of several processes DuckDB may be running, so it should not try
/// to own every core.
fn match_threads() -> usize {
    if cfg!(target_arch = "wasm32") {
        return 1;
    }
    if let Some(n) = std::env::var("VGI_MR_MATCH_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        return n.max(1);
    }
    std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(1)
}

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

    /// Matched partitions waiting to be emitted, each entry one partition's rows, in
    /// partition order. Keeping them grouped means a partition's rows stay contiguous
    /// and a match is never split across output batches.
    ready: std::collections::VecDeque<Vec<Vec<mr_core::value::Value>>>,
    /// Threads to match on. Read once, so a query cannot change it midway.
    threads: usize,
}

impl PartitionStream {
    /// Rows currently queued for emission.
    fn ready_rows(&self) -> usize {
        self.ready.iter().map(|g| g.len()).sum()
    }

    /// Match the next chunk of partitions, appending their rows to `ready`.
    ///
    /// Partitions are independent — `run_partition` takes an immutable plan and store,
    /// its own tape, and MATCH_NUMBER restarts at 1 for each — so a chunk is matched
    /// concurrently and the results are appended **in partition order**, which keeps
    /// the output byte-identical to the serial path.
    fn match_chunk(&mut self) -> Result<bool> {
        let mut chunk: Vec<(String, Vec<usize>)> = Vec::new();
        let mut input_rows = 0usize;
        for tape in self.tapes.by_ref() {
            input_rows += tape.1.len();
            chunk.push(tape);
            if input_rows >= LOOKAHEAD_ROWS || chunk.len() >= MAX_CHUNK_PARTITIONS {
                break;
            }
        }
        if chunk.is_empty() {
            return Ok(false);
        }

        let threads = self.threads.min(chunk.len());
        // One thread, or too little work to be worth spawning any: stay on this one.
        if threads <= 1 || input_rows < TARGET_BATCH_ROWS {
            for (label, tape) in &mut chunk {
                let mut rows = Vec::new();
                self.plan
                    .run_partition(&self.store, label, tape, &mut rows)
                    .map_err(ve)?;
                self.ready.push_back(rows);
            }
            return Ok(true);
        }

        // Contiguous slices, so each thread owns its tapes outright and the results
        // land in their original positions with no synchronisation.
        let per_thread = chunk.len().div_ceil(threads);
        let mut slots: Vec<mr_core::error::Result<Vec<Vec<mr_core::value::Value>>>> =
            (0..chunk.len()).map(|_| Ok(Vec::new())).collect();
        let plan = &self.plan;
        let store = &self.store;
        std::thread::scope(|scope| {
            for (out_slice, work) in slots
                .chunks_mut(per_thread)
                .zip(chunk.chunks_mut(per_thread))
            {
                scope.spawn(move || {
                    for (slot, (label, tape)) in out_slice.iter_mut().zip(work.iter_mut()) {
                        let mut rows = Vec::new();
                        *slot = plan
                            .run_partition(store, label, tape, &mut rows)
                            .map(|()| rows);
                    }
                });
            }
        });
        // First failure in partition order wins, so an error is reproducible.
        for slot in slots {
            self.ready.push_back(slot.map_err(ve)?);
        }
        Ok(true)
    }
}

impl TableProducer for PartitionStream {
    fn next_batch(&mut self, _out: &mut OutputCollector) -> Result<Option<RecordBatch>> {
        // Match ahead until there is a batch's worth queued, or the input is drained.
        while self.ready_rows() < TARGET_BATCH_ROWS {
            if !self.match_chunk()? {
                break;
            }
        }
        // Emit whole partitions: a single one may overshoot the target, which is
        // deliberate — a match is never split across output batches.
        let mut rows: Vec<Vec<mr_core::value::Value>> = Vec::new();
        while let Some(group) = self.ready.pop_front() {
            rows.extend(group);
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
