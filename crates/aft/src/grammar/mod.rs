pub use aft_parse::grammar::*;

#[cfg(all(test, not(feature = "wasm-grammars")))]
mod disabled;
#[cfg(all(test, feature = "wasm-grammars"))]
mod runtime;
