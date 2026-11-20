pub use aft_parse::grammar::*;

#[cfg(all(test, not(feature = "wasm-grammars")))]
pub mod disabled;
#[cfg(all(test, feature = "wasm-grammars"))]
pub mod runtime;
