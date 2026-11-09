//! Reports what one family's store folder holds and who reads it: every
//! segment file, the manifests and leases that keep it, each checkout's root
//! and age, which segments each checkout's published state reads, and how
//! many bytes each kind of section takes.
//!
//! Point it at a copy of a family folder, never at a live one: reading the
//! newest generation takes a lease (a file under `leases/`) as any reader
//! does. It never takes the writer lock and never removes anything.
//!
//! ```text
//! cargo run --release -p aft-store --example store_census -- <family dir> [roots file] [idle hours]
//! ```
//!
//! A checkout's root is classified as present, gone or unknown. Without a
//! roots file the example checks the folder itself, by the rule the data
//! layer retires checkouts by (gone: not found while its parent can be
//! listed). A roots file overrides that: one line per root,
//! `present<TAB><root>` or `gone<TAB><root>`, for roots this process may not
//! stat. A checkout whose state file is older than `idle hours` (default 24)
//! counts as idle in the last estimate.
//!
//! Which segment a checkout reads each unit from follows the readers' rule:
//! the newest segment in manifest order that holds it (text units, content
//! records, call-graph fragments), and for vectors the rule
//! `needed_vector_segments` in the data layer applies. Content records are
//! keyed by the data layer's extractor version, given as
//! `AFT_EXTRACTOR_VERSION` or defaulting to the one below; with the wrong
//! one no record is found and the report says so.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::too_many_lines,
    reason = "a measurement tool: it stops on any failure and prints its result"
)]

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use aft_store::Family;
use aft_store::cache::BlockCache;
use aft_store::checkout::{CheckoutState, PathKind};
use aft_store::content::{ContentSegment, RecordKey};
use aft_store::graph::{FragmentKey, GraphSegment};
use aft_store::ids::{CheckoutId, ContentKey, Digest128, FamilyId};
use aft_store::segment::{KnownSection, SectionKind, Segment};
use aft_store::text::{Located, TextLocator, TextSegment};
use aft_store::vector::{ChunkMapKey, ModelId, TextDigest, VectorSegment};

/// The data layer's `EXTRACTOR_VERSION` (crates/aft/src/data_layer/content.rs)
/// in the build that wrote the store being read.
const EXTRACTOR_VERSION: &str = "ruled-callgraph-v14-resolver-precision+record-12";

/// The plane a section kind belongs to, by the kind's high byte: text,
/// content, graph, vector, or anything else.
fn plane_of(kind: SectionKind) -> usize {
    match kind.get() >> 8 {
        1 => 0,
        2 => 1,
        3 => 2,
        4 => 3,
        _ => 4,
    }
}

type Bytes = [u64; 5];

fn add(into: &mut Bytes, from: &Bytes) {
    for (a, b) in into.iter_mut().zip(from) {
        *a += b;
    }
}

fn mb(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / 1e6)
}

fn bytes_row(bytes: &Bytes, total: u64) -> String {
    let named = bytes.iter().sum::<u64>();
    format!(
        "text {} | content {} | graph {} | vector {} | other {} | overhead {} | total {} MB",
        mb(bytes[0]),
        mb(bytes[1]),
        mb(bytes[2]),
        mb(bytes[3]),
        mb(bytes[4]),
        mb(total.saturating_sub(named)),
        mb(total)
    )
}

/// A UTC timestamp, minutes precision.
fn utc(unix_ms: u64) -> String {
    let secs = unix_ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60
    )
}

fn mtime_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn hex_digest(text: &str) -> Digest128 {
    assert_eq!(
        text.len(),
        32,
        "a family folder is named by its 32-digit id"
    );
    let mut bytes = [0; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * index..2 * index + 2], 16).unwrap();
    }
    Digest128::from_bytes(bytes)
}

/// The payload of a record file (manifest, horizon, lease), checked for the
/// record magic and family. The store's record reader is crate-private; the
/// layout is `crate::family::record::encode`'s: a 48-byte header (magic,
/// kind, version, padding, family, number, payload length), the payload, and
/// a digest of everything before it.
fn record_payload(path: &Path, family: FamilyId) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 48 + 16 || &bytes[..6] != b"AFTREC" {
        return None;
    }
    if bytes[16..32] != family.digest().as_bytes()[..] {
        return None;
    }
    let len = u64::from_le_bytes(bytes[40..48].try_into().unwrap()) as usize;
    if bytes.len() != 48 + len + 16 {
        return None;
    }
    if Digest128::of(&bytes[..48 + len]).as_bytes()[..] != bytes[48 + len..] {
        return None;
    }
    Some(bytes[48..48 + len].to_vec())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// One manifest as the store encodes it (`Manifest::encode`): the segment
/// ids it names and its checkouts with their state numbers.
struct RawManifest {
    created_ms: u64,
    segments: Vec<(u64, u64)>,
    checkouts: Vec<(String, u64)>,
}

