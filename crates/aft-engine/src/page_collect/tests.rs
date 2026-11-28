//! Page collection without a subc connection: reference scanning, path
//! resolution, the collection walk and its limits, and the snapshot store.

use super::scan::{css_references, html_references, srcset_urls, RefKind};
use super::snapshot::{BeginError, ReadRefusal, SnapshotStore, MAX_LIVE, TTL};
use super::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

fn raws(references: &[scan::Reference]) -> Vec<(&str, RefKind)> {
    references
        .iter()
        .map(|reference| (reference.raw.as_str(), reference.kind))
        .collect()
}

fn path(segments: &[&str]) -> Resolved {
    Resolved::Path(segments.iter().map(|segment| segment.to_string()).collect())
}

#[test]
fn page_collect_html_references_in_document_order() {
    let html = br#"<!doctype html><html><head>
        <link rel="stylesheet" href="css/site.css">
        <link rel="shortcut icon" href="favicon.ico">
        <link rel="preload" href="skipped.woff2">
        <style>@import "inline-import.css"; body { background: url('bg.png') }</style>
        <script src="app.js"></script>
        </head><body style="background-image: url(body.png)">
        <img src="a.png" srcset="a-1x.png 1x, a-2x.png 2x">
        <a href="not-collected.html">link</a>
        <source srcset="video.webm">
        </body></html>"#;
    let scan = html_references(html);
    assert_eq!(scan.dropped, 0);
    assert_eq!(scan.base, None);
    assert_eq!(
        raws(&scan.references),
        vec![
            ("css/site.css", RefKind::Stylesheet),
            ("favicon.ico", RefKind::Plain),
            ("inline-import.css", RefKind::Stylesheet),
            ("bg.png", RefKind::Plain),
            ("app.js", RefKind::Plain),
            ("body.png", RefKind::Plain),
            ("a.png", RefKind::Plain),
            ("a-1x.png", RefKind::Plain),
            ("a-2x.png", RefKind::Plain),
            ("video.webm", RefKind::Plain),
        ]
    );
}

#[test]
fn page_collect_malformed_html_never_panics() {
    let mut nested = Vec::new();
    for _ in 0..5_000 {
        nested.extend_from_slice(b"<div><span");
    }
    nested.extend_from_slice(b" src=deep.png>");
    for input in [
        &b"<img src='unterminated"[..],
        b"<<<>>><img src=ok.png",
        b"\xff\xfe\x00<img src=x>",
        b"",
        &nested,
    ] {
        let _ = html_references(input);
    }
    let scan = html_references(b"<p><img src=ok.png <img src='two.png'>");
    assert!(scan
        .references
        .iter()
        .any(|reference| reference.raw == "ok.png"));
}

