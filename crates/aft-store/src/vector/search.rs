//! Exact meaning search over the vectors of one model.
//!
//! A search scans the int8 codes of the members' vectors, which gives every
//! member an interval its exact score is proven to lie in. A member whose
//! upper bound is below the k-th highest lower bound cannot reach the top
//! k, so only the rest (the guaranteed set) can. Those are rescored from
//! their f32 rows with today's exact arithmetic, highest upper bound first,
//! stopping once no remaining bound can reach the k-th exact score. The
//! answer is today's f32 brute-force answer by construction, not by a tuned
//! window. When the guaranteed set is too large to be worth reading row by
//! row, the search reads every member's f32 row instead, which is just as
//! exact, and says so.

use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{BinaryHeap, HashMap};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use super::VectorError;
use super::model::{ModelId, ModelIdentity, TextDigest};
use super::score::{QueryTerms, approx_kernel, dot_product, rank_order, score};
use super::sections::{
    Chunk, FileDetails, VectorSegment, VectorSegmentParts, f32s, scan_codes, scan_params,
    scan_row_len,
};
use crate::ids::ContentKey;
use crate::segment::{BLOCK_LEN, Segment};

/// Bytes of scan rows or f32 rows read per call while scanning: one block,
/// so the copy stays in the core's own cache while the scan reads it.
const READ_BYTES: usize = BLOCK_LEN;

/// Where one vector is: a segment of a [`VectorIndex`] and the vector's
/// ordinal within it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct UnitRef {
    /// Position of the segment in the index's segment list.
    pub segment: u32,
    pub ordinal: u32,
}

/// The vectors of one model across a family's segments.
///
/// Built once per segment set; every search and every checkout reading the
/// same segments shares it, and its segments read through the shared block
/// cache.
#[derive(Debug)]
pub struct VectorIndex<'s> {
    token: u64,
    model: ModelIdentity,
    model_id: ModelId,
    segments: Vec<VectorSegment<'s>>,
    /// Shared with every index lent by the same [`SharedVectorIndex`].
    by_text: Arc<HashMap<TextDigest, UnitRef>>,
}

impl<'s> VectorIndex<'s> {
    /// The vectors of `model` in `segments`, which are in the order a
    /// snapshot lists them: by ascending id, so newer segments come later.
    /// Segments of other models are left out.
    ///
    /// # Errors
    ///
    /// When a segment cannot be read.
    pub fn build(
        segments: impl IntoIterator<Item = VectorSegment<'s>>,
        model: &ModelIdentity,
    ) -> Result<Self, VectorError> {
        static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
        let segments: Vec<_> = segments
            .into_iter()
            .filter(|segment| segment.model() == model)
            .collect();
        if u32::try_from(segments.len()).is_err() {
            return Err(VectorError::TooManySegments);
        }
        let mut by_text = HashMap::new();
        for (index, segment) in segments.iter().enumerate() {
            let index = u32::try_from(index).expect("checked above");
            for (ordinal, text) in segment.texts()?.into_iter().enumerate() {
                let ordinal = u32::try_from(ordinal).expect("a segment holds below 2^32 units");
                // A text embedded again in a newer segment is found there.
                by_text.insert(
                    text,
                    UnitRef {
                        segment: index,
                        ordinal,
                    },
                );
            }
        }
        Ok(Self {
            token: NEXT_TOKEN.fetch_add(1, AtomicOrdering::Relaxed),
            model: model.clone(),
            model_id: model.id(),
            segments,
            by_text: Arc::new(by_text),
        })
    }

    #[must_use]
    pub fn model(&self) -> &ModelIdentity {
        &self.model
    }

    /// How many distinct vectors of the model the segments hold. Zero means
    /// no vectors for this model at all, which a search reports as
    /// [`VectorError::NoVectors`] rather than as an empty answer.
    #[must_use]
    pub fn vectors(&self) -> usize {
        self.by_text.len()
    }

    #[must_use]
    pub fn segments(&self) -> &[VectorSegment<'s>] {
        &self.segments
    }

