use super::*;

#[test]
fn payload_omits_extraction_only_ast_nodes() {
    let blob = CallgraphBlob::extract(
        "export function run(x: C) { x.m(); helper(); }",
        "typescript",
        "payload-test",
    )
    .unwrap();
    assert!(!blob.parse().unwrap().ast_nodes.is_empty());
    let bytes = blob.to_bytes().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        value.get("ast_nodes").is_none(),
        "stored payload still contains ast_nodes"
    );
    let mut expected = blob;
    let CallgraphBlob::Parse(parse) = &mut expected else {
        unreachable!()
    };
    parse.ast_nodes.clear();
    assert_eq!(CallgraphBlob::from_bytes(&bytes).unwrap(), expected);
}

#[test]
fn payload_decodes_without_ast_nodes() {
    let blob = CallgraphBlob::extract("fn run() { helper(); }", "rust", "payload-test").unwrap();
    let mut value = serde_json::to_value(&blob).unwrap();
    value.as_object_mut().unwrap().remove("ast_nodes");
    let decoded = CallgraphBlob::from_bytes(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(decoded.parse().unwrap().ast_nodes.is_empty());
    assert_eq!(decoded.parse().unwrap().refs, blob.parse().unwrap().refs);
}

fn cpu_ns() -> u128 {
    #[cfg(target_os = "linux")]
    {
        let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // The benchmark runs serially; thread CPU time excludes scheduling waits.
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, time.as_mut_ptr()) },
            0
        );
        let time = unsafe { time.assume_init() };
        time.tv_sec as u128 * 1_000_000_000 + time.tv_nsec as u128
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Produce immutable per-file payloads for offline codec measurements. No live
/// database is opened, and each extraction is dropped before reading the next.
#[test]
#[ignore = "requires AFT_EXTRACT_CORPUS and AFT_EXTRACT_OUTPUT snapshots"]
fn payload_corpus_measurement() {
    let root = std::path::PathBuf::from(std::env::var_os("AFT_EXTRACT_CORPUS").unwrap());
    let output = std::path::PathBuf::from(std::env::var_os("AFT_EXTRACT_OUTPUT").unwrap());
    measure_corpus(&root, &output);
}

fn measure_corpus(root: &Path, output: &Path) {
    std::fs::create_dir_all(&output).unwrap();
    let mut paths = ignore::WalkBuilder::new(&root)
        .hidden(false)
        .git_ignore(false)
        .build()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .map(|entry| entry.into_path())
        .filter(|path| crate::parser::detect_language(path).is_some())
        .collect::<Vec<_>>();
    paths.sort();
    assert!(!paths.is_empty());
    let mut measurements = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let language =
            format!("{:?}", crate::parser::detect_language(path).unwrap()).to_lowercase();
        let source = std::fs::read_to_string(path).unwrap();
        let started = std::time::Instant::now();
        let cpu_started = cpu_ns();
        let blob = CallgraphBlob::extract(&source, &language, "corpus-measurement").unwrap();
        let extraction_cpu_ns = cpu_ns() - cpu_started;
        let extraction_ns = started.elapsed().as_nanos();
        let mut legacy = serde_json::to_value(&blob).unwrap();
        if let CallgraphBlob::Parse(parse) = &blob {
            legacy["ast_nodes"] = serde_json::to_value(&parse.ast_nodes).unwrap();
        }
        let legacy = serde_json::to_vec(&legacy).unwrap();
        let encode_started = cpu_ns();
        let bytes = blob.to_bytes().unwrap();
        let encode_cpu_ns = cpu_ns() - encode_started;
        let decode_started = cpu_ns();
        let decoded = CallgraphBlob::from_bytes(&bytes).unwrap();
        let decode_cpu_ns = cpu_ns() - decode_started;
        let mut expected = blob;
        if let CallgraphBlob::Parse(parse) = &mut expected {
            parse.ast_nodes.clear();
        }
        let mut decoded = decoded;
        if let CallgraphBlob::Parse(parse) = &mut decoded {
            parse.ast_nodes.clear();
        }
        assert_eq!(expected, decoded);
        let legacy_payload = format!("{index}.legacy.json");
        let slim_payload = format!("{index}.slim.json");
        std::fs::write(output.join(&legacy_payload), &legacy).unwrap();
        std::fs::write(output.join(&slim_payload), &bytes).unwrap();
        measurements.push(serde_json::json!({
            "path": path.strip_prefix(&root).unwrap(),
            "language": language,
            "extraction_ns": extraction_ns,
            "extraction_cpu_ns": extraction_cpu_ns,
            "encode_cpu_ns": encode_cpu_ns,
            "decode_cpu_ns": decode_cpu_ns,
            "bytes": bytes.len(),
            "legacy_bytes": legacy.len(),
            "legacy_payload": legacy_payload,
            "slim_payload": slim_payload,
        }));
    }
    std::fs::write(
        output.join("index.json"),
        serde_json::to_vec(&measurements).unwrap(),
    )
    .unwrap();
    println!("measured {} supported files", paths.len());
}

#[test]
fn payload_measurement_distinguishes_legacy_and_slim() {
    let dir = tempfile::tempdir().unwrap();
    let corpus = dir.path().join("corpus");
    std::fs::create_dir(&corpus).unwrap();
    std::fs::write(
        corpus.join("fixture.ts"),
        "export function run(x: C) { x.m(); helper(); }",
    )
    .unwrap();
    let output = dir.path().join("output");
    measure_corpus(&corpus, &output);
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(output.join("index.json")).unwrap()).unwrap();
    let entry = &index[0];
    let legacy = std::fs::read(output.join(entry["legacy_payload"].as_str().unwrap())).unwrap();
    let slim = std::fs::read(output.join(entry["slim_payload"].as_str().unwrap())).unwrap();
    println!("fixture legacy={} slim={} bytes", legacy.len(), slim.len());
    assert!(
        legacy.len() > slim.len(),
        "legacy={} slim={}: measurement erased the AST-size difference",
        legacy.len(),
        slim.len()
    );
    let mut legacy: serde_json::Value = serde_json::from_slice(&legacy).unwrap();
    let slim: serde_json::Value = serde_json::from_slice(&slim).unwrap();
    let nodes = legacy.as_object_mut().unwrap().remove("ast_nodes").unwrap();
    assert!(!nodes.as_array().unwrap().is_empty());
    assert_eq!(legacy, slim, "only the AST index may differ");
}
