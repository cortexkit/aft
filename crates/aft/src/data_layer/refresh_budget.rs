//! Limits an incremental update by the files invalidated since publication.

use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RefreshLimit {
    pub(crate) invalidated: usize,
    pub(crate) threshold: usize,
    pub(crate) parsed: usize,
}

pub(crate) struct RefreshBudget {
    files: BTreeSet<String>,
    parsed: usize,
}

impl RefreshBudget {
    pub(crate) fn new(parsed: usize, files: BTreeSet<String>) -> Self {
        Self { files, parsed }
    }

    pub(crate) fn set_parsed(&mut self, parsed: usize) -> Result<(), RefreshLimit> {
        self.parsed = parsed;
        self.check(self.files.len())
    }

    pub(crate) fn check(&self, count: usize) -> Result<(), RefreshLimit> {
        let threshold = self.parsed.div_ceil(4).max(1024);
        if count > threshold {
            Err(RefreshLimit {
                invalidated: count,
                threshold,
                parsed: self.parsed,
            })
        } else {
            Ok(())
        }
    }

    pub(crate) fn include<'a>(
        &mut self,
        files: impl IntoIterator<Item = &'a String>,
    ) -> Result<(), RefreshLimit> {
        self.files.extend(files.into_iter().cloned());
        self.check(self.files.len())
    }
}
