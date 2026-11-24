use std::sync::Arc;

use super::SiteResolution;

impl SiteResolution {
    #[doc(hidden)]
    pub fn target_count_for_test(&self) -> usize {
        self.resolution.targets.len()
    }

    #[doc(hidden)]
    pub fn protected_count_for_test(&self) -> usize {
        self.resolution.protected.len()
    }

    #[doc(hidden)]
    pub fn shares_resolution_for_test(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.resolution, &other.resolution)
    }

    #[doc(hidden)]
    pub fn line_for_test(&self) -> u32 {
        self.site.line
    }
}