    /// Where the vector embedded for `text` is.
    #[must_use]
    pub fn locate(&self, text: TextDigest) -> Option<UnitRef> {
        self.by_text.get(&text).copied()
    }

    /// The stored f32 vector at `unit`.
    ///
    /// # Errors
    ///
    /// When `unit` is not in the index, or a read fails.
    pub fn vector(&self, unit: UnitRef) -> Result<Vec<f32>, VectorError> {
        let segment = self.segment_of(unit)?;
        Ok(segment.vector(unit.ordinal)?)
    }

    /// The chunks of the file at `path` with `content`, in chunk order, each
    /// with its vector if one is stored. `None` when no segment has a chunk
    /// map for the file: its chunks were never embedded under this model.
    ///
    /// # Errors
    ///
    /// When a read fails or a map is malformed.
    pub fn chunk_map(
        &self,
        content: ContentKey,
        path: &[u8],
    ) -> Result<Option<Vec<MappedChunk>>, VectorError> {
        for segment in self.segments.iter().rev() {
            if let Some(chunks) = segment.chunk_map(content, path)? {
                return Ok(Some(
                    chunks
                        .into_iter()
                        .map(|chunk| MappedChunk {
                            chunk,
                            unit: self.locate(chunk.text),
                        })
                        .collect(),
                ));
            }
        }
        Ok(None)
    }

    /// The details recorded with the chunk map [`chunk_map`](Self::chunk_map)
    /// returns for the same file: `None` when no segment has a map for it,
    /// or its map was stored without details.
    ///
    /// # Errors
    ///
    /// When a read fails or the details are malformed.
    pub fn chunk_details(
        &self,
        content: ContentKey,
        path: &[u8],
    ) -> Result<Option<FileDetails>, VectorError> {
        for segment in self.segments.iter().rev() {
            if segment.chunk_map(content, path)?.is_some() {
                return Ok(segment.chunk_details(content, path)?);
            }
        }
        Ok(None)
    }

    /// The members of a set of files, in the order given and then in chunk
    /// order, with how much of them has vectors.
    ///
    /// # Errors
    ///
    /// When a read fails or a map is malformed.
    pub fn file_members<'p>(
        &self,
        files: impl IntoIterator<Item = (ContentKey, &'p [u8])>,
    ) -> Result<FileMembers, VectorError> {
        let mut found = FileMembers::default();
        for (content, path) in files {
            found.coverage.files += 1;
            let Some(chunks) = self.chunk_map(content, path)? else {
                continue;
            };
            found.coverage.files_mapped += 1;
            found.coverage.chunks += chunks.len();
            for MappedChunk { chunk, unit } in chunks {
                if let Some(unit) = unit {
                    found.coverage.chunks_embedded += 1;
                    found.members.push(Member {
                        unit,
                        exported: chunk.exported,
                    });
                }
            }
        }
        Ok(found)
    }

    /// Prepares `members` for searching. Their order is the tie-break:
    /// among equal scores, the member listed first ranks first, as the entry
    /// enumerated first does in today's index.
    ///
    /// # Errors
    ///
    /// When a member names a vector the index does not have.
    pub fn member_set(&self, members: Vec<Member>) -> Result<MemberSet, VectorError> {
        if u32::try_from(members.len()).is_err() {
            return Err(VectorError::TooManyMembers);
        }
        for member in &members {
            self.segment_of(member.unit)?;
        }
        let mut order: Vec<u32> = (0..members.len())
            .map(|index| u32::try_from(index).expect("checked above"))
            .collect();
        order.sort_unstable_by_key(|index| members[*index as usize].unit);
        let mut slot_of = vec![0; members.len()];
        let mut slots: Vec<UnitRef> = Vec::new();
        for index in order {
            let unit = members[index as usize].unit;
            if slots.last() != Some(&unit) {
                slots.push(unit);
            }
            slot_of[index as usize] = u32::try_from(slots.len() - 1).expect("checked above");
        }
        let mut segment_slots = vec![0..0; self.segments.len()];
        let mut start = 0;
        while start < slots.len() {
            let segment = slots[start].segment;
            let end = start + slots[start..].partition_point(|unit| unit.segment == segment);
            segment_slots[segment as usize] = start..end;
            start = end;
        }
        Ok(MemberSet {
            index_token: self.token,
            members,
            slot_of,
            slots,
            segment_slots,
        })
    }

