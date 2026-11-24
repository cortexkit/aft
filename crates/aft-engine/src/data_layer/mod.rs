pub use aft_index::data_layer::*;

pub mod checkout {
    pub use aft_index::data_layer::checkout::*;

    #[cfg(feature = "test-hooks")]
    pub mod exactness;
}

pub mod surfaces {
    pub use aft_index::data_layer::surfaces::*;

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) use crate::index_test_binding::*;
}
