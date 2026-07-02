//! `mr_version()` — return the worker's version string.

use std::sync::Arc;

use arrow_array::{ArrayRef, RecordBatch, StringArray};
use arrow_schema::DataType;
use vgi::{
    ArgSpec, BindParams, BindResponse, FunctionExample, FunctionMetadata, ProcessParams,
    ScalarFunction,
};
use vgi_rpc::{Result, RpcError};

pub struct MrVersion;

impl ScalarFunction for MrVersion {
    fn name(&self) -> &str {
        "mr_version"
    }

    fn metadata(&self) -> FunctionMetadata {
        let mut tags = crate::meta::object_tags(
            "MatchRecognize Worker Version",
            "Return the version string of the running matchrecognize worker binary (the worker's \
             own build version, not the SDK/protocol version; it is the crate's Cargo version). \
             The string is semver, MAJOR.MINOR.PATCH (e.g. '0.1.0'). The function takes no \
             arguments, is deterministic, and never returns NULL — useful for diagnostics and \
             confirming which build is attached.",
            "Return the matchrecognize worker version string, e.g. `mr_version()` → '0.1.0'. \
             Argument-free and deterministic; returns a single semver (MAJOR.MINOR.PATCH) VARCHAR.",
            "version, build version, mr_version, diagnostics, worker version, semver, \
             match_recognize",
        );
        tags.push((
            "vgi.executable_examples".into(),
            r#"[
  {
    "description": "Return the worker version string.",
    "sql": "SELECT mr.main.mr_version() AS version"
  },
  {
    "description": "Show the compiled form of a V-shape pattern (a falling run then a rising run).",
    "sql": "SELECT mr.main.explain_pattern('START DOWN+ UP+') AS compiled"
  }
]"#
            .into(),
        ));
        tags.push(("vgi.category".into(), "Diagnostics".into()));
        FunctionMetadata {
            description: "Returns the matchrecognize worker version string".into(),
            return_type: Some(DataType::Utf8),
            examples: vec![FunctionExample {
                sql: "SELECT mr.main.mr_version();".into(),
                description: "Return the matchrecognize worker version string.".into(),
                expected_output: None,
            }],
            tags,
            ..Default::default()
        }
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        Vec::new()
    }

    fn on_bind(&self, _params: &BindParams) -> Result<BindResponse> {
        Ok(BindResponse::result(DataType::Utf8))
    }

    fn process(&self, params: &ProcessParams, batch: &RecordBatch) -> Result<RecordBatch> {
        let rows = batch.num_rows().max(1);
        let out: ArrayRef = Arc::new(StringArray::from(vec![mr_core::version(); rows]));
        RecordBatch::try_new(params.output_schema.clone(), vec![out])
            .map_err(|e| RpcError::runtime_error(e.to_string()))
    }
}