    /// The exact top `k` members for `request`: today's f32 scores, in
    /// today's order, ties broken by member position.
    ///
    /// # Errors
    ///
    /// [`VectorError::NoVectors`] when the segments hold no vector of this
    /// model; [`VectorError::DimensionMismatch`] or
    /// [`VectorError::NonFinite`] for a query the model cannot score;
    /// [`VectorError::ForeignMembers`] for a member set prepared by another
    /// index; otherwise when a read fails or a block fails its checksum.
    pub fn search(
        &self,
        members: &MemberSet,
        request: &SearchRequest<'_>,
    ) -> Result<SearchResult, VectorError> {
        let query = request.query;
        self.check(members, query)?;
        let mut report = SearchReport {
            members: members.members.len(),
            ..SearchReport::default()
        };
        let k = request.k.min(members.members.len());
        if k == 0 {
            return Ok(SearchResult {
                hits: Vec::new(),
                report,
            });
        }
        let terms = QueryTerms::new(query);
        let bounds = self.scan(members, &terms, query, &mut report)?;
        report.units_scanned = members.slots.len();
        let (lower, upper, bounded) = member_bounds(members, &terms, &bounds);
        if !bounded {
            report.full_scan = Some(FullScan::Unbounded);
            return self.full_scan(members, &terms, query, k, report);
        }

        // Every member below the k-th highest lower bound is beaten by at
        // least k members with strictly higher exact scores.
        let threshold = if k < lower.len() {
            let mut lows = lower.clone();
            lows.select_nth_unstable_by(k - 1, |a, b| b.total_cmp(a));
            lows[k - 1]
        } else {
            f32::NEG_INFINITY
        };
        let mut guaranteed: Vec<u32> = (0..members.members.len())
            .filter(|index| upper[*index] >= threshold)
            .map(|index| u32::try_from(index).expect("member count fits in u32"))
            .collect();
        report.guaranteed = guaranteed.len();
        let limit = request.rescore_limit.unwrap_or_else(|| {
            default_rescore_limit(members.slots.len(), self.model.dimensions as usize, k)
        });
        if guaranteed.len() > limit {
            report.full_scan = Some(FullScan::GuaranteedSetTooLarge { limit });
            return self.full_scan(members, &terms, query, k, report);
        }

        guaranteed.sort_unstable_by(|a, b| {
            upper[*b as usize]
                .total_cmp(&upper[*a as usize])
                .then(a.cmp(b))
        });
        let mut top: BinaryHeap<Ranked> = BinaryHeap::with_capacity(k + 1);
        let mut exact_dots: HashMap<u32, f32> = HashMap::new();
        let mut bytes = vec![0; query.len() * 4];
        for index in guaranteed {
            let position = index as usize;
            if top.len() == k
                && let Some(worst) = top.peek()
                && upper[position] < worst.score
            {
                break;
            }
            let slot = members.slot_of[position];
            let dot = match exact_dots.entry(slot) {
                Entry::Occupied(entry) => *entry.get(),
                Entry::Vacant(entry) => {
                    let unit = members.slots[slot as usize];
                    self.segments[unit.segment as usize].read_vector(unit.ordinal, &mut bytes)?;
                    report.rescored += 1;
                    report.f32_bytes_read += bytes.len() as u64;
                    *entry.insert(dot_product(query, &f32s(&bytes)))
                }
            };
            let denom = terms.norm * bounds[slot as usize].norm;
            let ranked = Ranked {
                score: score(dot, denom, members.members[position].exported),
                position: index,
            };
            if top.len() < k {
                top.push(ranked);
            } else if top
                .peek()
                .is_some_and(|worst| ranked.cmp(worst) == Ordering::Less)
            {
                top.pop();
                top.push(ranked);
            }
        }
        Ok(SearchResult {
            hits: top
                .into_sorted_vec()
                .into_iter()
                .map(|ranked| members.hit(ranked))
                .collect(),
            report,
        })
    }

