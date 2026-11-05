//! Vector-plane gate on real embeddings and real queries.
//!
//! Writes a dataset's vectors and chunk maps into a fresh family through the
//! normal publication path, binds it back, and for every query checks that
//! the plane's exact search returns what today's f32 brute force returns
//! over the same members: the same members, in the same order, with the same
//! score bits. It also checks, for a share of the queries, that every
//! member's exact score lies inside the bounds the int8 scan gave it, and it
//! measures sizes, latency, the rescoring set and memory, and what other
//! quantizations would have rescored. It prints one JSON object and exits
//! non-zero on any difference.
//!
//! Inputs (nothing is random; see `vector_gate_embed.py` for where each
//! vector comes from, and `vector_gate_job.sh` for the whole run on a build
//! server):
//!
//! ```text
//! # A dataset written by vector_gate_embed.py over export_chunks output:
//! cargo run --release -p aft-store --example vector_gate -- \
//!     dir <empty store dir> <dataset dir> <chunks.jsonl.gz> [--k 101]
//! # The benchmark's MiniLM vector packs (benchmarks/aft-search/*.bin):
//! cargo run --release -p aft-store --example vector_gate -- \
//!     pack <empty store dir> <pack.bin>... [--k 101]
//! ```

#![allow(
    clippy::unwrap_used,
    reason = "a measurement tool: any failure should stop it loudly"
)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    reason = "percentile arithmetic and test data conversions over small counts"
)]
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "it reports to the terminal"
)]

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use aft_store::cache::BlockCache;
use aft_store::family::{Family, Publication};
use aft_store::ids::{ContentKey, Digest128, FamilyId};
use aft_store::segment::Segment;
use aft_store::vector::{
    Chunk, KNOWN, MemberSet, ModelIdentity, SearchReport, SearchRequest, SectionBytes, TextDigest,
    VectorIndex, VectorSectionsBuilder, VectorSegment,
};
use rayon::prelude::*;
use serde_json::{Value, json};

/// One file's chunks, in the order today's index enumerates them.
struct GateFile {
    path: String,
    content: ContentKey,
    chunks: Vec<Chunk>,
}

/// Everything one gate run reads.
struct Dataset {
    label: String,
    model: ModelIdentity,
    source: Value,
    files: Vec<GateFile>,
    chunks: usize,
    vectors: HashMap<TextDigest, Vec<f32>>,
    queries: Vec<Vec<f32>>,
    query_texts: Vec<String>,
}

fn read_f32_rows(path: &Path, dimensions: usize) -> Vec<Vec<f32>> {
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(bytes.len() % (dimensions * 4), 0, "{}", path.display());
    bytes
        .as_chunks::<4>()
        .0
        .chunks(dimensions)
        .map(|row| row.iter().map(|b| f32::from_le_bytes(*b)).collect())
        .collect()
}

/// Groups chunks by file in first-appearance order. A file's content key
/// stands in as the digest of its chunk texts: the gate has no checkout to
/// hash, and the plane only needs the same key on write and lookup.
fn group_files(chunks: impl IntoIterator<Item = (String, Chunk, Vec<u8>)>) -> Vec<GateFile> {
    let mut order: Vec<String> = Vec::new();
    let mut by_path: HashMap<String, (Vec<Chunk>, blake3::Hasher)> = HashMap::new();
    for (path, chunk, text) in chunks {
        let entry = by_path.entry(path.clone()).or_insert_with(|| {
            order.push(path);
            (Vec::new(), blake3::Hasher::new())
        });
        entry.0.push(chunk);
        entry.1.update(&(text.len() as u64).to_le_bytes());
        entry.1.update(&text);
    }
    order
        .into_iter()
        .map(|path| {
            let (chunks, hasher) = by_path.remove(&path).unwrap();
            GateFile {
                content: ContentKey::from_digest(Digest128::from_hash(&hasher.finalize())),
                path,
                chunks,
            }
        })
        .collect()
}

