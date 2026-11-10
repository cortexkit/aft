use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::*;
use crate::cache::BlockCache;
use crate::ids::{ContentKey, Digest128, FamilyId, SegmentId, WriterIncarnation};
use crate::segment::{BLOCK_LEN, KnownSection, Segment, SegmentMeta, SegmentWriter};
use crate::test_rng::TestRng;

fn family() -> FamilyId {
    FamilyId::from_digest(Digest128::of(b"vector tests"))
}

fn model(dimensions: u32) -> ModelIdentity {
    ModelIdentity {
        name: "test/minilm".to_string(),
        revision: "r1".to_string(),
        dimensions,
        normalized: true,
        text_template: 3,
        chunk_template: 2,
    }
}

/// Writes segments into a temporary directory and opens them.
struct Store {
    dir: tempfile::TempDir,
    cache: Arc<BlockCache>,
    next: u64,
}

impl Store {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            cache: Arc::new(BlockCache::new(64 * BLOCK_LEN)),
            next: 1,
        }
    }

    fn write(&mut self, builder: VectorSectionsBuilder) -> PathBuf {
        let path = self.dir.path().join(format!("{}.seg", self.next));
        let meta = SegmentMeta {
            family: family(),
            segment: SegmentId::new(self.next),
            incarnation: WriterIncarnation::new(1),
            created_unix_ms: 1_793_700_000_000,
        };
        self.next += 1;
        let mut writer = SegmentWriter::create(&path, meta).unwrap();
        builder.write(&mut writer).unwrap();
        writer.finish().unwrap();
        path
    }

    fn open(&self, path: &Path) -> Segment {
        Segment::open(path, family(), &KNOWN, Arc::clone(&self.cache)).unwrap()
    }
}

fn text(index: usize) -> TextDigest {
    TextDigest::of(format!("chunk text {index}").as_bytes())
}

fn uniform(rng: &mut TestRng) -> f32 {
    (rng.next_u64() >> 40) as f32 / 16_777_216.0 * 2.0 - 1.0
}

/// Unit vectors around a few shared directions, so many score close to one
/// another, as real embeddings of one repository do.
fn clustered(rng: &mut TestRng, count: usize, dimensions: usize) -> Vec<Vec<f32>> {
    let centers: Vec<Vec<f32>> = (0..4)
        .map(|_| (0..dimensions).map(|_| uniform(rng)).collect())
        .collect();
    (0..count)
        .map(|index| {
            let center = &centers[index % centers.len()];
            let raw: Vec<f32> = center.iter().map(|c| c + 0.6 * uniform(rng)).collect();
            let norm = raw.iter().map(|v| v * v).sum::<f32>().sqrt();
            raw.iter().map(|v| v / norm).collect()
        })
        .collect()
}

/// Today's `search_filtered`, copied from AFT's resident index, over the
/// given members: the reference every answer must equal bit for bit.
fn reference(
    vectors: &HashMap<UnitRef, Vec<f32>>,
    members: &[Member],
    query: &[f32],
    top_k: usize,
) -> Vec<(u32, f32)> {
    fn vector_norm(vector: &[f32]) -> f32 {
        vector.iter().map(|value| value * value).sum::<f32>().sqrt()
    }
    fn dot_product(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(a, b)| a * b).sum::<f32>()
    }
    fn semantic_score_order(a: &(f32, usize), b: &(f32, usize)) -> std::cmp::Ordering {
        let score_a = if a.0.is_finite() {
            a.0
        } else {
            f32::NEG_INFINITY
        };
        let score_b = if b.0.is_finite() {
            b.0
        } else {
            f32::NEG_INFINITY
        };
        if score_a == score_b {
            a.1.cmp(&b.1)
        } else {
            score_b.total_cmp(&score_a)
        }
    }
    let query_norm = vector_norm(query);
    let mut scored: Vec<(f32, usize)> = Vec::new();
    for (i, member) in members.iter().enumerate() {
        let vector = &vectors[&member.unit];
        let dot = dot_product(query, vector);
        let denom = query_norm * vector_norm(vector);
        let mut score = if denom == 0.0 { 0.0 } else { dot / denom };
        if member.exported {
            score *= if score >= 0.0 { 1.1 } else { 0.9 };
        }
        scored.push((score, i));
    }
    let keep = top_k.min(scored.len());
    if keep == 0 {
        return Vec::new();
    }
    if keep < scored.len() {
        scored.select_nth_unstable_by(keep, semantic_score_order);
        scored.truncate(keep);
    }
    scored.sort_by(semantic_score_order);
    scored
        .into_iter()
        .map(|(score, i)| (u32::try_from(i).unwrap(), score))
        .collect()
}

fn answer(result: &SearchResult) -> Vec<(u32, f32)> {
    result
        .hits
        .iter()
        .map(|hit| (hit.member, hit.score))
        .collect()
}

