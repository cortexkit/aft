#![allow(
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::double_ended_iterator_last,
    clippy::int_plus_one,
    clippy::large_enum_variant,
    clippy::len_without_is_empty,
    clippy::let_and_return,
    clippy::manual_contains,
    clippy::manual_pattern_char_comparison,
    clippy::manual_repeat_n,
    clippy::manual_strip,
    clippy::manual_unwrap_or_default,
    clippy::map_clone,
    clippy::iter_kv_map,
    clippy::needless_borrow,
    clippy::needless_borrows_for_generic_args,
    clippy::needless_range_loop,
    clippy::new_without_default,
    clippy::obfuscated_if_else,
    clippy::ptr_arg,
    clippy::question_mark,
    clippy::same_item_push,
    clippy::should_implement_trait,
    clippy::single_match,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::unnecessary_sort_by,
    clippy::unnecessary_cast,
    clippy::unnecessary_lazy_evaluations,
    clippy::unnecessary_map_or
)]

// Expose these items only in tests or when the test-support feature is enabled.
macro_rules! test_support_visibility {
    ($(#[$attribute:meta])* $visibility:vis $keyword:ident $($item:tt)*) => {
        #[cfg(any(test, feature = "test-support"))]
        $(#[$attribute])*
        pub $keyword $($item)*

        #[cfg(not(any(test, feature = "test-support")))]
        $(#[$attribute])*
        $visibility $keyword $($item)*
    };
}

// Lower module aliases retain the implementation's existing paths.
use aft_base::*;

#[cfg(test)]
#[global_allocator]
static TEST_ALLOCATOR: aft_base::test_allocations::CountingAllocator =
    aft_base::test_allocations::CountingAllocator;

pub mod ast_grep_hints;
pub mod ast_grep_lang;
pub mod callgraph;
pub mod callgraph_extractor;
pub mod callgraph_store;
pub mod calls;
pub mod extract;
pub mod fuzzy_match;
pub mod grammar;
pub mod imports;
pub mod indent;
pub mod line_matcher;
pub mod luau;
pub mod parser;
mod systemverilog;