/// A dataset written by `vector_gate_embed.py`.
fn load_dir(dir: &Path, chunks_path: &Path) -> Dataset {
    let meta: Value =
        serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
    let info = &meta["model"];
    let dimensions = info["dimensions"].as_u64().unwrap() as usize;
    let model = ModelIdentity {
        name: info["name"].as_str().unwrap().to_string(),
        revision: info["revision"].as_str().unwrap_or_default().to_string(),
        dimensions: dimensions as u32,
        normalized: info["normalized"].as_bool().unwrap(),
        text_template: 1,
        chunk_template: 1,
    };
    let reader = BufReader::new(flate2::read::GzDecoder::new(
        std::fs::File::open(chunks_path).unwrap(),
    ));
    let exported: Vec<Value> = reader
        .lines()
        .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
        .collect();
    let rows = read_f32_rows(&dir.join("vectors.f32"), dimensions);
    let embedded: Vec<usize> = match meta["chunk_indices"].as_array() {
        Some(indices) => indices
            .iter()
            .map(|i| i.as_u64().unwrap() as usize)
            .collect(),
        None => (0..exported.len()).collect(),
    };
    assert_eq!(rows.len(), embedded.len(), "one vector per embedded chunk");
    let mut vectors = HashMap::new();
    for (row, index) in rows.into_iter().zip(&embedded) {
        let text = exported[*index]["embed_text"].as_str().unwrap();
        vectors
            .entry(TextDigest::of(text.as_bytes()))
            .or_insert(row);
    }
    let files = group_files(exported.iter().map(|chunk| {
        let text = chunk["embed_text"].as_str().unwrap().as_bytes().to_vec();
        (
            chunk["file"].as_str().unwrap().to_string(),
            Chunk {
                text: TextDigest::of(&text),
                exported: chunk["exported"].as_bool().unwrap(),
                failed: false,
            },
            text,
        )
    }));
    let queries = read_f32_rows(&dir.join("queries.f32"), dimensions);
    let query_texts: Vec<String> =
        serde_json::from_slice(&std::fs::read(dir.join("queries.json")).unwrap()).unwrap();
    assert_eq!(queries.len(), query_texts.len());
    let mut source = meta.clone();
    source.as_object_mut().unwrap().remove("chunk_indices");
    Dataset {
        label: format!("{} on {}", model.name, chunks_path.display()),
        model,
        source,
        files,
        chunks: exported.len(),
        vectors,
        queries,
        query_texts,
    }
}

/// IEEE half precision to f32, exactly.
fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15) << 31;
    let exponent = u32::from((bits >> 10) & 0x1f);
    let mantissa = u32::from(bits & 0x3ff);
    let value = match (exponent, mantissa) {
        (0, 0) => sign,
        (0, _) => {
            // Subnormal: normalize.
            let mut e = 127 - 15 + 1;
            let mut m = mantissa;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            sign | (e << 23) | ((m & 0x3ff) << 13)
        }
        (0x1f, 0) => sign | 0x7f80_0000,
        (0x1f, _) => sign | 0x7fc0_0000,
        _ => sign | ((exponent + 127 - 15) << 23) | (mantissa << 13),
    };
    f32::from_bits(value)
}

/// The benchmark's `AFTVECP2` packs: real all-MiniLM-L6-v2 vectors of the
/// benchmark corpus's chunks and of real and authored queries, stored as
/// f16 (`benchmarks/aft-search/vector_pack.py`). The packs carry no chunk
/// metadata, so each corpus vector is its own file and none is exported.
fn load_packs(paths: &[PathBuf]) -> Dataset {
    let mut corpus: Vec<([u8; 32], Vec<f32>)> = Vec::new();
    let mut queries: Vec<([u8; 32], Vec<f32>)> = Vec::new();
    let mut headers = Vec::new();
    let mut dimensions = 0;
    for path in paths {
        let data = std::fs::read(path).unwrap();
        assert_eq!(&data[..8], b"AFTVECP2", "{}", path.display());
        let header_len = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
        let header: Value = serde_json::from_slice(&data[12..12 + header_len]).unwrap();
        let count = header["count"].as_u64().unwrap() as usize;
        dimensions = header["dimension"].as_u64().unwrap() as usize;
        let index_at = 12 + header_len;
        let vectors_at = index_at + count * 33;
        assert_eq!(data.len(), vectors_at + count * dimensions * 2);
        for row in 0..count {
            let record = &data[index_at + row * 33..index_at + (row + 1) * 33];
            let digest: [u8; 32] = record[1..].try_into().unwrap();
            let start = vectors_at + row * dimensions * 2;
            let vector = data[start..start + dimensions * 2]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| f16_to_f32(u16::from_le_bytes(*b)))
                .collect();
            match record[0] {
                0 => corpus.push((digest, vector)),
                1 => queries.push((digest, vector)),
                kind => panic!("unknown pack record kind {kind}"),
            }
        }
        let mut header = header;
        header["file"] = json!(path.display().to_string());
        headers.push(header);
    }
    let mut seen = HashSet::new();
    corpus.retain(|(digest, _)| seen.insert(*digest));
    let mut seen = HashSet::new();
    queries.retain(|(digest, _)| seen.insert(*digest));
    let model = ModelIdentity {
        name: "all-MiniLM-L6-v2".to_string(),
        revision: headers[0]["source"]["model_revision"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        dimensions: dimensions as u32,
        normalized: true,
        text_template: 1,
        chunk_template: 1,
    };
    let mut vectors = HashMap::new();
    let files = group_files(corpus.into_iter().map(|(digest, vector)| {
        let text = TextDigest::of(&digest);
        vectors.insert(text, vector);
        (
            format!(
                "pack/{}",
                Digest128::from_bytes(digest[..16].try_into().unwrap())
            ),
            Chunk {
                text,
                exported: false,
                failed: false,
            },
            digest.to_vec(),
        )
    }));
    let chunks = files.len();
    Dataset {
        label: "all-MiniLM-L6-v2 benchmark packs".to_string(),
        model,
        source: json!({ "packs": headers }),
        files,
        chunks,
        vectors,
        query_texts: queries
            .iter()
            .map(|(digest, _)| {
                format!(
                    "query sha256 {}",
                    Digest128::from_bytes(digest[..16].try_into().unwrap())
                )
            })
            .collect(),
        queries: queries.into_iter().map(|(_, vector)| vector).collect(),
    }
}