fn decode_manifest(payload: &[u8]) -> RawManifest {
    let created_ms = u64_at(payload, 0);
    let count = u32::from_le_bytes(payload[16..20].try_into().unwrap()) as usize;
    let mut at = 20;
    let mut segments = Vec::with_capacity(count);
    for _ in 0..count {
        segments.push((u64_at(payload, at), u64_at(payload, at + 8)));
        at += 16;
    }
    let checkouts_count = u32::from_le_bytes(payload[at..at + 4].try_into().unwrap()) as usize;
    at += 4;
    let mut checkouts = Vec::with_capacity(checkouts_count);
    for _ in 0..checkouts_count {
        let id = Digest128::from_bytes(payload[at..at + 16].try_into().unwrap()).to_string();
        checkouts.push((id, u64_at(payload, at + 16)));
        at += 48;
    }
    RawManifest {
        created_ms,
        segments,
        checkouts,
    }
}

/// Files named `<prefix><number>` in `dir`, by number.
fn numbered(dir: &Path, prefix: &str) -> BTreeMap<u64, PathBuf> {
    let mut found = BTreeMap::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Some(number) = name.strip_prefix(prefix).and_then(|n| n.parse().ok()) {
            found.insert(number, path);
        }
    }
    found
}

/// Every section kind and version a segment could hold, so that sizes count
/// every section, including any this build does not read.
fn every_section() -> Vec<KnownSection> {
    let mut all = Vec::new();
    for kind in 0..0x1000u16 {
        for version in 0..8u16 {
            all.push(KnownSection {
                kind: SectionKind::new(kind),
                version,
            });
        }
    }
    all
}

fn readable_sections() -> Vec<KnownSection> {
    aft_store::text::KNOWN
        .into_iter()
        .chain(aft_store::content::KNOWN)
        .chain(aft_store::graph::KNOWN)
        .chain(aft_store::graph::KNOWN_OPTIONAL)
        .chain(aft_store::vector::KNOWN)
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Root {
    Present,
    Gone,
    Unknown,
}

/// The data layer's `folder_is_gone`, three-way.
fn classify_root(root: &Path) -> Root {
    match std::fs::symlink_metadata(root) {
        Ok(_) => Root::Present,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if root
                .parent()
                .is_some_and(|parent| std::fs::read_dir(parent).is_ok())
            {
                Root::Gone
            } else {
                Root::Unknown
            }
        }
        Err(_) => Root::Unknown,
    }
}

/// A unit a checkout reads, by plane: the plane's key bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Unit {
    Text(ContentKey),
    Record(RecordKey),
    Fragment(FragmentKey),
    Vector(ModelId, TextDigest),
    /// A chunk map: counted with no bytes of its own, since the vector
    /// plane's bytes are averaged over its vectors.
    Map(ModelId, ChunkMapKey),
}

impl Unit {
    const fn plane(self) -> usize {
        match self {
            Self::Text(_) => 0,
            Self::Record(_) => 1,
            Self::Fragment(_) => 2,
            Self::Vector(..) => 3,
            Self::Map(..) => 4,
        }
    }
}

struct SegmentFile {
    id: u64,
    len: u64,
    mtime_ms: u64,
    created_ms: u64,
    bytes: Bytes,
    /// Units each plane holds: contents, records, fragments, vectors (and
    /// none counted for the last).
    units: [u64; 5],
    error: Option<String>,
}

