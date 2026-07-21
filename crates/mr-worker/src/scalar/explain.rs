//! `explain_pattern(pattern VARCHAR)` — return the parsed/compiled form of a
//! pattern (variables, quantifier tree, anchors) for debugging. No data access.

use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::{Array, ArrayRef, RecordBatch, StringArray};
use arrow_schema::DataType;
use vgi::{ArgSpec, BindParams, BindResponse, FunctionMetadata, ProcessParams, ScalarFunction};
use vgi_rpc::{Result, RpcError};

pub struct ExplainPattern;

impl ScalarFunction for ExplainPattern {
    fn name(&self) -> &str {
        "explain_pattern"
    }

    fn metadata(&self) -> FunctionMetadata {
        let mut tags = crate::meta::object_tags(
            "Explain Row Pattern",
            "Parse a MATCH_RECOGNIZE row pattern and return a human-readable rendering of its \
             compiled form — the variable references, concatenation, alternation, grouping, \
             quantifiers (with their greedy/reluctant disposition), and partition-edge anchors — \
             without touching any data. A development aid for checking that a pattern parses the \
             way you expect before running it over a relation. Returns one `VARCHAR` per input \
             pattern; a malformed pattern raises a clean parse error.",
            "Pretty-print the compiled form of a row pattern (variables, quantifier tree, \
             anchors) for debugging, e.g. `explain_pattern('A B+ C?')`. Pure: no data access. \
             Returns `VARCHAR`.",
            "explain pattern, debug, parse, row pattern, quantifier, alternation, anchor, \
             match_recognize, dev aid",
        );
        // VGI509 wants at least one guaranteed-runnable example worker-wide;
        // these two run with no data. Descriptions satisfy VGI515. The same JSON
        // is mirrored into vgi.example_queries (VGI306) — the native
        // duckdb_functions().examples carrier drops descriptions, so we do not
        // populate FunctionMetadata.examples.
        const EXPLAIN_EXAMPLES: &str = r#"[
  {
    "description": "Render the compiled form of a V-shape pattern (a falling run then a rising run).",
    "sql": "SELECT mr.main.explain_pattern('START DOWN+ UP+') AS compiled"
  },
  {
    "description": "Show how a reluctant quantifier and an alternation group parse.",
    "sql": "SELECT mr.main.explain_pattern('A+? (B | C) D') AS compiled"
  }
]"#;
        tags.push(("vgi.executable_examples".into(), EXPLAIN_EXAMPLES.into()));
        tags.push(("vgi.example_queries".into(), EXPLAIN_EXAMPLES.into()));
        tags.push(("vgi.category".into(), "Diagnostics".into()));
        FunctionMetadata {
            description: "Pretty-print the compiled form of a row pattern (no data access)".into(),
            return_type: Some(DataType::Utf8),
            tags,
            ..Default::default()
        }
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        vec![ArgSpec::column(
            "pattern",
            0,
            "varchar",
            "The MATCH_RECOGNIZE row pattern to parse and render, e.g. 'START DOWN+ UP+'.",
        )]
    }

    fn on_bind(&self, _params: &BindParams) -> Result<BindResponse> {
        Ok(BindResponse::result(DataType::Utf8))
    }

    fn process(&self, params: &ProcessParams, batch: &RecordBatch) -> Result<RecordBatch> {
        let col = batch.column(0).as_string::<i32>();
        let mut out: Vec<Option<String>> = Vec::with_capacity(batch.num_rows());
        for i in 0..batch.num_rows() {
            if col.is_null(i) {
                out.push(None);
                continue;
            }
            let rendered = mr_core::explain_pattern(col.value(i))
                .map_err(|e| RpcError::value_error(e.to_string()))?;
            out.push(Some(rendered));
        }
        let arr: ArrayRef = Arc::new(StringArray::from(out));
        RecordBatch::try_new(params.output_schema.clone(), vec![arr])
            .map_err(|e| RpcError::runtime_error(e.to_string()))
    }
}