/// The dataset's distinct vectors, with today's cached norm of each.
struct Units {
    texts: Vec<TextDigest>,
    vectors: Vec<Vec<f32>>,
    norms: Vec<f32>,
    position: HashMap<TextDigest, usize>,
}

impl Units {
    fn new(data: &Dataset) -> Self {
        let mut texts: Vec<TextDigest> = data.vectors.keys().copied().collect();
        texts.sort();
        let vectors: Vec<Vec<f32>> = texts.iter().map(|t| data.vectors[t].clone()).collect();
        let norms = vectors.iter().map(|v| today::vector_norm(v)).collect();
        let position = texts.iter().enumerate().map(|(i, t)| (*t, i)).collect();
        Self {
            texts,
            vectors,
            norms,
            position,
        }
    }
}

/// Today's arithmetic, copied from `search_filtered` in AFT's resident
/// index (`crates/aft/src/semantic_index.rs`) rather than shared with the
/// plane, so a mistake in one cannot hide in both.
mod today {
    pub(crate) fn vector_norm(vector: &[f32]) -> f32 {
        vector.iter().map(|value| value * value).sum::<f32>().sqrt()
    }

    fn dot_product(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(a, b)| a * b).sum::<f32>()
    }

    pub(crate) fn semantic_score_order(a: &(f32, usize), b: &(f32, usize)) -> std::cmp::Ordering {
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

    /// Every member's score, in member order.
    pub(crate) fn scores(
        vectors: &[Vec<f32>],
        norms: &[f32],
        members: &[(usize, bool)],
        query_vector: &[f32],
    ) -> Vec<f32> {
        let query_norm = vector_norm(query_vector);
        members
            .iter()
            .map(|(unit, exported)| {
                let dot = dot_product(query_vector, &vectors[*unit]);
                let denom = query_norm * norms[*unit];
                let mut score = if denom == 0.0 { 0.0 } else { dot / denom };
                if *exported {
                    score *= if score >= 0.0 { 1.1 } else { 0.9 };
                }
                score
            })
            .collect()
    }

    /// The top `top_k` of `scores`, selected and sorted as today.
    pub(crate) fn top(scores: &[f32], top_k: usize) -> Vec<(u32, f32)> {
        let mut scored: Vec<(f32, usize)> = scores.iter().copied().zip(0..).collect();
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
            .map(|(score, idx)| (idx as u32, score))
            .collect()
    }
}

fn percentiles(mut values: Vec<f64>) -> Value {
    if values.is_empty() {
        return json!(null);
    }
    values.sort_by(f64::total_cmp);
    let at = |p: f64| values[((values.len() - 1) as f64 * p).round() as usize];
    json!({
        "p50": at(0.5),
        "p95": at(0.95),
        "p99": at(0.99),
        "max": values[values.len() - 1],
        "mean": values.iter().sum::<f64>() / values.len() as f64,
    })
}

/// Today's boost on an f64 score, for the quantization comparison.
fn boost(score: f64, exported: bool) -> f64 {
    if !exported {
        score
    } else if score >= 0.0 {
        score * 1.1
    } else {
        score * 0.9
    }
}

/// Per-vector symmetric int8 of `rows`, as the plane stores it.
struct Codes {
    dimensions: usize,
    codes: Vec<i8>,
    scale: Vec<f64>,
    residual: Vec<f64>,
    max_residual: Vec<f64>,
}