fn assert_same(found: &[(u32, f32)], expected: &[(u32, f32)], context: &str) {
    assert_eq!(found.len(), expected.len(), "{context}: lengths differ");
    for (rank, (found, expected)) in found.iter().zip(expected).enumerate() {
        assert_eq!(found.0, expected.0, "{context}: member at rank {rank}");
        assert_eq!(
            found.1.to_bits(),
            expected.1.to_bits(),
            "{context}: score bits at rank {rank}"
        );
    }
}

fn request(query: &[f32], k: usize) -> SearchRequest<'_> {
    SearchRequest {
        query,
        k,
        rescore_limit: None,
    }
}

/// A request that never falls back to a full scan, to test the bound.
fn bounded_request(query: &[f32], k: usize) -> SearchRequest<'_> {
    SearchRequest {
        query,
        k,
        rescore_limit: Some(usize::MAX),
    }
}

/// One segment holding `vectors`, keyed `text(0..)`, and its open index.
fn single_segment(
    store: &mut Store,
    dimensions: u32,
    vectors: &[Vec<f32>],
) -> (Segment, HashMap<UnitRef, Vec<f32>>) {
    let mut builder = VectorSectionsBuilder::new(model(dimensions)).unwrap();
    for (index, vector) in vectors.iter().enumerate() {
        builder.add_vector(text(index), vector).unwrap();
    }
    let path = store.write(builder);
    let segment = store.open(&path);
    let opened = VectorSegment::open(&segment).unwrap().unwrap();
    let mut by_unit = HashMap::new();
    for (index, vector) in vectors.iter().enumerate() {
        let ordinal = opened.find(text(index)).unwrap().unwrap();
        by_unit.insert(
            UnitRef {
                segment: 0,
                ordinal,
            },
            vector.clone(),
        );
    }
    drop(opened);
    (segment, by_unit)
}

fn index_of<'s>(segments: &'s [Segment], dimensions: u32) -> VectorIndex<'s> {
    VectorIndex::build(
        segments
            .iter()
            .map(|segment| VectorSegment::open(segment).unwrap().unwrap()),
        &model(dimensions),
    )
    .unwrap()
}

#[test]
fn vectors_and_chunk_maps_round_trip() {
    let mut store = Store::new();
    let mut rng = TestRng::new(7);
    // 70 dimensions: not a multiple of the 16-byte code padding.
    let vectors = clustered(&mut rng, 40, 70);
    let mut builder = VectorSectionsBuilder::new(model(70)).unwrap();
    for (index, vector) in vectors.iter().enumerate() {
        builder.add_vector(text(index), vector).unwrap();
    }
    // A second vector for an existing text is ignored.
    builder.add_vector(text(3), &vectors[9]).unwrap();
    let content = ContentKey::of(b"fn a() {}");
    let chunks = [
        Chunk {
            text: text(5),
            exported: true,
            failed: false,
        },
        Chunk {
            text: text(1),
            exported: false,
            failed: false,
        },
        Chunk {
            text: TextDigest::of(b"never embedded"),
            exported: false,
            failed: false,
        },
    ];
    builder.add_chunk_map(content, b"src/a.rs", &chunks);
    assert_eq!((builder.vectors(), builder.chunk_maps()), (40, 1));
    let path = store.write(builder);
    let segment = store.open(&path);
    segment.verify().unwrap();
    let opened = VectorSegment::open(&segment).unwrap().unwrap();
    assert_eq!(opened.model(), &model(70));
    assert_eq!(opened.len(), 40);
    // The center is the mean of the stored vectors, to f32 precision.
    for (dimension, center) in opened.center().iter().enumerate() {
        let mean = vectors.iter().map(|v| f64::from(v[dimension])).sum::<f64>() / 40.0;
        assert!(
            (f64::from(*center) - mean).abs() < 1e-6,
            "dimension {dimension}"
        );
    }
    let mut texts = opened.texts().unwrap();
    assert!(texts.is_sorted(), "units are in key order");
    texts.sort();
    let mut expected: Vec<_> = (0..40).map(text).collect();
    expected.sort();
    assert_eq!(texts, expected);
    for (index, vector) in vectors.iter().enumerate() {
        let ordinal = opened.find(text(index)).unwrap().unwrap();
        let stored = opened.vector(ordinal).unwrap();
        let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&stored), bits(vector), "vector {index}");
        // The scan row's norm is today's norm of the same vector.
        let mut row = vec![0; sections::scan_row_len(70)];
        opened.read_scan_rows(ordinal, &mut row).unwrap();
        assert_eq!(
            sections::scan_params(&row).norm.to_bits(),
            vector_norm(vector).to_bits()
        );
    }
    assert_eq!(opened.find(TextDigest::of(b"absent")).unwrap(), None);
    assert_eq!(
        opened.chunk_map(content, b"src/a.rs").unwrap(),
        Some(chunks.to_vec())
    );
    // The same content at another path is another file.
    assert_eq!(opened.chunk_map(content, b"src/b.rs").unwrap(), None);

    drop(opened);
    let segments = [segment];
    let index = index_of(&segments, 70);
    let map = index.chunk_map(content, b"src/a.rs").unwrap().unwrap();
    assert!(map[0].unit.is_some() && map[1].unit.is_some());
    assert_eq!(map[2].unit, None, "a chunk without a stored vector");
    let found = index
        .file_members([(content, b"src/a.rs".as_slice()), (content, b"src/c.rs")])
        .unwrap();
    assert_eq!(
        found.coverage,
        Coverage {
            files: 2,
            files_mapped: 1,
            chunks: 3,
            chunks_embedded: 2,
        }
    );
    assert_eq!(found.members.len(), 2);
    assert!(found.members[0].exported && !found.members[1].exported);
}

