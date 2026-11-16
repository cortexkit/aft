//! Decides whether an incremental update is cheaper than a cold build, by
//! the files it has to work on since the publication.
//!
//! Files are counted in two classes, each against its own limit:
//!
//! - full work: the changed files and every file bound, resolved or traced
//!   again (passes 1-3, which pass 4 follows). A cold build redoes all of
//!   that for every file, so past a quarter of the parsed files a cold
//!   build costs about as much;
//! - method-call sites only: files whose call sites read a method name a
//!   changed file declares or declared, and whose sites alone are resolved
//!   again (pass 4). Adding a type with a `get` or `new` method reaches
//!   every file that calls a method of that name, often a third of a
//!   repository, yet redoing only their sites costs a fraction of a cold
//!   build (see [`SITE_READERS_PERCENT`]).

use std::collections::BTreeSet;
use std::fmt;

/// The part of an incremental update that admitted files to its budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    /// The changed paths themselves.
    Changed,
    /// Files whose method-call sites read a method name a changed file
    /// declares or declared.
    MethodReaders,
    /// Files bound again (pass 1).
    Rebind,
    /// Files resolved again (pass 2).
    Resolve,
    /// Files whose dispatch targets are computed again (pass 3).
    Dispatch,
    /// Files whose method-call sites are resolved again (pass 4).
    Sites,
}

impl Stage {
    const ALL: [Self; 6] = [
        Self::Changed,
        Self::MethodReaders,
        Self::Rebind,
        Self::Resolve,
        Self::Dispatch,
        Self::Sites,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Changed => "changed",
            Self::MethodReaders => "method_readers",
            Self::Rebind => "rebind",
            Self::Resolve => "resolve",
            Self::Dispatch => "dispatch",
            Self::Sites => "sites",
        }
    }

    /// Whether the stage's files only get their method-call sites resolved
    /// again.
    fn sites_only(self) -> bool {
        matches!(self, Self::MethodReaders | Self::Sites)
    }
}

/// How large a share of the parsed files, in percent, may have only their
/// method-call sites resolved again before a cold build is cheaper.
///
/// Measured on a build optimized as a release (`--profile stage`, 8
/// threads), with the CPU time of the refresh and of the publication of
/// its changes against a cold build reusing the stored records:
///
/// - AFT (6,144 parsed files, mostly Rust): a cold build took 310 s of CPU
///   (45 s wall). Re-resolving the sites of 888 readers (14%) took 27-51 s,
///   of 1,652 (27%) 35 s, of 2,382 (39%, every reader of the 315 method
///   names with ten or more) 39 s, each plus 8-12 s to publish. The cost
///   grows by about 0.5 s per percent, so even every file would cost about
///   a third of a cold build.
/// - TypeORM (3,672 parsed TypeScript files): a cold build took 72 s of CPU
///   (10 s wall); the sites of 960 readers (26%) took 3 s and of 1,245 (34%,
///   every reader there is) 3.6 s, each plus 1.3-1.5 s to publish.
///
/// No share of the files reached a cold build's cost, so sites-only work
/// never makes a cold build cheaper: the limit is every parsed file.
const SITE_READERS_PERCENT: usize = 100;

/// Why an update gave way to a cold build: what it had admitted by then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RefreshLimit {
    /// The stage whose files went over a limit.
    pub(crate) stage: Stage,
    /// Files needing full work.
    pub(crate) invalidated: usize,
    pub(crate) threshold: usize,
    /// Files needing only their method-call sites resolved again.
    pub(crate) sites_only: usize,
    pub(crate) sites_threshold: usize,
    pub(crate) parsed: usize,
    /// Paths changed since the publication.
    pub(crate) changed: usize,
    /// Each stage's count, and the counts carried over from earlier
    /// refreshes since the publication.
    counts: String,
}

impl fmt::Display for RefreshLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalidated={} threshold={} sites_only={} sites_threshold={} parsed={} changed={} crossed_at={} admitted: {}; stages after {} did not run, so their files are not counted",
            self.invalidated,
            self.threshold,
            self.sites_only,
            self.sites_threshold,
            self.parsed,
            self.changed,
            self.stage.name(),
            self.counts,
            self.stage.name()
        )
    }
}

