//! The static type system (`Ty`) and bind-time type inference.

pub mod infer;
pub mod ty;

pub use infer::{infer, BindSchema};
pub use ty::{TimeUnit, Ty};
