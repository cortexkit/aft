//! Claude tokenizer matching `ai-tokenizer`'s lookup/BPE hybrid encoding.

mod claude;

pub use claude::{count_tokens, encode};
