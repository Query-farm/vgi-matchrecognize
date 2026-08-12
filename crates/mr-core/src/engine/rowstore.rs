//! The `RowStore` trait — the Arrow-agnostic data interface the engine reads
//! through. `mr-worker` implements it over Arrow `RecordBatch` columns; tests
//! use the in-memory [`VecRowStore`].

use std::cmp::Ordering;

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

    /// Order two cells of the same column for sorting, with NULLs first or last.
    ///
    /// Sorting calls this O(n log n) times, so it is the one hot path where
    /// materializing a [`Value`] is worth avoiding: the default implementation
    /// below does exactly that and allocates a `String` per comparison for a
    /// VARCHAR key, which measured ~2x slower than a BIGINT key over 1.6M rows.
    /// A store backed by typed columns should override this and compare in place.
    fn cmp_cells(&self, a: usize, b: usize, col: usize, desc: bool, nulls_first: bool) -> Ordering {
        super::valops::cmp_for_sort(&self.cell(a, col), &self.cell(b, col), desc, nulls_first)
    }
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
        let types = headers.iter().map(|(_, t)| t.clone()).collect();
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
        self.types[idx].clone()
    }
    fn cell(&self, row: usize, col: usize) -> Value {
        self.rows[row][col].clone()
    }

    /// Borrow both cells instead of cloning them — the values are already here, so
    /// the default implementation's clone (a `String` allocation for a VARCHAR key,
    /// on every one of the O(n log n) comparisons) is pure waste.
    fn cmp_cells(&self, a: usize, b: usize, col: usize, desc: bool, nulls_first: bool) -> Ordering {
        super::valops::cmp_for_sort(&self.rows[a][col], &self.rows[b][col], desc, nulls_first)
    }
}
