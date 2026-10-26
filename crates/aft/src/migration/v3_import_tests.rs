use std::path::{Path, PathBuf};

use super::*;
use crate::semantic_index::{EmbedTextCaps, SemanticIndex, SemanticIndexFingerprint};

pub(crate) const POLICY: TrigramPolicy = TrigramPolicy {
    max_file_size: 1 << 20,
};
pub(crate) const FAMILY: &str = "family";
pub(crate) const KEY: &str = "family";

pub(crate) fn write_tree(root: &Path, files: &[(&str, &[u8])]) {
    for (path, bytes) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
}

/// A checkout with a few source files, prose and a binary file.
pub(crate) fn checkout(dir: &Path) -> PathBuf {
    let root = dir.join("checkout");
    fs::create_dir_all(&root).unwrap();
    write_tree(
        &root,
        &[
            (
                "src/alpha.rs",
                b"pub fn alpha_total(values: &[u32]) -> u32 {\n    values.iter().sum()\n}\n",
            ),
            (
                "src/beta.rs",
                b"pub struct BetaCache {\n    entries: Vec<String>,\n}\n",
            ),
            (
                "notes/readme.md",
                b"# Notes\n\nSome prose about the alpha total.\n",
            ),
            ("assets/blob.bin", &[0, 159, 146, 150, 0, 1, 2, 3, 0, 0]),
        ],
    );
    fs::canonicalize(&root).unwrap()
}

/// Adds `count` generated source files, so an artifact is many times larger
/// than a small working set.
pub(crate) fn add_generated(root: &Path, count: usize) {
    for index in 0..count {
        let body = (0..20)
            .map(|line| {
                format!("pub fn generated_{index}_{line}(value: u64) -> u64 {{ value ^ {line} }}\n")
            })
            .collect::<String>();
        write_tree(root, &[(&format!("gen/file_{index}.rs"), body.as_bytes())]);
    }
}

/// Writes the previous release's trigram cache for `root` where the importer
/// reads it, and returns its bytes.
pub(crate) fn legacy_trigram(storage: &Path, root: &Path) -> Vec<u8> {
    let dir = storage.join("index").join(KEY);
    let mut index = crate::search_index::SearchIndex::build_with_limit(root, POLICY.max_file_size);
    assert!(index.write_to_disk(&dir, None), "legacy cache written");
    fs::read(dir.join("cache.bin")).unwrap()
}

pub(crate) fn deterministic(text: &str) -> Vec<f32> {
    blake3::hash(text.as_bytes()).as_bytes()[..16]
        .iter()
        .map(|byte| (f32::from(*byte) - 127.5) / 127.5)
        .collect()
}

pub(crate) fn fingerprint() -> SemanticIndexFingerprint {
    SemanticIndexFingerprint {
        backend: "test".to_owned(),
        model: "deterministic".to_owned(),
        base_url: "none".to_owned(),
        dimension: 16,
        chunking_version: 2,
        embed_text_caps: EmbedTextCaps::default(),
        ..SemanticIndexFingerprint::default()
    }
}

pub(crate) fn producer() -> SemanticProducer {
    let fingerprint = fingerprint();
    SemanticProducer::current(fingerprint.as_string(), fingerprint.embed_text_caps)
}

/// Writes the previous release's `semantic.bin` for `root` where the
/// importer reads it, and returns its bytes.
pub(crate) fn legacy_semantic(storage: &Path, root: &Path) -> Vec<u8> {
    let files = ["src/alpha.rs", "src/beta.rs", "notes/readme.md"]
        .iter()
        .map(|path| root.join(path))
        .collect::<Vec<_>>();
    let mut index = SemanticIndex::build(
        root,
        &files,
        &mut |texts: Vec<String>| Ok(texts.iter().map(|text| deterministic(text)).collect()),
        64,
    )
    .unwrap();
    index.set_fingerprint(fingerprint());
    let bytes = index.to_bytes();
    let path = super::super::per_checkout::legacy_semantic_path(storage, KEY);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, &bytes).unwrap();
    bytes
}

