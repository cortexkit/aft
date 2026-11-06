//! Finding the files of a checkout that may match a pattern.
//!
//! A query runs once per segment over the contents that the checkout's
//! paths in scope hold, then maps the contents found back to every path that
//! holds them. Paths whose content is not indexed, or not in any segment yet,
//! are always candidates: the caller verifies each candidate against its real
//! bytes, so including a file costs a read and never a wrong answer.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use super::plan::Query;
use super::sections::{TextSegment, TextStatus};
use super::trigrams::ContentTrigrams;
use crate::checkout::{CheckoutState, PathKind};
use crate::ids::ContentKey;
use crate::segment::SegmentError;

/// Where one content's text unit is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Located {
    Indexed {
        segment: u32,
        ordinal: u32,
    },
    /// Known and not indexed: binary, or larger than the limit it was
    /// indexed under.
    Unindexed {
        status: TextStatus,
        len: u64,
    },
}

/// Where each content of a family's text segments is, shared by every
/// checkout of the family. Built once per segment set; a publication that
/// only adds segments extends it.
#[derive(Clone, Debug, Default)]
pub struct TextLocator {
    by_key: HashMap<ContentKey, Located>,
}

impl TextLocator {
    /// Locates every content of `segments`, which are in the order a
    /// snapshot lists them: by ascending id, so newer segments come later.
    ///
    /// # Errors
    ///
    /// When a segment cannot be read.
    pub fn build(segments: &[TextSegment<'_>]) -> Result<Self, SegmentError> {
        let mut locator = Self::default();
        for (index, segment) in segments.iter().enumerate() {
            locator.extend(index, segment)?;
        }
        Ok(locator)
    }

    /// Adds the contents of the segment at `index` of the segment list. A
    /// content it shares with an earlier segment is located in this one, so
    /// a content indexed again (for instance under a larger size limit) is
    /// found in its newest form.
    ///
    /// # Errors
    ///
    /// When the segment cannot be read.
    pub fn extend(&mut self, index: usize, segment: &TextSegment<'_>) -> Result<(), SegmentError> {
        let index = u32::try_from(index).expect("fewer than 2^32 segments");
        for (ordinal, key) in segment.keys()?.into_iter().enumerate() {
            let ordinal = u32::try_from(ordinal).expect("a segment holds fewer than 2^32 units");
            let unit = segment.unit(ordinal)?;
            let located = if unit.status == TextStatus::Indexed {
                Located::Indexed {
                    segment: index,
                    ordinal,
                }
            } else {
                Located::Unindexed {
                    status: unit.status,
                    len: unit.len,
                }
            };
            self.by_key.insert(key, located);
        }
        Ok(())
    }

    #[must_use]
    pub fn get(&self, key: ContentKey) -> Option<Located> {
        self.by_key.get(&key).copied()
    }

    /// How many contents the segments hold.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

/// Where one path's content is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    Indexed {
        segment: u32,
        ordinal: u32,
    },
    /// Not indexed, or in no segment yet: always a candidate.
    Unchecked,
    /// A symlink: grep never follows one, so it is never a candidate.
    Excluded,
}

/// Where each path of one checkout state finds its content's text unit.
/// Built once per state and locator: one hash lookup per path.
#[derive(Clone, Debug)]
pub struct TextMembership {
    places: Vec<Place>,
    missing: usize,
}

impl TextMembership {
    /// Locates the content of every path of `state`.
    #[must_use]
    pub fn build(state: &CheckoutState, locator: &TextLocator) -> Self {
        let mut missing = 0;
        let places = state
            .paths()
            .iter()
            .map(|record| {
                match record.kind {
                    PathKind::Text => {}
                    PathKind::Symlink => return Place::Excluded,
                    PathKind::Binary
                    | PathKind::Oversize
                    | PathKind::Unreadable
                    | PathKind::Large => {
                        return Place::Unchecked;
                    }
                }
                match locator.get(record.content) {
                    Some(Located::Indexed { segment, ordinal }) => {
                        Place::Indexed { segment, ordinal }
                    }
                    Some(Located::Unindexed { .. }) => Place::Unchecked,
                    None => {
                        missing += 1;
                        Place::Unchecked
                    }
                }
            })
            .collect();
        Self { places, missing }
    }