#[test]
fn every_scan_kernel_sums_the_same_bits() {
    let mut rng = TestRng::new(17);
    for dimensions in [1, 15, 16, 70, 384, 1027] {
        let query: Vec<f32> = (0..dimensions).map(|_| uniform(&mut rng)).collect();
        let codes = rng.bytes(dimensions);
        let portable = score::approx_dot(&query, &codes);
        let chosen = score::approx_kernel()(&query, &codes);
        assert_eq!(
            portable.to_bits(),
            chosen.to_bits(),
            "{dimensions} dimensions"
        );
    }
}

#[test]
fn model_identity_round_trips_and_every_field_counts() {
    let identity = model(384);
    assert_eq!(
        ModelIdentity::decode(&identity.encode()),
        Some(identity.clone())
    );
    let mut other = identity.clone();
    other.text_template += 1;
    assert_ne!(identity.id(), other.id());
    let mut other = identity.clone();
    other.revision.push('x');
    assert_ne!(identity.id(), other.id());
    let mut bytes = identity.encode();
    bytes.push(0);
    assert_eq!(ModelIdentity::decode(&bytes), None, "trailing bytes");
    assert_eq!(ModelIdentity::decode(&bytes[..10]), None, "truncated");
}

#[test]
fn builder_refuses_vectors_it_cannot_score() {
    assert!(matches!(
        VectorSectionsBuilder::new(model(0)),
        Err(VectorError::UnsupportedDimensions(0))
    ));
    let mut builder = VectorSectionsBuilder::new(model(3)).unwrap();
    assert!(matches!(
        builder.add_vector(text(0), &[1.0, 2.0]),
        Err(VectorError::DimensionMismatch {
            expected: 3,
            found: 2
        })
    ));
    assert!(matches!(
        builder.add_vector(text(0), &[1.0, f32::NAN, 0.0]),
        Err(VectorError::NonFinite)
    ));
    assert!(matches!(
        builder.add_vector(text(0), &[f32::MAX, f32::MAX, 0.0]),
        Err(VectorError::NonFinite)
    ));
}

#[test]
fn search_equals_brute_force_and_bounds_hold() {
    let mut store = Store::new();
    let mut rng = TestRng::new(11);
    let dimensions = 48;
    let vectors = clustered(&mut rng, 600, dimensions);
    let (segment, by_unit) = single_segment(&mut store, 48, &vectors);
    let segments = [segment];
    let index = index_of(&segments, 48);
    let mut units: Vec<UnitRef> = by_unit.keys().copied().collect();
    units.sort();
    let members: Vec<Member> = units
        .iter()
        .map(|unit| Member {
            unit: *unit,
            exported: rng.below(3) == 0,
        })
        .collect();
    let set = index.member_set(members.clone()).unwrap();
    let mut rescored = 0;
    for round in 0..40 {
        let query: Vec<f32> = if round % 2 == 0 {
            clustered(&mut rng, 1, dimensions).remove(0)
        } else {
            // Not unit length, as remote backends may return.
            (0..dimensions).map(|_| 3.0 * uniform(&mut rng)).collect()
        };
        for k in [1, 10, 101] {
            let result = index.search(&set, &bounded_request(&query, k)).unwrap();
            assert_same(
                &answer(&result),
                &reference(&by_unit, &members, &query, k),
                &format!("round {round}, k {k}"),
            );
            assert_eq!(result.report.full_scan, None);
            assert_eq!(result.report.units_scanned, 600);
            assert!(result.report.rescored <= result.report.guaranteed);
            rescored += result.report.rescored;
        }
        let bounds = index.bounds(&set, &query).unwrap();
        for (position, (member, bound)) in members.iter().zip(&bounds).enumerate() {
            let exact = score(
                dot_product(&query, &by_unit[&member.unit]),
                vector_norm(&query) * vector_norm(&by_unit[&member.unit]),
                member.exported,
            );
            assert!(
                bound.low <= exact && exact <= bound.high,
                "round {round}, member {position}: {exact} outside {bound:?}"
            );
        }
    }
    assert!(
        rescored < 40 * 3 * 600 / 4,
        "the bound should exclude most members: {rescored} rescored"
    );
}