fn sorted(mut entries: Vec<BundleEntry>) -> Vec<BundleEntry> {
    entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
    entries
}

fn stream_trigram(
    path: &Path,
    working_set: usize,
) -> Result<(Vec<BundleEntry>, ReadStats), String> {
    let mut source = Source::open(path)?;
    let mut entries = Vec::new();
    convert_trigram_streaming(&mut source, &POLICY, working_set, &mut |entry| {
        entries.push(entry);
        Ok(())
    })?;
    Ok((entries, source.stats))
}

pub(super) fn input<'a>(
    storage: &'a Path,
    root: &'a Path,
    dir: &'a Path,
    semantic: Option<&'a SemanticProducer>,
) -> StageInput<'a> {
    StageInput {
        storage,
        legacy_key: KEY,
        root,
        dir,
        trigram: &POLICY,
        semantic,
        working_set: DEFAULT_WORKING_SET,
    }
}

/// Every file below `dir`, with its bytes, for byte-identity checks.
pub(crate) fn snapshot_tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    files
}

/// The streaming inversion equals the whole-file v2 conversion even when its
/// working set is far smaller than the artifact, and it never holds more
/// than the budget (or one file's postings) of decoded postings at once.
#[test]
fn streaming_trigram_conversion_is_bounded_and_equals_the_whole_file_conversion() {
    let dir = tempfile::tempdir().unwrap();
    let root = checkout(dir.path());
    add_generated(&root, 150);
    let storage = dir.path().join("storage");
    let bytes = legacy_trigram(&storage, &root);
    let whole = super::super::per_checkout::convert_trigram(&bytes, &POLICY).unwrap();
    let path = storage.join("index").join(KEY).join("cache.bin");

    const BUDGET: usize = 2 * 1024;
    let (entries, stats) = stream_trigram(&path, BUDGET).unwrap();
    assert_eq!(sorted(entries), whole.entries);
    let largest_file = whole
        .entries
        .iter()
        .map(|entry| {
            TrigramPayload::decode(&entry.payload)
                .unwrap()
                .records
                .len() as u64
                * std::mem::size_of::<TrigramRecord>() as u64
        })
        .max()
        .unwrap();
    assert!(
        bytes.len() > 20 * BUDGET,
        "the artifact ({} bytes) must dwarf the working set",
        bytes.len()
    );
    assert!(
        stats.peak_working_set <= (BUDGET as u64).max(largest_file),
        "held {} bytes of postings with a {BUDGET}-byte budget",
        stats.peak_working_set
    );
    assert!(stats.posting_passes > 3, "{stats:?}");
    assert!(stats.largest_read <= READ_CHUNK, "{stats:?}");

    // With the default budget the same artifact needs one counting pass and
    // one collecting pass.
    let (entries, stats) = stream_trigram(&path, DEFAULT_WORKING_SET).unwrap();
    assert_eq!(sorted(entries), whole.entries);
    assert_eq!(stats.posting_passes, 2);
}

/// Anything not provably equal to a cold build is rejected, never relabelled.
#[test]
fn streaming_trigram_conversion_rejects_incompatible_and_corrupt_caches() {
    let dir = tempfile::tempdir().unwrap();
    let root = checkout(dir.path());
    let storage = dir.path().join("storage");
    let bytes = legacy_trigram(&storage, &root);
    let path = dir.path().join("variant.bin");
    let run = |bytes: &[u8], policy: &TrigramPolicy| {
        fs::write(&path, bytes).unwrap();
        let mut source = Source::open(&path).unwrap();
        convert_trigram_streaming(&mut source, policy, DEFAULT_WORKING_SET, &mut |_| Ok(()))
            .map(|_| ())
    };
    assert!(run(
        &bytes,
        &TrigramPolicy {
            max_file_size: 4096
        }
    )
    .unwrap_err()
    .contains("max_file_size"));
    let mut version = bytes.clone();
    version[4] ^= 0xff;
    assert!(run(&version, &POLICY).unwrap_err().contains("version"));
    let mut flipped = bytes.clone();
    let middle = 40 + (bytes.len() - 40) / 3;
    flipped[middle] ^= 0x01;
    assert!(run(&flipped, &POLICY).unwrap_err().contains("checksum"));
    assert!(run(&bytes[..bytes.len() - 7], &POLICY).is_err());
    assert!(run(&bytes, &POLICY).is_ok());
}

