//! A concrete [`RowStore`] over the buffered Arrow batches (zero-copy cell access
//! over the relation's columns).
//!
//! The batches are held **as they arrived** rather than concatenated into one:
//! `concat_batches` would need the source batches and the merged copy alive
//! simultaneously, doubling peak memory for no benefit, since the engine only
//! ever reads individual cells by row index.

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Date64Type, Decimal128Type, Float32Type, Float64Type, Int16Type, Int32Type,
    Int64Type, Int8Type, IntervalMonthDayNanoType, Time32MillisecondType, Time32SecondType,
    Time64MicrosecondType, Time64NanosecondType, TimestampMicrosecondType,
    TimestampMillisecondType, TimestampNanosecondType, TimestampSecondType, UInt16Type, UInt32Type,
    UInt64Type, UInt8Type,
};
use arrow_array::{Array, RecordBatch};
use arrow_schema::{DataType, IntervalUnit, SchemaRef, TimeUnit as ArrowTimeUnit};
use mr_core::engine::RowStore;
use mr_core::types::{TimeUnit, Ty};
use mr_core::value::{Interval, Value};

use crate::schema::arrow_to_ty;

/// A [`RowStore`] over a sequence of buffered `RecordBatch`es, addressed as one
/// contiguous row space.
pub struct BatchRowStore {
    batches: Vec<RecordBatch>,
    /// Running row offset of each batch, plus the grand total as a final entry,
    /// so a row index maps to `(batch, row-in-batch)` by binary search.
    offsets: Vec<usize>,
    schema: SchemaRef,
    types: Vec<Ty>,
}

impl BatchRowStore {
    /// Build a store over `batches`, precomputing each column's core type and the
    /// per-batch row offsets. `schema` is the relation's schema (needed when
    /// `batches` is empty).
    pub fn new(schema: SchemaRef, batches: Vec<RecordBatch>) -> Self {
        let types = schema
            .fields()
            .iter()
            .map(|f| arrow_to_ty(f.data_type()).unwrap_or(Ty::Varchar))
            .collect();
        let mut offsets = Vec::with_capacity(batches.len() + 1);
        let mut acc = 0usize;
        for b in &batches {
            offsets.push(acc);
            acc += b.num_rows();
        }
        offsets.push(acc);
        BatchRowStore {
            batches,
            offsets,
            schema,
            types,
        }
    }

    /// Build a store over a single batch (used by the unit tests).
    #[cfg(test)]
    pub fn single(batch: RecordBatch) -> Self {
        let schema = batch.schema();
        Self::new(schema, vec![batch])
    }

    /// Map a global row index to `(batch index, row within that batch)`.
    fn locate(&self, row: usize) -> (usize, usize) {
        // `offsets` is sorted and ends with the total row count. partition_point
        // gives the first offset strictly greater than `row`; the batch is the
        // one before it.
        let b = self.offsets.partition_point(|&o| o <= row) - 1;
        (b, row - self.offsets[b])
    }
}

impl RowStore for BatchRowStore {
    fn num_rows(&self) -> usize {
        *self.offsets.last().unwrap_or(&0)
    }

    fn col_index(&self, name: &str) -> Option<usize> {
        self.schema
            .fields()
            .iter()
            .position(|f| f.name().eq_ignore_ascii_case(name))
    }

    fn col_ty(&self, idx: usize) -> Ty {
        self.types[idx].clone()
    }

    fn cell(&self, row: usize, col: usize) -> Value {
        let (b, r) = self.locate(row);
        let arr = self.batches[b].column(col);
        if arr.is_null(r) {
            return Value::Null;
        }
        cell_value(arr, r)
    }
}

fn unit(u: &ArrowTimeUnit) -> TimeUnit {
    match u {
        ArrowTimeUnit::Second => TimeUnit::Second,
        ArrowTimeUnit::Millisecond => TimeUnit::Milli,
        ArrowTimeUnit::Microsecond => TimeUnit::Micro,
        ArrowTimeUnit::Nanosecond => TimeUnit::Nano,
    }
}

fn cell_value(arr: &dyn Array, row: usize) -> Value {
    match arr.data_type() {
        DataType::Boolean => Value::Bool(arr.as_boolean().value(row)),
        DataType::Int8 => Value::Int(arr.as_primitive::<Int8Type>().value(row) as i64),
        DataType::Int16 => Value::Int(arr.as_primitive::<Int16Type>().value(row) as i64),
        DataType::Int32 => Value::Int(arr.as_primitive::<Int32Type>().value(row) as i64),
        DataType::Int64 => Value::Int(arr.as_primitive::<Int64Type>().value(row)),
        DataType::UInt8 => Value::Int(arr.as_primitive::<UInt8Type>().value(row) as i64),
        DataType::UInt16 => Value::Int(arr.as_primitive::<UInt16Type>().value(row) as i64),
        DataType::UInt32 => Value::Int(arr.as_primitive::<UInt32Type>().value(row) as i64),
        DataType::UInt64 => Value::Int(arr.as_primitive::<UInt64Type>().value(row) as i64),
        DataType::Float32 => Value::Double(arr.as_primitive::<Float32Type>().value(row) as f64),
        DataType::Float64 => Value::Double(arr.as_primitive::<Float64Type>().value(row)),
        DataType::Utf8 => Value::Str(arr.as_string::<i32>().value(row).to_string()),
        DataType::LargeUtf8 => Value::Str(arr.as_string::<i64>().value(row).to_string()),
        DataType::Utf8View => Value::Str(arr.as_string_view().value(row).to_string()),
        DataType::Date32 => Value::Date(arr.as_primitive::<Date32Type>().value(row)),
        DataType::Date64 => {
            Value::Date((arr.as_primitive::<Date64Type>().value(row) / 86_400_000) as i32)
        }
        DataType::Timestamp(u, _) => {
            let v = match u {
                ArrowTimeUnit::Second => arr.as_primitive::<TimestampSecondType>().value(row),
                ArrowTimeUnit::Millisecond => {
                    arr.as_primitive::<TimestampMillisecondType>().value(row)
                }
                ArrowTimeUnit::Microsecond => {
                    arr.as_primitive::<TimestampMicrosecondType>().value(row)
                }
                ArrowTimeUnit::Nanosecond => {
                    arr.as_primitive::<TimestampNanosecondType>().value(row)
                }
            };
            Value::Timestamp(v, unit(u))
        }
        DataType::Time32(u) => {
            let v = match u {
                ArrowTimeUnit::Second => arr.as_primitive::<Time32SecondType>().value(row) as i64,
                _ => arr.as_primitive::<Time32MillisecondType>().value(row) as i64,
            };
            Value::Time(v, unit(u))
        }
        DataType::Time64(u) => {
            let v = match u {
                ArrowTimeUnit::Nanosecond => arr.as_primitive::<Time64NanosecondType>().value(row),
                _ => arr.as_primitive::<Time64MicrosecondType>().value(row),
            };
            Value::Time(v, unit(u))
        }
        DataType::Decimal128(_, s) => {
            Value::Decimal(arr.as_primitive::<Decimal128Type>().value(row), *s)
        }
        DataType::Interval(IntervalUnit::MonthDayNano) => {
            let v = arr.as_primitive::<IntervalMonthDayNanoType>().value(row);
            Value::Interval(Interval {
                months: v.months,
                days: v.days,
                nanos: v.nanoseconds,
            })
        }
        _ => Value::Null,
    }
}