#[test]
fn members_outside_the_set_are_never_returned() {
    let mut store = Store::new();
    let mut rng = TestRng::new(5);
    let mut vectors = clustered(&mut rng, 200, 32);
    let query = vectors[0].clone();
    // Copies of the query score highest, but are not members.
    for _ in 0..5 {
        vectors.push(query.clone());
    }
    let (segment, by_unit) = single_segment(&mut store, 32, &vectors);
    let segments = [segment];
    let index = index_of(&segments, 32);
    let excluded: Vec<UnitRef> = std::iter::once(0)
        .chain(200..205)
        .map(|i| index.locate(text(i)).unwrap())
        .collect();
    let members: Vec<Member> = (1..200)
        .rev()
        .map(|i| Member {
            unit: index.locate(text(i)).unwrap(),
            exported: false,
        })
        .collect();
    let set = index.member_set(members.clone()).unwrap();
    let result = index.search(&set, &request(&query, 20)).unwrap();
    assert!(result.hits.iter().all(|hit| !excluded.contains(&hit.unit)));
    assert_same(
        &answer(&result),
        &reference(&by_unit, &members, &query, 20),
        "subset",
    );
    assert_eq!(result.report.members, 199);
    assert_eq!(result.report.units_scanned, 199);
}

#[test]
fn equal_scores_rank_by_member_position() {
    let mut store = Store::new();
    let mut rng = TestRng::new(3);
    let base = clustered(&mut rng, 1, 16).remove(0);
    let mut vectors = vec![base.clone(); 30];
    // Zero vectors score exactly zero, whatever the query.
    vectors.extend(vec![vec![0.0; 16]; 10]);
    vectors.extend(clustered(&mut rng, 30, 16));
    let (segment, by_unit) = single_segment(&mut store, 16, &vectors);
    let segments = [segment];
    let index = index_of(&segments, 16);
    let mut members = Vec::new();
    for round in 0..3 {
        for i in (0..vectors.len()).rev() {
            members.push(Member {
                unit: index.locate(text(i)).unwrap(),
                // The same vector shared by members that differ only in
                // the boost, listed three times over.
                exported: (i + round) % 4 == 0,
            });
        }
    }
    let set = index.member_set(members.clone()).unwrap();
    assert_eq!(set.units(), vectors.len());
    let negative: Vec<f32> = base.iter().map(|v| -v).collect();
    for query in [&base, &negative] {
        for k in [5, 31, 89, 150, members.len()] {
            let result = index.search(&set, &request(query, k)).unwrap();
            assert_same(
                &answer(&result),
                &reference(&by_unit, &members, query, k),
                &format!("k {k}"),
            );
        }
    }
    // Every member of a zero query scores zero, so the order is positions.
    let zero = vec![0.0; 16];
    let result = index.search(&set, &request(&zero, 12)).unwrap();
    assert_same(
        &answer(&result),
        &reference(&by_unit, &members, &zero, 12),
        "zero query",
    );
    assert_eq!(
        result.hits.iter().map(|hit| hit.member).collect::<Vec<_>>(),
        (0..12).collect::<Vec<_>>()
    );
}

#[test]
fn a_large_guaranteed_set_falls_back_to_a_full_scan() {
    let mut store = Store::new();
    let mut rng = TestRng::new(9);
    let vectors = clustered(&mut rng, 300, 24);
    let (segment, by_unit) = single_segment(&mut store, 24, &vectors);
    let segments = [segment];
    let index = index_of(&segments, 24);
    let mut units: Vec<UnitRef> = by_unit.keys().copied().collect();
    units.sort();
    let members: Vec<Member> = units
        .iter()
        .map(|unit| Member {
            unit: *unit,
            exported: unit.ordinal % 2 == 0,
        })
        .collect();
    let set = index.member_set(members.clone()).unwrap();
    let query = clustered(&mut rng, 1, 24).remove(0);
    let expected = reference(&by_unit, &members, &query, 25);
    let bounded = index.search(&set, &bounded_request(&query, 25)).unwrap();
    assert_eq!(bounded.report.full_scan, None);
    let limited = index
        .search(
            &set,
            &SearchRequest {
                query: &query,
                k: 25,
                rescore_limit: Some(bounded.report.guaranteed - 1),
            },
        )
        .unwrap();
    assert_eq!(
        limited.report.full_scan,
        Some(FullScan::GuaranteedSetTooLarge {
            limit: bounded.report.guaranteed - 1
        })
    );
    assert_eq!(limited.report.rescored, 300);
    assert_eq!(limited.report.f32_bytes_read, 300 * 24 * 4);
    assert_same(&answer(&limited), &expected, "full scan");
    assert_same(&answer(&bounded), &expected, "bounded");

    // Identical vectors leave the bound nothing to exclude.
    let mut store = Store::new();
    let same = vec![vectors[0].clone(); 400];
    let mut builder = VectorSectionsBuilder::new(model(24)).unwrap();
    for (i, vector) in same.iter().enumerate() {
        builder.add_vector(text(i), vector).unwrap();
    }
    let path = store.write(builder);
    let segments = [store.open(&path)];
    let index = index_of(&segments, 24);
    let members: Vec<Member> = (0..400)
        .map(|i| Member {
            unit: index.locate(text(i)).unwrap(),
            exported: false,
        })
        .collect();
    let by_unit: HashMap<UnitRef, Vec<f32>> = members
        .iter()
        .map(|m| (m.unit, vectors[0].clone()))
        .collect();
    let set = index.member_set(members.clone()).unwrap();
    let result = index.search(&set, &request(&query, 10)).unwrap();
    assert!(matches!(
        result.report.full_scan,
        Some(FullScan::GuaranteedSetTooLarge { .. })
    ));
    assert_same(
        &answer(&result),
        &reference(&by_unit, &members, &query, 10),
        "duplicates",
    );
}