#[test]
fn streaming_semantic_conversion_equals_the_v2_conversion() {
    let dir = tempfile::tempdir().unwrap();
    let root = checkout(dir.path());
    let storage = dir.path().join("storage");
    let snapshot = legacy_semantic(&storage, &root);
    fs::write(root.join("src/beta.rs"), b"pub struct BetaCache;\n").unwrap();
    let producer = producer();
    let (v2, v2_skipped) =
        super::super::per_checkout::convert_semantic(&snapshot, &root, &producer).unwrap();
    let mut source = Source::open(&super::super::per_checkout::legacy_semantic_path(
        &storage, KEY,
    ))
    .unwrap();
    let mut entries = Vec::new();
    let (hash, skipped) = convert_semantic_streaming(&mut source, &root, &producer, &mut |entry| {
        entries.push(entry);
        Ok(())
    })
    .unwrap();
    assert!(!v2.entries.is_empty());
    assert_eq!(sorted(entries), v2.entries);
    assert_eq!(skipped, v2_skipped as u64);
    assert_eq!(hash, *blake3::hash(&snapshot).as_bytes());
    assert!(source.stats.largest_read <= READ_CHUNK);
}

/// The semantic decoder never repairs: appended bytes (a segment or a torn
/// tail) reject the artifact, whose bytes stay exactly as they were.
#[test]
fn semantic_trailing_bytes_are_refused_without_repair() {
    let dir = tempfile::tempdir().unwrap();
    let root = checkout(dir.path());
    let storage = dir.path().join("storage");
    let mut snapshot = legacy_semantic(&storage, &root);
    snapshot.extend_from_slice(b"torn tail");
    let path = super::super::per_checkout::legacy_semantic_path(&storage, KEY);
    fs::write(&path, &snapshot).unwrap();
    let staging = dir.path().join("staging");
    let producer = producer();
    let rejected = stage(
        LegacyArtifact::Semantic,
        &input(&storage, &root, &staging, Some(&producer)),
    )
    .unwrap()
    .unwrap_err();
    assert!(rejected.contains("trailing bytes"), "{rejected}");
    assert_eq!(fs::read(&path).unwrap(), snapshot);
    // Nothing is left in staging after a rejection.
    assert!(snapshot_tree(&staging).is_empty());
}

