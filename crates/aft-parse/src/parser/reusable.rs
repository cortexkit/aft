use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::size_of;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use tree_sitter::Tree;

#[cfg(any(test, feature = "test-support"))]
use super::work_counters;
#[cfg(feature = "test-hooks")]
use super::SEARCH_PARSE_COUNT;
use super::{
    initialize_parser, markdown_too_deep_to_parse, parse_with_parser, AftError, LangId, Parser,
    MARKDOWN_TOO_DEEP,
};

type Parsers = HashMap<LangId, Parser>;

#[derive(Clone, Copy, Default)]
struct PoolTotals {
    threads: usize,
    parsers: usize,
}

static TOTALS: Mutex<PoolTotals> = Mutex::new(PoolTotals {
    threads: 0,
    parsers: 0,
});

pub struct ParserPool {
    parsers: RefCell<Parsers>,
    totals: &'static Mutex<PoolTotals>,
}

impl ParserPool {
    fn new(totals: &'static Mutex<PoolTotals>) -> Self {
        Self {
            parsers: RefCell::new(HashMap::new()),
            totals,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn borrow(&self) -> std::cell::Ref<'_, Parsers> {
        self.parsers.borrow()
    }

    pub(super) fn edit<T>(&self, edit: impl FnOnce(&mut Parsers) -> T) -> T {
        struct Update<'a> {
            pool: &'a ParserPool,
            before: usize,
            parsers: std::cell::RefMut<'a, Parsers>,
        }
        impl Drop for Update<'_> {
            fn drop(&mut self) {
                self.pool.record_len_change(self.before, self.parsers.len());
            }
        }
        let parsers = self.parsers.borrow_mut();
        let mut update = Update {
            pool: self,
            before: parsers.len(),
            parsers,
        };
        edit(&mut update.parsers)
    }

    fn record_len_change(&self, before: usize, after: usize) {
        if before == after {
            return;
        }
        // Reusing the same set of languages needs no shared lock. Publish only
        // growth, cancellation that discards a parser, and thread exit.
        let mut totals = self
            .totals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        totals.parsers = totals.parsers - before + after;
        totals.threads = totals.threads - usize::from(before != 0) + usize::from(after != 0);
    }
}

impl Drop for ParserPool {
    fn drop(&mut self) {
        let retained = self.parsers.get_mut().len();
        self.record_len_change(retained, 0);
    }
}

thread_local! {
    // Mutable parsers stay on their owning worker thread.
    pub static REUSABLE_PARSERS: ParserPool = ParserPool::new(&TOTALS);
}

pub fn reusable_parser_pools_memory() -> aft_base::memory::MemoryEstimate {
    let totals = *TOTALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    totals.estimate()
}

impl PoolTotals {
    fn estimate(self) -> aft_base::memory::MemoryEstimate {
        let bytes = self
            .threads
            .saturating_mul(size_of::<ParserPool>())
            .saturating_add(self.parsers.saturating_mul(size_of::<(LangId, Parser)>()));
        aft_base::memory::MemoryEstimate::partial(bytes as u64)
            .count("threads_with_parsers", self.threads)
            .count("parsers", self.parsers)
            .gap("native_tree_sitter_parser_allocations")
            .gap("hash_table_spare_capacity_and_empty_thread_pools")
            .gap("other_parser_instances_and_retained_trees")
    }
}

pub fn parse_source_with_parser_deadline(
    path: &Path,
    source: &str,
    lang: LangId,
    deadline: Option<Duration>,
) -> Result<Option<Tree>, AftError> {
    if markdown_too_deep_to_parse(lang, source) {
        return Err(AftError::ParseError {
            message: format!("{}: {MARKDOWN_TOO_DEEP}", path.display()),
        });
    }
    #[cfg(feature = "test-hooks")]
    SEARCH_PARSE_COUNT.with(|count| count.set(count.get() + 1));
    #[cfg(any(test, feature = "test-support"))]
    work_counters::CACHED_PARSER_PARSES.with(|count| count.set(count.get() + 1));
    REUSABLE_PARSERS.with(|pool| {
        pool.edit(|parsers| {
            let mut parser = match parsers.remove(&lang) {
                Some(parser) => parser,
                None => {
                    #[cfg(any(test, feature = "test-support"))]
                    work_counters::GRAMMAR_LOADS.with(|count| count.set(count.get() + 1));
                    let mut parser = Parser::new();
                    initialize_parser(&mut parser, lang)?;
                    parser
                }
            };
            #[cfg(any(test, feature = "test-support"))]
            parser.note_parse();
            let result = if let Some(deadline) = deadline {
                let start = std::time::Instant::now();
                let mut expired = false;
                let mut progress = |_: &tree_sitter::ParseState| {
                    if start.elapsed() >= deadline {
                        expired = true;
                        std::ops::ControlFlow::Break(())
                    } else {
                        std::ops::ControlFlow::Continue(())
                    }
                };
                let bytes = source.as_bytes();
                let tree = parser.parse_with_options(
                    &mut |offset, _| bytes.get(offset..).unwrap_or_default(),
                    None,
                    Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
                );
                // A cancelled parser retains partial native state. Discard it rather
                // than resuming that state on the next unrelated header.
                if expired {
                    Ok(None)
                } else {
                    tree.map(Some).ok_or_else(|| AftError::ParseError {
                        message: format!("tree-sitter parse returned None for {}", path.display()),
                    })
                }
            } else {
                parse_with_parser(&mut parser, path, source, lang, None).map(Some)
            };
            if matches!(result, Ok(Some(_))) {
                parsers.insert(lang, parser);
            }
            result
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_census_counts_reusable_parser_slots_until_worker_exit() {
        static TEST_TOTALS: Mutex<PoolTotals> = Mutex::new(PoolTotals {
            threads: 0,
            parsers: 0,
        });
        thread_local! { static POOL: ParserPool = ParserPool::new(&TEST_TOTALS); }
        let estimate = || TEST_TOTALS.lock().unwrap().estimate();
        std::thread::spawn(move || {
            POOL.with(|pool| {
                pool.edit(|parsers| {
                    parsers.insert(LangId::Rust, Parser::new());
                    parsers.insert(LangId::Python, Parser::new());
                });
                let both = estimate();
                assert_eq!(both.counts["threads_with_parsers"], 1);
                assert_eq!(both.counts["parsers"], 2);
                assert_eq!(both.bytes_status, "partial");
                assert_eq!(
                    both.estimated_bytes,
                    Some((size_of::<ParserPool>() + 2 * size_of::<(LangId, Parser)>()) as u64)
                );
                pool.edit(|parsers| parsers.remove(&LangId::Rust));
                assert_eq!(estimate().counts["parsers"], 1);
                assert_eq!(estimate().counts["threads_with_parsers"], 1);
            });
        })
        .join()
        .unwrap();
        assert_eq!(estimate().counts["threads_with_parsers"], 0);
        assert_eq!(estimate().counts["parsers"], 0);
        assert_eq!(estimate().estimated_bytes, Some(0));
    }
}
