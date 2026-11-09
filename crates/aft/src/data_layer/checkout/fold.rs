//! Folding a checkout's segments into one, from what the store holds.
//!
//! Each publication of a checkout's changes adds a segment, so a checkout
//! that keeps editing comes to read its units from many of them. Past
//! [`super::COMPACT_AT`] of its own, or [`super::READ_AT`] in all (see
//! [`super::DeltaReport::wants_fold`]), its segments are folded: every
//! unit its published state reads (text units, content records and
//! call-graph fragments, with the terms each fragment is found by) is
//! copied from the newest segment holding it into one new segment, and
//! the generation that names it leaves out every segment no checkout reads
//! any more. Nothing is read from the checkout's files, parsed or resolved:
//! every unit is already stored. A whole build used to do this, reading and
//! parsing every file of the checkout again to write the same units.
//!
//! A fold keeps the units another checkout already reads where that one
//! reads them, chooses the segments it shares from as a build does
//! ([`Checkout::leave_out_shared`]), and retires what no checkout reads as a
//! build does ([`Checkout::unneeded`]), so afterwards the store holds what a
//! build would leave and the checkout answers as it did before.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use aft_store::cache::BlockCache;
use aft_store::content::{ContentSegment, RecordKey};
use aft_store::family::{Publication, Snapshot};
use aft_store::graph::{FragmentKey, GraphSegment};
use aft_store::ids::ContentKey;
use aft_store::segment::Segment;
use aft_store::text::{Located, TextCopy, TextLocator, TextSegment};

use super::{
    checkpoint, elapsed_ms, known_sections, Checkout, DataError, Published, ReadByOthers, ReadUnit,
    SharedUnits, EXTRACTOR_VERSION,
};

/// What folding a checkout's segments did.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct FoldReport {
    /// Units the new segment holds.
    pub text_units: usize,
    pub records: usize,
    pub fragments: usize,
    /// What it left out because another checkout already reads it from a
    /// segment the fold shares, as a build leaves it out.
    pub shared: SharedUnits,
    /// Segments the checkout's text, content and call graph read from
    /// before the fold.
    pub read_before: usize,
    /// The new segment's length.
    pub segment_bytes: u64,
    /// Segments no checkout read any more, and other checkouts whose folder
    /// is gone, left out of the new generation as a build leaves them out.
    pub retired_segments: usize,
    pub retired_checkouts: usize,
    /// Files the reclamation after publishing removed, or why it could not
    /// run (the publication stands either way).
    pub reclaimed_files: usize,
    pub reclaim_error: Option<String>,
    pub ms: f64,
}

/// How a fold ended.
#[derive(Clone, Debug)]
pub enum Folded {
    Done(FoldReport),
    /// Another process published this checkout first. Nothing was written;
    /// the checkout serves that publication, as a build that stands down
    /// does.
    Superseded,
    /// The store cannot give what the fold needs: nothing is published, a
    /// segment cannot be read by this build, or a unit the publication
    /// names cannot be found or copied. Nothing was written; a whole build
    /// is needed, and this says why.
    NeedsBuild(String),
}

/// The block cache a fold copies through (see [`Checkout::fold`]). Each
/// segment is read in key order, a few blocks at a time per section, so a
/// few hundred blocks are as fast as a large cache: folding a 7,000-file
/// checkout's 21 segments took the same time with 16 MiB as with 256 MiB.
const COPY_CACHE: usize = 16 << 20;

/// Where a unit the fold copies is read from: the place in the manifest of
/// the segment holding it, and its ordinal there.
type Holder = (usize, u32);

/// Every unit a published state reads, each from the newest segment that
/// holds it, as readers take it.
#[derive(Default)]
struct Plan {
    text: BTreeMap<ContentKey, TextFrom>,
    records: BTreeMap<RecordKey, Holder>,
    fragments: BTreeMap<FragmentKey, Holder>,
}

/// Where a text unit comes from.
#[derive(Clone, Copy)]
enum TextFrom {
    /// An indexed unit, copied with its postings.
    Stored(Holder),
    /// A content whose trigrams are not indexed: its unit alone.
    Unindexed(TextCopy),
}

impl Plan {
    /// The places in the manifest of the segments the plan reads from.
    fn holders(&self) -> BTreeSet<usize> {
        let text = self.text.values().filter_map(|copy| match copy {
            TextFrom::Stored((place, _)) => Some(*place),
            TextFrom::Unindexed(_) => None,
        });
        text.chain(self.records.values().map(|(place, _)| *place))
            .chain(self.fragments.values().map(|(place, _)| *place))
            .collect()
    }

