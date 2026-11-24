impl PublicationFixture {
    #[cfg(test)]
    pub(super) fn context(&self) -> crate::context::AppContext {
        self.context_with(crate::context::AppContext::new)
    }

    pub fn context_with(
        &self,
        make_context: impl FnOnce(
            Box<dyn crate::language::LanguageProvider>,
            crate::config::Config,
        ) -> crate::context::AppContext,
    ) -> crate::context::AppContext {
        let ctx = make_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(self.handle.root().to_path_buf()),
                storage_dir: Some(self._storage.path().to_path_buf()),
                harness: Some(crate::harness::Harness::Opencode),
                ..crate::config::Config::default()
            },
        );
        ctx.set_canonical_cache_root(self.handle.root().to_path_buf());
        ctx.set_harness(crate::harness::Harness::Opencode);
        ctx.pin_data_checkout(Arc::clone(&self.handle));
        ctx
    }

}
