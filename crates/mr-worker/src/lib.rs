//! The `matchrecognize` VGI worker (library).
//!
//! Function registration and catalog metadata live here so both entrypoints
//! share them verbatim: `main.rs` (the native binary, stdio/HTTP transport) and
//! the `mr-wasm` crate (the browser build, which serves the same `Worker` over a
//! SharedArrayBuffer byte channel instead).
//!
//! A standalone binary DuckDB launches and talks to over Apache Arrow IPC. It
//! brings SQL:2016 `MATCH_RECOGNIZE` row pattern matching to SQL under the
//! catalog `mr`, schema `main`:
//!
//! - `mr.main.match_recognize((<rel>), partition_by:=, order_by:=, pattern:=,
//!   define:=, measures:=, rows:=, after:=)` — row pattern matching (table-in/out)
//! - `mr.main.explain_pattern(p)` — pretty-print a compiled pattern (no data)

pub mod args;
pub mod arrow_in;
mod arrow_out;
pub mod buffer;
mod catalog;
mod match_recognize;
mod meta;
mod scalar;
mod schema;
pub mod shard;
pub mod spool;
#[cfg(test)]
mod tests;

use vgi::Worker;

/// The catalog name DuckDB sees in `ATTACH 'mr' (TYPE vgi, …)`. Defaults to
/// `mr`, but honors an explicit override so a test harness can rename it.
/// Also exports the variable so downstream SDK code observes the same default.
pub fn catalog_name() -> String {
    if std::env::var_os("VGI_WORKER_CATALOG_NAME").is_none() {
        std::env::set_var("VGI_WORKER_CATALOG_NAME", "mr");
    }
    std::env::var("VGI_WORKER_CATALOG_NAME").unwrap_or_else(|_| "mr".to_string())
}

/// Build a fully-registered worker: every scalar and buffering function plus the
/// catalog metadata. Callers choose the transport — `run()` natively,
/// `serve_reader_writer()` in the browser.
pub fn build_worker() -> Worker {
    let name = catalog_name();
    let mut worker = Worker::new();
    scalar::register(&mut worker);
    worker.register_buffering(match_recognize::MatchRecognize);
    worker.set_catalog(catalog::catalog_metadata(&name));
    worker
}