    fn units(&self) -> impl Iterator<Item = ReadUnit> + '_ {
        self.text
            .keys()
            .map(|key| ReadUnit::Text(*key))
            .chain(self.records.keys().map(|key| ReadUnit::Record(*key)))
            .chain(self.fragments.keys().map(|key| ReadUnit::Fragment(*key)))
    }

    /// Leaves out the units `shared` accepts, counting them.
    fn leave_out(&mut self, shared: impl Fn(ReadUnit) -> bool) -> SharedUnits {
        let before = (self.text.len(), self.records.len(), self.fragments.len());
        self.text.retain(|key, _| !shared(ReadUnit::Text(*key)));
        self.records
            .retain(|key, _| !shared(ReadUnit::Record(*key)));
        self.fragments
            .retain(|key, _| !shared(ReadUnit::Fragment(*key)));
        SharedUnits {
            text_units: before.0 - self.text.len(),
            records: before.1 - self.records.len(),
            fragments: before.2 - self.fragments.len(),
        }
    }
}

impl Checkout {
    /// Folds the segments the checkout's publication reads into one new
    /// segment, copying the stored units (see the module documentation).
    /// Changes recorded since the publication are left as they are: the
    /// new generation publishes the same state, and the changes stay laid
    /// over it. The call graph resolves them again from the new
    /// publication on its next refresh, since what it resolved was laid
    /// over the old one.
    ///
    /// # Errors
    ///
    /// When the store cannot be written, or a newer build wrote a file it
    /// names. Nothing is published then, and the publication keeps serving.
    pub fn fold(&self) -> Result<Folded, DataError> {
        let _alloc_label =
            crate::alloc_trace::scope(crate::alloc_trace::Label::SEGMENT_FOLD, Some(self.root()));
        let started = Instant::now();
        self.family.check_formats().map_err(DataError::store)?;
        self.check_disk_floor()?;
        checkpoint()?;
        let Some(published) = self
            .live
            .read()
            .expect("no panics while holding it")
            .published
            .clone()
        else {
            return Ok(Folded::NeedsBuild("nothing is published".to_string()));
        };
        if self.planes.callgraph && !published.graph_built {
            return Ok(Folded::NeedsBuild(
                "the publication has no call graph".to_string(),
            ));
        }
        let mut session = self.writer()?;
        // With the writer held, no process publishes until this one does,
        // so this is the generation the fold changes.
        let snapshot = self.family.snapshot().map_err(DataError::store)?;
        if snapshot.manifest().checkout(self.checkout)
            != published._snapshot.manifest().checkout(self.checkout)
        {
            drop(session);
            self.adopt_newest()?;
            return Ok(Folded::Superseded);
        }
        let known = known_sections();
        // What the fold reads it reads once, in order, so it reads through a
        // small cache of its own: through the block cache every checkout
        // shares, copying a large checkout's units evicted the blocks other
        // checkouts' queries keep reading. The fold opens each segment again
        // for this, apart from the shared copy, and closes it when it ends.
        let copying = Arc::new(BlockCache::new(COPY_CACHE));
        let mut segments = Vec::with_capacity(snapshot.manifest().segments().len());
        for entry in snapshot.manifest().segments() {
            match snapshot.open_segment(entry.id, &known, Arc::clone(&copying)) {
                Ok(segment) => segments.push(Arc::new(segment)),
                Err(error) if error.is_unusable_file() => {
                    return Ok(Folded::NeedsBuild(format!(
                        "a segment this build cannot read: {error}"
                    )));
                }
                Err(error) => return Err(DataError::store(error)),
            }
        }
        let mut plan = match self.plan(&published, &segments)? {
            Ok(plan) => plan,
            Err(why) => return Ok(Folded::NeedsBuild(why)),
        };
        let read_before = plan.holders().len();
        let shared = self.share_from(&snapshot, &mut plan)?;
        checkpoint()?;

        let mut pending = session.create_segment().map_err(DataError::store)?;
        if let Err(error) = write(pending.writer(), &segments, &plan) {
            return match DataError::store(error) {
                refused @ DataError::NewerFormat(_) => Err(refused),
                error => Ok(Folded::NeedsBuild(format!(
                    "a stored unit could not be copied: {error}"
                ))),
            };
        }
        let segment = session.finish_segment(pending).map_err(DataError::store)?;
        // The new segment holds everything the state reads that other
        // checkouts do not read from where it shares, as a build's does.
        let unneeded = self.unneeded(&published.state, false)?;
        let mut report = FoldReport {
            text_units: plan.text.len(),
            records: plan.records.len(),
            fragments: plan.fragments.len(),
            shared,
            read_before,
            retired_segments: unneeded.segments.len(),
            retired_checkouts: unneeded.checkouts.len(),
            ..FoldReport::default()
        };
        // The state stays the one published: it names its units by key,
        // and the new segment holds them under the same keys.
        session
            .publish(&Publication {
                add_segments: vec![segment],
                remove_segments: unneeded.segments,
                states: Vec::new(),
                retire_checkouts: unneeded.checkouts,
            })
            .map_err(DataError::store)?;
        // As after a build: a view still reading the older generation holds
        // a lease on it, so its files stay until the view drops.
        match session.reclaim() {
            Ok(reclaimed) => {
                report.reclaimed_files = reclaimed.removed_segments
                    + reclaimed.removed_states
                    + reclaimed.removed_manifests;
            }
            Err(error) => report.reclaim_error = Some(error.to_string()),
        }
        drop(session);
        drop(segments);
        drop(snapshot);
        let snapshot = self.family.snapshot().map_err(DataError::store)?;
        report.segment_bytes = snapshot
            .manifest()
            .segment(segment)
            .map_or(0, |entry| entry.len);
        self.serve_folded(snapshot, &published)?;
        report.ms = elapsed_ms(started);
        Ok(Folded::Done(report))
    }