impl Codes {
    fn of(rows: impl Iterator<Item = Vec<f64>>, dimensions: usize) -> Self {
        let mut out = Self {
            dimensions,
            codes: Vec::new(),
            scale: Vec::new(),
            residual: Vec::new(),
            max_residual: Vec::new(),
        };
        for row in rows {
            let peak = row.iter().fold(0f64, |p, v| p.max(v.abs()));
            let scale = peak / 127.0;
            let (mut squares, mut max) = (0f64, 0f64);
            for value in &row {
                let code = if scale > 0.0 {
                    (value / scale).round().clamp(-127.0, 127.0)
                } else {
                    0.0
                };
                out.codes.push(code as i8);
                let residual = (value - scale * code).abs();
                squares += residual * residual;
                max = max.max(residual);
            }
            out.scale.push(scale);
            out.residual.push(squares.sqrt());
            out.max_residual.push(max);
        }
        out
    }

    /// `(approx, L1 error, L2 error)` of `query · row` for every row.
    fn estimate(&self, query: &[f64]) -> Vec<(f64, f64, f64)> {
        let l1: f64 = query.iter().map(|v| v.abs()).sum();
        let l2: f64 = query.iter().map(|v| v * v).sum::<f64>().sqrt();
        self.codes
            .chunks(self.dimensions)
            .enumerate()
            .map(|(unit, codes)| {
                let dot: f64 = query
                    .iter()
                    .zip(codes)
                    .map(|(q, c)| q * f64::from(*c))
                    .sum();
                (
                    self.scale[unit] * dot,
                    self.max_residual[unit] * l1,
                    self.residual[unit] * l2,
                )
            })
            .collect()
    }
}

/// The int8 layouts compared with each other, all bounded the same way: by
/// the residual each layout leaves. The plane stores the centered one, with
/// each segment's own mean; these use the corpus mean.
struct Alternatives {
    symmetric: Codes,
    /// The corpus mean subtracted before quantizing.
    centered: Codes,
    mean: Vec<f64>,
    /// Each dimension divided by its largest magnitude before quantizing.
    per_dimension: Codes,
    sigma: Vec<f64>,
}

impl Alternatives {
    fn new(units: &Units, dimensions: usize) -> Self {
        let rows = || {
            units
                .vectors
                .iter()
                .map(|v| v.iter().map(|x| f64::from(*x)).collect::<Vec<f64>>())
        };
        let mut mean = vec![0f64; dimensions];
        let mut sigma = vec![0f64; dimensions];
        for row in rows() {
            for (d, value) in row.iter().enumerate() {
                mean[d] += value;
                sigma[d] = sigma[d].max(value.abs());
            }
        }
        let count = units.vectors.len().max(1) as f64;
        mean.iter_mut().for_each(|m| *m /= count);
        sigma.iter_mut().for_each(|s| {
            if *s == 0.0 {
                *s = 1.0;
            }
        });
        Self {
            symmetric: Codes::of(rows(), dimensions),
            centered: Codes::of(
                rows().map(|row| row.iter().zip(&mean).map(|(v, m)| v - m).collect()),
                dimensions,
            ),
            per_dimension: Codes::of(
                rows().map(|row| row.iter().zip(&sigma).map(|(v, s)| v / s).collect()),
                dimensions,
            ),
            mean,
            sigma,
        }
    }
}

/// How many members a bounded search over these estimates would keep (the
/// guaranteed set) and rescore (stopping once no bound reaches the k-th
/// exact score).
fn bounded_counts(
    members: &[(usize, bool)],
    norms: &[f32],
    query_norm: f64,
    estimates: &[(f64, f64)],
    exact: &[f32],
    k: usize,
) -> (usize, usize) {
    let mut lower = Vec::with_capacity(members.len());
    let mut upper = Vec::with_capacity(members.len());
    for (unit, exported) in members {
        let (approx, error) = estimates[*unit];
        let denom = query_norm * f64::from(norms[*unit]);
        let scale = |dot: f64| if denom == 0.0 { 0.0 } else { dot / denom };
        lower.push(boost(scale(approx - error), *exported));
        upper.push(boost(scale(approx + error), *exported));
    }
    let threshold = if k < lower.len() {
        let mut lows = lower.clone();
        lows.select_nth_unstable_by(k - 1, |a, b| b.total_cmp(a));
        lows[k - 1]
    } else {
        f64::NEG_INFINITY
    };
    let mut guaranteed: Vec<usize> = (0..members.len())
        .filter(|m| upper[*m] >= threshold)
        .collect();
    let kept = guaranteed.len();
    guaranteed.sort_by(|a, b| upper[*b].total_cmp(&upper[*a]));
    let mut top: std::collections::BinaryHeap<std::cmp::Reverse<OrderedF64>> =
        std::collections::BinaryHeap::new();
    let mut rescored = 0;
    for m in guaranteed {
        if top.len() == k && top.peek().is_some_and(|worst| upper[m] < worst.0.0) {
            break;
        }
        rescored += 1;
        top.push(std::cmp::Reverse(OrderedF64(f64::from(exact[m]))));
        if top.len() > k {
            top.pop();
        }
    }
    (kept, rescored)
}

