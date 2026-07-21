//! The `matchrecognize` VGI worker.
//!
//! A standalone binary DuckDB launches and talks to over Apache Arrow IPC. It
//! brings SQL:2016 `MATCH_RECOGNIZE` row pattern matching to SQL under the
//! catalog `mr`, schema `main`:
//!
//! - `mr.main.match_recognize((<rel>), partition_by:=, order_by:=, pattern:=,
//!   define:=, measures:=, rows:=, after:=)` — row pattern matching (table-in/out)
//! - `mr.main.explain_pattern(p)` — pretty-print a compiled pattern (no data)
//!
//! The worker build version is published as the catalog's
//! `implementation_version` (readable from `vgi_catalogs()`), not a scalar.

mod arrow_in;
mod arrow_out;
mod catalog;
mod match_recognize;
mod meta;
mod scalar;
mod schema;
#[cfg(test)]
mod tests;

use vgi::Worker;

fn main() {
    // Logs MUST go to stderr — stdout is the Arrow-IPC channel.
    let _ = env_logger::Builder::from_env(env_logger::Env::default().filter_or("VGI_LOG", "info"))
        .format_timestamp_millis()
        .try_init();

    // The catalog name DuckDB sees in `ATTACH 'mr' (TYPE vgi, …)`. Default to
    // `mr`, but honor an explicit override so a test harness can rename it.
    if std::env::var_os("VGI_WORKER_CATALOG_NAME").is_none() {
        std::env::set_var("VGI_WORKER_CATALOG_NAME", "mr");
    }
    let catalog_name =
        std::env::var("VGI_WORKER_CATALOG_NAME").unwrap_or_else(|_| "mr".to_string());

    let mut worker = Worker::new();
    scalar::register(&mut worker);
    worker.register_buffering(match_recognize::MatchRecognize);
    worker.set_catalog(catalog::catalog_metadata(&catalog_name));
    worker.run();
}
