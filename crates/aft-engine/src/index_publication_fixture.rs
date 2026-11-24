use std::ops::Deref;
use std::time::Duration;

use aft_index::data_layer::runtime::publication_fixture::PublicationFixture as IndexFixture;

pub struct PublicationFixture(IndexFixture);

impl Deref for PublicationFixture {
    type Target = IndexFixture;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl PublicationFixture {
    pub fn new(settle: Duration, quiet: Duration) -> Self {
        Self(IndexFixture::new(settle, quiet))
    }

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
        compose_context(&self.0, make_context)
    }
}

pub(crate) fn compose_context(
    fixture: &IndexFixture,
    make_context: impl FnOnce(
        Box<dyn crate::language::LanguageProvider>,
        crate::config::Config,
    ) -> crate::context::AppContext,
) -> crate::context::AppContext {
    let ctx = make_context(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(fixture.root().to_path_buf()),
            storage_dir: Some(fixture.storage().to_path_buf()),
            harness: Some(crate::harness::Harness::Opencode),
            ..crate::config::Config::default()
        },
    );
    ctx.set_canonical_cache_root(fixture.root().to_path_buf());
    ctx.set_harness(crate::harness::Harness::Opencode);
    ctx.pin_data_checkout(fixture.checkout_handle());
    ctx
}