struct OrderedF64(f64);
impl PartialEq for OrderedF64 {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for OrderedF64 {}
impl PartialOrd for OrderedF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrderedF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Whether rescoring a fixed number of the best int8 estimates reproduces
/// the exact ordered top k, for each window size.
fn fixed_windows(
    members: &[(usize, bool)],
    norms: &[f32],
    query_norm: f64,
    approx: &[f64],
    exact: &[f32],
    expected: &[(u32, f32)],
    windows: &[usize],
) -> Vec<bool> {
    let mut ranked: Vec<(f64, usize)> = members
        .iter()
        .enumerate()
        .map(|(m, (unit, exported))| {
            let denom = query_norm * f64::from(norms[*unit]);
            let score = if denom == 0.0 {
                0.0
            } else {
                approx[*unit] / denom
            };
            (boost(score, *exported), m)
        })
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    windows
        .iter()
        .map(|window| {
            let mut picked: Vec<(f32, usize)> = ranked
                .iter()
                .take(*window)
                .map(|(_, m)| (exact[*m], *m))
                .collect();
            picked.sort_by(today::semantic_score_order);
            picked.truncate(expected.len());
            picked.len() == expected.len()
                && picked
                    .iter()
                    .zip(expected)
                    .all(|(p, e)| p.1 as u32 == e.0 && p.0.to_bits() == e.1.to_bits())
        })
        .collect()
}

fn peak_rss_mb() -> Value {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return json!(null);
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|rest| {
            rest.trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<f64>()
                .ok()
        })
        .map_or(json!(null), |kb| json!(kb / 1024.0))
}

/// Writes the dataset into a fresh family: vectors in segments under the
/// container's size limit, every chunk map in the last one, so maps name
/// vectors in other segments as they will after incremental fills.
fn write_store(store: &Path, data: &Dataset, units: &Units) -> (Family, usize, f64) {
    let family = Family::open(store, FamilyId::from_digest(Digest128::of(b"vector-gate"))).unwrap();
    let started = Instant::now();
    let per_segment =
        (200 << 20) / VectorSectionsBuilder::bytes_per_vector(data.model.dimensions as usize);
    let batches: Vec<&[TextDigest]> = units.texts.chunks(per_segment.max(1)).collect();
    let mut session = family.try_write().unwrap().unwrap();
    let mut segments = Vec::new();
    for (index, batch) in batches.iter().enumerate() {
        let mut builder = VectorSectionsBuilder::new(data.model.clone()).unwrap();
        for text in *batch {
            builder
                .add_vector(*text, &units.vectors[units.position[text]])
                .unwrap();
        }
        if index + 1 == batches.len() {
            for file in &data.files {
                builder.add_chunk_map(file.content, file.path.as_bytes(), &file.chunks);
            }
        }
        let mut pending = session.create_segment().unwrap();
        builder.write(pending.writer()).unwrap();
        segments.push(session.finish_segment(pending).unwrap());
    }
    let count = segments.len();
    session
        .publish(&Publication {
            add_segments: segments,
            ..Publication::default()
        })
        .unwrap();
    drop(session);
    (family, count, started.elapsed().as_secs_f64() * 1000.0)
}

fn open_segments(family: &Family, cache: &Arc<BlockCache>) -> Vec<Segment> {
    let snapshot = family.snapshot().unwrap();
    snapshot
        .manifest()
        .segments()
        .iter()
        .map(|entry| {
            snapshot
                .open_segment(entry.id, &KNOWN, Arc::clone(cache))
                .unwrap()
        })
        .collect()
}

fn index_of<'s>(segments: &'s [Segment], model: &ModelIdentity) -> VectorIndex<'s> {
    VectorIndex::build(
        segments
            .iter()
            .map(|segment| VectorSegment::open(segment).unwrap().unwrap()),
        model,
    )
    .unwrap()
}

/// A path today's test filter would most likely leave out.
fn test_like(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.starts_with("tests/")
        || path.contains("/tests/")
        || path.contains("/test/")
        || path.contains("/__tests__/")
        || name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.starts_with("test_")
}

/// One member set: the plane's prepared set and the reference's members.
struct Scope {
    name: &'static str,
    set: MemberSet,
    members: Vec<(usize, bool)>,
    coverage: Value,
}

