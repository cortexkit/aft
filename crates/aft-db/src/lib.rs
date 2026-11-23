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

// Module aliases preserve the implementation's existing module paths.
use aft_base::*;

pub mod access_policy;
#[doc(hidden)]
pub mod artifact_publish_epoch;
#[doc(hidden)]
pub mod build_breaker;
pub mod db;
#[doc(hidden)]
pub mod path_filters;
#[doc(hidden)]
pub mod persisted_format;
#[doc(hidden)]
pub mod read_fence;
#[doc(hidden)]
pub mod reader_floor;
#[doc(hidden)]
pub mod root_identity;
pub mod sandbox_profile;
#[doc(hidden)]
pub mod scoped_key;
#[doc(hidden)]
pub mod standing_publication_lease;
#[doc(hidden)]
pub mod standing_roots;
pub mod write_ledger;
