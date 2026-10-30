//! Thread-local work census for the extractor's complexity tests and corpus probe.
//! Counters measure operations, not elapsed time, so busy build machines do not
//! weaken the bounds. Production extraction contains no counter bookkeeping.
use std::cell::RefCell;
use std::ops::{Deref, DerefMut};

#[derive(Clone, Debug, Default)]
pub(crate) struct Work {
    pub whole_file_parses: usize,
    pub macro_ranges: Vec<(usize, usize)>,
    pub child_vectors: usize,
    pub ordinal_visits: usize,
    pub dispatch_visits: usize,
    pub receiver_type_calls: usize,
    pub receiver_function_nodes: usize,
    pub receiver_constructor_scans: usize,
    pub receiver_constructor_nodes: usize,
    pub receiver_index_passes: usize,
    pub receiver_index_nodes: usize,
    pub position_bytes: usize,
    pub call_kind_vectors: usize,
    pub kind_copies: usize,
}

thread_local! {
    static WORK: RefCell<Option<Work>> = const { RefCell::new(None) };
}

pub(crate) fn note(update: impl FnOnce(&mut Work)) {
    WORK.with(|slot| {
        if let Some(work) = slot.borrow_mut().as_mut() {
            update(work);
        }
    });
}

pub(crate) fn measure<T>(run: impl FnOnce() -> T) -> (T, Work) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            WORK.with(|slot| *slot.borrow_mut() = None);
        }
    }
    WORK.with(|slot| *slot.borrow_mut() = Some(Work::default()));
    let _reset = Reset;
    let output = run();
    let work = WORK.with(|slot| slot.borrow_mut().take().unwrap());
    (output, work)
}

/// Count actual parser invocations, including restricted macro-body parses.
pub(crate) struct CountingParser(tree_sitter::Parser);

impl CountingParser {
    pub fn new() -> Self {
        Self(tree_sitter::Parser::new())
    }

    pub fn parse<T: AsRef<[u8]>>(
        &mut self,
        source: T,
        old_tree: Option<&tree_sitter::Tree>,
    ) -> Option<tree_sitter::Tree> {
        note(|work| {
            let ranges = self.0.included_ranges();
            if ranges.is_empty()
                || (ranges.len() == 1
                    && ranges[0].start_byte == 0
                    && ranges[0].end_byte == u32::MAX as usize)
            {
                work.whole_file_parses += 1;
            } else {
                work.macro_ranges
                    .extend(ranges.iter().map(|r| (r.start_byte, r.end_byte)));
            }
        });
        self.0.parse(source, old_tree)
    }
}

impl Deref for CountingParser {
    type Target = tree_sitter::Parser;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for CountingParser {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