    /// Every unit `published`'s state reads, from the newest of `segments`
    /// (the generation's, in manifest order) that holds it. `Err` names a
    /// fragment the state names that no segment holds: only a build can
    /// write it again. A text unit or content record no segment holds is
    /// one the publication serves without, and so does the fold.
    fn plan(
        &self,
        published: &Published,
        segments: &[Arc<Segment>],
    ) -> Result<Result<Plan, String>, DataError> {
        let mut plan = Plan::default();
        let state = &published.state;
        if self.planes.text {
            // TextLocator numbers only the segments with a text section.
            let mut places = Vec::new();
            let mut texts = Vec::new();
            for (place, segment) in segments.iter().enumerate() {
                if let Some(text) = TextSegment::open(segment).map_err(DataError::store)? {
                    places.push(place);
                    texts.push(text);
                }
            }
            let locator = TextLocator::build(&texts).map_err(DataError::store)?;
            for record in state.paths() {
                let copy = match locator.get(record.content) {
                    Some(Located::Indexed { segment, ordinal }) => {
                        TextFrom::Stored((places[segment as usize], ordinal))
                    }
                    Some(Located::Unindexed { status, len }) => {
                        TextFrom::Unindexed(TextCopy::Unindexed { len, status })
                    }
                    None => continue,
                };
                plan.text.insert(record.content, copy);
            }
        }
        if !self.planes.callgraph {
            return Ok(Ok(plan));
        }
        let contents = segments
            .iter()
            .map(|segment| ContentSegment::open(segment).map_err(DataError::store))
            .collect::<Result<Vec<_>, _>>()?;
        let graphs = segments
            .iter()
            .map(|segment| GraphSegment::open(segment).map_err(DataError::store))
            .collect::<Result<Vec<_>, _>>()?;
        for (position, record) in state.paths().iter().enumerate() {
            if !record.language.is_empty() {
                let key = RecordKey::new(record.content, &record.language, EXTRACTOR_VERSION);
                if let Some(holder) = newest(&contents, |content| content.find(key))? {
                    plan.records.insert(key, holder);
                }
            }
            if let Some(fragment) = state.fragment(position) {
                let key = FragmentKey::from_digest(fragment);
                match newest(&graphs, |graph| graph.find(key))? {
                    Some(holder) => {
                        plan.fragments.insert(key, holder);
                    }
                    None => {
                        return Ok(Err(format!(
                            "no segment holds the call graph of {}",
                            String::from_utf8_lossy(&record.path)
                        )));
                    }
                }
            }
        }
        Ok(Ok(plan))
    }

    /// Leaves out of `plan` what other checkouts read from the segments a
    /// build of this checkout would share from (see
    /// [`Self::leave_out_shared`]).
    fn share_from(&self, snapshot: &Snapshot, plan: &mut Plan) -> Result<SharedUnits, DataError> {
        let Some(read) = self.read_by_others(snapshot)? else {
            return Ok(SharedUnits::default());
        };
        let kept = shared_holders(&read, plan.units());
        Ok(plan.leave_out(|unit| {
            read.holder(unit)
                .is_some_and(|holder| kept.contains(&holder))
        }))
    }