/// For measurement: while set, a budget never refuses, and each budget
/// appends its counts here when it is dropped.
#[cfg(test)]
pub(crate) static RECORD: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

pub(crate) struct RefreshBudget {
    /// Files needing full work.
    files: BTreeSet<String>,
    /// Files admitted for their method-call sites; those also in `files`
    /// count there.
    sites: BTreeSet<String>,
    parsed: usize,
    changed: usize,
    /// How many files each stage added to its class.
    added: [usize; Stage::ALL.len()],
    /// The files each class started with: those earlier refreshes since
    /// the publication worked on.
    seeded: (usize, usize),
    last: Stage,
}

impl RefreshBudget {
    /// A budget over `parsed` files, starting with the files earlier
    /// refreshes since the publication resolved again: `files` in full,
    /// `sites` for their method-call sites only.
    pub(crate) fn new(parsed: usize, files: BTreeSet<String>, sites: BTreeSet<String>) -> Self {
        let seeded = (files.len(), sites.len());
        Self {
            files,
            sites,
            parsed,
            changed: 0,
            added: [0; Stage::ALL.len()],
            seeded,
            last: Stage::Changed,
        }
    }

    pub(crate) fn set_parsed(&mut self, parsed: usize) -> Result<(), RefreshLimit> {
        self.parsed = parsed;
        self.check(self.last, self.files.len())
    }

    /// Whether `changed` paths alone, each needing full work, stay within
    /// the limit: checked before anything is extracted or started.
    pub(crate) fn check_changed(&mut self, changed: usize) -> Result<(), RefreshLimit> {
        self.changed = self.changed.max(changed);
        self.check(Stage::Changed, changed.max(self.files.len()))
    }

    /// Whether `file` was admitted for full work rather than for its
    /// method-call sites only.
    pub(crate) fn full_work(&self, file: &str) -> bool {
        self.files.contains(file)
    }

    fn sites_only(&self) -> usize {
        self.sites
            .iter()
            .filter(|file| !self.files.contains(*file))
            .count()
    }

    fn thresholds(&self) -> (usize, usize) {
        (
            self.parsed.div_ceil(4).max(1024),
            (self.parsed * SITE_READERS_PERCENT / 100).max(1024),
        )
    }

    fn check(&self, stage: Stage, count: usize) -> Result<(), RefreshLimit> {
        let (threshold, sites_threshold) = self.thresholds();
        let sites_only = self.sites_only();
        #[cfg(test)]
        if RECORD.lock().unwrap().is_some() {
            return Ok(());
        }
        if count > threshold || sites_only > sites_threshold {
            Err(RefreshLimit {
                stage,
                invalidated: count,
                threshold,
                sites_only,
                sites_threshold,
                parsed: self.parsed,
                changed: self.changed,
                counts: self.counts(),
            })
        } else {
            Ok(())
        }
    }

    pub(crate) fn include<'a>(
        &mut self,
        stage: Stage,
        files: impl IntoIterator<Item = &'a String>,
    ) -> Result<(), RefreshLimit> {
        let class = if stage.sites_only() {
            &mut self.sites
        } else {
            &mut self.files
        };
        let before = class.len();
        let mut offered = 0;
        for file in files {
            offered += 1;
            class.insert(file.clone());
        }
        self.added[stage as usize] += class.len() - before;
        if stage == Stage::Changed {
            self.changed = self.changed.max(offered);
        }
        self.last = stage;
        self.check(stage, self.files.len())
    }

    /// Each class's carried-over count and each stage's.
    fn counts(&self) -> String {
        let mut text = format!(
            "seeded_full={} seeded_sites={}",
            self.seeded.0, self.seeded.1
        );
        for stage in Stage::ALL {
            text.push_str(&format!(" {}={}", stage.name(), self.added[stage as usize]));
        }
        text
    }
}

#[cfg(test)]
impl Drop for RefreshBudget {
    fn drop(&mut self) {
        if let Some(record) = RECORD.lock().unwrap().as_mut() {
            record.push(format!(
                "budget parsed={} full={} sites_only={} {}",
                self.parsed,
                self.files.len(),
                self.sites_only(),
                self.counts()
            ));
        }
    }
}