#[test]
fn page_collect_attribute_entities_are_decoded() {
    let scan = html_references(br#"<img src="a&amp;b.png"><img src="c&#x2e;png">"#);
    assert_eq!(
        raws(&scan.references),
        vec![("a&b.png", RefKind::Plain), ("c.png", RefKind::Plain)]
    );
}

#[test]
fn page_collect_srcset_parsing() {
    assert_eq!(
        srcset_urls("a.png 1x, b.png 2x"),
        vec!["a.png".to_string(), "b.png".to_string()]
    );
    // A comma inside a URL is part of it; only a trailing one ends it.
    assert_eq!(
        srcset_urls("  a.png,b.png, c.png 640w  "),
        vec!["a.png,b.png", "c.png"]
    );
    // A trailing comma ends a URL; descriptors with commas inside
    // parentheses do not split a candidate.
    assert_eq!(
        srcset_urls("img/x.png, y.png (min-width: 1px, 2px) 2x, z.png"),
        vec!["img/x.png", "y.png", "z.png"]
    );
    assert_eq!(srcset_urls(" , ,"), Vec::<String>::new());
}

#[test]
fn page_collect_css_references() {
    let css = br#"
        /* url(commented.png) @import "commented.css"; */
        @import "a.css";
        @IMPORT url(b.css) screen;
        @import url( "c.css" );
        .x { background: URL(img/x.png) no-repeat; content: "url(in-string.png)"; }
        .y { background: url('img/y\'s.png'); }
        .z { background: url(img/\7a .png); }
        .w { background: url(); my-url(nope.png); }
        @font-face { src: url("fonts/f.woff2") format("woff2"); }
    "#;
    let (references, dropped) = css_references(css);
    assert_eq!(dropped, 0);
    assert_eq!(
        raws(&references),
        vec![
            ("a.css", RefKind::Stylesheet),
            ("b.css", RefKind::Stylesheet),
            ("c.css", RefKind::Stylesheet),
            ("img/x.png", RefKind::Plain),
            ("img/y's.png", RefKind::Plain),
            ("img/z.png", RefKind::Plain),
            ("fonts/f.woff2", RefKind::Plain),
        ]
    );
}

#[test]
fn page_collect_references_past_the_cap_are_counted() {
    let mut css = String::new();
    for index in 0..scan::MAX_REFERENCES_PER_FILE + 5 {
        css.push_str(&format!("a{{background:url(i{index}.png)}}"));
    }
    let (references, dropped) = css_references(css.as_bytes());
    assert_eq!(references.len(), scan::MAX_REFERENCES_PER_FILE);
    assert_eq!(dropped, 5);
}

#[test]
fn page_collect_resolve_reference_rules() {
    let none: Vec<String> = Vec::new();
    let css_dir = vec!["css".to_string()];
    // Fragments and queries name no other file.
    for raw in ["", "  ", "#top", "?v=1"] {
        assert_eq!(resolve_reference(raw, &none), Resolved::Skip, "{raw:?}");
    }
    // Web addresses, as written or protocol-relative, are remote.
    for raw in [
        "http://example.com/a.png",
        "HTTPS://x/y",
        "//host/a.png",
        "/\\host/a.png",
    ] {
        assert_eq!(resolve_reference(raw, &none), Resolved::Remote, "{raw:?}");
    }
    // Everything else that is not a relative path is not_relative.
    for raw in [
        "data:image/png;base64,AAAA",
        "file:///etc/passwd",
        "blob:https://example.com/uuid",
        "javascript:alert(1)",
        "ftp://example.com/a.png",
        "/etc/passwd",
        "\\\\server\\share\\a.png",
        "C:/Windows/win.ini",
        "c:\\Windows\\win.ini",
    ] {
        assert_eq!(
            resolve_reference(raw, &none),
            Resolved::NotRelative,
            "{raw:?}"
        );
    }
    assert_eq!(
        resolve_reference("img/a.png?v=2#x", &none),
        path(&["img", "a.png"])
    );
    // Percent-encoding is decoded per segment.
    assert_eq!(
        resolve_reference("img/my%20photo%2Ejpg", &none),
        path(&["img", "my photo.jpg"])
    );
    assert_eq!(
        resolve_reference("caf%C3%A9.png", &none),
        path(&["café.png"])
    );
    // An encoded separator, an invalid UTF-8 byte, or a folder names no file.
    for raw in [
        "a%2Fb.png",
        "a%5Cb.png",
        "a%00.png",
        "%FF.png",
        "img/",
        "..",
        "img/a:b.png",
    ] {
        assert_eq!(resolve_reference(raw, &none), Resolved::Invalid, "{raw:?}");
    }
    // `..` that stays inside the page's folder, and `..` that leaves it.
    assert_eq!(
        resolve_reference("../img/a.png", &css_dir),
        path(&["img", "a.png"])
    );
    assert_eq!(resolve_reference("./a/../b.png", &none), path(&["b.png"]));
    assert_eq!(
        resolve_reference("../../shared/a.png", &css_dir),
        path(&["..", "shared", "a.png"])
    );
    assert_eq!(
        resolve_reference("%2E%2E/up.png", &none),
        path(&["..", "up.png"])
    );
    // A backslash reads as a slash, as browsers read it.
    assert_eq!(
        resolve_reference("img\\a.png", &none),
        path(&["img", "a.png"])
    );
}

#[test]
fn page_collect_limits_defaults_and_ceilings() {
    assert_eq!(Limits::from_request(None).unwrap(), Limits::DEFAULT);
    let limits = Limits::from_request(Some(&json!({
        "entry_max_bytes": 10, "max_files": 100_000, "max_file_bytes": u64::MAX,
        "max_total_bytes": 5, "import_depth": 99, "deadline_ms": 600_000,
    })))
    .unwrap();
    assert_eq!(
        limits,
        Limits {
            entry_max_bytes: 10,
            max_files: Limits::CEILING.max_files,
            max_file_bytes: Limits::CEILING.max_file_bytes,
            max_total_bytes: 5,
            import_depth: Limits::CEILING.import_depth,
            deadline_ms: Limits::CEILING.deadline_ms,
        }
    );
    assert!(Limits::from_request(Some(&json!({"max_files": 0}))).is_err());
    assert!(Limits::from_request(Some(&json!({"deadline_ms": -1}))).is_err());
    assert!(Limits::from_request(Some(&json!([1]))).is_err());
}

#[test]
fn page_collect_mime_by_extension() {
    assert_eq!(mime_for("index.HTML"), "text/html");
    assert_eq!(mime_for("css/a.css"), "text/css");
    assert_eq!(mime_for("img/a.svg"), "image/svg+xml");
    assert_eq!(mime_for("fonts/x.woff2"), "font/woff2");
    assert_eq!(mime_for("README"), "application/octet-stream");
    assert_eq!(mime_for("blob.xyz"), "application/octet-stream");
}

/// Files by absolute path, with denied paths and a record of every read.
#[derive(Default)]
struct FakeSource {
    files: HashMap<PathBuf, Vec<u8>>,
    denied: Vec<PathBuf>,
    reads: Vec<PathBuf>,
}

impl FakeSource {
    fn with(root: &Path, files: &[(&str, &[u8])]) -> Self {
        Self {
            files: files
                .iter()
                .map(|(name, bytes)| (root.join(name), bytes.to_vec()))
                .collect(),
            ..Self::default()
        }
    }
}

impl PageSource for FakeSource {
    fn read(
        &mut self,
        path: &Path,
        limit: u64,
        _deadline: Instant,
    ) -> Result<Vec<u8>, ReadFailure> {
        self.reads.push(path.to_path_buf());
        if self.denied.iter().any(|denied| path.starts_with(denied)) {
            return Err(ReadFailure::Denied);
        }
        let bytes = self.files.get(path).ok_or(ReadFailure::Missing)?;
        if bytes.len() as u64 > limit {
            return Err(ReadFailure::TooLarge {
                size: bytes.len() as u64,
            });
        }
        Ok(bytes.clone())
    }
}

/// Keeps records in memory, computing them as the snapshot builder does.
#[derive(Default)]
struct MemorySink(Vec<(String, Vec<u8>)>);

impl SnapshotSink for MemorySink {
    fn add(&mut self, path: &str, bytes: &[u8]) -> std::io::Result<FileRecord> {
        use sha2::Digest;
        self.0.push((path.to_string(), bytes.to_vec()));
        Ok(FileRecord {
            path: path.to_string(),
            size: bytes.len() as u64,
            sha256: format!("{:x}", sha2::Sha256::digest(bytes)),
            mime: mime_for(path),
        })
    }

    fn bytes(&self, path: &str) -> std::io::Result<Vec<u8>> {
        self.0
            .iter()
            .find(|(stored, _)| stored == path)
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(|| std::io::ErrorKind::NotFound.into())
    }
}

/// The default limits with stylesheet imports followed four levels deep,
/// for tests about nested stylesheets rather than the depth limit.
const DEEP: Limits = Limits {
    import_depth: 4,
    ..Limits::DEFAULT
};

fn root() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\site")
    } else {
        PathBuf::from("/site")
    }
}

