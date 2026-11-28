#[allow(dead_code)]
pub const LETTERS: &str = "abc";
static ALPHABET: &[u8] = b"abc";
pub type Alphabet = &'static [u8];

mod outer {
    mod inner {
        pub(super) const MODULE_COUNT: usize = 3;
        static MODULE_TABLE: [u8; 3] = [1, 2, 3];
        type ModuleIndex = usize;

        pub trait Service {
            const TRAIT_LIMIT: usize;
            type TraitOutput: Copy;
            // Associated statics are not valid Rust, but the syntax tree accepts them.
            static TRAIT_TABLE: [u8; 1] = [4];
        }

        pub struct Widget;
        impl Service for Widget {
            const TRAIT_LIMIT: usize = 1;
            type TraitOutput = u8;
            static IMPL_TABLE: [u8; 1] = [5];
        }

        impl Widget {
            pub const DEFAULT_COUNT: usize = 2;
        }
    }
}

const _: () = ();
fn unrelated_function() {
    const LOCAL_COUNT: usize = 1;
    static LOCAL_TABLE: [u8; 1] = [6];
    type LocalIndex = usize;
}
