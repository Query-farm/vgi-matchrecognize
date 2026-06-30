//! A concrete [`RowStore`] over a buffered Arrow `RecordBatch` (zero-copy cell
//! access over the relation's columns).

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Date64Type, Decimal128Type, Float32Type, Float64Type, Int16Type, Int32Type,
    Int64Type, Int8Type, IntervalMonthDayNanoType, Time32MillisecondType, Time32SecondType,
    Time64MicrosecondType, Time64NanosecondType, TimestampMicrosecondType,
    TimestampMillisecondType, TimestampNanosecondType, TimestampSecondType, UInt16Type, UInt32Type,
    UInt64Type, UInt8Type,
};
use arrow_array::{Array, RecordBatch};
use arrow_schema::{DataType, IntervalUnit, TimeUnit as ArrowTimeUnit};
use mr_core::engine::RowStore;
use mr_core::types::{TimeUnit, Ty};
use mr_core::value::{Interval, Value};

use crate::schema::arrow_to_ty;

/// A [`RowStore`] backed by one (concatenated) `RecordBatch`.
pub struct BatchRowStore {
    batch: RecordBatch,
    types: Vec<Ty>,
}

impl BatchRowStore {
    /// Build a store over `batch`, precomputing each column's core type.
    pub fn new(batch: RecordBatch) -> Self {
        let types = batch
            .schema()
            .fields()
            .iter()
            .map(|f| arrow_to_ty(f.data_type()).unwrap_or(Ty::Varchar))
            .collect();
        BatchRowStore { batch, types }
    }
}

impl RowStore for BatchRowStore {
    fn num_rows(&self) -> usize {
        self.batch.num_rows()
    }

    fn col_index(&self, name: &str) -> Option<usize> {
        self.batch
            .schema()
            .fields()
            .iter()
            .position(|f| f.name().eq_ignore_ascii_case(name))
    }

    fn col_ty(&self, idx: usize) -> Ty {
        self.types[idx]
    }

    fn cell(&self, row: usize, col: usize) -> Value {
        let arr = self.batch.column(col);
        if arr.is_null(row) {
            return Value::Null;
        }
        cell_value(arr, row)
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
