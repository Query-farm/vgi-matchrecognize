//! Structured arguments: `define`, `subset` and `measures` accept a DuckDB `MAP` or
//! `STRUCT` as well as a JSON string.
//!
//! The three of them are maps from a name to something — a pattern variable to its
//! predicate, a union variable to its members, an output column to its measure expression
//! — so writing them as a `MAP` says what they are, and leaves DuckDB to check the syntax
//! instead of the user hand-assembling JSON inside a SQL string:
//!
//! ```sql
//! define := MAP {'DOWN': $$price < PREV(price)$$}
//! define := '{"DOWN": "price < PREV(price)"}'   -- the same thing
//! ```
//!
//! **This is not what solves quoting.** A `MAP`'s values are still SQL string literals, so
//! a predicate containing a string literal still needs its quotes doubled —
//! `MAP {'F': 'outcome = ''fail'''}`. What removes that is dollar-quoting, and it works on
//! either form: `MAP {'F': $$outcome = 'fail'$$}`, or
//! `$${"F": "outcome = 'fail'"}$$`. The map form and the quoting fix are separate wins that
//! compose.
//!
//! Both forms are accepted because they have different callers: a person writing a query by
//! hand wants the `MAP`, while a tool assembling a query wants to serialise one string. So
//! the argument type is declared `any` and the shape is resolved here, at bind time.
//!
//! Everything converts to JSON text and then follows exactly the same path as a
//! hand-written JSON string — one parser, one set of error messages, one set of semantics.
//! `serde_json` is built with `preserve_order`, and a DuckDB `MAP` is an ordered list of
//! entries rather than a hash table, so **key order survives the conversion**. That matters:
//! a `measures` object's key order is its output column order.

use arrow_array::cast::AsArray;
use arrow_array::{Array, ArrayRef};
use arrow_schema::DataType;
use vgi::arguments::Arguments;
use vgi_rpc::{Result, RpcError};

fn ve(e: impl std::fmt::Display) -> RpcError {
    RpcError::value_error(e.to_string())
}

/// Read a named argument that carries a JSON *object* (or the array form of `measures`),
/// written either as a JSON string or as a DuckDB `MAP`/`STRUCT`.
///
/// `Ok(None)` means the argument was absent or NULL. A string is returned verbatim — it is
/// the JSON parser's job to reject a malformed one, so that its error message is the same
/// whichever form was used.
pub fn structured_arg(args: &Arguments, name: &str) -> Result<Option<String>> {
    let Some(array) = args.named(name) else {
        return Ok(None);
    };
    if array.is_empty() || array.is_null(0) {
        return Ok(None);
    }
    match array.data_type() {
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => {
            Ok(to_json(array, 0, name)?.as_str().map(str::to_string))
        }
        _ => Ok(Some(to_json(array, 0, name)?.to_string())),
    }
}

/// One cell of an argument array as JSON.
///
/// Recursive, because the accepted spellings nest: `subset`'s values are lists of names, and
/// the `measures` array form is a list of `{as, expr, type}` structs.
fn to_json(array: &ArrayRef, row: usize, name: &str) -> Result<serde_json::Value> {
    use serde_json::Value;
    if array.is_null(row) {
        return Ok(Value::Null);
    }
    match array.data_type() {
        DataType::Utf8 => Ok(Value::from(array.as_string::<i32>().value(row))),
        DataType::LargeUtf8 => Ok(Value::from(array.as_string::<i64>().value(row))),
        DataType::Utf8View => Ok(Value::from(array.as_string_view().value(row))),
        DataType::Boolean => Ok(Value::from(array.as_boolean().value(row))),

        // A DuckDB MAP arrives as an Arrow map: one struct of (key, value) per entry, in the
        // order the entries were written.
        DataType::Map(..) => {
            let entries = array.as_map().value(row);
            let keys = entries.column(0);
            let values = entries.column(1);
            let mut obj = serde_json::Map::with_capacity(entries.len());
            for i in 0..entries.len() {
                let key = to_json(keys, i, name)?;
                let Value::String(key) = key else {
                    return Err(ve(format!(
                        "match_recognize: '{name}' has a non-string key; a MAP argument must \
                         be keyed by VARCHAR"
                    )));
                };
                obj.insert(key, to_json(values, i, name)?);
            }
            Ok(Value::Object(obj))
        }

        // A STRUCT literal — `{'DOWN': '…'}` — is an object whose keys are field names, so
        // it reads the same as a MAP. Its field order is its declared order.
        DataType::Struct(fields) => {
            let sa = array.as_struct();
            let mut obj = serde_json::Map::with_capacity(fields.len());
            for (i, field) in fields.iter().enumerate() {
                obj.insert(field.name().clone(), to_json(sa.column(i), row, name)?);
            }
            Ok(Value::Object(obj))
        }

        DataType::List(_) => json_array(&array.as_list::<i32>().value(row), name),
        DataType::LargeList(_) => json_array(&array.as_list::<i64>().value(row), name),

        // Numbers: measures' explicit `type` is text and predicates are text, so a number can
        // only turn up inside a hand-built structure. Carry it rather than refusing it.
        dt if dt.is_integer() => Ok(Value::from(
            arrow_cast::cast(array.as_ref(), &DataType::Int64)
                .map_err(|e| ve(format!("match_recognize: '{name}': {e}")))?
                .as_primitive::<arrow_array::types::Int64Type>()
                .value(row),
        )),
        dt if dt.is_floating() => Ok(Value::from(
            arrow_cast::cast(array.as_ref(), &DataType::Float64)
                .map_err(|e| ve(format!("match_recognize: '{name}': {e}")))?
                .as_primitive::<arrow_array::types::Float64Type>()
                .value(row),
        )),

        other => Err(ve(format!(
            "match_recognize: '{name}' must be a JSON string, a MAP or a STRUCT, not {other}"
        ))),
    }
}

