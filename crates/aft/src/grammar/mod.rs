pub use aft_parse::grammar::*;

#[cfg(all(test, feature = "wasm-grammars"))]
mod runtime;
#[cfg(all(test, not(feature = "wasm-grammars")))]
mod disabled;
