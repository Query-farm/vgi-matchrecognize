//! [`RowBuf`] — the output rows of a batch, row-major in one allocation.
//!
//! The emit path used to hand back a `Vec<Value>` per output row, collected into a
//! `Vec<Vec<Value>>`. That is a 24-byte header plus a separate allocation for every
//! row, and `Value` is 32 bytes wide (the `i128` variants set the alignment), so a
//! three-measure row cost ~136 bytes and one `malloc`/`free` pair to carry 96 bytes
//! of payload. On an 8M-row ALL ROWS result that was over a gigabyte of resident
//! memory and 8M allocations.
//!
//! Here the values live in one growable `Vec<Value>` with a fixed column stride, so
//! a row costs exactly its cells and appending one is a `push` per cell. The buffer
//! is reused across output batches, which means the steady state is zero allocation
//! per row.

use crate::value::Value;

/// A row-major buffer of output values, `ncols` cells per row.
#[derive(Debug, Clone, Default)]
pub struct RowBuf {
    vals: Vec<Value>,
    ncols: usize,
}

impl RowBuf {
    /// An empty buffer for rows of `ncols` columns.
    pub fn new(ncols: usize) -> RowBuf {
        RowBuf {
            vals: Vec::new(),
            ncols,
        }
    }

    /// Columns per row.
    pub fn ncols(&self) -> usize {
        self.ncols
    }

    /// Complete rows held.
    ///
    /// Zero columns is a real shape — `rows := 'one'` with no partition keys and no
    /// measures — and it holds no rows however many times it is emitted.
    pub fn len(&self) -> usize {
        self.vals.len().checked_div(self.ncols).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Row `i`'s cells.
    pub fn row(&self, i: usize) -> &[Value] {
        &self.vals[i * self.ncols..(i + 1) * self.ncols]
    }

    /// Every row in order.
    pub fn iter(&self) -> impl Iterator<Item = &[Value]> {
        self.vals.chunks_exact(self.ncols.max(1))
    }

    /// Append one cell. A row is complete once `ncols` of them have been pushed;
    /// [`RowBuf::end_row`] checks that in debug builds.
    pub fn push(&mut self, v: Value) {
        self.vals.push(v);
    }

    /// Mark the end of a row.
    ///
    /// Purely an assertion: a measure list and the pushes that emit it are written
    /// in two different places, and a mismatch would silently shift every later
    /// row's columns rather than fail.
    pub fn end_row(&self) {
        debug_assert_eq!(
            self.vals.len() % self.ncols.max(1),
            0,
            "output row has the wrong number of cells"
        );
    }

    /// Drop every row, keeping the allocation.
    pub fn clear(&mut self) {
        self.vals.clear();
    }

    /// How many cells have been pushed — the mark [`RowBuf::truncate_cells`] takes.
    pub(crate) fn cells(&self) -> usize {
        self.vals.len()
    }

    /// Roll back to a mark, discarding a partially emitted row.
    pub(crate) fn truncate_cells(&mut self, cells: usize) {
        self.vals.truncate(cells);
    }

    /// Copy out as one `Vec` per row.
    ///
    /// The shape [`crate::plan::Plan::run`] hands back — convenient for tests and
    /// for callers that materialize everything anyway. The streaming path in the
    /// worker reads [`RowBuf::row`] directly and never pays this.
    pub fn to_rows(&self) -> Vec<Vec<Value>> {
        self.iter().map(|r| r.to_vec()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_addressable_and_reusable() {
        let mut b = RowBuf::new(2);
        assert!(b.is_empty());
        b.push(Value::Int(1));
        b.push(Value::Int(2));
        b.end_row();
        b.push(Value::Int(3));
        b.push(Value::Int(4));
        b.end_row();
        assert_eq!(b.len(), 2);
        assert_eq!(b.row(1), &[Value::Int(3), Value::Int(4)]);
        assert_eq!(
            b.to_rows(),
            vec![
                vec![Value::Int(1), Value::Int(2)],
                vec![Value::Int(3), Value::Int(4)],
            ]
        );
        b.clear();
        assert_eq!(b.len(), 0);
    }

    #[test]
    fn a_partial_row_rolls_back_to_the_mark() {
        let mut b = RowBuf::new(3);
        b.push(Value::Int(1));
        b.push(Value::Int(2));
        b.push(Value::Int(3));
        b.end_row();
        let mark = b.cells();
        b.push(Value::Int(9));
        b.truncate_cells(mark);
        assert_eq!(b.len(), 1);
    }

    /// Zero-column output (`rows := 'one'` with no partition keys and no measures)
    /// must not divide by zero.
    #[test]
    fn zero_columns_is_not_a_division_by_zero() {
        let b = RowBuf::new(0);
        assert_eq!(b.len(), 0);
        assert!(b.is_empty());
    }
}