fn run(source: &mut FakeSource, limits: Limits) -> Result<Collected, CollectRefusal> {
    collect(
        &root().join("index.html"),
        &limits,
        Instant::now(),
        source,
        &mut MemorySink::default(),
    )
}

fn names(collected: &Collected) -> Vec<&str> {
    collected
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect()
}

fn omitted(collected: &Collected) -> Vec<(&str, &str)> {
    collected
        .omitted
        .iter()
        .map(|omitted| (omitted.path.as_str(), omitted.reason.as_str()))
        .collect()
}

#[test]
fn page_collect_walk_lists_files_and_omissions() {
    let root = root();
    let mut source = FakeSource::with(
        &root,
        &[
            (
                "index.html",
                br#"<link rel=stylesheet href="css/site.css"><img src="img/a.png">
                <img src="img/a.png#again"><img src="https://cdn/x.png"><img src="https://cdn/x.png">
                <img src="secret/key.png"><img src="gone.png"><img src="index.html">"#,
            ),
            ("css/site.css", b"body{background:url(../img/bg.png)} @import 'more.css';"),
            ("css/more.css", b".m{background:url(m.png)}"),
            ("css/m.png", b"M"),
            ("img/a.png", b"A"),
            ("img/bg.png", b"BG"),
            ("secret/key.png", b"KEY"),
        ],
    );
    source.denied.push(root.join("secret"));
    let collected = run(&mut source, DEEP).unwrap();
    assert_eq!(collected.entry, "index.html");
    assert_eq!(
        names(&collected),
        vec![
            "index.html",
            "css/site.css",
            "img/a.png",
            "img/bg.png",
            "css/more.css",
            "css/m.png"
        ]
    );
    assert_eq!(
        omitted(&collected),
        vec![
            ("https://cdn/x.png", "remote"),
            ("secret/key.png", "denied"),
            ("gone.png", "missing"),
        ]
    );
    // Every file is read once, and nothing named only by a not-relative
    // reference is ever read.
    let mut reads = source.reads.clone();
    reads.sort();
    reads.dedup();
    assert_eq!(reads.len(), source.reads.len(), "a file was read twice");
    assert!(!source
        .reads
        .iter()
        .any(|read| read.to_string_lossy().contains("cdn")));
    let a = &collected.files[2];
    assert_eq!(
        (a.size, a.mime, a.sha256.as_str()),
        (
            1,
            "image/png",
            "559aead08264d5795d3909718cdd05abd49572e84fe55590eef31a88a08fdffd"
        )
    );
}

