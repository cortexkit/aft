// Keep production internals private unless retained higher-layer tests need them.
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
pub use aft_base::live_db_files;
use aft_base::*;

pub mod access_policy;
pub mod db;
pub mod sandbox_profile;
pub mod write_ledger;
