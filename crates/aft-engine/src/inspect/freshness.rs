use std::path::Path;

use crate::cache_freshness::{self, FileFreshness, FreshnessVerdict};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContributionFreshness {
    Fresh {
        metadata_changed: bool,
        freshness: FileFreshness,
    },
    Stale,
    Deleted,
}

impl ContributionFreshness {
    pub fn is_fresh(self) -> bool {
        matches!(self, ContributionFreshness::Fresh { .. })
    }

    pub fn freshness(self) -> Option<FileFreshness> {
        match self {
            ContributionFreshness::Fresh { freshness, .. } => Some(freshness),
            ContributionFreshness::Stale | ContributionFreshness::Deleted => None,
        }
    }
}

pub fn verify_contribution_file(path: &Path, cached: &FileFreshness) -> ContributionFreshness {
    match cache_freshness::verify_file(path, cached) {
        FreshnessVerdict::HotFresh => ContributionFreshness::Fresh {
            metadata_changed: false,
            freshness: *cached,
        },
        FreshnessVerdict::ContentFresh {
            new_mtime,
            new_size,
        } => ContributionFreshness::Fresh {
            metadata_changed: true,
            freshness: FileFreshness {
                mtime: new_mtime,
                size: new_size,
                content_hash: cached.content_hash,
            },
        },
        FreshnessVerdict::Stale => ContributionFreshness::Stale,
        FreshnessVerdict::Deleted => ContributionFreshness::Deleted,
    }
}

pub fn contribution_is_fresh(path: &Path, cached: &FileFreshness) -> bool {
    verify_contribution_file(path, cached).is_fresh()
}