#[test]
fn vectors_in_older_segments_serve_newer_chunk_maps() {
    let mut store = Store::new();
    let mut rng = TestRng::new(21);
    let vectors = clustered(&mut rng, 90, 20);
    let mut older = VectorSectionsBuilder::new(model(20)).unwrap();
    for (i, vector) in vectors.iter().enumerate().take(60) {
        older.add_vector(text(i), vector).unwrap();
    }
    let mut newer = VectorSectionsBuilder::new(model(20)).unwrap();
    for (i, vector) in vectors.iter().enumerate().skip(60) {
        newer.add_vector(text(i), vector).unwrap();
    }
    let content = ContentKey::of(b"file");
    let chunks: Vec<Chunk> = (50..70)
        .map(|i| Chunk {
            text: text(i),
            exported: i % 3 == 0,
            failed: false,
        })
        .collect();
    newer.add_chunk_map(content, b"lib.rs", &chunks);
    let paths = [store.write(older), store.write(newer)];
    let segments: Vec<Segment> = paths.iter().map(|p| store.open(p)).collect();
    let index = index_of(&segments, 20);
    assert_eq!(index.vectors(), 90);
    let found = index
        .file_members([(content, b"lib.rs".as_slice())])
        .unwrap();
    assert_eq!(found.coverage.chunks_embedded, 20);
    let segments_used: std::collections::BTreeSet<u32> =
        found.members.iter().map(|m| m.unit.segment).collect();
    assert_eq!(segments_used.len(), 2, "the file's chunks span both");
    let by_unit: HashMap<UnitRef, Vec<f32>> = (0..90)
        .map(|i| (index.locate(text(i)).unwrap(), vectors[i].clone()))
        .collect();
    let set = index.member_set(found.members.clone()).unwrap();
    // A file's best chunk, as today's per-file cosine asks for it.
    let query = vectors[55].clone();
    for k in [1, 5, 20] {
        let result = index.search(&set, &request(&query, k)).unwrap();
        assert_same(
            &answer(&result),
            &reference(&by_unit, &found.members, &query, k),
            &format!("k {k}"),
        );
    }
}

#[test]
fn a_damaged_vector_block_is_refused() {
    let mut store = Store::new();
    let mut rng = TestRng::new(4);
    let mut vectors = clustered(&mut rng, 50, 32);
    // A distinctive row to find in the file.
    vectors[17] = (0..32u16).map(|i| 1000.0 + f32::from(i)).collect();
    let (segment, _) = single_segment(&mut store, 32, &vectors);
    let path = segment.path().to_path_buf();
    drop(segment);
    let mut bytes = std::fs::read(&path).unwrap();
    let needle: Vec<u8> = vectors[17].iter().flat_map(|v| v.to_le_bytes()).collect();
    let at = bytes
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("the f32 row is stored as written");
    bytes[at + 5] ^= 0x40;
    std::fs::write(&path, &bytes).unwrap();

    let segment = store.open(&path);
    let segments = [segment];
    let index = index_of(&segments, 32);
    let unit = index.locate(text(17)).unwrap();
    let error = index.vector(unit).unwrap_err();
    assert!(
        matches!(&error, VectorError::Segment(e) if e.is_corruption()),
        "{error}"
    );
    // A search that has to rescore the damaged row fails rather than
    // answering from bad bytes.
    let set = index
        .member_set(vec![Member {
            unit,
            exported: false,
        }])
        .unwrap();
    let error = index.search(&set, &request(&vectors[17], 1)).unwrap_err();
    assert!(
        matches!(&error, VectorError::Segment(e) if e.is_corruption()),
        "{error}"
    );
}