struct CheckoutInfo {
    id: CheckoutId,
    short: String,
    state_number: u64,
    state_mtime: u64,
    root: Option<PathBuf>,
    root_class: Root,
    paths: usize,
    has_fragments: bool,
    /// Places in the manifest its text, content and call graph read from.
    reads: BTreeSet<usize>,
    /// Places its vectors (chunk maps and vectors) are read from.
    vector_reads: BTreeSet<usize>,
    /// Each unit it reads, with the place it reads it from.
    units: Vec<(usize, Unit)>,
    /// Indexed text, records and fragments its state names that no segment
    /// holds.
    missing: [usize; 3],
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let dir = PathBuf::from(&args[1]);
    let overrides = args
        .get(2)
        .filter(|path| !path.is_empty() && *path != "-")
        .map(|path| {
            std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .filter_map(|line| {
                    let (class, root) = line.split_once('\t')?;
                    let class = match class {
                        "present" => Root::Present,
                        "gone" => Root::Gone,
                        _ => Root::Unknown,
                    };
                    Some((PathBuf::from(root), class))
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let idle_hours = args
        .get(3)
        .map_or(24.0, |hours| hours.parse::<f64>().unwrap());
    let extractor =
        std::env::var("AFT_EXTRACTOR_VERSION").unwrap_or_else(|_| EXTRACTOR_VERSION.to_string());
    let family_id = FamilyId::from_digest(hex_digest(dir.file_name().unwrap().to_str().unwrap()));
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    println!("# family {}", dir.display());
    println!(
        "extractor version: {extractor}; idle past {idle_hours} h; now {}",
        utc(now_ms)
    );
    // With AFT_CENSUS_RECLAIM=1, first removes what nothing can read any
    // more, as the next publication's reclamation would: for a copy whose
    // last writer left files that its readers' leases held then.
    if std::env::var("AFT_CENSUS_RECLAIM").is_ok_and(|value| value == "1") {
        let family = Family::open(&dir, family_id).unwrap();
        let mut session = family
            .try_write()
            .unwrap()
            .expect("no writer holds the copy");
        println!("reclaimed first: {:?}", session.reclaim().unwrap());
    }
    // Manifests, horizon and leases, as files.
    println!("\n## manifests, horizon, leases");
    let manifests = numbered(&dir, "MANIFEST-")
        .into_iter()
        .filter_map(|(number, path)| {
            let payload = record_payload(&path, family_id);
            if payload.is_none() {
                println!("MANIFEST-{number}: does not check (torn or damaged)");
            }
            payload.map(|payload| (number, decode_manifest(&payload)))
        })
        .collect::<BTreeMap<_, _>>();
    for (number, manifest) in &manifests {
        println!(
            "MANIFEST-{number}: created {}, {} segments, {} checkouts",
            utc(manifest.created_ms),
            manifest.segments.len(),
            manifest.checkouts.len()
        );
    }
    let horizon = numbered(&dir, "HORIZON-")
        .into_iter()
        .rev()
        .find_map(|(number, path)| {
            let payload = record_payload(&path, family_id)?;
            Some((number, u64_at(&payload, 0)))
        });
    match horizon {
        Some((number, generation)) => println!("HORIZON-{number}: generation {generation}"),
        None => println!("no valid horizon"),
    }
    let mut owners = BTreeMap::<String, Vec<(u64, u64, u64)>>::new();
    for entry in std::fs::read_dir(dir.join("leases")).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        if let Some(stem) = name.strip_suffix(".owner") {
            owners.entry(stem.to_string()).or_default();
        } else if let Some(stem) = name.strip_suffix(".lease") {
            let (owner, number) = stem.rsplit_once('.').unwrap();
            let generation = record_payload(&path, family_id).map_or(0, |p| u64_at(&p, 0));
            owners.entry(owner.to_string()).or_default().push((
                number.parse().unwrap(),
                generation,
                mtime_ms(&path),
            ));
        }
    }
    for (owner, leases) in &owners {
        let listed = leases
            .iter()
            .map(|(number, generation, mtime)| {
                format!("#{number} gen {generation} at {}", utc(*mtime))
            })
            .collect::<Vec<_>>();
        println!("owner {}: {}", &owner[..8], listed.join(", "));
    }

    // Every segment file, with its sections' sizes by plane.
    let cache = Arc::new(BlockCache::new(512 << 20));
    let all_sections = every_section();
    let mut files = BTreeMap::<u64, SegmentFile>::new();
    for entry in std::fs::read_dir(dir.join("segments")).unwrap() {
        let path = entry.unwrap().path();
        let Some(id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".seg"))
            .and_then(|id| id.parse::<u64>().ok())
        else {
            println!("not a segment: {}", path.display());
            continue;
        };
        let len = std::fs::metadata(&path).unwrap().len();
        let mut file = SegmentFile {
            id,
            len,
            mtime_ms: mtime_ms(&path),
            created_ms: 0,
            bytes: [0; 5],
            units: [0; 5],
            error: None,
        };
        match Segment::open(&path, family_id, &all_sections, Arc::clone(&cache)) {
            Ok(segment) => {
                file.created_ms = segment.created_unix_ms();
                for (_, info) in segment.sections() {
                    file.bytes[plane_of(info.id.kind)] += info.len;
                }
            }
            Err(error) => file.error = Some(error.to_string()),
        }
        files.insert(id, file);
    }

    // The newest generation, read as a reader reads it.
    let family = Family::open(&dir, family_id).unwrap();
    let snapshot = family.snapshot().unwrap();
    let manifest = snapshot.manifest();
    let generation = snapshot.generation().get();
    let entries = manifest.segments().to_vec();
    println!(
        "\nnewest generation {generation}: {} segments, {} checkouts",
        entries.len(),
        manifest.checkouts().len()
    );
    let readable = readable_sections();
    let opened = entries
        .iter()
        .map(|entry| {
            snapshot
                .open_segment(entry.id, &readable, Arc::clone(&cache))
                .map_err(|error| println!("segment {} unreadable: {error}", entry.id.get()))
                .ok()
        })
        .collect::<Vec<_>>();
    let mut text_places = Vec::new();
    let mut texts = Vec::new();
    let mut contents = Vec::new();
    let mut graphs = Vec::new();
    let mut vectors = Vec::new();
    for (place, segment) in opened.iter().enumerate() {
        let file = files.get_mut(&entries[place].id.get()).unwrap();
        let Some(segment) = segment else {
            contents.push(None);
            graphs.push(None);
            vectors.push(None);
            continue;
        };
        if let Some(text) = TextSegment::open(segment).unwrap() {
            file.units[0] = u64::from(text.len());
            text_places.push(place);
            texts.push(text);
        }
        let content = ContentSegment::open(segment).unwrap();
        file.units[1] = content.as_ref().map_or(0, |c| u64::from(c.len()));
        contents.push(content);
        let graph = GraphSegment::open(segment).unwrap();
        file.units[2] = graph.as_ref().map_or(0, |g| u64::from(g.len()));
        graphs.push(graph);
        let vector = VectorSegment::open(segment).unwrap();
        file.units[3] = vector.as_ref().map_or(0, |v| u64::from(v.len()));
        vectors.push(vector);
    }
    let locator = TextLocator::build(&texts).unwrap();
    let max_indexed_len = aft_store::scan::ScanPolicy::default().max_indexed_len;

    // Vector holders, newest last, by model, as `needed_vector_segments`.
    let mut map_holders = HashMap::<(ModelId, ChunkMapKey), usize>::new();
    let mut text_holders = HashMap::<(ModelId, TextDigest), usize>::new();
    let mut models = Vec::new();
    for (place, vector) in vectors.iter().enumerate() {
        let Some(vector) = vector else { continue };
        let model = vector.model().id();
        if !models.contains(&model) {
            models.push(model);
        }
        for key in vector.chunk_map_keys().unwrap() {
            map_holders.insert((model, key), place);
        }
        for text in vector.texts().unwrap() {
            text_holders.insert((model, text), place);
        }
    }

    let newest = |find: &dyn Fn(usize) -> bool| (0..entries.len()).rev().find(|&p| find(p));
    let mut record_memo = HashMap::<RecordKey, Option<usize>>::new();
    let mut fragment_memo = HashMap::<FragmentKey, Option<usize>>::new();
    let mut chunk_memo = HashMap::<(ModelId, ChunkMapKey), Vec<TextDigest>>::new();
    let mut checkouts = Vec::new();
    for state_ref in manifest.checkouts() {
        let state: CheckoutState = snapshot
            .checkout_state(state_ref.checkout)
            .unwrap()
            .expect("the manifest names the state");
        let id = state_ref.checkout;
        let state_path = dir
            .join("checkouts")
            .join(id.to_string())
            .join(format!("STATE-{}", state_ref.number));
        let root = state.root().map(Path::to_path_buf);
        let root_class = root.as_ref().map_or(Root::Unknown, |root| {
            overrides
                .get(root)
                .copied()
                .unwrap_or_else(|| classify_root(root))
        });
        let mut info = CheckoutInfo {
            id,
            short: id.to_string()[..8].to_string(),
            state_number: state_ref.number,
            state_mtime: mtime_ms(&state_path),
            root,
            root_class,
            paths: state.paths().len(),
            has_fragments: state.has_fragments(),
            reads: BTreeSet::new(),
            vector_reads: BTreeSet::new(),
            units: Vec::new(),
            missing: [0; 3],
        };
        for (position, record) in state.paths().iter().enumerate() {
            match locator.get(record.content) {
                Some(Located::Indexed { segment, .. }) => {
                    let place = text_places[segment as usize];
                    info.reads.insert(place);
                    info.units.push((place, Unit::Text(record.content)));
                }
                Some(Located::Unindexed { .. }) => {}
                None if record.kind == PathKind::Text && record.stat.size <= max_indexed_len => {
                    info.missing[0] += 1;
                }
                None => {}
            }
            if !record.language.is_empty() {
                let key = RecordKey::new(record.content, &record.language, &extractor);
                let holder = *record_memo.entry(key).or_insert_with(|| {
                    newest(&|place| {
                        contents[place]
                            .as_ref()
                            .is_some_and(|content| content.find(key).unwrap().is_some())
                    })
                });
                match holder {
                    Some(place) => {
                        info.reads.insert(place);
                        info.units.push((place, Unit::Record(key)));
                    }
                    None => info.missing[1] += 1,
                }
            }
            if let Some(fragment) = state.fragment(position) {
                let key = FragmentKey::from_digest(fragment);
                let holder = *fragment_memo.entry(key).or_insert_with(|| {
                    newest(&|place| {
                        graphs[place]
                            .as_ref()
                            .is_some_and(|graph| graph.find(key).unwrap().is_some())
                    })
                });
                match holder {
                    Some(place) => {
                        info.reads.insert(place);
                        info.units.push((place, Unit::Fragment(key)));
                    }
                    None => info.missing[2] += 1,
                }
            }
            if record.kind.has_content() {
                let key = ChunkMapKey::new(record.content, &record.path);
                for model in &models {
                    let Some(&holder) = map_holders.get(&(*model, key)) else {
                        continue;
                    };
                    info.vector_reads.insert(holder);
                    info.units.push((holder, Unit::Map(*model, key)));
                    let chunks = chunk_memo.entry((*model, key)).or_insert_with(|| {
                        vectors[holder]
                            .as_ref()
                            .unwrap()
                            .chunk_map(record.content, &record.path)
                            .unwrap()
                            .unwrap_or_default()
                            .into_iter()
                            .map(|chunk| chunk.text)
                            .collect()
                    });
                    for text in chunks.iter() {
                        if let Some(&text_holder) = text_holders.get(&(*model, *text)) {
                            info.vector_reads.insert(text_holder);
                            info.units.push((text_holder, Unit::Vector(*model, *text)));
                        }
                    }
                }
            }
        }
        checkouts.push(info);
    }
    report(
        &dir, &files, &manifests, horizon, &entries, &checkouts, now_ms, idle_hours,
    );
}

/// Which checkouts a simulation folds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FoldRule {
    /// None.
    Never,
    /// Those whose counts call for a fold, by the data layer's rule: more
    /// than `COMPACT_AT` (8) segments only it reads, or more than `READ_AT`
    /// (16) in all; text, content and call graph only.
    WhenDue,
    /// Every member, once.
    Every,
}

/// The outcome of a simulation: segments kept, how many of them hold
/// vectors, and the bytes kept by plane (whole files for existing segments,
/// estimated bytes for new ones).
struct Simulated {
    segments: usize,
    vector_segments: usize,
    folds: usize,
    bytes: [f64; 5],
}

/// The bytes one unit of `plane` takes in `file`: the plane's bytes over
/// its unit count. An average: postings and lookup tables are spread over
/// the units.
fn unit_bytes(file: &SegmentFile, plane: usize) -> f64 {
    if file.units[plane] == 0 {
        0.0
    } else {
        file.bytes[plane] as f64 / file.units[plane] as f64
    }
}

/// Simulates folds of `members` (in order of most segments only it reads)
/// as `Checkout::fold` does them, and optionally one merge of vector-only
/// segments (smallest reached bytes first, while the reached bytes copied
/// stay within `merge_budget`) into one, on the unit placement read from
/// the store. A fold
/// copies every text, content and call-graph unit its checkout reads into a
/// new segment, except those another member reads from one of the 4
/// segments serving the most of them (`SHARE_FROM`); copied units are read
/// from the new segment by every member, as the newest holder.
fn simulate(
    checkouts: &[CheckoutInfo],
    members: &[usize],
    files: &[&SegmentFile],
    rule: FoldRule,
    merge_budget: Option<f64>,
) -> Simulated {
    let existing = files.len();
    let mut holder = HashMap::<Unit, usize>::new();
    for &member in members {
        for (place, unit) in &checkouts[member].units {
            holder.insert(*unit, *place);
        }
    }
    // New segments' estimated bytes and unit counts by plane.
    let mut new_bytes = Vec::<([f64; 5], [usize; 5])>::new();
    let reads_of = |member: usize, holder: &HashMap<Unit, usize>| {
        checkouts[member]
            .units
            .iter()
            .filter(|(_, unit)| unit.plane() < 3)
            .map(|(_, unit)| holder[unit])
            .collect::<BTreeSet<_>>()
    };
    let mut order = members.to_vec();
    let mut folds = 0;
    // Each member folds at most once per round, and rounds stop when none
    // folds: at most `members.len()` rounds.
    for _ in 0..members.len().max(1) {
        if rule == FoldRule::Never {
            break;
        }
        let mut folded_any = false;
        let own_count = |member: usize, holder: &HashMap<Unit, usize>| {
            let others = members
                .iter()
                .filter(|&&other| other != member)
                .flat_map(|&other| reads_of(other, holder))
                .collect::<HashSet<_>>();
            let reads = reads_of(member, holder);
            (
                reads.iter().filter(|p| !others.contains(p)).count(),
                reads.len(),
            )
        };
        order.sort_by_key(|&member| std::cmp::Reverse(own_count(member, &holder).0));
        for &member in &order {
            let (own, read) = own_count(member, &holder);
            let due = match rule {
                FoldRule::Never => false,
                FoldRule::WhenDue => own > 8 || read > 16,
                FoldRule::Every => folds < members.len() && read > 1,
            };
            if !due {
                continue;
            }
            let others_units = members
                .iter()
                .filter(|&&other| other != member)
                .flat_map(|&other| checkouts[other].units.iter().map(|(_, unit)| *unit))
                .filter(|unit| unit.plane() < 3)
                .collect::<HashSet<_>>();
            let mut serves = HashMap::<usize, usize>::new();
            let mine = checkouts[member]
                .units
                .iter()
                .map(|(_, unit)| *unit)
                .filter(|unit| unit.plane() < 3)
                .collect::<HashSet<_>>();
            for unit in &mine {
                if others_units.contains(unit) {
                    *serves.entry(holder[unit]).or_default() += 1;
                }
            }
            let mut ranked = serves.into_iter().collect::<Vec<_>>();
            ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
            let kept = ranked
                .into_iter()
                .take(4)
                .map(|(place, _)| place)
                .collect::<HashSet<_>>();
            let place = existing + new_bytes.len();
            let mut bytes = [0.0; 5];
            let mut counts = [0; 5];
            for unit in mine {
                let from = holder[&unit];
                if others_units.contains(&unit) && kept.contains(&from) {
                    continue;
                }
                let plane = unit.plane();
                bytes[plane] += if from < existing {
                    unit_bytes(files[from], plane)
                } else {
                    // A unit an earlier simulated fold copied: that fold's
                    // average for the plane.
                    let (earlier, earlier_counts) = &new_bytes[from - existing];
                    earlier[plane] / 1.0f64.max(earlier_counts[plane] as f64)
                };
                counts[plane] += 1;
                holder.insert(unit, place);
            }
            new_bytes.push((bytes, counts));
            folds += 1;
            folded_any = true;
        }
        if !folded_any {
            break;
        }
    }
    let reachable = holder.values().copied().collect::<BTreeSet<_>>();
    let mut vectors_at = HashMap::<usize, usize>::new();
    for (unit, place) in &holder {
        if unit.plane() == 3 {
            *vectors_at.entry(*place).or_default() += 1;
        }
    }
    let mut result = Simulated {
        segments: 0,
        vector_segments: 0,
        folds,
        bytes: [0.0; 5],
    };
    let mut merged_vectors = 0.0;
    // The vector-only segments one merge takes: smallest reached bytes
    // first, while what it copies stays within the budget.
    let mut merging = HashSet::new();
    if let Some(budget) = merge_budget {
        let mut candidates = reachable
            .iter()
            .filter(|&&place| place < existing)
            .filter(|&&place| {
                let file = files[place];
                file.bytes[3] > 0 && file.bytes[..3].iter().all(|b| *b == 0)
            })
            .map(|&place| {
                let read = vectors_at.get(&place).copied().unwrap_or(0);
                (unit_bytes(files[place], 3) * read as f64, place)
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut taken = 0.0;
        for (bytes, place) in candidates {
            if taken + bytes > budget {
                break;
            }
            taken += bytes;
            merging.insert(place);
        }
        merged_vectors = taken;
    }
    for place in reachable {
        if place >= existing {
            result.segments += 1;
            for (sum, bytes) in result.bytes.iter_mut().zip(new_bytes[place - existing].0) {
                *sum += bytes;
            }
            continue;
        }
        let file = files[place];
        if merging.contains(&place) {
            continue;
        }
        result.segments += 1;
        if file.bytes[3] > 0 {
            result.vector_segments += 1;
        }
        for (sum, bytes) in result.bytes.iter_mut().zip(file.bytes) {
            *sum += bytes as f64;
        }
    }
    if !merging.is_empty() {
        result.segments += 1;
        result.vector_segments += 1;
        result.bytes[3] += merged_vectors;
    }
    result
}

/// What a set of checkouts keeps: the places they read from, and for each
/// place and plane the distinct units they read there.
struct Kept {
    places: BTreeSet<usize>,
    units: HashMap<(usize, usize), HashSet<Unit>>,
}

fn kept_by<'c>(members: impl Iterator<Item = &'c CheckoutInfo>) -> Kept {
    let mut kept = Kept {
        places: BTreeSet::new(),
        units: HashMap::new(),
    };
    for checkout in members {
        kept.places.extend(&checkout.reads);
        kept.places.extend(&checkout.vector_reads);
        for (place, unit) in &checkout.units {
            kept.units
                .entry((*place, unit.plane()))
                .or_default()
                .insert(*unit);
        }
    }
    kept
}

#[allow(clippy::too_many_arguments, reason = "one report over everything read")]
fn report(
    dir: &Path,
    files: &BTreeMap<u64, SegmentFile>,
    manifests: &BTreeMap<u64, RawManifest>,
    horizon: Option<(u64, u64)>,
    entries: &[aft_store::family::SegmentEntry],
    checkouts: &[CheckoutInfo],
    now_ms: u64,
    idle_hours: f64,
) {
    let place_of = entries
        .iter()
        .enumerate()
        .map(|(place, entry)| (entry.id.get(), place))
        .collect::<HashMap<_, _>>();
    let horizon_generation = horizon.map_or(0, |(_, generation)| generation);
    let mut named_by = HashMap::<u64, Vec<u64>>::new();
    for (number, manifest) in manifests {
        for (id, _) in &manifest.segments {
            named_by.entry(*id).or_default().push(*number);
        }
    }

    println!("\n## segment files on disk");
    let mut classes = BTreeMap::<&str, (usize, Bytes, u64)>::new();
    for file in files.values() {
        let class = if place_of.contains_key(&file.id) {
            "named by the newest manifest"
        } else if named_by
            .get(&file.id)
            .is_some_and(|numbers| numbers.iter().any(|n| *n >= horizon_generation))
        {
            "named only by older manifests at or above the horizon (leased)"
        } else {
            "named by no manifest at or above the horizon (reclaimable now)"
        };
        let slot = classes.entry(class).or_insert((0, [0; 5], 0));
        slot.0 += 1;
        add(&mut slot.1, &file.bytes);
        slot.2 += file.len;
        if let Some(error) = &file.error {
            println!("segment {} could not be opened: {error}", file.id);
        }
    }
    for (class, (count, bytes, len)) in &classes {
        println!("{class}: {count} files; {}", bytes_row(bytes, *len));
    }
    for file in files.values() {
        if !place_of.contains_key(&file.id) {
            println!(
                "  not in newest: segment {} {} MB written {} named by {:?}",
                file.id,
                mb(file.len),
                utc(file.mtime_ms),
                named_by.get(&file.id).cloned().unwrap_or_default()
            );
        }
    }
    let mut by_day = BTreeMap::<String, (usize, u64, usize, u64)>::new();
    for file in files.values() {
        let day = utc(file.mtime_ms)[..10].to_string();
        let slot = by_day.entry(day).or_default();
        slot.0 += 1;
        slot.1 += file.len;
        if place_of.contains_key(&file.id) {
            slot.2 += 1;
            slot.3 += file.len;
        }
    }
    for (day, (count, len, newest_count, newest_len)) in &by_day {
        println!(
            "  written {day}: {count} files {} MB; of them in the newest manifest {newest_count} files {} MB",
            mb(*len),
            mb(*newest_len)
        );
    }

    // Readers of each place.
    let mut readers = vec![Vec::<usize>::new(); entries.len()];
    let mut vector_readers = vec![Vec::<usize>::new(); entries.len()];
    for (index, checkout) in checkouts.iter().enumerate() {
        for place in &checkout.reads {
            readers[*place].push(index);
        }
        for place in &checkout.vector_reads {
            vector_readers[*place].push(index);
        }
    }
    let idle_ms = (idle_hours * 3_600_000.0) as u64;
    let is_idle = |checkout: &CheckoutInfo| now_ms.saturating_sub(checkout.state_mtime) > idle_ms;

    println!("\n## checkouts of the newest generation");
    for (index, checkout) in checkouts.iter().enumerate() {
        let others = |place: &usize, exclude_gone: bool| {
            readers[*place].iter().any(|other| {
                *other != index && !(exclude_gone && checkouts[*other].root_class == Root::Gone)
            })
        };
        let own = checkout.reads.iter().filter(|p| !others(p, false)).count();
        let own_live = checkout.reads.iter().filter(|p| !others(p, true)).count();
        let only_mine = |place: &usize| {
            !readers[*place].iter().any(|other| *other != index)
                && !vector_readers[*place].iter().any(|other| *other != index)
        };
        let only = checkout
            .reads
            .iter()
            .chain(&checkout.vector_reads)
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(only_mine)
            .collect::<Vec<_>>();
        let only_bytes = only.iter().map(|p| entries[*p].len).sum::<u64>();
        let wants_fold = own > 8 || checkout.reads.len() > 16;
        println!(
            "{} STATE-{} written {} ({:.1} h ago){} root {} [{:?}]",
            checkout.short,
            checkout.state_number,
            utc(checkout.state_mtime),
            now_ms.saturating_sub(checkout.state_mtime) as f64 / 3.6e6,
            if is_idle(checkout) { " idle" } else { "" },
            checkout
                .root
                .as_ref()
                .map_or_else(|| "(not recorded)".to_string(), |r| r.display().to_string()),
            checkout.root_class
        );
        println!(
            "    {} paths, fragments {}; text/content/graph read from {} segments, {} that no other checkout reads ({} counting gone checkouts as gone); fold due by the counts: {}; vectors read from {} segments; only it reads {} segments, {} MB; not found: {} text, {} records, {} fragments",
            checkout.paths,
            if checkout.has_fragments {
                "named"
            } else {
                "none"
            },
            checkout.reads.len(),
            own,
            own_live,
            wants_fold,
            checkout.vector_reads.len(),
            only.len(),
            mb(only_bytes),
            checkout.missing[0],
            checkout.missing[1],
            checkout.missing[2],
        );
        let _ = checkout.id;
    }

    println!(
        "\n## segments of the newest generation (place, id, MB, written, planes MB, units held, units some checkout reads there, readers)"
    );
    let all_kept = kept_by(checkouts.iter());
    for (place, entry) in entries.iter().enumerate() {
        let file = &files[&entry.id.get()];
        let read = |plane: usize| all_kept.units.get(&(place, plane)).map_or(0, HashSet::len);
        let names = |list: &[usize]| {
            list.iter()
                .map(|index| checkouts[*index].short.clone())
                .collect::<Vec<_>>()
                .join(",")
        };
        println!(
            "{place:3} seg {:4} {:>8} MB {} t{} c{} g{} v{} | units t{} c{} g{} v{} | read t{} c{} g{} v{} | reads [{}] vectors [{}]",
            entry.id.get(),
            mb(entry.len),
            utc(if file.created_ms > 0 {
                file.created_ms
            } else {
                file.mtime_ms
            }),
            mb(file.bytes[0]),
            mb(file.bytes[1]),
            mb(file.bytes[2]),
            mb(file.bytes[3]),
            file.units[0],
            file.units[1],
            file.units[2],
            file.units[3],
            read(0),
            read(1),
            read(2),
            read(3),
            names(&readers[place]),
            names(&vector_readers[place]),
        );
    }

    println!("\n## what each set of checkouts keeps");
    let scenarios: [(&str, Box<dyn Fn(&CheckoutInfo) -> bool>); 4] = [
        ("every checkout the manifest names", Box::new(|_| true)),
        (
            "without checkouts whose folder is gone",
            Box::new(|c: &CheckoutInfo| c.root_class != Root::Gone),
        ),
        (
            "without gone or unknown-root checkouts",
            Box::new(|c: &CheckoutInfo| c.root_class == Root::Present),
        ),
        (
            "present and not idle",
            Box::new(|c: &CheckoutInfo| c.root_class == Root::Present && !is_idle(c)),
        ),
    ];
    let total_len = files.values().map(|file| file.len).sum::<u64>();
    let mut total_bytes = [0; 5];
    for file in files.values() {
        add(&mut total_bytes, &file.bytes);
    }
    println!(
        "on disk: {} segment files; {}",
        files.len(),
        bytes_row(&total_bytes, total_len)
    );
    for (name, member) in &scenarios {
        let members = checkouts.iter().filter(|c| member(c)).collect::<Vec<_>>();
        let kept = kept_by(members.iter().copied());
        let mut bytes = [0; 5];
        let mut len = 0;
        let mut estimate = [0.0f64; 4];
        let mut vector_segments = 0;
        for place in &kept.places {
            let file = &files[&entries[*place].id.get()];
            add(&mut bytes, &file.bytes);
            len += file.len;
            if file.bytes[3] > 0 {
                vector_segments += 1;
            }
            for plane in 0..4 {
                if file.units[plane] == 0 {
                    continue;
                }
                let read = kept.units.get(&(*place, plane)).map_or(0, HashSet::len);
                estimate[plane] +=
                    file.bytes[plane] as f64 * read as f64 / file.units[plane] as f64;
            }
        }
        println!(
            "{name}: {} checkouts read {} segments ({} with vectors); {}",
            members.len(),
            kept.places.len(),
            vector_segments,
            bytes_row(&bytes, len)
        );
        println!(
            "    if each kept segment held only the units these checkouts read there (by unit count): text {} | content {} | graph {} | vector {} MB",
            mb(estimate[0] as u64),
            mb(estimate[1] as u64),
            mb(estimate[2] as u64),
            mb(estimate[3] as u64),
        );
    }
    let unread = (0..entries.len())
        .filter(|place| readers[*place].is_empty() && vector_readers[*place].is_empty())
        .collect::<Vec<_>>();
    println!(
        "segments of the newest manifest no checkout reads: {} ({} MB): {:?}",
        unread.len(),
        mb(unread.iter().map(|p| entries[*p].len).sum()),
        unread
            .iter()
            .map(|p| entries[*p].id.get())
            .collect::<Vec<_>>()
    );

    println!("\n## simulated fixes (segments kept, of them holding vectors, bytes by plane)");
    let by_place = entries
        .iter()
        .map(|entry| &files[&entry.id.get()])
        .collect::<Vec<_>>();
    let everyone = (0..checkouts.len()).collect::<Vec<_>>();
    let active = (0..checkouts.len())
        .filter(|&index| {
            checkouts[index].root_class == Root::Present && !is_idle(&checkouts[index])
        })
        .collect::<Vec<_>>();
    // A fill round's byte budget (`ROUND_BYTES` in the data layer's
    // vectors module): what one merge may copy.
    let round = Some(f64::from(64u32 << 20));
    let all = Some(f64::INFINITY);
    let cases = [
        ("as is", &everyone, FoldRule::Never, None),
        (
            "fold every checkout whose counts call for it",
            &everyone,
            FoldRule::WhenDue,
            None,
        ),
        ("fold every checkout once", &everyone, FoldRule::Every, None),
        (
            "merge vector segments, one 64 MiB merge",
            &everyone,
            FoldRule::Never,
            round,
        ),
        (
            "merge every vector segment into one",
            &everyone,
            FoldRule::Never,
            all,
        ),
        (
            "fold when due + one 64 MiB vector merge",
            &everyone,
            FoldRule::WhenDue,
            round,
        ),
        ("retire idle checkouts", &active, FoldRule::Never, None),
        (
            "retire idle + one 64 MiB vector merge",
            &active,
            FoldRule::Never,
            round,
        ),
        (
            "retire idle + fold when due + one 64 MiB vector merge",
            &active,
            FoldRule::WhenDue,
            round,
        ),
    ];
    for (name, members, rule, merge) in cases {
        let outcome = simulate(checkouts, members, &by_place, rule, merge);
        println!(
            "{name}: {} checkouts, {} folds -> {} segments ({} with vectors); text {} | content {} | graph {} | vector {} | total {} MB",
            members.len(),
            outcome.folds,
            outcome.segments,
            outcome.vector_segments,
            mb(outcome.bytes[0] as u64),
            mb(outcome.bytes[1] as u64),
            mb(outcome.bytes[2] as u64),
            mb(outcome.bytes[3] as u64),
            mb(outcome.bytes.iter().sum::<f64>() as u64),
        );
    }
    let _ = dir;
}