    /// The interval each member's exact score is proven to lie in, from the
    /// int8 scan alone, in member order. A search excludes members by these
    /// same bounds; gates use this to check that the bounds hold.
    ///
    /// # Errors
    ///
    /// As [`search`](Self::search).
    pub fn bounds(
        &self,
        members: &MemberSet,
        query: &[f32],
    ) -> Result<Vec<ScoreBounds>, VectorError> {
        self.check(members, query)?;
        let terms = QueryTerms::new(query);
        let bounds = self.scan(members, &terms, query, &mut SearchReport::default())?;
        let (lower, upper, _) = member_bounds(members, &terms, &bounds);
        Ok(lower
            .into_iter()
            .zip(upper)
            .map(|(low, high)| ScoreBounds { low, high })
            .collect())
    }

    fn check(&self, members: &MemberSet, query: &[f32]) -> Result<(), VectorError> {
        if self.by_text.is_empty() {
            return Err(VectorError::NoVectors(self.model_id));
        }
        if members.index_token != self.token {
            return Err(VectorError::ForeignMembers);
        }
        if query.len() != self.model.dimensions as usize {
            return Err(VectorError::DimensionMismatch {
                expected: self.model.dimensions,
                found: query.len(),
            });
        }
        if query.iter().any(|value| !value.is_finite()) {
            return Err(VectorError::NonFinite);
        }
        Ok(())
    }

    /// Scans the int8 codes of every member's vector, in segment and
    /// ordinal order, giving each distinct vector bounds on its exact dot
    /// product with the query.
    fn scan(
        &self,
        members: &MemberSet,
        terms: &QueryTerms,
        query: &[f32],
        report: &mut SearchReport,
    ) -> Result<Vec<UnitBounds>, VectorError> {
        let dimensions = query.len();
        let row_len = scan_row_len(dimensions);
        let approx_dot = approx_kernel();
        let mut bounds = Vec::with_capacity(members.slots.len());
        let mut buf = Vec::new();
        for (segment, range) in self.segments.iter().zip(&members.segment_slots) {
            if range.is_empty() {
                continue;
            }
            let center = terms.center(query, segment.center());
            for_each_window(
                &members.slots[range.clone()],
                (READ_BYTES / row_len).max(1),
                |window| {
                    let first = window[0].ordinal;
                    let rows = (window[window.len() - 1].ordinal - first) as usize + 1;
                    buf.resize(rows * row_len, 0);
                    segment.read_scan_rows(first, &mut buf)?;
                    report.scan_bytes += buf.len() as u64;
                    for unit in window {
                        let at = (unit.ordinal - first) as usize * row_len;
                        let row = &buf[at..at + row_len];
                        let params = scan_params(row);
                        let approx = approx_dot(query, scan_codes(row, dimensions));
                        let (low, high) = terms.dot_bounds(approx, &params, &center);
                        bounds.push(UnitBounds {
                            low,
                            high,
                            norm: params.norm,
                        });
                    }
                    Ok::<_, VectorError>(())
                },
            )?;
        }
        Ok(bounds)
    }