#[test]
fn missing_vectors_are_not_an_empty_answer() {
    let mut store = Store::new();
    let segments: [Segment; 0] = [];
    let index = index_of(&segments, 8);
    let set = index.member_set(Vec::new()).unwrap();
    let query = vec![0.5; 8];
    assert!(matches!(
        index.search(&set, &request(&query, 10)),
        Err(VectorError::NoVectors(id)) if id == model(8).id()
    ));

    // Vectors of another model are not this model's.
    let mut builder = VectorSectionsBuilder::new(model(8)).unwrap();
    builder.add_vector(text(0), &query).unwrap();
    let path = store.write(builder);
    let segments = [store.open(&path)];
    let mut other = model(8);
    other.name = "test/other".to_string();
    let index = VectorIndex::build(
        segments
            .iter()
            .map(|segment| VectorSegment::open(segment).unwrap().unwrap()),
        &other,
    )
    .unwrap();
    assert_eq!(index.vectors(), 0);
    let set = index.member_set(Vec::new()).unwrap();
    assert!(matches!(
        index.search(&set, &request(&query, 10)),
        Err(VectorError::NoVectors(id)) if id == other.id()
    ));

    // With vectors, no members is a real, empty answer.
    let index = index_of(&segments, 8);
    let set = index.member_set(Vec::new()).unwrap();
    let result = index.search(&set, &request(&query, 10)).unwrap();
    assert!(result.hits.is_empty());
    assert_eq!(result.report, SearchReport::default());
    assert_eq!(
        index.chunk_map(ContentKey::of(b"x"), b"x.rs").unwrap(),
        None
    );
}

#[test]
fn queries_and_members_the_index_cannot_use_are_refused() {
    let mut store = Store::new();
    let (segment, _) = single_segment(&mut store, 4, &[vec![1.0, 0.0, 0.0, 0.0]]);
    let segments = [segment];
    let index = index_of(&segments, 4);
    let unit = index.locate(text(0)).unwrap();
    let set = index
        .member_set(vec![Member {
            unit,
            exported: false,
        }])
        .unwrap();
    assert!(matches!(
        index.search(&set, &request(&[1.0, 0.0], 1)),
        Err(VectorError::DimensionMismatch {
            expected: 4,
            found: 2
        })
    ));
    assert!(matches!(
        index.search(&set, &request(&[1.0, f32::INFINITY, 0.0, 0.0], 1)),
        Err(VectorError::NonFinite)
    ));
    assert!(matches!(
        index.member_set(vec![Member {
            unit: UnitRef {
                segment: 0,
                ordinal: 1
            },
            exported: false
        }]),
        Err(VectorError::UnknownUnit(_))
    ));
    let other = index_of(&segments, 4);
    assert!(matches!(
        other.search(&set, &request(&[1.0, 0.0, 0.0, 0.0], 1)),
        Err(VectorError::ForeignMembers)
    ));
}

#[test]
fn concurrent_searches_share_the_block_cache() {
    let mut store = Store::new();
    let mut rng = TestRng::new(13);
    let vectors = clustered(&mut rng, 2000, 64);
    let (segment, by_unit) = single_segment(&mut store, 64, &vectors);
    let segments = [segment];
    let index = index_of(&segments, 64);
    let mut units: Vec<UnitRef> = by_unit.keys().copied().collect();
    units.sort();
    let members: Vec<Member> = units
        .iter()
        .map(|unit| Member {
            unit: *unit,
            exported: false,
        })
        .collect();
    let set = index.member_set(members.clone()).unwrap();
    let queries = clustered(&mut rng, 16, 64);
    std::thread::scope(|scope| {
        for query in &queries {
            let (index, set, by_unit, members) = (&index, &set, &by_unit, &members);
            scope.spawn(move || {
                for _ in 0..3 {
                    let result = index.search(set, &request(query, 30)).unwrap();
                    assert_same(
                        &answer(&result),
                        &reference(by_unit, members, query, 30),
                        "thread",
                    );
                }
            });
        }
    });
    let stats = store.cache.stats();
    let segment_blocks = segments[0]
        .sections()
        .map(|(_, info)| info.len.div_ceil(BLOCK_LEN as u64))
        .sum::<u64>();
    assert!(
        stats.misses <= segment_blocks * 16,
        "blocks are read once and shared: {stats:?}"
    );
    assert!(stats.hits > stats.misses);
}

fn detail(name: &str, failure: Option<&str>) -> ChunkDetail {
    ChunkDetail {
        kind: 3,
        start_line: 10,
        end_line: 24,
        name: name.to_string(),
        qualified_name: Some(format!("Outer.{name}")),
        snippet: format!("fn {name}() {{\n    body();\n}}").repeat(8),
        failure: failure.map(str::to_string),
    }
}

