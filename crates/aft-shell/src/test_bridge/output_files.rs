use std::path::Path;

use crate::output_store::{OutputFiles, OutputStoreHost};

pub trait OutputFilesTest {
    fn file_count(&self) -> usize;
    fn files_are_empty(&self) -> bool;
    fn canonicalizations(&self) -> usize;
    fn restore(&self, ctx: &dyn OutputStoreHost, session: &str, path: &Path);
    fn is_result_path(&self, ctx: &dyn OutputStoreHost, path: &Path) -> bool;
    fn owner(&self, path: &Path) -> Option<String>;
}

impl OutputFilesTest for OutputFiles {
    fn file_count(&self) -> usize {
        self.files.lock().len()
    }
    fn files_are_empty(&self) -> bool {
        self.files.lock().is_empty()
    }
    fn canonicalizations(&self) -> usize {
        self.canonicalizations
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    fn restore(&self, ctx: &dyn OutputStoreHost, session: &str, path: &Path) {
        self.restore(ctx, session, path);
    }
    fn is_result_path(&self, ctx: &dyn OutputStoreHost, path: &Path) -> bool {
        self.is_result_path(ctx, path)
    }
    fn owner(&self, path: &Path) -> Option<String> {
        self.owner(path)
    }
}