fn scope(
    name: &'static str,
    index: &VectorIndex<'_>,
    data: &Dataset,
    units: &Units,
    keep: impl Fn(&GateFile) -> bool,
) -> Scope {
    let files: Vec<&GateFile> = data.files.iter().filter(|f| keep(f)).collect();
    let found = index
        .file_members(files.iter().map(|f| (f.content, f.path.as_bytes())))
        .unwrap();
    let mut members = Vec::new();
    for file in &files {
        for chunk in &file.chunks {
            if let Some(unit) = units.position.get(&chunk.text) {
                members.push((*unit, chunk.exported));
            }
        }
    }
    assert_eq!(found.members.len(), members.len(), "{name}: member counts");
    for (member, (unit, exported)) in found.members.iter().zip(&members) {
        assert_eq!(index.locate(units.texts[*unit]), Some(member.unit));
        assert_eq!(member.exported, *exported);
    }
    let coverage = json!({
        "files": found.coverage.files,
        "files_mapped": found.coverage.files_mapped,
        "chunks": found.coverage.chunks,
        "chunks_embedded": found.coverage.chunks_embedded,
    });
    Scope {
        name,
        set: index.member_set(found.members).unwrap(),
        members,
        coverage,
    }
}

fn add_bytes(a: SectionBytes, b: SectionBytes) -> SectionBytes {
    SectionBytes {
        units: a.units + b.units,
        scan: a.scan + b.scan,
        vectors: a.vectors + b.vectors,
        chunk_maps: a.chunk_maps + b.chunk_maps,
        chunk_details: a.chunk_details + b.chunk_details,
    }
}

/// The first difference between the plane's answer and the reference, if
/// any.
fn difference(found: &[(u32, f32)], expected: &[(u32, f32)]) -> Option<String> {
    if found.len() != expected.len() {
        return Some(format!(
            "{} hits, the reference has {}",
            found.len(),
            expected.len()
        ));
    }
    found
        .iter()
        .zip(expected)
        .enumerate()
        .find(|(_, (f, e))| f.0 != e.0 || f.1.to_bits() != e.1.to_bits())
        .map(|(rank, (f, e))| {
            format!(
                "rank {rank}: member {} score {:?}, the reference has member {} score {:?}",
                f.0, f.1, e.0, e.1
            )
        })
}

fn hits(
    index: &VectorIndex<'_>,
    set: &MemberSet,
    query: &[f32],
    k: usize,
) -> (Vec<(u32, f32)>, SearchReport) {
    let result = index
        .search(
            set,
            &SearchRequest {
                query,
                k,
                rescore_limit: None,
            },
        )
        .unwrap();
    (
        result
            .hits
            .iter()
            .map(|hit| (hit.member, hit.score))
            .collect(),
        result.report,
    )
}

fn report_stats(reports: &[SearchReport]) -> Value {
    json!({
        "guaranteed": percentiles(reports.iter().map(|r| r.guaranteed as f64).collect()),
        "rescored": percentiles(reports.iter().map(|r| r.rescored as f64).collect()),
        "f32_bytes_read": percentiles(reports.iter().map(|r| r.f32_bytes_read as f64).collect()),
        "scan_bytes": percentiles(reports.iter().map(|r| r.scan_bytes as f64).collect()),
        "full_scans": reports.iter().filter(|r| r.full_scan.is_some()).count(),
    })
}

/// Per query: (guaranteed, rescored) for each alternative layout, and
/// whether each fixed window was exact.
type Simulated = ([(usize, usize); 5], Vec<bool>);

