//! PATTERN front-end: lexer, parser, and the backtracking-VM compiler.

pub mod compile;
pub mod lexer;
pub mod parser;

pub use compile::{compile, explain, Program};
pub use parser::{parse, Anchor, Pattern};