#[test]
fn page_collect_dot_dot_escaping_the_page_folder_is_listed_relative() {
    let root = root();
    let parent = root.parent().unwrap().to_path_buf();
    let mut source = FakeSource::with(
        &root,
        &[
            (
                "index.html",
                b"<img src='../shared/logo.png'><img src='sub/../in.png'>",
            ),
            ("in.png", b"I"),
        ],
    );
    source
        .files
        .insert(parent.join("shared").join("logo.png"), b"L".to_vec());
    let collected = run(&mut source, Limits::DEFAULT).unwrap();
    assert_eq!(
        names(&collected),
        vec!["index.html", "../shared/logo.png", "in.png"]
    );
    assert!(source
        .reads
        .contains(&parent.join("shared").join("logo.png")));
}

#[test]
fn page_collect_every_limit_fires_with_its_reason() {
    let root = root();
    let html: &[u8] = br#"<link rel=stylesheet href=d1.css><img src=big.png><img src=one.png>
        <img src=two.png><img src=three.png>"#;
    let files: &[(&str, &[u8])] = &[
        ("index.html", html),
        ("d1.css", b"@import 'd2.css';"),
        ("d2.css", b"@import 'd3.css';"),
        ("d3.css", b".x{}"),
        ("big.png", &[0u8; 100]),
        ("one.png", &[1u8; 10]),
        ("two.png", &[2u8; 10]),
        ("three.png", &[3u8; 10]),
    ];

    // depth_limit: d1 is one stylesheet deep, d2 two, d3 three.
    let mut source = FakeSource::with(&root, files);
    let collected = run(
        &mut source,
        Limits {
            import_depth: 2,
            ..Limits::DEFAULT
        },
    )
    .unwrap();
    assert_eq!(omitted(&collected), vec![("d3.css", "depth_limit")]);
    assert!(!source.reads.contains(&root.join("d3.css")));

    // over_file_limit
    let mut source = FakeSource::with(&root, files);
    let collected = run(
        &mut source,
        Limits {
            max_file_bytes: 50,
            ..DEEP
        },
    )
    .unwrap();
    assert_eq!(omitted(&collected), vec![("big.png", "over_file_limit")]);

    // over_total_limit: after the page, d1.css and big.png, 15 bytes are
    // left: one.png (10) fits, nothing after it does. d2.css is reached last
    // because the walk takes the page's own references first.
    let used = html.len() as u64 + 17 + 100;
    let mut source = FakeSource::with(&root, files);
    let collected = run(
        &mut source,
        Limits {
            max_total_bytes: used + 15,
            ..DEEP
        },
    )
    .unwrap();
    assert_eq!(
        omitted(&collected),
        vec![
            ("two.png", "over_total_limit"),
            ("three.png", "over_total_limit"),
            ("d2.css", "over_total_limit"),
        ]
    );

    // over_count_limit: the page counts as one file.
    let mut source = FakeSource::with(&root, files);
    let collected = run(
        &mut source,
        Limits {
            max_files: 3,
            ..DEEP
        },
    )
    .unwrap();
    assert_eq!(names(&collected), vec!["index.html", "d1.css", "big.png"]);
    assert_eq!(
        omitted(&collected),
        vec![
            ("one.png", "over_count_limit"),
            ("two.png", "over_count_limit"),
            ("three.png", "over_count_limit"),
            ("d2.css", "over_count_limit"),
        ]
    );

    // deadline: the page is still read; nothing it names is.
    let mut source = FakeSource::with(&root, files);
    let collected = run(
        &mut source,
        Limits {
            deadline_ms: 0,
            ..DEEP
        },
    )
    .unwrap();
    assert_eq!(names(&collected), vec!["index.html"]);
    assert!(omitted(&collected)
        .iter()
        .all(|(_, reason)| *reason == "deadline"));
    assert_eq!(source.reads, vec![root.join("index.html")]);
}

#[test]
fn page_collect_entry_refusals() {
    let root = root();
    let refusal = |files: &[(&str, &[u8])], entry: &str, limits: Limits| {
        let mut source = FakeSource::with(&root, files);
        collect(
            &root.join(entry),
            &limits,
            Instant::now(),
            &mut source,
            &mut MemorySink::default(),
        )
        .unwrap_err()
    };
    let error = refusal(&[("notes.txt", b"<p>")], "notes.txt", Limits::DEFAULT);
    assert_eq!(
        (error.code(), error.detail()),
        ("entry_not_html", json!({"reason": "extension"}))
    );
    let error = refusal(
        &[("index.html", b"\x89PNG\r\n\x1a\n\0\0")],
        "index.html",
        Limits::DEFAULT,
    );
    assert_eq!(
        (error.code(), error.detail()),
        ("entry_not_html", json!({"reason": "content"}))
    );
    let error = refusal(
        &[("index.html", b"just text")],
        "index.html",
        Limits::DEFAULT,
    );
    assert_eq!(error.code(), "entry_not_html");
    let error = refusal(&[], "index.html", Limits::DEFAULT);
    assert_eq!(
        (error.code(), error.detail()),
        ("entry_unreadable", json!({"reason": "missing"}))
    );
    let error = refusal(
        &[("index.html", b"<html>0123456789</html>")],
        "index.html",
        Limits {
            entry_max_bytes: 8,
            ..Limits::DEFAULT
        },
    );
    assert_eq!(
        (error.code(), error.detail()),
        ("entry_too_large", json!({"size": 23, "limit": 8}))
    );
    let mut source = FakeSource::with(&root, &[("index.html", b"<html>")]);
    source.denied.push(root.join("index.html"));
    let error = collect(
        &root.join("index.html"),
        &Limits::DEFAULT,
        Instant::now(),
        &mut source,
        &mut MemorySink::default(),
    )
    .unwrap_err();
    assert_eq!(
        (error.code(), error.detail()),
        ("entry_unreadable", json!({"reason": "denied"}))
    );
    assert!(sniff_html(b"\xEF\xBB\xBF  \n<!doctype html>"));
}