fn json_array(values: &ArrayRef, name: &str) -> Result<serde_json::Value> {
    let mut out = Vec::with_capacity(values.len());
    for i in 0..values.len() {
        out.push(to_json(values, i, name)?);
    }
    Ok(serde_json::Value::Array(out))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::builder::{ListBuilder, MapBuilder, StringBuilder};
    use arrow_array::{ArrayRef, StringArray, StructArray};
    use arrow_schema::{DataType, Field, Fields};
    use vgi::arguments::Arguments;

    use super::structured_arg;

    fn args_with(name: &str, array: ArrayRef) -> Arguments {
        let mut args = Arguments::default();
        args.named.insert(name.to_string(), array);
        args
    }

    /// A JSON string is handed through untouched, so its diagnostics are unchanged.
    #[test]
    fn a_json_string_passes_through_verbatim() {
        let json = r#"{"DOWN": "price < PREV(price)"}"#;
        let args = args_with("define", Arc::new(StringArray::from(vec![json])));
        assert_eq!(
            structured_arg(&args, "define").unwrap().as_deref(),
            Some(json)
        );
    }

    /// Even a malformed one: rejecting it is the JSON parser's job, so that the error a user
    /// sees does not depend on which spelling they used.
    #[test]
    fn a_malformed_string_is_not_rejected_here() {
        let args = args_with("define", Arc::new(StringArray::from(vec!["{not json"])));
        assert_eq!(
            structured_arg(&args, "define").unwrap().as_deref(),
            Some("{not json")
        );
    }

    #[test]
    fn absent_and_null_are_none() {
        let args = Arguments::default();
        assert!(structured_arg(&args, "define").unwrap().is_none());
        let null = args_with("define", Arc::new(StringArray::from(vec![None::<&str>])));
        assert!(structured_arg(&null, "define").unwrap().is_none());
    }

    /// `MAP {'A': '…', 'B': '…'}` becomes the equivalent JSON object — **in written order**,
    /// which is what makes a `measures` map's order its output column order.
    #[test]
    fn a_map_becomes_a_json_object_in_written_order() {
        let mut b = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
        // Deliberately not alphabetical: the output must follow insertion, not sorting.
        for (k, v) in [("z_first", "COUNT(*)"), ("a_second", "LAST(price)")] {
            b.keys().append_value(k);
            b.values().append_value(v);
        }
        b.append(true).unwrap();
        let args = args_with("measures", Arc::new(b.finish()));
        assert_eq!(
            structured_arg(&args, "measures").unwrap().as_deref(),
            Some(r#"{"z_first":"COUNT(*)","a_second":"LAST(price)"}"#)
        );
    }

    /// `subset := MAP {'U': ['A', 'B']}` — the values are lists, so the conversion recurses.
    #[test]
    fn a_map_of_lists_becomes_an_object_of_arrays() {
        let mut b = MapBuilder::new(
            None,
            StringBuilder::new(),
            ListBuilder::new(StringBuilder::new()),
        );
        b.keys().append_value("U");
        b.values().append_value([Some("A"), Some("B")]);
        b.append(true).unwrap();
        let args = args_with("subset", Arc::new(b.finish()));
        assert_eq!(
            structured_arg(&args, "subset").unwrap().as_deref(),
            Some(r#"{"U":["A","B"]}"#)
        );
    }

    /// A STRUCT literal reads like a MAP, keyed by field name in declared order.
    #[test]
    fn a_struct_becomes_a_json_object() {
        let fields: Fields = vec![
            Field::new("DOWN", DataType::Utf8, true),
            Field::new("UP", DataType::Utf8, true),
        ]
        .into();
        let sa = StructArray::new(
            fields,
            vec![
                Arc::new(StringArray::from(vec!["price < PREV(price)"])) as ArrayRef,
                Arc::new(StringArray::from(vec!["price > PREV(price)"])) as ArrayRef,
            ],
            None,
        );
        let args = args_with("define", Arc::new(sa));
        assert_eq!(
            structured_arg(&args, "define").unwrap().as_deref(),
            Some(r#"{"DOWN":"price < PREV(price)","UP":"price > PREV(price)"}"#)
        );
    }

    /// The type an argument arrived as belongs in the error, since the whole point of
    /// accepting several is that a user cannot tell which one the function got.
    #[test]
    fn an_unusable_type_names_itself() {
        let args = args_with(
            "define",
            Arc::new(arrow_array::BooleanArray::from(vec![true])) as ArrayRef,
        );
        // Booleans are convertible, so this one succeeds as JSON `true`; a date is not.
        assert_eq!(
            structured_arg(&args, "define").unwrap().as_deref(),
            Some("true")
        );

        let args = args_with(
            "define",
            Arc::new(arrow_array::Date32Array::from(vec![19_000])) as ArrayRef,
        );
        let err = structured_arg(&args, "define").unwrap_err().to_string();
        assert!(
            err.contains("must be a JSON string, a MAP or a STRUCT") && err.contains("Date32"),
            "the error should name the offending type: {err}"
        );
    }
}
