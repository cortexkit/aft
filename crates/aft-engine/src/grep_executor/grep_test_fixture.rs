use crate::pattern_compile::{compile, CompileOpts, CompileResult, CompiledPattern};

pub fn literal(pattern: &str) -> CompiledPattern {
    match compile(
        pattern,
        CompileOpts {
            literal: true,
            ..CompileOpts::default()
        },
    ) {
        CompileResult::Ok(pattern) => pattern,
        other => panic!("compile literal: {other:?}"),
    }
}
