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
pub mod db;
pub mod sandbox_profile;
pub mod write_ledger;
