use super::*;

impl OutputFiles {
    #[doc(hidden)]
    pub fn restore_for_policy(&self, host: &dyn OutputStoreHost, session: &str, path: &Path) {
        self.restore(host, session, path);
    }

    #[doc(hidden)]
    pub fn result_path_for_policy(&self, host: &dyn OutputStoreHost, path: &Path) -> bool {
        self.is_result_path(host, path)
    }

    #[doc(hidden)]
    pub fn owner_for_policy(&self, path: &Path) -> Option<String> {
        self.owner(path)
    }
}
