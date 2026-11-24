pub trait Host: Send + Sync {
    fn context_ceiling(&self) -> usize {
        4096
    }

    fn held_calls(&self) -> usize;
}

pub struct Runner;

impl Runner {
    pub fn new() -> Self {
        Self
    }
}

impl Host for Runner {
    fn held_calls(&self) -> usize {
        0
    }
}

mod nested {
    pub trait Nested {
        fn default_method(&self) -> bool {
            true
        }

        fn required_method(&self);
    }

    pub struct NestedRunner;

    impl NestedRunner {
        pub fn create() -> Self {
            Self
        }
    }
}
