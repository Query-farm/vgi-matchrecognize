//! Worker-side integration tests: Arrow `RecordBatch` -> mr-core engine ->
//! Arrow `RecordBatch`, plus the bind-time output-schema construction.

use std::sync::Arc;

use arrow_array::{Int64Array, RecordBatch, StringArray, TimestampMicrosecondArray};
use arrow_schema::{DataType, Field, Schema, TimeUnit as ArrowTimeUnit};
use mr_core::plan::{Plan, PlanConfig};

use crate::arrow_in::BatchRowStore;
use crate::arrow_out::build_batch;
use crate::schema::{output_field, ArrowBindSchema};

fn ticks_batch() -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![
        Field::new("symbol", DataType::Utf8, false),
        Field::new(
            "ts",
            DataType::Timestamp(ArrowTimeUnit::Microsecond, None),
            false,
        ),
        Field::new("price", DataType::Int64, false),
    ]));
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(vec![
                "ACME", "ACME", "ACME", "ACME", "ACME",
            ])),
            Arc::new(TimestampMicrosecondArray::from(vec![1, 2, 3, 4, 5])),
            Arc::new(Int64Array::from(vec![10, 8, 6, 9, 11])),
        ],
    )
    .unwrap()
}

fn build_plan(batch: &RecordBatch, measures: &str, rows_all: bool) -> Plan {
    let cfg = PlanConfig {
        pattern: "START DOWN+ UP+".into(),
        define_json: r#"{"DOWN":"price < PREV(price)","UP":"price > PREV(price)"}"#.into(),
        subset_json: String::new(),
        measures_json: Some(measures.into()),
        partition_by: vec!["symbol".into()],
        order_by: vec!["ts".into()],
        rows_all,
        after: "past last row".into(),
        omit_empty_matches: false,
        step_budget: Some(1_000_000),
    };
    let pat = mr_core::pattern::parse(&cfg.pattern).unwrap();
    let bs = ArrowBindSchema::new(batch.schema(), pat.variables());
    Plan::build(&cfg, &bs).unwrap()
}

#[test]
fn arrow_round_trip_one_row() {
    let batch = ticks_batch();
    // A timestamp measure + a numeric measure exercise typed array building.
    let plan = build_plan(
        &batch,
        r#"{"match_no":"MATCH_NUMBER()","bottom_ts":"LAST(DOWN.ts)","drawdown":"FIRST(START.price) - LAST(DOWN.price)"}"#,
        false,
    );

    // Output schema: symbol(VARCHAR), match_no(BIGINT), bottom_ts(TIMESTAMP), drawdown(BIGINT).
    let fields: Vec<Field> = plan
        .output_columns()
        .iter()
        .map(|c| output_field(&c.name, &c.ty))
        .collect();
    let out_schema = Arc::new(Schema::new(fields));
    assert_eq!(out_schema.field(0).data_type(), &DataType::Utf8);
    assert_eq!(out_schema.field(1).data_type(), &DataType::Int64);
    assert_eq!(
        out_schema.field(2).data_type(),
        &DataType::Timestamp(ArrowTimeUnit::Microsecond, None)
    );
    assert_eq!(out_schema.field(3).data_type(), &DataType::Int64);

    let store = BatchRowStore::single(batch);
    let rows = plan.run(&store).unwrap();
    let out = build_batch(out_schema, plan.output_columns(), &rows).unwrap();

    assert_eq!(out.num_rows(), 1);
    let symbol = out
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(symbol.value(0), "ACME");
    let match_no = out.column(1).as_any().downcast_ref::<Int64Array>().unwrap();
    assert_eq!(match_no.value(0), 1);
    let bottom_ts = out
        .column(2)
        .as_any()
        .downcast_ref::<TimestampMicrosecondArray>()
        .unwrap();
    assert_eq!(bottom_ts.value(0), 3); // ts of the bottom (price 6)
    let drawdown = out.column(3).as_any().downcast_ref::<Int64Array>().unwrap();
    assert_eq!(drawdown.value(0), 4); // 10 - 6
}

#[test]
fn arrow_all_rows_has_auto_columns() {
    let batch = ticks_batch();
    let plan = build_plan(&batch, r#"{"n":"COUNT(*)"}"#, true);
    // ALL ROWS: symbol, ts, match_number, classifier, n
    let cols = plan.output_columns();
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["symbol", "ts", "match_number", "classifier", "n"]
    );

    let store = BatchRowStore::single(batch);
    let rows = plan.run(&store).unwrap();
    let fields: Vec<Field> = cols.iter().map(|c| output_field(&c.name, &c.ty)).collect();
    let out = build_batch(Arc::new(Schema::new(fields)), cols, &rows).unwrap();
    // The V is START(10) DOWN(8) DOWN(6) UP(9) UP(11) -> 5 matched rows.
    assert_eq!(out.num_rows(), 5);
    let classifier = out
        .column(3)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(classifier.value(0), "START");
    assert_eq!(classifier.value(4), "UP");
}