    /// How many paths no segment holds yet. Grep stays correct by reading
    /// them; this tells the caller the index is still catching up.
    #[must_use]
    pub fn missing(&self) -> usize {
        self.missing
    }
}

/// What the text plane holds for one path of a checkout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathText {
    /// Indexed, with its distinct trigrams.
    Indexed { distinct: u32 },
    /// Not indexed: binary, too large, or not read yet.
    Unindexed,
    /// A symlink, which text queries never follow.
    Excluded,
}

/// What the text plane holds for the path at `index` of `state`, with
/// `overlay`'s change in its place.
///
/// # Errors
///
/// When a segment cannot be read.
pub fn path_text(
    state: &CheckoutState,
    membership: &TextMembership,
    segments: &[TextSegment<'_>],
    overlay: &TextOverlay,
    path: &[u8],
) -> Result<Option<PathText>, SegmentError> {
    if let Some(entry) = overlay.entries.get(path) {
        return Ok(match entry {
            OverlayEntry::Content(trigrams) => Some(PathText::Indexed {
                distinct: u32::try_from(trigrams.distinct()).unwrap_or(u32::MAX),
            }),
            OverlayEntry::Unindexed => Some(PathText::Unindexed),
            OverlayEntry::Deleted => None,
        });
    }
    let Some(index) = state.position(path) else {
        return Ok(None);
    };
    Ok(Some(match membership.places[index] {
        Place::Indexed { segment, ordinal } => {
            let unit = segments[segment as usize].unit(ordinal)?;
            PathText::Indexed {
                distinct: unit.distinct,
            }
        }
        Place::Unchecked => PathText::Unindexed,
        Place::Excluded => PathText::Excluded,
    }))
}

/// How many contents in `segments` hold `trigram`: a member may be counted
/// once per segment that holds its content, and contents no path of the
/// checkout uses are counted too, so this ranks trigrams by rarity rather
/// than counting files.
///
/// # Errors
///
/// When a segment cannot be read.
pub fn trigram_frequency(segments: &[TextSegment<'_>], trigram: u32) -> Result<u64, SegmentError> {
    segments
        .iter()
        .map(|segment| segment.count(trigram).map(u64::from))
        .sum()
}

/// The paths of `state`, with `overlay`'s changes, whose content holds
/// `trigram`, in byte order. Unlike [`candidates`], a path whose content is
/// not indexed is left out: this answers which indexed contents hold a
/// trigram, for ranking.
///
/// # Errors
///
/// When a segment cannot be read.
pub fn paths_with_trigram(
    state: &CheckoutState,
    membership: &TextMembership,
    segments: &[TextSegment<'_>],
    overlay: &TextOverlay,
    trigram: u32,
) -> Result<Vec<Vec<u8>>, SegmentError> {
    let members = TextMembers::build(state, membership, segments.len(), overlay);
    paths_with_trigram_among(state, &members, segments, overlay, trigram)
}

/// The indexed paths of one checkout state, without those an overlay
/// replaces, grouped by the segment their content is in: what
/// [`paths_with_trigram_among`] reads for each trigram. Build it once for
/// many trigrams over the same state and overlay.
#[derive(Debug)]
pub struct TextMembers {
    segments: Vec<SegmentMembers>,
}

/// One segment's members among a checkout's paths.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SegmentMembers {
    /// (ordinal, path index), sorted. Paths with the same content share an
    /// ordinal.
    members: Vec<(u32, usize)>,
    /// The distinct ordinals of `members`, ascending.
    ordinals: Vec<u32>,
    /// By ordinal, the position in `members` of its first member, or
    /// [`SegmentMembers::NONE`]: a posting maps to its paths without a search.
    first: Vec<u32>,
}

impl SegmentMembers {
    const NONE: u32 = u32::MAX;

    fn new(mut members: Vec<(u32, usize)>) -> Self {
        members.sort_unstable();
        let mut ordinals = members
            .iter()
            .map(|(ordinal, _)| *ordinal)
            .collect::<Vec<_>>();
        ordinals.dedup();
        let len = ordinals.last().map_or(0, |last| *last as usize + 1);
        let mut first = vec![Self::NONE; len];
        for (position, (ordinal, _)) in members.iter().enumerate().rev() {
            first[*ordinal as usize] = position as u32;
        }
        Self {
            members,
            ordinals,
            first,
        }
    }

    /// Whether this checkout holds most of the ordinal range its members
    /// span, so walking a trigram's whole posting list costs no more than
    /// walking the members.
    fn dense(&self) -> bool {
        self.ordinals.len() * 4 >= self.first.len()
    }

    /// The path indexes holding `ordinal`.
    fn paths(&self, ordinal: u32) -> impl Iterator<Item = usize> + '_ {
        let start = self
            .first
            .get(ordinal as usize)
            .copied()
            .unwrap_or(Self::NONE);
        let members = if start == Self::NONE {
            &[][..]
        } else {
            &self.members[start as usize..]
        };
        members
            .iter()
            .take_while(move |(held, _)| *held == ordinal)
            .map(|(_, index)| *index)
    }
}

impl TextMembers {
    /// Groups the indexed paths of `state` across `segment_count` segments,
    /// leaving out the paths `overlay` replaces.
    #[must_use]
    pub fn build(
        state: &CheckoutState,
        membership: &TextMembership,
        segment_count: usize,
        overlay: &TextOverlay,
    ) -> Self {
        let mut members = vec![Vec::<(u32, usize)>::new(); segment_count];
        for (index, record) in state.paths().iter().enumerate() {
            if overlay.contains(&record.path) {
                continue;
            }
            if let Place::Indexed { segment, ordinal } = membership.places[index] {
                members[segment as usize].push((ordinal, index));
            }
        }
        let segments = members.into_iter().map(SegmentMembers::new).collect();
        Self { segments }
    }
}

/// [`paths_with_trigram`], with the paths grouped once in `members`, which
/// was built from the same `state`, membership and `overlay`.
///
/// # Errors
///
/// When a segment cannot be read.
pub fn paths_with_trigram_among(
    state: &CheckoutState,
    members: &TextMembers,
    segments: &[TextSegment<'_>],
    overlay: &TextOverlay,
    trigram: u32,
) -> Result<Vec<Vec<u8>>, SegmentError> {
    let holders = trigram_holders_among(members, segments, overlay, trigram)?;
    let mut found = holders
        .published
        .iter()
        .map(|&index| state.paths()[index].path.clone())
        .chain(holders.changed)
        .collect::<Vec<_>>();
    found.sort_unstable();
    found.dedup();
    Ok(found)
}

/// The paths of one checkout state, with an overlay's changes, whose content
/// holds a trigram, without copying a path: [`TrigramHolders::published`]
/// are positions in the state's path list and [`TrigramHolders::changed`]
/// the overlay's paths. A published path the overlay changed is only among
/// the changed ones, so no path is in both.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrigramHolders {
    /// Positions in the state's path list, in no particular order.
    pub published: Vec<usize>,
    /// The overlay's paths, in byte order.
    pub changed: Vec<Vec<u8>>,
}

impl TrigramHolders {
    /// How many paths hold the trigram.
    #[must_use]
    pub fn len(&self) -> usize {
        self.published.len() + self.changed.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// [`paths_with_trigram_among`] as positions and changed paths (see
/// [`TrigramHolders`]), with `members` built from the same state, membership
/// and `overlay`.
///
/// # Errors
///
/// When a segment cannot be read.
pub fn trigram_holders_among(
    members: &TextMembers,
    segments: &[TextSegment<'_>],
    overlay: &TextOverlay,
    trigram: u32,
) -> Result<TrigramHolders, SegmentError> {
    let mut published = Vec::new();
    for (segment, members) in members.segments.iter().enumerate() {
        if members.members.is_empty() {
            continue;
        }
        // Where this checkout holds most of the segment, one pass over the
        // trigram's postings maps each to its paths. Where it holds a few
        // (a segment the family's other checkouts mostly fill), the postings
        // are skipped to its members instead.
        let hits = if members.dense() {
            segments[segment].all_ordinals(trigram)?
        } else {
            segments[segment].ordinals(trigram, 0, &members.ordinals)?
        };
        for ordinal in hits {
            published.extend(members.paths(ordinal));
        }
    }
    let changed = overlay
        .entries
        .iter()
        .filter(|(_, entry)| {
            matches!(entry, OverlayEntry::Content(trigrams) if trigrams.get(trigram).is_some())
        })
        .map(|(path, _)| path.clone())
        .collect();
    Ok(TrigramHolders { published, changed })
}

/// What the text plane holds for the published path at position `index` of
/// `state`, which the overlay has not changed.
///
/// # Errors
///
/// When a segment cannot be read.
pub fn published_path_text(
    membership: &TextMembership,
    segments: &[TextSegment<'_>],
    index: usize,
) -> Result<PathText, SegmentError> {
    Ok(match membership.places[index] {
        Place::Indexed { segment, ordinal } => PathText::Indexed {
            distinct: segments[segment as usize].unit(ordinal)?.distinct,
        },
        Place::Unchecked => PathText::Unindexed,
        Place::Excluded => PathText::Excluded,
    })
}

/// Which paths a search covers.
#[derive(Clone, Copy, Debug)]
pub enum Scope<'a> {
    All,
    /// The paths inside one directory, given without its trailing slash.
    Directory(&'a [u8]),
}

impl Scope<'_> {
    fn contains(&self, path: &[u8]) -> bool {
        match self {
            Self::All => true,
            Self::Directory(dir) => {
                dir.is_empty()
                    || (path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/')
            }
        }
    }
}

/// A path whose content changed in this process and is not published yet.
#[derive(Clone, Debug)]
pub enum OverlayEntry {
    Content(Arc<ContentTrigrams>),
    /// Changed to a content that is not indexed.
    Unindexed,
    Deleted,
}

/// The unpublished changes of one checkout, by path. Searches see them in
/// place of the state's records for the same paths.
#[derive(Clone, Debug, Default)]
pub struct TextOverlay {
    entries: BTreeMap<Vec<u8>, OverlayEntry>,
}

impl TextOverlay {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, path: Vec<u8>, entry: OverlayEntry) {
        self.entries.insert(path, entry);
    }

    pub fn remove(&mut self, path: &[u8]) {
        self.entries.remove(path);
    }

    /// The change recorded for `path`, if any.
    #[must_use]
    pub fn get(&self, path: &[u8]) -> Option<&OverlayEntry> {
        self.entries.get(path)
    }

    /// The changed paths, in byte order.
    pub fn paths(&self) -> impl Iterator<Item = &[u8]> {
        self.entries.keys().map(Vec::as_slice)
    }

    /// Keeps only the paths `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(&[u8]) -> bool) {
        self.entries.retain(|path, _| keep(path));
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn contains(&self, path: &[u8]) -> bool {
        self.entries.contains_key(path)
    }
}

impl TextOverlay {
    /// How many paths the overlay holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// An estimate of the bytes the overlay holds in memory, for the memory
    /// census: each path, its map entry, and the trigrams of each changed
    /// content. A content shared by several paths is counted once per path,
    /// so the estimate errs high.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        // A B-tree's per-entry overhead varies with its fill; one pointer
        // and one length per entry is a fair middle.
        const MAP_ENTRY_OVERHEAD: usize = 2 * std::mem::size_of::<usize>();
        let bytes = self
            .entries
            .iter()
            .map(|(path, entry)| {
                let content = match entry {
                    OverlayEntry::Content(trigrams) => {
                        // The shared allocation's two counters, then the list.
                        2 * std::mem::size_of::<usize>()
                            + std::mem::size_of::<ContentTrigrams>()
                            + std::mem::size_of_val(trigrams.entries())
                    }
                    OverlayEntry::Unindexed | OverlayEntry::Deleted => 0,
                };
                std::mem::size_of::<Vec<u8>>()
                    + path.capacity()
                    + std::mem::size_of::<OverlayEntry>()
                    + MAP_ENTRY_OVERHEAD
                    + content
            })
            .sum::<usize>();
        u64::try_from(bytes).unwrap_or(u64::MAX)
    }
}

/// A conjunct whose posting list is this many times longer than the
/// candidates left is not read: it would narrow them little, and verifying a
/// few extra files costs less than reading a long list. Leaving out a
/// conjunct only widens the result, which stays a superset.
const SKIP_FACTOR: u64 = 16;

/// The paths of `state` in `scope`, and the overlay's paths in scope, that
/// may match `query`, in byte order.
///
/// # Errors
///
/// When a segment cannot be read.
pub fn candidates(
    state: &CheckoutState,
    membership: &TextMembership,
    segments: &[TextSegment<'_>],
    overlay: &TextOverlay,
    query: &Query,
    scope: Scope<'_>,
) -> Result<Vec<Vec<u8>>, SegmentError> {
    search(
        state,
        membership,
        segments,
        overlay,
        query,
        scope,
        Some(SKIP_FACTOR),
    )
}

fn search(
    state: &CheckoutState,
    membership: &TextMembership,
    segments: &[TextSegment<'_>],
    overlay: &TextOverlay,
    query: &Query,
    scope: Scope<'_>,
    skip_factor: Option<u64>,
) -> Result<Vec<Vec<u8>>, SegmentError> {
    if *query == Query::None {
        return Ok(Vec::new());
    }
    let range = match scope {
        Scope::All => 0..state.paths().len(),
        Scope::Directory(dir) => state.directory(dir),
    };
    let mut members = vec![Vec::<(u32, usize)>::new(); segments.len()];
    let mut found = Vec::new();
    for index in range {
        let path = &state.paths()[index].path;
        if overlay.contains(path) {
            continue;
        }
        match membership.places[index] {
            Place::Indexed { segment, ordinal } => members[segment as usize].push((ordinal, index)),
            Place::Unchecked => found.push(path.clone()),
            Place::Excluded => {}
        }
    }
    for (segment, mut members) in members.into_iter().enumerate() {
        if members.is_empty() {
            continue;
        }
        members.sort_unstable();
        let mut ordinals = members
            .iter()
            .map(|(ordinal, _)| *ordinal)
            .collect::<Vec<_>>();
        ordinals.dedup();
        let hits = evaluate(&segments[segment], query, &ordinals, skip_factor)?;
        let mut hits = hits.into_iter().peekable();
        for (ordinal, index) in members {
            while hits.peek().is_some_and(|hit| *hit < ordinal) {
                hits.next();
            }
            if hits.peek() == Some(&ordinal) {
                found.push(state.paths()[index].path.clone());
            }
        }
    }
    for (path, entry) in &overlay.entries {
        if !scope.contains(path) {
            continue;
        }
        let candidate = match entry {
            OverlayEntry::Content(trigrams) => query.matches(trigrams),
            OverlayEntry::Unindexed => true,
            OverlayEntry::Deleted => false,
        };
        if candidate {
            found.push(path.clone());
        }
    }
    found.sort_unstable();
    found.dedup();
    Ok(found)
}

/// The ordinals in `within` (ascending) whose content satisfies `query`.
fn evaluate(
    segment: &TextSegment<'_>,
    query: &Query,
    within: &[u32],
    skip_factor: Option<u64>,
) -> Result<Vec<u32>, SegmentError> {
    Ok(match query {
        Query::None => Vec::new(),
        Query::All => within.to_vec(),
        Query::Trigram { trigram, next } => segment.ordinals(*trigram, *next, within)?,
        Query::And(parts) => {
            // Rarest trigram first; compound parts after every trigram.
            let mut ordered = parts
                .iter()
                .map(|part| {
                    let cost = match part {
                        Query::Trigram { trigram, .. } => u64::from(segment.count(*trigram)?),
                        _ => u64::MAX,
                    };
                    Ok((cost, part))
                })
                .collect::<Result<Vec<_>, SegmentError>>()?;
            ordered.sort_by_key(|(cost, _)| *cost);
            let mut current = within.to_vec();
            for (cost, part) in ordered {
                if current.is_empty() {
                    break;
                }
                if let Some(factor) = skip_factor
                    && current.len() < within.len()
                    && cost != u64::MAX
                    && cost > factor * current.len() as u64
                {
                    // Every later trigram is at least as long.
                    break;
                }
                current = evaluate(segment, part, &current, skip_factor)?;
            }
            current
        }
        Query::Or(parts) => {
            let mut union = Vec::new();
            for part in parts {
                union.extend(evaluate(segment, part, within, skip_factor)?);
            }
            union.sort_unstable();
            union.dedup();
            union
        }
    })
}

crate::heap_bytes_none!(Place, Located);
crate::heap_bytes_of_fields!(TextMembership { places, missing });
crate::heap_bytes_of_fields!(TextMembers { segments });
crate::heap_bytes_of_fields!(SegmentMembers {
    members,
    ordinals,
    first,
});
crate::heap_bytes_of_fields!(TextOverlay { entries });

impl crate::heap::HeapBytes for OverlayEntry {
    fn heap_bytes(&self, seen: &mut crate::heap::Seen) -> usize {
        match self {
            Self::Content(trigrams) => crate::heap::HeapBytes::heap_bytes(trigrams, seen),
            Self::Unindexed | Self::Deleted => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::cache::BlockCache;
    use crate::checkout::{PathRecord, StatEvidence};
    use crate::ids::{Digest128, FamilyId, SegmentId, WriterIncarnation};
    use crate::segment::{BLOCK_LEN, Segment, SegmentMeta, SegmentWriter};
    use crate::test_rng::TestRng;
    use crate::text::plan::plan_pattern;
    use crate::text::sections::{KNOWN, TextSectionsBuilder};

    const WORDS: &[&str] = &[
        "fn",
        "foo",
        "Foo",
        "bar",
        "baz",
        "get_value",
        "set_value",
        "Kind",
        "kind",
        "é",
        "\n",
        " ",
        "(",
        ")",
        "x",
        "load_config",
        "ConfigLoader",
        "\u{212A}ind",
    ];
    const PATTERNS: &[&str] = &[
        "foo",
        "(?i)foo",
        "get_value|set_value",
        r"\b(get|set)_value\b",
        "load.*config",
        "(?i)configloader",
        "Kind",
        "(?i)kind",
        "bar|baz|qux",
        "[Ff]oo bar",
        "é",
        ".",
        "zzz",
        "fn foo",
        r"\w+_value",
    ];

    fn family() -> FamilyId {
        FamilyId::from_digest(Digest128::of(b"repo"))
    }

    fn random_text(rng: &mut TestRng) -> String {
        (0..rng.below_usize(30))
            .map(|_| WORDS[rng.below_usize(WORDS.len())])
            .collect()
    }

    /// Writes one segment holding `contents`; the first content of each
    /// tenth is recorded as not indexed.
    fn write(dir: &Path, id: u64, contents: &[String]) -> Segment {
        let path = dir.join(format!("{id}.seg"));
        let mut writer = SegmentWriter::create(
            &path,
            SegmentMeta {
                family: family(),
                segment: SegmentId::new(id),
                incarnation: WriterIncarnation::new(1),
                created_unix_ms: 0,
            },
        )
        .unwrap();
        let mut builder = TextSectionsBuilder::new();
        for (index, text) in contents.iter().enumerate() {
            let key = ContentKey::of(text.as_bytes());
            if index.is_multiple_of(10) {
                builder.add_unindexed(key, text.len() as u64, TextStatus::Oversize);
            } else {
                builder.add_indexed(
                    key,
                    text.len() as u64,
                    &ContentTrigrams::of(text.as_bytes()),
                );
            }
        }
        builder.write(&mut writer).unwrap();
        writer.finish().unwrap();
        Segment::open(
            &path,
            family(),
            &KNOWN,
            Arc::new(BlockCache::new(16 * BLOCK_LEN)),
        )
        .unwrap()
    }

    /// What a path holds, as the test knows it independently of the index.
    enum Expect {
        /// Indexed content: a candidate exactly when it satisfies the query.
        Indexed(String),
        /// Not indexed or not in any segment: always a candidate, and
        /// verified against these bytes.
        Unchecked(String),
    }

    #[test]
    fn candidates_are_exactly_the_query_and_cover_every_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut rng = TestRng::new(11);
        let mut seen = BTreeSet::new();
        let mut contents = Vec::new();
        while contents.len() < 400 {
            let text = random_text(&mut rng);
            if seen.insert(text.clone()) {
                contents.push(text);
            }
        }
        // Two overlapping segments; contents 380.. are in neither. Within a
        // segment every tenth content is recorded as not indexed, and the
        // overlap starts on a multiple of ten, so both segments agree.
        let first = write(dir.path(), 1, &contents[..250]);
        let second = write(dir.path(), 2, &contents[200..380]);
        let segments =
            [&first, &second].map(|segment| TextSegment::open(segment).unwrap().unwrap());

        let mut expect = BTreeMap::new();
        let records = (0..1000)
            .map(|index| {
                let which = rng.below_usize(contents.len());
                let text = contents[which].clone();
                let path = format!("dir{}/f{index}.rs", index % 4).into_bytes();
                let entry = if which >= 380 || which.is_multiple_of(10) {
                    Expect::Unchecked(text.clone())
                } else {
                    Expect::Indexed(text.clone())
                };
                expect.insert(path.clone(), entry);
                PathRecord {
                    path,
                    mode: 0,
                    kind: PathKind::Text,
                    content: ContentKey::of(text.as_bytes()),
                    link: Vec::new(),
                    stat: StatEvidence::default(),
                    language: String::new(),
                }
            })
            .collect();
        let state = CheckoutState::new(Vec::new(), Digest128::of(b""), records).unwrap();
        let membership = TextMembership::build(&state, &TextLocator::build(&segments).unwrap());
        assert!(membership.missing() > 0);

        // An edit, a deletion and a new file that is not indexed.
        let mut overlay = TextOverlay::new();
        let edited = random_text(&mut rng);
        overlay.set(
            b"dir1/f1.rs".to_vec(),
            OverlayEntry::Content(Arc::new(ContentTrigrams::of(edited.as_bytes()))),
        );
        expect.insert(b"dir1/f1.rs".to_vec(), Expect::Indexed(edited));
        overlay.set(b"dir2/f2.rs".to_vec(), OverlayEntry::Deleted);
        expect.remove(&b"dir2/f2.rs"[..]);
        overlay.set(b"dir3/new.rs".to_vec(), OverlayEntry::Unindexed);
        expect.insert(
            b"dir3/new.rs".to_vec(),
            Expect::Unchecked("fn foo".to_string()),
        );

        let mut nonempty = 0;
        for pattern in PATTERNS {
            let query = plan_pattern(pattern, false).unwrap();
            let regex = regex::bytes::Regex::new(pattern).unwrap();
            for scope in [
                Scope::All,
                Scope::Directory(b"dir1"),
                Scope::Directory(b"dir3"),
            ] {
                let exact = search(
                    &state,
                    &membership,
                    &segments,
                    &overlay,
                    &query,
                    scope,
                    None,
                )
                .unwrap();
                let fast =
                    candidates(&state, &membership, &segments, &overlay, &query, scope).unwrap();
                let mut expected = Vec::new();
                for (path, entry) in &expect {
                    if !scope.contains(path) {
                        continue;
                    }
                    let (text, candidate) = match entry {
                        Expect::Indexed(text) => {
                            (text, query.matches(&ContentTrigrams::of(text.as_bytes())))
                        }
                        Expect::Unchecked(text) => (text, query != Query::None),
                    };
                    if candidate {
                        expected.push(path.clone());
                    }
                    if regex.is_match(text.as_bytes()) {
                        assert!(
                            fast.contains(path),
                            "{pattern:?}: {} matches and is not a candidate",
                            String::from_utf8_lossy(path)
                        );
                    }
                }
                nonempty += usize::from(!expected.is_empty());
                assert_eq!(exact, expected, "{pattern:?} in {scope:?}");
                assert!(fast.iter().all(|path| scope.contains(path)));
                assert!(
                    exact.iter().all(|path| fast.contains(path)),
                    "{pattern:?}: skipping a conjunct lost a candidate"
                );
            }
        }
        assert!(nonempty > PATTERNS.len(), "too few searches found anything");
    }
}