fn main() {
    let mut k = 101;
    let mut positional = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--k" {
            k = args.next().unwrap().parse().unwrap();
        } else {
            positional.push(arg);
        }
    }
    let store = Path::new(&positional[1]);
    let data = match positional[0].as_str() {
        "dir" => load_dir(Path::new(&positional[2]), Path::new(&positional[3])),
        "pack" => load_packs(
            &positional[2..]
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>(),
        ),
        mode => panic!("unknown mode {mode}; use dir or pack"),
    };
    let units = Units::new(&data);
    let dimensions = data.model.dimensions as usize;
    let (family, segment_count, publish_ms) = write_store(store, &data, &units);
    let segment_bytes: u64 = family
        .snapshot()
        .unwrap()
        .manifest()
        .segments()
        .iter()
        .map(|entry| entry.len)
        .sum();
    // Large enough to hold every segment: the warm passes measure the plane,
    // not evictions. The cold pass below starts from an empty cache.
    let budget = usize::try_from(segment_bytes).unwrap() + (64 << 20);
    let cache = Arc::new(BlockCache::new(budget));
    let segments = open_segments(&family, &cache);
    let started = Instant::now();
    let index = index_of(&segments, &data.model);
    let scopes = [
        scope("all", &index, &data, &units, |_| true),
        scope("without test-like paths", &index, &data, &units, |file| {
            !test_like(&file.path)
        }),
    ];
    let bind_ms = started.elapsed().as_secs_f64() * 1000.0;
    let bytes = segments
        .iter()
        .map(|segment| {
            VectorSegment::open(segment)
                .unwrap()
                .unwrap()
                .section_bytes()
        })
        .fold(SectionBytes::default(), add_bytes);

    // Parity, with queries in parallel over the shared index and cache.
    let started = Instant::now();
    let outcomes: Vec<(Vec<Option<String>>, usize, Vec<SearchReport>)> = data
        .queries
        .par_iter()
        .map(|query| {
            let mut differences = Vec::new();
            let mut violations = 0;
            let mut reports = Vec::new();
            for scope in &scopes {
                let exact = today::scores(&units.vectors, &units.norms, &scope.members, query);
                let expected = today::top(&exact, k);
                let (found, report) = hits(&index, &scope.set, query, k);
                differences.push(difference(&found, &expected));
                reports.push(report);
                let bounds = index.bounds(&scope.set, query).unwrap();
                violations += bounds
                    .iter()
                    .zip(&exact)
                    .filter(|(bound, exact)| !(bound.low <= **exact && **exact <= bound.high))
                    .count();
            }
            (differences, violations, reports)
        })
        .collect();
    let parity_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut failed = false;
    let mut scope_values = Vec::new();
    for (position, scope) in scopes.iter().enumerate() {
        let mut differing = 0;
        for (query, outcome) in outcomes.iter().enumerate() {
            if let Some(difference) = &outcome.0[position] {
                differing += 1;
                if differing <= 20 {
                    eprintln!(
                        "DIFFERENCE [{}] query {query} {:?}: {difference}",
                        scope.name, data.query_texts[query]
                    );
                }
            }
        }
        failed |= differing > 0;
        let reports: Vec<SearchReport> = outcomes.iter().map(|o| o.2[position]).collect();
        scope_values.push(json!({
            "name": scope.name,
            "members": scope.set.members().len(),
            "units": scope.set.units(),
            "coverage": scope.coverage,
            "queries": data.queries.len(),
            "queries_differing": differing,
            "work": report_stats(&reports),
        }));
    }
    let violations: usize = outcomes.iter().map(|o| o.1).sum();
    failed |= violations > 0;

    // Warm latency, one query at a time.
    let all = &scopes[0];
    let mut search_us = Vec::new();
    let mut scan_us = Vec::new();
    let mut sequential = Vec::new();
    for query in &data.queries {
        let started = Instant::now();
        let (found, _) = hits(&index, &all.set, query, k);
        search_us.push(started.elapsed().as_secs_f64() * 1e6);
        sequential.push(found);
        let started = Instant::now();
        let _ = index.bounds(&all.set, query).unwrap();
        scan_us.push(started.elapsed().as_secs_f64() * 1e6);
    }
    let mut brute_us = Vec::new();
    for query in data.queries.iter().take(50) {
        let started = Instant::now();
        let exact = today::scores(&units.vectors, &units.norms, &all.members, query);
        let _ = today::top(&exact, k);
        brute_us.push(started.elapsed().as_secs_f64() * 1e6);
    }

    // The same searches from every thread at once must answer the same.
    let started = Instant::now();
    let parallel: Vec<Vec<(u32, f32)>> = data
        .queries
        .par_iter()
        .map(|query| hits(&index, &all.set, query, k).0)
        .collect();
    let parallel_ms = started.elapsed().as_secs_f64() * 1000.0;
    let parallel_differing = parallel
        .iter()
        .zip(&sequential)
        .filter(|(a, b)| difference(a, b).is_some())
        .count();
    failed |= parallel_differing > 0;
    let warm_cache = cache.stats();

    // From an empty block cache (the files stay in the OS page cache).
    let cold_cache = Arc::new(BlockCache::new(budget));
    let cold_segments = open_segments(&family, &cold_cache);
    let cold_index = index_of(&cold_segments, &data.model);
    let cold_set = cold_index.member_set(all.set.members().to_vec()).unwrap();
    let mut cold_us = Vec::new();
    let mut cold_blocks = Vec::new();
    for query in data.queries.iter().take(50) {
        let before = cold_cache.stats().misses;
        let started = Instant::now();
        let _ = hits(&cold_index, &cold_set, query, k);
        cold_us.push(started.elapsed().as_secs_f64() * 1e6);
        cold_blocks.push((cold_cache.stats().misses - before) as f64);
    }

    // Other int8 layouts, by how many members they would rescore.
    let alternatives = Alternatives::new(&units, dimensions);
    let windows: Vec<usize> = [k, 150, 200, 300].into_iter().filter(|w| *w >= k).collect();
    let simulated: Vec<Simulated> = data
        .queries
        .par_iter()
        .map(|query| {
            let exact = today::scores(&units.vectors, &units.norms, &all.members, query);
            let expected = today::top(&exact, k);
            let q: Vec<f64> = query.iter().map(|v| f64::from(*v)).collect();
            let query_norm = f64::from(today::vector_norm(query));
            let symmetric = alternatives.symmetric.estimate(&q);
            let shift: f64 = q.iter().zip(&alternatives.mean).map(|(a, b)| a * b).sum();
            let centered = alternatives.centered.estimate(&q);
            let scaled: Vec<f64> = q
                .iter()
                .zip(&alternatives.sigma)
                .map(|(a, b)| a * b)
                .collect();
            let per_dimension = alternatives.per_dimension.estimate(&scaled);
            let pick = |estimates: &[(f64, f64, f64)], shift: f64, error: fn(f64, f64) -> f64| {
                let bounds: Vec<(f64, f64)> = estimates
                    .iter()
                    .map(|(a, l1, l2)| (a + shift, error(*l1, *l2)))
                    .collect();
                bounded_counts(&all.members, &units.norms, query_norm, &bounds, &exact, k)
            };
            let counts = [
                pick(&centered, shift, f64::min),
                pick(&centered, shift, |_, l2| l2),
                pick(&centered, shift, |l1, _| l1),
                pick(&symmetric, 0.0, f64::min),
                pick(&per_dimension, 0.0, f64::min),
            ];
            let approx: Vec<f64> = centered.iter().map(|e| e.0 + shift).collect();
            let exact_windows = fixed_windows(
                &all.members,
                &units.norms,
                query_norm,
                &approx,
                &exact,
                &expected,
                &windows,
            );
            (counts, exact_windows)
        })
        .collect();
    let names = [
        "mean subtracted, per-vector symmetric, min(L1, L2) bound (stored)",
        "mean subtracted, per-vector symmetric, L2 residual bound only",
        "mean subtracted, per-vector symmetric, L1 (largest residual) bound only",
        "per-vector symmetric without centering, min(L1, L2) bound",
        "each dimension scaled by its corpus maximum, then per-vector symmetric",
    ];
    let quantization: serde_json::Map<String, Value> = names
        .iter()
        .enumerate()
        .map(|(variant, name)| {
            (
                (*name).to_string(),
                json!({
                    "guaranteed": percentiles(simulated.iter().map(|s| s.0[variant].0 as f64).collect()),
                    "rescored": percentiles(simulated.iter().map(|s| s.0[variant].1 as f64).collect()),
                }),
            )
        })
        .collect();
    let window_exact: serde_json::Map<String, Value> = windows
        .iter()
        .enumerate()
        .map(|(w, window)| {
            (
                window.to_string(),
                json!(simulated.iter().filter(|s| s.1[w]).count()),
            )
        })
        .collect();

    let summary = json!({
        "dataset": data.label,
        "model": {
            "name": data.model.name,
            "revision": data.model.revision,
            "dimensions": data.model.dimensions,
            "normalized": data.model.normalized,
        },
        "source": data.source,
        "chunks": data.chunks,
        "files": data.files.len(),
        "distinct_vectors": units.texts.len(),
        "queries": data.queries.len(),
        "k": k,
        "segments": segment_count,
        "publish_ms": publish_ms,
        "bind_ms": bind_ms,
        "bytes": {
            "scan_int8": bytes.scan,
            "f32": bytes.vectors,
            "vector_units": bytes.units,
            "chunk_maps": bytes.chunk_maps,
            "segments_total": segment_bytes,
            "today_resident_f32": units.texts.len() * dimensions * 4,
        },
        "parity": {
            "all_equal": !failed,
            "bound_violations": violations,
            "parallel_differing": parallel_differing,
            "scopes": scope_values,
            "wall_ms": parity_ms,
        },
        "latency_us_warm": {
            "search": percentiles(search_us),
            "scan_only": percentiles(scan_us),
            "brute_force_f32_first_50": percentiles(brute_us),
        },
        "parallel": {
            "threads": rayon::current_num_threads(),
            "queries": data.queries.len(),
            "wall_ms": parallel_ms,
        },
        "warm_cache": {
            "budget_bytes": warm_cache.budget_bytes,
            "resident_bytes": warm_cache.resident_bytes,
            "hits": warm_cache.hits,
            "misses": warm_cache.misses,
        },
        "cold_cache_first_50": {
            "first_query_us": cold_us.first().copied(),
            "search_us": percentiles(cold_us),
            "blocks_loaded": percentiles(cold_blocks),
        },
        "quantization_alternatives": quantization,
        "fixed_window_exact_queries": window_exact,
        "peak_rss_mb": peak_rss_mb(),
    });
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    assert!(!failed, "the plane's answers differ from the reference");
}