    /// Serves the folded publication in `snapshot` in place of `started`,
    /// the one the fold began from, unless another publication has been
    /// served since (it was opened after the fold published, so it reads
    /// the new segment already). The changes recorded since stay laid over
    /// it; each is pending in the call graph again, since what the last
    /// refresh resolved was laid over the old publication's segments.
    fn serve_folded(&self, snapshot: Snapshot, started: &Arc<Published>) -> Result<(), DataError> {
        self.touch_memory();
        let folded = Arc::new(
            self.open_published(snapshot)?
                .ok_or_else(|| DataError::Store("the published state is missing".to_string()))?,
        );
        {
            let mut live = self.write_live();
            if !live
                .published
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, started))
            {
                return Ok(());
            }
            if self.planes.callgraph {
                live.callgraph_pending = Arc::new(live.changes.keys().cloned().collect());
            }
            live.published = Some(Arc::clone(&folded));
            live.generation += 1;
            live.graph = None;
            live.refreshed = None;
            live.text_members = Arc::default();
        }
        // The fold holds what the old publication held, so an engine that
        // has applied no change since it started from that one goes on from
        // the fold's; one that has is dropped, and the next refresh starts a
        // new one, as it resolved the changes over the old segments.
        self.carry_engine(started, &folded, &BTreeMap::new());
        Ok(())
    }
}

/// The segments a new segment shares units from, by place in the manifest:
/// of those the other checkouts read the given `units` from, the
/// [`super::SHARE_FROM`] that serve the most; between equals, the newer.
pub(super) fn shared_holders(
    read: &ReadByOthers,
    units: impl IntoIterator<Item = ReadUnit>,
) -> HashSet<usize> {
    let mut serves = HashMap::<usize, usize>::new();
    for unit in units {
        if let Some(holder) = read.holder(unit) {
            *serves.entry(holder).or_default() += 1;
        }
    }
    let mut ranked = serves.into_iter().collect::<Vec<_>>();
    ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
    ranked
        .into_iter()
        .take(super::SHARE_FROM)
        .map(|(holder, _)| holder)
        .collect()
}

/// The place and ordinal of the unit `find` finds in the newest of
/// `segments` that holds it.
fn newest<T>(
    segments: &[Option<T>],
    find: impl Fn(&T) -> Result<Option<u32>, aft_store::segment::SegmentError>,
) -> Result<Option<Holder>, DataError> {
    for (place, segment) in segments.iter().enumerate().rev() {
        let Some(segment) = segment else {
            continue;
        };
        if let Some(ordinal) = find(segment).map_err(DataError::store)? {
            return Ok(Some((place, ordinal)));
        }
    }
    Ok(None)
}

/// Writes the plan's units into `writer`, each plane's sections copied from
/// the segments that hold its units.
fn write(
    writer: &mut aft_store::segment::SegmentWriter,
    segments: &[Arc<Segment>],
    plan: &Plan,
) -> Result<(), aft_store::segment::SegmentError> {
    // Each plane's writer takes the sources it copies from as a list; this
    // maps a segment's place in the manifest to its place in that list.
    fn sources<'s, T>(
        segments: &'s [Arc<Segment>],
        places: &BTreeSet<usize>,
        open: impl Fn(&'s Segment) -> Result<Option<T>, aft_store::segment::SegmentError>,
    ) -> Result<(Vec<T>, HashMap<usize, usize>), aft_store::segment::SegmentError> {
        let mut opened = Vec::new();
        let mut at = HashMap::new();
        for &place in places {
            if let Some(sections) = open(segments[place].as_ref())? {
                at.insert(place, opened.len());
                opened.push(sections);
            }
        }
        Ok((opened, at))
    }

    let places = plan
        .text
        .values()
        .filter_map(|copy| match copy {
            TextFrom::Stored((place, _)) => Some(*place),
            TextFrom::Unindexed(_) => None,
        })
        .collect();
    let (texts, at) = sources(segments, &places, TextSegment::open)?;
    let units = plan
        .text
        .iter()
        .map(|(key, copy)| {
            let copy = match *copy {
                TextFrom::Stored((place, ordinal)) => TextCopy::Stored {
                    source: at[&place],
                    ordinal,
                },
                TextFrom::Unindexed(copy) => copy,
            };
            (*key, copy)
        })
        .collect::<Vec<_>>();
    aft_store::text::write_folded(writer, &texts, &units)?;
    drop((texts, units));

    let places = plan.records.values().map(|(place, _)| *place).collect();
    let (contents, at) = sources(segments, &places, ContentSegment::open)?;
    let units = plan
        .records
        .iter()
        .map(|(key, (place, ordinal))| (*key, at[place], *ordinal))
        .collect::<Vec<_>>();
    aft_store::content::write_folded(writer, &contents, &units)?;
    drop((contents, units));

    let places = plan.fragments.values().map(|(place, _)| *place).collect();
    let (graphs, at) = sources(segments, &places, GraphSegment::open)?;
    let units = plan
        .fragments
        .iter()
        .map(|(key, (place, ordinal))| (*key, at[place], *ordinal))
        .collect::<Vec<_>>();
    aft_store::graph::write_folded(writer, &graphs, &units)
}
