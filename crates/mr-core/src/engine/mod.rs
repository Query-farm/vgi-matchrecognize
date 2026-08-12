//! The matching engine: row store, evaluation frame, value operations, and the
//! backtracking matcher.

pub mod aggmemo;
pub mod bindindex;
pub mod eval;
pub mod matcher;
pub mod rowstore;
pub mod scalar;
pub mod valops;

pub use aggmemo::AggMemo;
pub use bindindex::BindIndex;
pub use eval::{Bind, Frame, SubsetMap};
pub use matcher::{AfterSkip, Match, Matcher};
pub use rowstore::{RowStore, VecRowStore};
