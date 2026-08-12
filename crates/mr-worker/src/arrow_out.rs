//! Build an Arrow `RecordBatch` from mr-core output rows (`Vec<Vec<Value>>`)
//! against the bind-time output schema.

use std::sync::Arc;

use arrow_array::{
    Array, ArrayRef, BooleanArray, Date32Array, Decimal128Array, Float64Array, Int64Array,
    IntervalMonthDayNanoArray, RecordBatch, StringArray, Time32MillisecondArray, Time32SecondArray,
    Time64MicrosecondArray, Time64NanosecondArray, TimestampMicrosecondArray,
    TimestampMillisecondArray, TimestampNanosecondArray, TimestampSecondArray,
};
use arrow_buffer::{IntervalMonthDayNano, OffsetBuffer};
use arrow_schema::{Field, SchemaRef};
use mr_core::plan::OutputColumn;
use mr_core::types::{TimeUnit, Ty};
use mr_core::value::Value;
use vgi_rpc::{Result, RpcError};

fn re(e: impl std::fmt::Display) -> RpcError {
    RpcError::runtime_error(e.to_string())
}

/// Build the output batch. `rows` are row-major; `columns[j].ty` drives the
/// Arrow array type for column `j`.
pub fn build_batch(
    schema: SchemaRef,
    columns: &[OutputColumn],
    rows: &[Vec<Value>],
) -> Result<RecordBatch> {
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(columns.len());
    for (j, col) in columns.iter().enumerate() {
        arrays.push(build_column(&col.ty, rows, j)?);
    }
    RecordBatch::try_new(schema, arrays).map_err(re)
}

fn build_column(ty: &Ty, rows: &[Vec<Value>], j: usize) -> Result<ArrayRef> {
    Ok(match ty {
        Ty::Boolean => Arc::new(
            rows.iter()
                .map(|r| match &r[j] {
                    Value::Bool(b) => Some(*b),
                    Value::Null => None,
                    other => other.as_bool(),
                })
                .collect::<BooleanArray>(),
        ),
        Ty::Int64 => Arc::new(rows.iter().map(|r| to_i64(&r[j])).collect::<Int64Array>()),
        Ty::HugeInt => {
            let v: Vec<Option<i128>> = rows.iter().map(|r| to_i128(&r[j])).collect();
            Arc::new(
                Decimal128Array::from(v)
                    .with_precision_and_scale(38, 0)
                    .map_err(re)?,
            )
        }
        Ty::Double => Arc::new(rows.iter().map(|r| r[j].as_f64()).collect::<Float64Array>()),
        Ty::Decimal(p, s) => {
            let v: Vec<Option<i128>> = rows.iter().map(|r| to_decimal(&r[j], *s)).collect();
            Arc::new(
                Decimal128Array::from(v)
                    .with_precision_and_scale(*p, *s)
                    .map_err(re)?,
            )
        }
        Ty::Varchar => Arc::new(
            rows.iter()
                .map(|r| match &r[j] {
                    Value::Null => None,
                    Value::Str(s) => Some(s.clone()),
                    other => Some(display(other)),
                })
                .collect::<StringArray>(),
        ),
        Ty::Date => Arc::new(
            rows.iter()
                .map(|r| match &r[j] {
                    Value::Date(d) => Some(*d),
                    _ => None,
                })
                .collect::<Date32Array>(),
        ),
        // A list column: flatten every row's elements into one child array and
        // record where each row's slice starts. NULL and the empty list are
        // distinct — an empty match yields an empty list, not NULL.
        Ty::List(elem) => {
            let mut flat: Vec<Vec<Value>> = Vec::new();
            let mut offsets: Vec<i32> = Vec::with_capacity(rows.len() + 1);
            let mut nulls: Vec<bool> = Vec::with_capacity(rows.len());
            let mut acc = 0i32;
            offsets.push(acc);
            for r in rows {
                match &r[j] {
                    Value::List(items) => {
                        nulls.push(true);
                        acc += items.len() as i32;
                        for it in items {
                            flat.push(vec![it.clone()]);
                        }
                    }
                    Value::Null => nulls.push(false),
                    other => {
                        return Err(re(format!("expected a list value, got {other:?}")));
                    }
                }
                offsets.push(acc);
            }
            // Reuse the scalar builders for the child array by presenting the
            // flattened elements as single-column rows.
            let child = build_column(elem, &flat, 0)?;
            Arc::new(
                arrow_array::ListArray::try_new(
                    Arc::new(Field::new("item", child.data_type().clone(), true)),
                    OffsetBuffer::new(offsets.into()),
                    child,
                    Some(nulls.into()),
                )
                .map_err(re)?,
            )
        }
        Ty::Timestamp(u) => build_timestamp(rows, j, *u),
        Ty::Time(u) => build_time(rows, j, *u),
        Ty::Interval => Arc::new(
            rows.iter()
                .map(|r| match &r[j] {
                    Value::Interval(i) => {
                        Some(IntervalMonthDayNano::new(i.months, i.days, i.nanos))
                    }
                    _ => None,
                })
                .collect::<IntervalMonthDayNanoArray>(),
        ),
        Ty::Null => Arc::new(rows.iter().map(|_| None::<String>).collect::<StringArray>()),
    })
}