    /// Today's brute force over the members' f32 rows, read in segment and
    /// ordinal order.
    fn full_scan(
        &self,
        members: &MemberSet,
        terms: &QueryTerms,
        query: &[f32],
        k: usize,
        mut report: SearchReport,
    ) -> Result<SearchResult, VectorError> {
        let row_bytes = query.len() * 4;
        let mut dots = Vec::with_capacity(members.slots.len());
        let mut norms = Vec::with_capacity(members.slots.len());
        let mut buf = Vec::new();
        for (segment, range) in self.segments.iter().zip(&members.segment_slots) {
            for_each_window(
                &members.slots[range.clone()],
                (READ_BYTES / row_bytes).max(1),
                |window| {
                    let first = window[0].ordinal;
                    let rows = (window[window.len() - 1].ordinal - first) as usize + 1;
                    buf.resize(rows * row_bytes, 0);
                    segment.read_vectors(first, &mut buf)?;
                    report.f32_bytes_read += buf.len() as u64;
                    for unit in window {
                        let at = (unit.ordinal - first) as usize * row_bytes;
                        let vector = f32s(&buf[at..at + row_bytes]);
                        dots.push(dot_product(query, &vector));
                        norms.push(super::score::vector_norm(&vector));
                    }
                    Ok::<_, VectorError>(())
                },
            )?;
        }
        report.rescored = members.slots.len();
        let mut scored: Vec<(f32, u32)> = members
            .members
            .iter()
            .zip(&members.slot_of)
            .enumerate()
            .map(|(index, (member, slot))| {
                let denom = terms.norm * norms[*slot as usize];
                (
                    score(dots[*slot as usize], denom, member.exported),
                    u32::try_from(index).expect("member count fits in u32"),
                )
            })
            .collect();
        let order = |a: &(f32, u32), b: &(f32, u32)| rank_order(*a, *b);
        if k < scored.len() {
            scored.select_nth_unstable_by(k, order);
            scored.truncate(k);
        }
        scored.sort_by(order);
        Ok(SearchResult {
            hits: scored
                .into_iter()
                .map(|(score, position)| members.hit(Ranked { score, position }))
                .collect(),
            report,
        })
    }

    fn segment_of(&self, unit: UnitRef) -> Result<&VectorSegment<'s>, VectorError> {
        self.segments
            .get(unit.segment as usize)
            .filter(|segment| unit.ordinal < segment.len())
            .ok_or(VectorError::UnknownUnit(unit))
    }
}

/// A [`VectorIndex`] that owns its segments, so it can be kept between
/// searches and shared by threads. Building it opens and checks every
/// segment and reads every vector key once; [`index`](Self::index) then
/// lends an index over the same open segments without reading anything.
/// Member sets prepared through one lent index serve every other.
#[derive(Debug)]
pub struct SharedVectorIndex {
    token: u64,
    model: ModelIdentity,
    model_id: ModelId,
    /// This model's segments, in the order given, each with what opening its
    /// vector sections found.
    segments: Vec<(Arc<Segment>, VectorSegmentParts)>,
    by_text: Arc<HashMap<TextDigest, UnitRef>>,
}

impl SharedVectorIndex {
    /// The vectors of `model` in `segments`, which are in the order a
    /// snapshot lists them (see [`VectorIndex::build`]). Segments without
    /// vector sections, or with another model's, are left out.
    ///
    /// # Errors
    ///
    /// When a segment cannot be read.
    pub fn build(
        segments: impl IntoIterator<Item = Arc<Segment>>,
        model: &ModelIdentity,
    ) -> Result<Self, VectorError> {
        Self::assemble(segments, model, None)
    }

    /// The vectors of the same model in `segments`, reusing what building
    /// this index read for each segment it already holds (the same open
    /// segment, not just the same file), so only the others are opened and
    /// checked. Member sets prepared for this index do not serve the new one.
    ///
    /// # Errors
    ///
    /// When a segment cannot be read.
    pub fn rebuild(
        &self,
        segments: impl IntoIterator<Item = Arc<Segment>>,
    ) -> Result<Self, VectorError> {
        Self::assemble(segments, &self.model, Some(self))
    }

    fn assemble(
        segments: impl IntoIterator<Item = Arc<Segment>>,
        model: &ModelIdentity,
        previous: Option<&Self>,
    ) -> Result<Self, VectorError> {
        let mut kept = Vec::new();
        for segment in segments {
            let reused = previous.and_then(|previous| {
                previous
                    .segments
                    .iter()
                    .find(|(held, _)| Arc::ptr_eq(held, &segment))
                    .map(|(_, parts)| parts.clone())
            });
            let parts = match reused {
                Some(parts) => parts,
                None => match VectorSegment::open(&segment)? {
                    Some(opened) if opened.model() == model => opened.parts(),
                    _ => continue,
                },
            };
            kept.push((segment, parts));
        }
        let (token, model_id, by_text) = {
            let index = VectorIndex::build(
                kept.iter().map(|(segment, parts)| reopened(segment, parts)),
                model,
            )?;
            (index.token, index.model_id, Arc::clone(&index.by_text))
        };
        Ok(Self {
            token,
            model: model.clone(),
            model_id,
            segments: kept,
            by_text,
        })
    }