/// Imported postings are the rows a cold v3 build of the same bytes would
/// store, under the same keys, and producing them reads no checkout file:
/// the import retokenizes nothing. It writes only under `blobs/v3` (and its
/// staging directory) and leaves the legacy cache byte-identical.
#[test]
fn imported_trigram_rows_equal_a_cold_build_and_read_no_checkout_file() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let dir = tempfile::tempdir().unwrap();
    let root = checkout(dir.path());
    let storage = dir.path().join("storage");
    let bytes = legacy_trigram(&storage, &root);
    let sources = snapshot_tree(&root)
        .into_iter()
        .map(|(path, bytes)| (path.strip_prefix(&root).unwrap().to_path_buf(), bytes))
        .collect::<BTreeMap<_, _>>();
    // The checkout is gone: every row must come from the legacy postings.
    fs::remove_dir_all(&root).unwrap();
    let before = snapshot_tree(&storage);
    let staging = staging_dir(&storage, FAMILY, KEY);
    let input = input(&storage, &root, &staging, None);
    let staged = stage(LegacyArtifact::Trigram, &input).unwrap().unwrap();
    assert_eq!(staged.source_hash, *blake3::hash(&bytes).as_bytes());
    check_staged(LegacyArtifact::Trigram, &staged.path, &staged.hash, &input).unwrap();
    let mut entries = Vec::new();
    read_staged(
        &staged.path,
        LegacyArtifact::Trigram,
        &staged.hash,
        &mut |entry| {
            entries.push(entry);
            Ok(())
        },
    )
    .unwrap();
    assert!(entries.len() >= 3, "{} entries", entries.len());
    let outcome = register_staged(
        &storage,
        FAMILY,
        LegacyArtifact::Trigram,
        &staged.path,
        &staged.hash,
        &mut |_| {},
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        outcome,
        RegisterOutcome::Registered(Registered {
            inserted: entries.len() as u64,
            ..Registered::default()
        })
    );
    let reader = v3::PlaneReader::open_existing(&storage, FAMILY, FamilyPlane::Trigram).unwrap();
    for entry in &entries {
        let relative = crate::views::segment_store::rel_path_to_os(&entry.rel_path).unwrap();
        let source = &sources[&relative];
        let key = TrigramKey {
            content: ContentHash::of(source),
            policy: POLICY,
        }
        .family_key();
        assert_eq!(key.as_bytes(), &entry.key, "key of {}", relative.display());
        assert_eq!(
            reader.get(&key).unwrap(),
            Some(TrigramPayload::extract(source, &POLICY).encode()),
            "row of {}",
            relative.display()
        );
    }
    // Only v3 rows and staging were added; legacy bytes are untouched and no
    // v1/v2 store, view or ledger was created.
    let after = snapshot_tree(&storage);
    for (path, bytes) in &before {
        assert_eq!(after.get(path), Some(bytes), "{} changed", path.display());
    }
    for path in after.keys().filter(|path| !before.contains_key(*path)) {
        let relative = path.strip_prefix(&storage).unwrap();
        // The floor file is the v3 publication boundary's reader floor.
        assert!(
            relative.starts_with("blobs/v3")
                || relative.starts_with("migration/v3")
                || relative == Path::new("reader-floor.json"),
            "import wrote {}",
            relative.display()
        );
    }
    assert!(after
        .keys()
        .all(|path| !path.starts_with(storage.join("blobs/v2"))));

    // Registering again is idempotent: every row is reused.
    let again = register_staged(
        &storage,
        FAMILY,
        LegacyArtifact::Trigram,
        &staged.path,
        &staged.hash,
        &mut |_| {},
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        again,
        RegisterOutcome::Registered(Registered {
            reused: entries.len() as u64,
            ..Registered::default()
        })
    );
}

/// A reservation refused for lack of space is reported as the plane's named
/// storage gap: no row is written and nothing reads the legacy artifact in
/// its place.
#[test]
fn a_refused_reservation_is_a_named_storage_gap() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let dir = tempfile::tempdir().unwrap();
    let root = checkout(dir.path());
    let storage = dir.path().join("storage");
    legacy_trigram(&storage, &root);
    let staging = staging_dir(&storage, FAMILY, KEY);
    let input = input(&storage, &root, &staging, None);
    let staged = stage(LegacyArtifact::Trigram, &input).unwrap().unwrap();
    v3::set_free_space_for_test(1 << 20, 1 << 40);
    let outcome = register_staged(
        &storage,
        FAMILY,
        LegacyArtifact::Trigram,
        &staged.path,
        &staged.hash,
        &mut |_| {},
    );
    v3::set_free_space_for_test(u64::MAX, u64::MAX / 2);
    assert_eq!(
        outcome.unwrap().unwrap(),
        RegisterOutcome::StorageGap("storage_gap:device_free_space_floor".to_owned())
    );
    assert!(!v3::plane_path(&storage, FAMILY, FamilyPlane::Trigram)
        .unwrap()
        .exists());
}