fn build_timestamp(rows: &[Vec<Value>], j: usize, u: TimeUnit) -> ArrayRef {
    let vals: Vec<Option<i64>> = rows
        .iter()
        .map(|r| match &r[j] {
            Value::Timestamp(v, _) => Some(*v),
            _ => None,
        })
        .collect();
    match u {
        TimeUnit::Second => Arc::new(TimestampSecondArray::from(vals)),
        TimeUnit::Milli => Arc::new(TimestampMillisecondArray::from(vals)),
        TimeUnit::Micro => Arc::new(TimestampMicrosecondArray::from(vals)),
        TimeUnit::Nano => Arc::new(TimestampNanosecondArray::from(vals)),
    }
}

fn build_time(rows: &[Vec<Value>], j: usize, u: TimeUnit) -> ArrayRef {
    let vals: Vec<Option<i64>> = rows
        .iter()
        .map(|r| match &r[j] {
            Value::Time(v, _) => Some(*v),
            _ => None,
        })
        .collect();
    match u {
        TimeUnit::Second => Arc::new(Time32SecondArray::from(
            vals.iter().map(|o| o.map(|v| v as i32)).collect::<Vec<_>>(),
        )),
        TimeUnit::Milli => Arc::new(Time32MillisecondArray::from(
            vals.iter().map(|o| o.map(|v| v as i32)).collect::<Vec<_>>(),
        )),
        TimeUnit::Micro => Arc::new(Time64MicrosecondArray::from(vals)),
        TimeUnit::Nano => Arc::new(Time64NanosecondArray::from(vals)),
    }
}

fn to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        Value::HugeInt(i) => Some(*i as i64),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

fn to_i128(v: &Value) -> Option<i128> {
    match v {
        Value::Int(i) => Some(*i as i128),
        Value::HugeInt(i) => Some(*i),
        _ => v.as_f64().map(|f| f as i128),
    }
}

fn to_decimal(v: &Value, scale: i8) -> Option<i128> {
    match v {
        Value::Decimal(u, s) => Some(rescale(*u, *s, scale)),
        Value::Null => None,
        other => other
            .as_f64()
            .map(|f| (f * 10f64.powi(scale as i32)).round() as i128),
    }
}

fn rescale(unscaled: i128, from: i8, to: i8) -> i128 {
    use std::cmp::Ordering::*;
    match from.cmp(&to) {
        Equal => unscaled,
        Less => unscaled * 10i128.pow((to - from) as u32),
        Greater => unscaled / 10i128.pow((from - to) as u32),
    }
}

fn display(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::HugeInt(i) => i.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Str(s) => s.clone(),
        other => format!("{other:?}"),
    }
}
