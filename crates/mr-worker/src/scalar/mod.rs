//! Scalar functions exposed by the matchrecognize worker.

mod explain;

use vgi::Worker;

/// Register every scalar function on the worker.
pub fn register(worker: &mut Worker) {
    worker.register_scalar(explain::ExplainPattern);
}