/// The `memory` backend is refused where the phases can be separate processes, and
/// allowed where they cannot. Neither half was covered before, and the wasm build
/// depends on the second: `mr-wasm` selects `memory` because emscripten has no
/// durable temp dir and there is only ever one module instance, so refusing it
/// unconditionally made the browser build fail on its only real function.
#[test]
fn memory_backend_is_refused_only_when_phases_can_be_separate_processes() {
    use crate::match_recognize::storage_backend_ok;

    // Multi-process (every native transport): memory is unsound, so it must error,
    // and the message has to say why rather than just "invalid".
    let err = storage_backend_ok(Some("memory"), true).expect_err("memory must be refused");
    let msg = err.to_string();
    assert!(msg.contains("different worker processes"), "{msg}");
    assert!(msg.contains("VGI_WORKER_SHARED_STORAGE"), "{msg}");
    // Case-insensitively, since the SDK compares that way when selecting a backend.
    assert!(storage_backend_ok(Some("MEMORY"), true).is_err());

    // Single address space (wasm): memory is the only workable backend.
    assert!(storage_backend_ok(Some("memory"), false).is_ok());

    // Durable backends and the default are fine either way.
    for backend in [None, Some("sqlite"), Some("fs"), Some("")] {
        assert!(storage_backend_ok(backend, true).is_ok(), "{backend:?}");
        assert!(storage_backend_ok(backend, false).is_ok(), "{backend:?}");
    }
}

/// A NULL element in a `VARCHAR[]` argument is rejected the same way whichever
/// string width DuckDB happened to build the list with.
///
/// The two arms had drifted: `Utf8` raised, while `LargeUtf8` read the NULL as
/// `""` and carried on, so `partition_by := ['a', NULL]` was either an error or
/// a phantom empty column name depending on nothing the user controls.
#[test]
fn a_null_in_a_varchar_list_argument_is_refused_at_either_width() {
    use arrow_array::builder::{LargeStringBuilder, ListBuilder, StringBuilder};
    use vgi::arguments::Arguments;

    // Utf8 (i32 offsets).
    let mut narrow = ListBuilder::new(StringBuilder::new());
    narrow.values().append_value("a");
    narrow.values().append_null();
    narrow.append(true);
    // LargeUtf8 (i64 offsets).
    let mut wide = ListBuilder::new(LargeStringBuilder::new());
    wide.values().append_value("a");
    wide.values().append_null();
    wide.append(true);

    for (label, array) in [
        ("Utf8", Arc::new(narrow.finish()) as arrow_array::ArrayRef),
        (
            "LargeUtf8",
            Arc::new(wide.finish()) as arrow_array::ArrayRef,
        ),
    ] {
        let mut args = Arguments::default();
        args.named.insert("partition_by".to_string(), array);
        let err = crate::match_recognize::named_str_list(&args, "partition_by")
            .expect_err("a NULL element must be refused");
        assert!(
            err.to_string().contains("must not contain NULL"),
            "{label}: {err}"
        );
    }
}

/// And a list with no NULLs reads identically at both widths.
#[test]
fn a_varchar_list_argument_reads_the_same_at_either_width() {
    use arrow_array::builder::{LargeStringBuilder, ListBuilder, StringBuilder};
    use vgi::arguments::Arguments;

    let mut narrow = ListBuilder::new(StringBuilder::new());
    narrow.values().append_value("sym");
    narrow.values().append_value("ts");
    narrow.append(true);
    let mut wide = ListBuilder::new(LargeStringBuilder::new());
    wide.values().append_value("sym");
    wide.values().append_value("ts");
    wide.append(true);

    for array in [
        Arc::new(narrow.finish()) as arrow_array::ArrayRef,
        Arc::new(wide.finish()) as arrow_array::ArrayRef,
    ] {
        let mut args = Arguments::default();
        args.named.insert("order_by".to_string(), array);
        assert_eq!(
            crate::match_recognize::named_str_list(&args, "order_by").unwrap(),
            vec!["sym".to_string(), "ts".to_string()]
        );
    }
}
