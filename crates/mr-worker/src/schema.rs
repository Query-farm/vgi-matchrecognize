//! Bridge between mr-core's `Ty` and Arrow `DataType`, plus the `BindSchema`
//! built from the input relation's Arrow schema at bind time.

use arrow_schema::{DataType, Field, SchemaRef, TimeUnit as ArrowTimeUnit};
use mr_core::types::{BindSchema, TimeUnit, Ty};

/// Map an Arrow input column type to a core [`Ty`] (the leaf types inference
/// reads). Unsupported types return `None` (the column can't be referenced).
pub fn arrow_to_ty(dt: &DataType) -> Option<Ty> {
    use DataType::*;
    Some(match dt {
        Boolean => Ty::Boolean,
        Int8 | Int16 | Int32 | Int64 | UInt8 | UInt16 | UInt32 | UInt64 => Ty::Int64,
        Float16 | Float32 | Float64 => Ty::Double,
        Utf8 | LargeUtf8 | Utf8View => Ty::Varchar,
        Date32 | Date64 => Ty::Date,
        Timestamp(u, _) => Ty::Timestamp(from_arrow_unit(u)),
        Time32(u) | Time64(u) => Ty::Time(from_arrow_unit(u)),
        Decimal128(p, s) => Ty::Decimal(*p, *s),
        Interval(_) | Duration(_) => Ty::Interval,
        List(f) | LargeList(f) => Ty::List(Box::new(arrow_to_ty(f.data_type())?)),
        _ => return None,
    })
}

/// Map a core [`Ty`] to the Arrow `DataType` emitted in the output schema.
pub fn ty_to_arrow(ty: &Ty) -> DataType {
    match ty {
        Ty::Boolean => DataType::Boolean,
        Ty::Int64 => DataType::Int64,
        Ty::HugeInt => DataType::Decimal128(38, 0),
        Ty::Double => DataType::Float64,
        Ty::Decimal(p, s) => DataType::Decimal128(*p, *s),
        Ty::Varchar => DataType::Utf8,
        Ty::Date => DataType::Date32,
        Ty::Timestamp(u) => DataType::Timestamp(to_arrow_unit(*u), None),
        Ty::Time(u) => match u {
            TimeUnit::Second | TimeUnit::Milli => DataType::Time32(to_arrow_unit(*u)),
            TimeUnit::Micro | TimeUnit::Nano => DataType::Time64(to_arrow_unit(*u)),
        },
        Ty::Interval => DataType::Interval(arrow_schema::IntervalUnit::MonthDayNano),
        // DuckDB reads an Arrow list as `elem[]`; the child field is nullable.
        Ty::List(elem) => DataType::List(std::sync::Arc::new(Field::new(
            "item",
            ty_to_arrow(elem),
            true,
        ))),
        // Should never reach output; fall back to a nullable Utf8.
        Ty::Null => DataType::Utf8,
    }
}

fn from_arrow_unit(u: &ArrowTimeUnit) -> TimeUnit {
    match u {
        ArrowTimeUnit::Second => TimeUnit::Second,
        ArrowTimeUnit::Millisecond => TimeUnit::Milli,
        ArrowTimeUnit::Microsecond => TimeUnit::Micro,
        ArrowTimeUnit::Nanosecond => TimeUnit::Nano,
    }
}

fn to_arrow_unit(u: TimeUnit) -> ArrowTimeUnit {
    match u {
        TimeUnit::Second => ArrowTimeUnit::Second,
        TimeUnit::Milli => ArrowTimeUnit::Millisecond,
        TimeUnit::Micro => ArrowTimeUnit::Microsecond,
        TimeUnit::Nano => ArrowTimeUnit::Nanosecond,
    }
}

/// A [`BindSchema`] over the input relation's Arrow schema plus the pattern's
/// variable set, used by mr-core's type inference at bind time.
pub struct ArrowBindSchema {
    schema: SchemaRef,
    variables: Vec<String>,
}

impl ArrowBindSchema {
    pub fn new(schema: SchemaRef, variables: Vec<String>) -> Self {
        ArrowBindSchema {
            schema,
            variables: variables.iter().map(|v| v.to_ascii_uppercase()).collect(),
        }
    }
}

impl BindSchema for ArrowBindSchema {
    fn col_ty(&self, name: &str) -> Option<Ty> {
        for f in self.schema.fields() {
            if f.name().eq_ignore_ascii_case(name) {
                return arrow_to_ty(f.data_type());
            }
        }
        None
    }
    fn is_variable(&self, name: &str) -> bool {
        self.variables.iter().any(|v| v.eq_ignore_ascii_case(name))
    }
}

/// A nullable output `Field` for a column.
pub fn output_field(name: &str, ty: &Ty) -> Field {
    Field::new(name, ty_to_arrow(ty), true)
}