/// The session the tests' snapshots are collected for.
fn owner() -> snapshot::SnapshotOwner {
    snapshot::SnapshotOwner {
        harness: "test-harness".into(),
        session: "page-session".into(),
    }
}

fn store() -> (tempfile::TempDir, Arc<SnapshotStore>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SnapshotStore::open(dir.path()));
    (dir, store)
}

fn one_file(store: &Arc<SnapshotStore>, bytes: &[u8]) -> String {
    let mut builder = store.begin(owner()).unwrap();
    builder.add("index.html", bytes).unwrap();
    builder.commit().0
}

#[test]
fn page_collect_snapshot_read_ranges() {
    let (_dir, store) = store();
    let id = one_file(&store, b"0123456789");
    assert_eq!(
        store.read(&id, &owner(), "index.html", 0, 4).unwrap(),
        b"0123"
    );
    assert_eq!(
        store.read(&id, &owner(), "index.html", 8, 100).unwrap(),
        b"89"
    );
    assert_eq!(store.read(&id, &owner(), "index.html", 10, 1).unwrap(), b"");
    assert_eq!(
        store.read(&id, &owner(), "index.html", 11, 1),
        Err(ReadRefusal::BadRange)
    );
    assert_eq!(
        store.read(&id, &owner(), "index.html", 0, 0),
        Err(ReadRefusal::BadRange)
    );
    assert_eq!(
        store.read(&id, &owner(), "index.html", 0, MAX_READ_LENGTH + 1),
        Err(ReadRefusal::BadRange)
    );
    assert!(store
        .read(&id, &owner(), "index.html", 0, MAX_READ_LENGTH)
        .is_ok());
    assert_eq!(
        store.read(&id, &owner(), "other.html", 0, 1),
        Err(ReadRefusal::NotInSnapshot)
    );
    assert_eq!(
        store.read("no-such-snapshot", &owner(), "index.html", 0, 1),
        Err(ReadRefusal::Expired)
    );
}

#[test]
fn page_collect_snapshot_expires_and_is_deleted() {
    let (_dir, store) = store();
    let id = one_file(&store, b"<html>");
    let folder = store.root().join(&id);
    assert!(folder.is_dir());
    store.advance_for_test(TTL - Duration::from_millis(1));
    assert!(store.read(&id, &owner(), "index.html", 0, 1).is_ok());
    store.advance_for_test(Duration::from_millis(1));
    store.expire_due();
    assert!(!folder.exists(), "an expired snapshot's files remain");
    assert_eq!(
        store.read(&id, &owner(), "index.html", 0, 1),
        Err(ReadRefusal::Expired)
    );
}

#[test]
fn page_collect_fifth_snapshot_is_busy_until_one_ends() {
    let (_dir, store) = store();
    let ids = (0..MAX_LIVE)
        .map(|_| one_file(&store, b"<html>"))
        .collect::<Vec<_>>();
    store.advance_for_test(Duration::from_secs(10));
    match store.begin(owner()) {
        Err(BeginError::Busy(busy)) => {
            assert!((40_000..=50_000).contains(&busy.retry_after_ms), "{busy:?}")
        }
        Err(BeginError::Io(error)) => panic!("{error}"),
        Ok(_) => panic!("a fifth snapshot was admitted"),
    }
    assert!(store.release(&ids[0], &owner()));
    let builder = store
        .begin(owner())
        .expect("a released place is free again");
    // A collection in progress holds a place too.
    assert!(matches!(store.begin(owner()), Err(BeginError::Busy(_))));
    drop(builder);
    assert_eq!(store.live_count(), MAX_LIVE - 1);
    store.advance_for_test(TTL);
    assert!(
        store.begin(owner()).is_ok(),
        "expired snapshots free their places"
    );
}

#[test]
fn page_collect_release_is_idempotent() {
    let (_dir, store) = store();
    let id = one_file(&store, b"<html>");
    let folder = store.root().join(&id);
    assert!(store.release(&id, &owner()));
    assert!(!folder.exists());
    assert!(!store.release(&id, &owner()));
    assert!(!store.release("never-existed", &owner()));
    assert_eq!(
        store.read(&id, &owner(), "index.html", 0, 1),
        Err(ReadRefusal::Expired)
    );
}

