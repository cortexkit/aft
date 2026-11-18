//! Decides whether an incremental update is cheaper than a cold build, by
//! the files it has to work on since the publication: past a tenth of the
//! parsed files (and at least 1,024) the update gives way to a cold build.
//!
//! Files are counted in two classes, which the limit counts together and
//! the log reports apart:
//!
//! - full work: the changed files and every file bound, resolved or traced
//!   again (passes 1-3);
//! - method-call sites only: files whose call sites read a method name a
//!   changed file declares or declared, and whose sites alone are resolved
//!   again (pass 4). Adding a type with a `get` or `new` method reaches
//!   every file that calls a method of that name, often a fifth or more of
//!   a repository.
//!
//! Sites-only work costs far less CPU than a cold build (on Spring
//! Framework, re-resolving the sites of 66% of the files took 75 s against
//! 135 s for a cold build; on AFT 39% took 37 s against 279 s), but pass 4
//! holds its whole working set in memory while a cold build is disk-backed.
//! Its peak passed a cold build's from about a tenth of the files (AFT: 535
//! readers peaked at 1.7 GiB against 0.9 GiB cold; Spring: 1,749 readers at
//! 2.1 GiB against 0.8 GiB), so sites-only files count against the same
//! limit as full work.

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

/// Why an update gave way to a cold build: what it had admitted by then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RefreshLimit {
    /// The stage whose files went over the limit.
    pub(crate) stage: Stage,
    /// Files counted against the limit: `full` and `sites_only` together.
    pub(crate) invalidated: usize,
    pub(crate) threshold: usize,
    /// Files needing full work.
    pub(crate) full: usize,
    /// Files needing only their method-call sites resolved again.
    pub(crate) sites_only: usize,
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
            "invalidated={} threshold={} full={} sites_only={} parsed={} changed={} crossed_at={} admitted: {}; stages after {} did not run, so their files are not counted",
            self.invalidated,
            self.threshold,
            self.full,
            self.sites_only,
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
        self.check(self.last, None)
    }

    /// Whether `changed` paths alone stay within the limit: checked before
    /// anything is extracted or started, so a single watcher batch larger
    /// than the limit never starts an engine.
    pub(crate) fn check_changed(&mut self, changed: usize) -> Result<(), RefreshLimit> {
        self.changed = self.changed.max(changed);
        self.check(Stage::Changed, Some(changed))
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

    /// Checks the files admitted so far, or `count` instead when given.
    fn check(&self, stage: Stage, count: Option<usize>) -> Result<(), RefreshLimit> {
        // Small checkouts rely on the memory guard rather than rebuilding on routine edits.
        let threshold = self.parsed.div_ceil(10).max(1024);
        let sites_only = self.sites_only();
        let count = count.unwrap_or(self.files.len() + sites_only);
        #[cfg(test)]
        if RECORD.lock().unwrap().is_some() {
            return Ok(());
        }
        if count > threshold {
            Err(RefreshLimit {
                stage,
                invalidated: count,
                threshold,
                full: self.files.len(),
                sites_only,
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
        self.check(stage, None)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn names(prefix: &str, count: usize) -> Vec<String> {
        (0..count).map(|n| format!("{prefix}{n}.ts")).collect()
    }

    /// Sites-only files count against the same limit as full work, each
    /// file once whichever classes admitted it, and the refusal says which
    /// stage crossed and how many files each class held.
    #[test]
    fn both_classes_count_against_one_limit_and_the_refusal_reports_each() {
        let mut budget = RefreshBudget::new(2_000, BTreeSet::new(), BTreeSet::new());
        budget
            .include(Stage::Changed, &names("changed", 1))
            .unwrap();
        let readers = names("reader", 900);
        budget.include(Stage::MethodReaders, &readers).unwrap();
        // Readers resolved in full count once.
        budget.include(Stage::Resolve, &readers[..100]).unwrap();
        let limit = budget
            .include(Stage::Sites, &names("site", 200))
            .unwrap_err();
        assert_eq!(limit.stage, Stage::Sites);
        assert_eq!(limit.threshold, 1024);
        assert_eq!((limit.full, limit.sites_only), (101, 1_000));
        assert_eq!(limit.invalidated, 1_101);
        assert_eq!(limit.changed, 1);
        let text = limit.to_string();
        assert!(text.contains("crossed_at=sites"), "{text}");
        assert!(
            text.contains("changed=1 method_readers=900 rebind=0 resolve=100 dispatch=0 sites=200"),
            "{text}"
        );
    }

    /// Files earlier refreshes since the publication worked on count from
    /// the start, in the class each was admitted to.
    #[test]
    fn carried_over_files_count_in_their_class() {
        let full = names("full", 600).into_iter().collect::<BTreeSet<_>>();
        let sites = names("site", 400).into_iter().collect::<BTreeSet<_>>();
        let mut budget = RefreshBudget::new(2_000, full, sites);
        budget.include(Stage::Changed, &names("full", 1)).unwrap();
        assert!(budget.full_work("full0.ts"));
        assert!(!budget.full_work("site0.ts"));
        let limit = budget
            .include(Stage::MethodReaders, &names("new", 30))
            .unwrap_err();
        assert_eq!((limit.full, limit.sites_only), (600, 430));
        assert!(
            limit
                .to_string()
                .contains("seeded_full=600 seeded_sites=400"),
            "{limit}"
        );
    }

    /// The check before anything is extracted counts the changed paths
    /// alone, as it always has.
    #[test]
    fn the_first_check_counts_only_the_changed_paths() {
        let seed = names("earlier", 1_000).into_iter().collect::<BTreeSet<_>>();
        let mut budget = RefreshBudget::new(2_000, seed, BTreeSet::new());
        budget.check_changed(30).unwrap();
        let limit = budget.check_changed(1_025).unwrap_err();
        assert_eq!((limit.stage, limit.invalidated), (Stage::Changed, 1_025));
    }

    #[test]
    fn large_checkouts_admit_one_tenth_of_the_parsed_files() {
        let mut budget = RefreshBudget::new(20_000, BTreeSet::new(), BTreeSet::new());
        budget.check_changed(2_000).unwrap();
        let limit = budget.check_changed(2_001).unwrap_err();
        assert_eq!(limit.threshold, 2_000);
    }
}
