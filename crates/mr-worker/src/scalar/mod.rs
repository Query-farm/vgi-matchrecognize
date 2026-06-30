//! Scalar functions exposed by the matchrecognize worker.

mod explain;
mod version;

use vgi::Worker;

/// Register every scalar function on the worker.
pub fn register(worker: &mut Worker) {
    worker.register_scalar(version::MrVersion);
    worker.register_scalar(explain::ExplainPattern);
}
