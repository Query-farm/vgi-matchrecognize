//! The `RowStore` trait — the Arrow-agnostic data interface the engine reads
//! through. `mr-worker` implements it over Arrow `RecordBatch` columns; tests
//! use the in-memory [`VecRowStore`].

use crate::types::Ty;
use crate::value::Value;

/// Indexed, typed cell access over a set of rows. All algorithms in the engine
/// read data exclusively through this trait, so they stay free of Arrow/VGI.
pub trait RowStore {
    /// Number of rows held.
    fn num_rows(&self) -> usize;
    /// Column index for `name` (case-insensitive), if present.
    fn col_index(&self, name: &str) -> Option<usize>;
    /// Static type of column `idx`.
    fn col_ty(&self, idx: usize) -> Ty;
    /// The value of cell `(row, col)` (a clone).
    fn cell(&self, row: usize, col: usize) -> Value;
}

/// A trivial in-memory `RowStore` over `Vec<Vec<Value>>` (row-major), for tests
/// and the engine's own unit tests.
#[derive(Debug, Clone)]
pub struct VecRowStore {
    names: Vec<String>,
    types: Vec<Ty>,
    rows: Vec<Vec<Value>>,
}

impl VecRowStore {
    /// Build a store from column `(name, ty)` headers and row-major data.
    pub fn new(headers: Vec<(&str, Ty)>, rows: Vec<Vec<Value>>) -> Self {
        let names = headers.iter().map(|(n, _)| n.to_string()).collect();
        let types = headers.iter().map(|(_, t)| *t).collect();
        VecRowStore { names, types, rows }
    }
}

impl RowStore for VecRowStore {
    fn num_rows(&self) -> usize {
        self.rows.len()
    }
    fn col_index(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n.eq_ignore_ascii_case(name))
    }
    fn col_ty(&self, idx: usize) -> Ty {
        self.types[idx]
    }
    fn cell(&self, row: usize, col: usize) -> Value {
        self.rows[row][col].clone()
    }
}