#[test]
fn page_collect_abandoned_builder_leaves_nothing() {
    let (_dir, store) = store();
    let mut builder = store.begin(owner()).unwrap();
    builder.add("index.html", b"<html>").unwrap();
    drop(builder);
    assert_eq!(store.live_count(), 0);
    assert_eq!(std::fs::read_dir(store.root()).unwrap().count(), 0);
}

#[test]
fn page_collect_leftover_snapshots_are_deleted_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let leftover = dir.path().join(snapshot::SNAPSHOT_DIR).join("old");
    std::fs::create_dir_all(&leftover).unwrap();
    std::fs::write(leftover.join("f0"), b"stale").unwrap();
    let _store = SnapshotStore::open(dir.path());
    assert!(!leftover.exists());
}

#[cfg(unix)]
#[test]
fn page_collect_snapshot_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, store) = store();
    let id = one_file(&store, b"<html>");
    let folder = store.root().join(&id);
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&folder), 0o700);
    assert_eq!(mode(&folder.join("f0")), 0o600);
}

#[test]
fn page_collect_same_page_twice_has_identical_hashes() {
    let (_dir, store) = store();
    let root = root();
    let files: &[(&str, &[u8])] = &[
        (
            "index.html",
            b"<img src=a.png><link rel=stylesheet href=s.css>",
        ),
        ("a.png", b"PNG"),
        ("s.css", b"p{}"),
    ];
    let mut snapshots = Vec::new();
    for _ in 0..2 {
        let mut builder = store.begin(owner()).unwrap();
        let collected = collect(
            &root.join("index.html"),
            &Limits::DEFAULT,
            Instant::now(),
            &mut FakeSource::with(&root, files),
            &mut builder,
        )
        .unwrap();
        let (id, _) = builder.commit();
        snapshots.push((id, collected.files));
    }
    assert_ne!(snapshots[0].0, snapshots[1].0);
    assert_eq!(snapshots[0].1, snapshots[1].1);
}

