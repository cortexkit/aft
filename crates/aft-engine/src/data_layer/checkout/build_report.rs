use std::path::Path;
use std::time::Instant;

use super::SharedUnits;

/// What a build did.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct BuildReport {
    pub files: usize,
    pub text_units: usize,
    /// What the new segment left out because another checkout already
    /// reads it from a segment this build shares.
    pub shared: SharedUnits,
    pub parsed_files: usize,
    pub extract_failures: usize,
    pub scan_ms: f64,
    pub resolve_ms: f64,
    pub write_ms: f64,
    pub publish_ms: f64,
    /// For a checkout with nothing published yet, when its text index was
    /// published, ahead of the call graph: milliseconds from the build's
    /// start.
    pub text_ready_ms: Option<f64>,
    pub segment_bytes: u64,
    /// Segments no checkout needs any more, left out of the new generation.
    pub retired_segments: usize,
    /// Other checkouts sharing the repository store whose folders are gone,
    /// left out of the new generation with their states.
    pub retired_checkouts: usize,
    /// Another process published this checkout while the build ran, so
    /// nothing was published; the checkout serves that publication.
    pub superseded: bool,
    /// When the build started from another checkout's publication, how
    /// many paths differed from it; `None` for a build from scratch.
    pub started_from_sibling: Option<usize>,
    /// Content records the build's scan read from the store instead of
    /// parsing their files again.
    pub stored_records: usize,
    /// Records an earlier attempt of the same build staged, used instead of
    /// extracting the files again.
    pub resumed_records: usize,
    /// Whether this build reused the staged call-graph resolution passes
    /// from an earlier attempt instead of running them again.
    pub resumed_passes: bool,
    /// Files the reclamation after publishing removed, or why it could not
    /// run (the publication stands either way).
    pub reclaimed_files: usize,
    pub reclaim_error: Option<String>,
    /// Memory in bytes by stage and component (for example, `scan.records`),
    /// alongside timing entries prefixed with `ms.`. Only reported by
    /// [`super::Checkout::build_measuring_memory`], since walking the
    /// structures costs time.
    pub memory: Vec<(String, usize)>,
}

pub(super) enum BuildCause {
    Build,
    RefreshFallback,
}

pub(super) struct ColdBuildLog {
    started: Instant,
    trigger: &'static str,
}

impl ColdBuildLog {
    pub(super) fn new(started: Instant, cause: BuildCause, graph_built: bool) -> Self {
        let trigger = match cause {
            BuildCause::RefreshFallback => "refresh_fallback",
            BuildCause::Build if graph_built => "forced_rebuild",
            BuildCause::Build => "first_build",
        };
        Self { started, trigger }
    }

    pub(super) fn completed(self, root: &Path, published: Instant, report: &BuildReport) {
        let wall_ms = published.duration_since(self.started).as_secs_f64() * 1000.0;
        crate::slog_info!(
            "cold call-graph build completed: root={} trigger={} wall_ms={:.3} scan_ms={:.3} \
             resolve_ms={:.3} write_ms={:.3} publish_ms={:.3} text_ready_ms={:.3?} stored_records={}",
            root.display(),
            self.trigger,
            wall_ms,
            report.scan_ms,
            report.resolve_ms,
            report.write_ms,
            report.publish_ms,
            report.text_ready_ms,
            report.stored_records,
        );
    }
}