#[test]
fn chunk_details_and_failures_round_trip() {
    let mut store = Store::new();
    let mut rng = TestRng::new(23);
    let vectors = clustered(&mut rng, 4, 16);
    let mut builder = VectorSectionsBuilder::new(model(16)).unwrap();
    for (index, vector) in vectors.iter().enumerate() {
        builder.add_vector(text(index), vector).unwrap();
    }
    let content = ContentKey::of(b"fn a() {}");
    let chunks = [
        Chunk {
            text: text(0),
            exported: true,
            failed: false,
        },
        Chunk {
            text: TextDigest::of(b"refused"),
            exported: false,
            failed: true,
        },
    ];
    let details = FileDetails {
        failure: None,
        chunks: vec![detail("a", None), detail("b", Some("provider refused"))],
    };
    // Details that do not describe the chunks are refused.
    for wrong in [
        FileDetails {
            failure: None,
            chunks: vec![detail("a", None)],
        },
        FileDetails {
            failure: None,
            chunks: vec![detail("a", None), detail("b", None)],
        },
        FileDetails {
            failure: Some("not UTF-8".to_string()),
            chunks: details.chunks.clone(),
        },
    ] {
        assert!(matches!(
            builder.add_chunk_map_with_details(content, b"src/a.rs", &chunks, wrong),
            Err(VectorError::DetailsMismatch)
        ));
    }
    builder
        .add_chunk_map_with_details(content, b"src/a.rs", &chunks, details.clone())
        .unwrap();
    // A file that could not be chunked: no chunks, and why.
    let broken = ContentKey::of(b"\xff\xfe");
    let failed_file = FileDetails {
        failure: Some("stream did not contain valid UTF-8".to_string()),
        chunks: Vec::new(),
    };
    builder
        .add_chunk_map_with_details(broken, b"src/b.rs", &[], failed_file.clone())
        .unwrap();
    // A map added without details has none.
    builder.add_chunk_map(content, b"src/c.rs", &chunks[..1]);
    let path = store.write(builder);
    let segment = store.open(&path);
    segment.verify().unwrap();
    let opened = VectorSegment::open(&segment).unwrap().unwrap();
    assert_eq!(
        opened.chunk_map(content, b"src/a.rs").unwrap(),
        Some(chunks.to_vec())
    );
    assert_eq!(
        opened.chunk_details(content, b"src/a.rs").unwrap(),
        Some(details.clone())
    );
    assert_eq!(
        opened.chunk_details(broken, b"src/b.rs").unwrap(),
        Some(failed_file)
    );
    assert_eq!(
        opened.chunk_map(broken, b"src/b.rs").unwrap(),
        Some(Vec::new())
    );
    assert_eq!(opened.chunk_details(content, b"src/c.rs").unwrap(), None);
    assert_eq!(opened.chunk_details(content, b"src/d.rs").unwrap(), None);
    let mut keys = opened.chunk_map_keys().unwrap();
    keys.sort();
    let mut expected = vec![
        ChunkMapKey::new(content, b"src/a.rs"),
        ChunkMapKey::new(broken, b"src/b.rs"),
        ChunkMapKey::new(content, b"src/c.rs"),
    ];
    expected.sort();
    assert_eq!(keys, expected);
    // The repeated snippets are stored compressed.
    let bytes = opened.section_bytes();
    assert!(
        bytes.chunk_details < details.encode().len() as u64,
        "{bytes:?}"
    );
    // An index finds the details beside the map it serves.
    drop(opened);
    let segments = [segment];
    let index = index_of(&segments, 16);
    assert_eq!(
        index.chunk_details(content, b"src/a.rs").unwrap(),
        Some(details)
    );
    let map = index.chunk_map(content, b"src/a.rs").unwrap().unwrap();
    assert!(map[0].unit.is_some() && map[1].unit.is_none() && map[1].chunk.failed);
}

#[test]
fn file_details_refuse_damaged_records() {
    let details = FileDetails {
        failure: None,
        chunks: vec![detail("a", None), detail("b", Some("refused"))],
    };
    let bytes = details.encode();
    assert_eq!(FileDetails::decode(&bytes).unwrap(), details);
    for cut in 0..bytes.len() {
        assert!(FileDetails::decode(&bytes[..cut]).is_err(), "cut at {cut}");
    }
    let mut longer = bytes;
    longer.push(0);
    assert!(FileDetails::decode(&longer).is_err());
}