/// An attribute of nothing but `&` is decoded in linear work: each `&`
/// looks for its `;` only in the few bytes a known reference can span.
#[test]
fn page_collect_entity_decoding_is_linear() {
    let ampersands = "&".repeat(1 << 20);
    let page = format!(r#"<img src="{ampersands}">"#);
    // Fails at once if any `&` searches much past its own reference window.
    scan::entity_bytes_examined::reset(12 << 20);
    let scan = html_references(page.as_bytes());
    let examined = scan::entity_bytes_examined::get();
    assert!(examined <= 11 << 20, "{examined} bytes examined");
    assert_eq!(scan.references.len(), 1);
    assert_eq!(scan.references[0].raw, ampersands);
    // References are still decoded when their `;` is in the window.
    assert_eq!(
        scan::decode_entities("a&#x2e;b&amp;c&#46;d&gt"),
        "a.b&c.d&gt"
    );
}

/// A file named first as a plain file and then as a stylesheet, or the
/// other way round, is copied once and scanned once as a stylesheet.
#[test]
fn page_collect_file_named_as_plain_and_stylesheet_is_scanned_once() {
    let root = root();
    let css: &[u8] = b"@import 'y.css'; a { background: url(z.png) }";
    for page in [
        &br#"<img src="x.css"><link rel=stylesheet href="x.css">"#[..],
        br#"<link rel=stylesheet href="x.css"><img src="x.css">"#,
    ] {
        let mut source = FakeSource::with(
            &root,
            &[
                ("index.html", page),
                ("x.css", css),
                ("y.css", b"b { color: red }"),
                ("z.png", b"Z"),
            ],
        );
        let collected = run(&mut source, DEEP).unwrap();
        let mut files = names(&collected);
        files.sort();
        assert_eq!(
            files,
            vec!["index.html", "x.css", "y.css", "z.png"],
            "{}",
            String::from_utf8_lossy(page)
        );
        assert!(collected.omitted.is_empty(), "{:?}", collected.omitted);
        let x_reads = source
            .reads
            .iter()
            .filter(|read| read.ends_with("x.css"))
            .count();
        assert_eq!(x_reads, 1, "x.css was read more than once");
    }
    // The stored bytes are read back from a real snapshot too.
    let (_dir, store) = store();
    let mut builder = store.begin(owner()).unwrap();
    let mut source = FakeSource::with(
        &root,
        &[
            (
                "index.html",
                br#"<img src="x.css"><link rel=stylesheet href="x.css">"#,
            ),
            ("x.css", css),
            ("y.css", b"b { color: red }"),
            ("z.png", b"Z"),
        ],
    );
    let collected = collect(
        &root.join("index.html"),
        &DEEP,
        Instant::now(),
        &mut source,
        &mut builder,
    )
    .unwrap();
    assert_eq!(collected.files.len(), 4);
}

#[test]
fn page_collect_resolve_base_rules() {
    let dir = |segments: &[&str]| {
        RefBase::Dir(segments.iter().map(|segment| segment.to_string()).collect())
    };
    assert_eq!(resolve_base("assets/"), dir(&["assets"]));
    assert_eq!(resolve_base("assets/page.html?x#y"), dir(&["assets"]));
    assert_eq!(resolve_base("a/../b/./c/"), dir(&["b", "c"]));
    assert_eq!(resolve_base("../../up/"), dir(&["..", "..", "up"]));
    assert_eq!(resolve_base("page.html"), dir(&[]));
    assert_eq!(resolve_base(""), dir(&[]));
    assert_eq!(resolve_base("#top"), dir(&[]));
    for web in [
        "https://cdn.example.com/x/",
        "http://x/",
        "//cdn.example.com/",
    ] {
        assert_eq!(resolve_base(web), RefBase::Web, "{web}");
    }
    for absolute in [
        "/static/",
        "file:///etc/",
        "data:text/html,",
        "\\\\server\\share\\",
        "C:/site/",
    ] {
        assert_eq!(resolve_base(absolute), RefBase::Absolute, "{absolute}");
    }
    assert_eq!(resolve_base("%FF/"), RefBase::Invalid);
}

/// Relative references, inline styles included, resolve against the first
/// `<base href>`; a later `<base>` is ignored.
#[test]
fn page_collect_relative_base_moves_references() {
    let root = root();
    let mut source = FakeSource::with(
        &root,
        &[
            (
                "index.html",
                br#"<head><base href="assets/"><base href="ignored/"></head>
                <img src="a.png"><div style="background: url(b.png)"></div>
                <link rel=stylesheet href="s.css"><img src="https://cdn/x.png">"#,
            ),
            ("assets/a.png", b"A"),
            ("assets/b.png", b"B"),
            ("assets/s.css", b"p { background: url(img/c.png) }"),
            ("assets/img/c.png", b"C"),
            ("ignored/a.png", b"wrong base"),
        ],
    );
    let collected = run(&mut source, Limits::DEFAULT).unwrap();
    assert_eq!(
        names(&collected),
        vec![
            "index.html",
            "assets/a.png",
            "assets/b.png",
            "assets/s.css",
            "assets/img/c.png"
        ]
    );
    assert_eq!(omitted(&collected), vec![("https://cdn/x.png", "remote")]);
}

/// A web base makes every relative reference a web address, listed as
/// `remote`; an absolute file or non-web base makes them `not_relative`.
/// Neither is ever read, and references with their own scheme keep theirs.
#[test]
fn page_collect_absolute_base_moves_references_out_of_reach() {
    let root = root();
    for (base, moved) in [
        ("https://cdn.example.com/site/", "remote"),
        ("//cdn.example.com/site/", "remote"),
        ("http://cdn.example.com/", "remote"),
        ("/static/", "not_relative"),
        ("file:///srv/site/", "not_relative"),
    ] {
        let page = format!(
            r##"<base href="{base}"><img src="a.png"><img src="a.png"><img src="data:x">
            <div style="background:url(b.png)"></div><img src="#top">"##
        );
        let mut source = FakeSource::with(
            &root,
            &[
                ("index.html", page.as_bytes()),
                ("a.png", b"A"),
                ("b.png", b"B"),
            ],
        );
        let collected = run(&mut source, Limits::DEFAULT).unwrap();
        assert_eq!(names(&collected), vec!["index.html"], "{base}");
        assert_eq!(
            omitted(&collected),
            vec![
                ("a.png", moved),
                ("data:x", "not_relative"),
                ("b.png", moved),
            ],
            "{base}"
        );
        assert_eq!(source.reads, vec![root.join("index.html")], "{base}");
    }
}

/// A snapshot answers only to the session it was collected for.
#[test]
fn page_collect_snapshot_answers_only_its_own_session() {
    let (_dir, store) = store();
    let id = one_file(&store, b"<html>");
    let stranger = snapshot::SnapshotOwner {
        harness: "test-harness".into(),
        session: "another-session".into(),
    };
    assert_eq!(
        store.read(&id, &stranger, "index.html", 0, 1),
        Err(ReadRefusal::Expired)
    );
    assert!(!store.release(&id, &stranger));
    assert_eq!(
        store.read(&id, &owner(), "index.html", 0, 6).unwrap(),
        b"<html>"
    );
    assert!(store.release(&id, &owner()));
}

/// The default import depth, 1: the page and the stylesheets it links are
/// scanned, and every `url()` in those stylesheets is collected. A linked
/// stylesheet's own `@import` of another stylesheet is `depth_limit`. An
/// `@import` in the page's `<style>` element counts like a link.
#[test]
fn page_collect_default_import_depth_follows_one_level() {
    assert_eq!(Limits::DEFAULT.import_depth, 1);
    let root = root();
    let mut source = FakeSource::with(
        &root,
        &[
            (
                "index.html",
                br#"<link rel=stylesheet href="a.css"><style>@import "c.css";</style>"#,
            ),
            (
                "a.css",
                b"@import 'b.css'; p { background: url(img/a.png) }",
            ),
            ("b.css", b"q { background: url(img/b.png) }"),
            (
                "c.css",
                b"@import 'd.css'; r { background: url(img/c.png) }",
            ),
            ("d.css", b"s {}"),
            ("img/a.png", b"A"),
            ("img/b.png", b"B"),
            ("img/c.png", b"C"),
        ],
    );
    let collected = run(&mut source, Limits::DEFAULT).unwrap();
    assert_eq!(
        names(&collected),
        vec!["index.html", "a.css", "c.css", "img/a.png", "img/c.png"]
    );
    assert_eq!(
        omitted(&collected),
        vec![("b.css", "depth_limit"), ("d.css", "depth_limit")]
    );
    assert!(!source.reads.contains(&root.join("b.css")));
    assert!(!source.reads.contains(&root.join("d.css")));
}

/// A page whose `<base href>` is a megabyte long, followed by thousands of
/// short references, shares that one base across every queued reference
/// instead of copying it per reference. References whose base plus path is
/// too long as written to resolve within the collector's 4096-byte path
/// limit are omitted as missing.
#[test]
fn page_collect_long_base_is_shared_not_copied() {
    let root = root();
    let base = format!("{}/", "b".repeat(1 << 20));
    let mut page = format!(r#"<base href="{base}">"#);
    for index in 0..4000 {
        page.push_str(&format!(r#"<img src="i{index}.png">"#));
    }
    assert!(page.len() < Limits::DEFAULT.entry_max_bytes as usize);
    let mut source = FakeSource::with(&root, &[("index.html", page.as_bytes())]);
    // Fails at once if the base is copied for even a few references.
    super::ref_base_bytes_copied::reset(4 << 20);
    let collected = run(&mut source, Limits::DEFAULT).unwrap();
    assert_eq!(super::ref_base_bytes_copied::get(), 0);
    assert_eq!(names(&collected), vec!["index.html"]);
    assert!(collected
        .omitted
        .iter()
        .all(|omitted| omitted.reason == OmitReason::Missing && omitted.path.len() < 16));
    assert_eq!(
        collected.omitted.len() as u64 + collected.omitted_unlisted,
        4000
    );
    assert_eq!(source.reads, vec![root.join("index.html")]);
}

/// The path limit applies to the path that would be opened, after percent
/// decoding, not to the reference as written: eight 200-byte folder names
/// spelled as `%61` escapes (over 4,800 bytes written, about 1,600
/// resolved) are collected, while a plain path that resolves past 4,096
/// bytes is omitted as missing without being read.
#[test]
fn page_collect_path_limit_applies_after_decoding() {
    let root = root();
    let folder = "a".repeat(200);
    let resolved = format!("{}/x.png", vec![folder.as_str(); 8].join("/"));
    let encoded_folder = "%61".repeat(200);
    let written = format!("{}/x.png", vec![encoded_folder.as_str(); 8].join("/"));
    assert!(written.len() > 4096 && resolved.len() < 4096);
    let too_long = format!("{}/y.png", vec![folder.as_str(); 21].join("/"));
    assert!(too_long.len() > 4096);
    let page = format!(r#"<img src="{written}"><img src="{too_long}">"#);
    let mut source = FakeSource::with(
        &root,
        &[
            ("index.html", page.as_bytes()),
            (resolved.as_str(), b"X"),
            (too_long.as_str(), b"Y"),
        ],
    );
    let collected = run(&mut source, Limits::DEFAULT).unwrap();
    assert_eq!(names(&collected), vec!["index.html", resolved.as_str()]);
    assert_eq!(collected.omitted.len(), 1);
    assert_eq!(collected.omitted[0].reason, OmitReason::Missing);
    assert!(!source.reads.contains(&root.join(&too_long)));
}

/// CSS names may be written with escapes; `url(` and `@import` are found
/// however they are spelled, and an escape that spells another name is not
/// taken for them.
#[test]
fn page_collect_css_escaped_names() {
    let (references, _) = css_references(
        br#"a { background: u\72l(image.png) }
        @\69mport 'theme.css';
        b { background: u\72x(not-a-url.png) }
        c { background: \75 rl(spaced.png) }
        d { background: \55\52\4c(upper.png) }
        @IMPORT "loud.css";
        @\69mportant 'nope.css';"#,
    );
    assert_eq!(
        raws(&references),
        vec![
            ("image.png", RefKind::Plain),
            ("theme.css", RefKind::Stylesheet),
            ("spaced.png", RefKind::Plain),
            ("upper.png", RefKind::Plain),
            ("loud.css", RefKind::Stylesheet),
        ]
    );
}