    /// An index over the same open segments, sharing everything building
    /// this one read. Cheap: it reads nothing.
    #[must_use]
    pub fn index(&self) -> VectorIndex<'_> {
        VectorIndex {
            token: self.token,
            model: self.model.clone(),
            model_id: self.model_id,
            segments: self
                .segments
                .iter()
                .map(|(segment, parts)| reopened(segment, parts))
                .collect(),
            by_text: Arc::clone(&self.by_text),
        }
    }

    #[must_use]
    pub fn model(&self) -> &ModelIdentity {
        &self.model
    }

    /// How many distinct vectors of the model the segments hold.
    #[must_use]
    pub fn vectors(&self) -> usize {
        self.by_text.len()
    }

    /// The segments holding this model's vectors, in index order: a
    /// [`UnitRef::segment`] is a position in this list.
    pub fn segments(&self) -> impl Iterator<Item = &Arc<Segment>> {
        self.segments.iter().map(|(segment, _)| segment)
    }
}

/// `segment`'s vector sections from `parts`, which opening that very segment
/// produced.
fn reopened<'s>(segment: &'s Segment, parts: &VectorSegmentParts) -> VectorSegment<'s> {
    VectorSegment::reopen(segment, parts).expect("the parts came from opening this segment")
}

/// Each member's lower and upper score bound, and whether every bound is
/// finite. Today's score is a non-decreasing function of the dot product, so
/// the dot product's bounds, scored as today scores, bound the score.
fn member_bounds(
    members: &MemberSet,
    terms: &QueryTerms,
    bounds: &[UnitBounds],
) -> (Vec<f32>, Vec<f32>, bool) {
    let mut lower = Vec::with_capacity(members.members.len());
    let mut upper = Vec::with_capacity(members.members.len());
    let mut bounded = terms.norm.is_finite();
    for (member, slot) in members.members.iter().zip(&members.slot_of) {
        let unit = &bounds[*slot as usize];
        let denom = terms.norm * unit.norm;
        let low = score(unit.low, denom, member.exported);
        let high = score(unit.high, denom, member.exported);
        bounded &= high.is_finite() && low.is_finite() && denom.is_finite();
        lower.push(low);
        upper.push(high);
    }
    (lower, upper, bounded)
}

/// Splits ascending `units` of one segment into runs whose ordinals span at
/// most `rows`, and calls `each` with every run, so consecutive members are
/// read together and isolated ones alone.
fn for_each_window<E>(
    units: &[UnitRef],
    rows: usize,
    mut each: impl FnMut(&[UnitRef]) -> Result<(), E>,
) -> Result<(), E> {
    let mut start = 0;
    while start < units.len() {
        let first = units[start].ordinal as usize;
        let end =
            start + units[start..].partition_point(|unit| (unit.ordinal as usize) < first + rows);
        each(&units[start..end])?;
        start = end;
    }
    Ok(())
}

/// How many vectors a search rescores one row at a time before it reads
/// every member's f32 row instead. Rescoring reads whole 64 KiB blocks, so
/// once the candidates outnumber the blocks the members' rows span, they
/// touch most of those blocks anyway and one sequential pass costs no more.
#[must_use]
pub fn default_rescore_limit(units: usize, dimensions: usize, k: usize) -> usize {
    let blocks = (units * dimensions * 4).div_ceil(BLOCK_LEN);
    blocks.max(k.saturating_mul(4))
}

/// The interval a member's exact score is proven to lie in.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ScoreBounds {
    pub low: f32,
    pub high: f32,
}

/// Bounds on one vector's exact dot product with the query.
#[derive(Clone, Copy, Debug)]
struct UnitBounds {
    low: f32,
    high: f32,
    norm: f32,
}