#[test]
fn a_shared_index_lends_indexes_that_answer_as_a_built_one() {
    let mut store = Store::new();
    let mut rng = TestRng::new(29);
    let vectors = clustered(&mut rng, 300, 24);
    let mut older = VectorSectionsBuilder::new(model(24)).unwrap();
    let mut newer = VectorSectionsBuilder::new(model(24)).unwrap();
    for (index, vector) in vectors.iter().enumerate() {
        if index < 150 {
            older.add_vector(text(index), vector).unwrap();
        } else {
            newer.add_vector(text(index), vector).unwrap();
        }
    }
    // Another model's segment is left out.
    let mut foreign = VectorSectionsBuilder::new(model(8)).unwrap();
    foreign.add_vector(text(0), &[1.0; 8]).unwrap();
    let paths = [store.write(older), store.write(foreign), store.write(newer)];
    let segments: Vec<Arc<Segment>> = paths.iter().map(|p| Arc::new(store.open(p))).collect();
    let shared = SharedVectorIndex::build(segments.iter().cloned(), &model(24)).unwrap();
    assert_eq!(shared.vectors(), 300);
    assert_eq!(shared.segments().count(), 2);
    let plain: Vec<&Segment> = segments.iter().map(|segment| &**segment).collect();
    let built = VectorIndex::build(
        plain
            .iter()
            .filter_map(|segment| VectorSegment::open(segment).unwrap()),
        &model(24),
    )
    .unwrap();
    let lent = shared.index();
    let members: Vec<Member> = (0..300)
        .map(|i| Member {
            unit: lent.locate(text(i)).unwrap(),
            exported: i % 4 == 0,
        })
        .collect();
    for (i, member) in members.iter().enumerate() {
        assert_eq!(built.locate(text(i)), Some(member.unit), "vector {i}");
    }
    let set = lent.member_set(members.clone()).unwrap();
    let built_set = built.member_set(members.clone()).unwrap();
    // Members prepared through one lent index serve another, even from
    // another thread; the built index refuses them.
    let again = shared.index();
    for query in clustered(&mut rng, 8, 24) {
        for k in [1, 7, 300] {
            let expected = built.search(&built_set, &request(&query, k)).unwrap();
            let found = again.search(&set, &request(&query, k)).unwrap();
            assert_same(&answer(&found), &answer(&expected), &format!("k {k}"));
            assert_eq!(found.report, expected.report);
        }
    }
    assert!(matches!(
        built.search(&set, &request(&vectors[0], 3)),
        Err(VectorError::ForeignMembers)
    ));
    std::thread::scope(|scope| {
        let (shared, set, query) = (&shared, &set, &vectors[5]);
        scope.spawn(move || {
            let result = shared.index().search(set, &request(query, 1)).unwrap();
            assert_eq!(result.hits[0].member, 5);
        });
    });
    // Parts belong to the open segment they came from, not to its file.
    let parts = VectorSegment::open(&segments[0]).unwrap().unwrap().parts();
    let other_open = store.open(&paths[0]);
    assert!(VectorSegment::reopen(&other_open, &parts).is_none());
    assert!(VectorSegment::reopen(&segments[0], &parts).is_some());
    // A rebuild over fewer segments keeps what it can and answers the same
    // as a build over them.
    let rebuilt = shared.rebuild([Arc::clone(&segments[2])]).unwrap();
    let fresh = SharedVectorIndex::build([Arc::clone(&segments[2])], &model(24)).unwrap();
    assert_eq!((rebuilt.vectors(), fresh.vectors()), (150, 150));
    let (rebuilt_index, fresh_index) = (rebuilt.index(), fresh.index());
    let newer: Vec<Member> = (150..300)
        .map(|i| Member {
            unit: fresh_index.locate(text(i)).unwrap(),
            exported: false,
        })
        .collect();
    assert_eq!(
        (150..300)
            .map(|i| rebuilt_index.locate(text(i)))
            .collect::<Vec<_>>(),
        newer.iter().map(|m| Some(m.unit)).collect::<Vec<_>>()
    );
    let query = &vectors[200];
    assert_same(
        &answer(
            &rebuilt_index
                .search(
                    &rebuilt_index.member_set(newer.clone()).unwrap(),
                    &request(query, 5),
                )
                .unwrap(),
        ),
        &answer(
            &fresh_index
                .search(&fresh_index.member_set(newer).unwrap(), &request(query, 5))
                .unwrap(),
        ),
        "rebuilt",
    );
}

#[test]
fn no_two_planes_share_a_section_kind() {
    // Readers find a plane's sections by kind, so a segment holding two
    // planes' sections would mix them up if any kind were shared.
    let planes: [(&str, Vec<KnownSection>); 4] = [
        ("text", crate::text::KNOWN.to_vec()),
        ("content", crate::content::KNOWN.to_vec()),
        (
            "graph",
            crate::graph::KNOWN
                .iter()
                .chain(&crate::graph::KNOWN_OPTIONAL)
                .copied()
                .collect(),
        ),
        ("vector", KNOWN.to_vec()),
    ];
    let mut owner = HashMap::new();
    for (plane, known) in &planes {
        for section in known {
            if let Some(other) = owner.insert(section.kind, *plane) {
                panic!(
                    "{plane} and {other} both use section kind {:?}",
                    section.kind
                );
            }
        }
    }
}