/// An exact score and its member, ordered as today ranks them: a smaller
/// value ranks first.
#[derive(Clone, Copy, Debug)]
struct Ranked {
    score: f32,
    position: u32,
}

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Ranked {}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        rank_order((self.score, self.position), (other.score, other.position))
    }
}

/// One chunk of a file's chunk map, with where its vector is, if stored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MappedChunk {
    pub chunk: Chunk,
    /// `None` until the chunk's text is embedded under the model.
    pub unit: Option<UnitRef>,
}

/// One chunk that a search may return.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Member {
    pub unit: UnitRef,
    /// Whether today's exported-chunk boost applies.
    pub exported: bool,
}

/// Members prepared for searching one [`VectorIndex`]: the chunks a
/// checkout may return, in tie-break order. Built once per checkout state
/// and shared by its searches.
#[derive(Clone, Debug)]
pub struct MemberSet {
    index_token: u64,
    members: Vec<Member>,
    /// The distinct vector each member uses.
    slot_of: Vec<u32>,
    /// Distinct vectors, ascending.
    slots: Vec<UnitRef>,
    /// Each segment's run of `slots`.
    segment_slots: Vec<Range<usize>>,
}

impl MemberSet {
    #[must_use]
    pub fn members(&self) -> &[Member] {
        &self.members
    }

    /// How many distinct vectors the members use.
    #[must_use]
    pub fn units(&self) -> usize {
        self.slots.len()
    }

    fn hit(&self, ranked: Ranked) -> Hit {
        Hit {
            member: ranked.position,
            unit: self.members[ranked.position as usize].unit,
            score: ranked.score,
        }
    }
}

/// The members of a set of files, and how many of their chunks have
/// vectors.
#[derive(Clone, Debug, Default)]
pub struct FileMembers {
    pub members: Vec<Member>,
    pub coverage: Coverage,
}

/// How much of a set of files meaning search can see.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Coverage {
    pub files: usize,
    /// Files with a chunk map under the model.
    pub files_mapped: usize,
    /// Chunks of the mapped files.
    pub chunks: usize,
    /// Chunks whose vector is stored.
    pub chunks_embedded: usize,
}

/// What to search for.
#[derive(Clone, Copy, Debug)]
pub struct SearchRequest<'q> {
    pub query: &'q [f32],
    /// How many members to return.
    pub k: usize,
    /// The largest guaranteed set rescored row by row; `None` for
    /// [`default_rescore_limit`].
    pub rescore_limit: Option<usize>,
}

/// One member in the answer.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Hit {
    /// The member's position in its [`MemberSet`].
    pub member: u32,
    pub unit: UnitRef,
    /// Today's exact f32 score, bit for bit.
    pub score: f32,
}

/// Why a search read every member's f32 row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FullScan {
    /// More members could reach the top k than the rescore limit allows.
    GuaranteedSetTooLarge { limit: usize },
    /// A bound was not finite, so it could not exclude anything: a query or
    /// vector so large that its scores overflow.
    Unbounded,
}

/// The work one search did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SearchReport {
    pub members: usize,
    /// Distinct vectors whose int8 codes were scanned.
    pub units_scanned: usize,
    /// Bytes of scan rows read.
    pub scan_bytes: u64,
    /// Members that could reach the top k after the scan.
    pub guaranteed: usize,
    /// Distinct vectors whose f32 rows were read.
    pub rescored: usize,
    /// Bytes of f32 rows read.
    pub f32_bytes_read: u64,
    /// Set when the search fell back to reading every member's f32 row.
    pub full_scan: Option<FullScan>,
}

/// A search's answer and the work it took.
#[derive(Clone, Debug)]
pub struct SearchResult {
    /// Best first.
    pub hits: Vec<Hit>,
    pub report: SearchReport,
}

crate::heap_bytes_none!(UnitRef, Member);
crate::heap_bytes_of_fields!(SharedVectorIndex {
    token,
    model,
    model_id,
    segments,
    by_text,
});
crate::heap_bytes_of_fields!(MemberSet {
    index_token,
    members,
    slot_of,
    slots,
    segment_slots,
});
