use super::*;
use extraction_work::{measure, Work};

fn synthetic(functions: usize) -> String {
    (0..functions)
        .map(|i| format!("export function f{i}(x: C) {{ x.m(); helper(); }}\n"))
        .collect()
}

fn measured(source: &str, language: &str) -> (CallgraphBlob, Work) {
    measure(|| {
        CallgraphBlob::extract(source, language, crate::callgraph_extractor::PRODUCER).unwrap()
    })
}

#[test]
fn extraction_uses_one_whole_file_parse() {
    for (source, language) in [
        (synthetic(32), "typescript"),
        ("mod child; fn f() { helper(); }".into(), "rust"),
    ] {
        let (_, work) = measured(&source, language);
        assert_eq!(work.whole_file_parses, 1, "{language}: {work:?}");
    }
}

#[test]
fn extraction_macro_ranges_are_parsed_once() {
    let source = "wrap! { fn first() {} }\nwrap! { fn second() {} }\n";
    let (blob, work) = measured(source, "rust");
    assert_eq!(work.macro_ranges.len(), 2, "{work:?}");
    assert_eq!(work.macro_ranges.iter().collect::<BTreeSet<_>>().len(), 2);
    let symbols = &blob.parse().unwrap().symbols;
    assert!(symbols.iter().any(|s| s.name == "first"));
    assert!(symbols.iter().any(|s| s.name == "second"));
}

#[test]
fn extraction_lookup_work_is_subquadratic() {
    let (_, small) = measured(&synthetic(64), "typescript");
    let (_, large) = measured(&synthetic(128), "typescript");
    let small_visits = small.ordinal_visits + small.dispatch_visits;
    let large_visits = large.ordinal_visits + large.dispatch_visits;
    println!("lookup work for 64 -> 128 functions: {small_visits} -> {large_visits}");
    assert!(small_visits > 0, "lookups must exercise the work counter");
    assert!(
        large_visits <= small_visits * 3,
        "doubling functions must not quadruple lookup work: {small_visits} -> {large_visits}"
    );
}

#[test]
fn extraction_kinds_are_borrowed() {
    let (blob, work) = measured(&synthetic(32), "typescript");
    assert_eq!(work.kind_copies, 0, "{work:?}");
    assert!(blob
        .parse()
        .unwrap()
        .ast_nodes
        .iter()
        .all(|n| matches!(n.kind, Cow::Borrowed(_))));
}

#[test]
fn extraction_walk_has_no_child_vectors() {
    let (_, work) = measured("mod child; fn f() { helper(); }", "rust");
    assert_eq!(work.child_vectors, 0, "{work:?}");
    let source = synthetic(128);
    let mut parser = Parser::new();
    parser
        .set_language(&grammar_for(LangId::TypeScript))
        .unwrap();
    let tree = parser.parse(&source, None).unwrap();
    let (nodes, allocations) =
        crate::test_allocations::count(|| extraction_index::preorder(tree.root_node()).count());
    assert!(nodes > 1_000);
    assert!(
        allocations <= 4,
        "cursor walking allocated {allocations} times for {nodes} nodes"
    );
}

#[test]
fn extraction_positions_scan_source_once() {
    let source = synthetic(64);
    let (_, work) = measured(&source, "typescript");
    assert_eq!(work.position_bytes, source.len(), "{work:?}");
}

#[test]
fn extraction_call_kinds_are_static() {
    let (_, work) = measured(&synthetic(32), "typescript");
    assert_eq!(work.call_kind_vectors, 0, "{work:?}");
    let (_, allocations) = crate::test_allocations::count(|| {
        for _ in 0..32 {
            std::hint::black_box(crate::calls::call_node_kinds(LangId::TypeScript));
        }
    });
    assert_eq!(allocations, 0, "call kinds must not allocate");
}

#[test]
fn extraction_range_index_matches_linear_reference() {
    // Include touching siblings, zero-width missing nodes, duplicate spans and
    // out-of-tree queries. Compare against the old minimum-span/preorder rule.
    let nodes = [
        (0, 20),
        (0, 10),
        (0, 10),
        (0, 3),
        (3, 3),
        (3, 10),
        (10, 20),
        (20, 20),
    ];
    let queries = (0..=22)
        .flat_map(|start| (start..=22).map(move |end| (start, end)))
        .collect::<Vec<_>>();
    let indexed =
        extraction_index::range_minima(&nodes, &queries, extraction_index::LookupWork::Ordinal);
    for ((start, end), result) in queries.into_iter().zip(indexed) {
        let expected = nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.0 <= start && n.1 >= end)
            .min_by_key(|(i, n)| (n.1 - n.0, *i))
            .map(|(i, _)| i);
        assert_eq!(result, expected, "range {start}..{end}");
    }
}

#[test]
fn extraction_line_index_matches_inclusive_helpers() {
    for source in ["", "\n", "a\r\nb\n", "a\rb", "é\n雪", "a\n\n"] {
        let index = super::super::LineIndex::new(source);
        for line in 0..6 {
            for column in 0..12 {
                let mut start = 0;
                let mut expected = source.len();
                for (row, segment) in source.split_inclusive('\n').enumerate() {
                    if row == line as usize {
                        expected = start + (column as usize).min(segment.len());
                        break;
                    }
                    start += segment.len();
                }
                assert_eq!(
                    index.inclusive_byte_offset(line, column),
                    expected,
                    "{source:?}, {line}:{column}"
                );
            }
        }
        for offset in 0..source.len() + 3 {
            let expected = source.as_bytes()[..offset.min(source.len())]
                .iter()
                .filter(|&&b| b == b'\n')
                .count() as u32
                + 1;
            assert_eq!(
                index.byte_to_line(offset),
                expected,
                "{source:?}, byte {offset}"
            );
        }
    }
}

/// Compare actual serialized bytes against an independently produced baseline.
/// The roots should be immutable repository snapshots, not the changing checkout.
#[test]
#[ignore = "requires AFT_EXTRACT_CORPUS and AFT_EXTRACT_OUTPUT snapshots"]
fn extraction_corpus_bytes_and_work() {
    fn files(root: &Path, output: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                files(&entry.path(), output);
            } else if kind.is_file() && crate::parser::detect_language(&entry.path()).is_some() {
                output.push(entry.path());
            }
        }
    }
    let root = std::path::PathBuf::from(std::env::var_os("AFT_EXTRACT_CORPUS").unwrap());
    let output = std::path::PathBuf::from(std::env::var_os("AFT_EXTRACT_OUTPUT").unwrap());
    let compare = std::env::var_os("AFT_EXTRACT_COMPARE").is_some();
    let mut paths = Vec::new();
    files(&root, &mut paths);
    if let Some(only) = std::env::var_os("AFT_EXTRACT_ONLY") {
        paths.retain(|path| path.strip_prefix(&root).unwrap() == Path::new(&only));
    }
    paths.sort();
    let largest_ts = paths
        .iter()
        .filter(|p| crate::parser::detect_language(p) == Some(LangId::TypeScript))
        .max_by_key(|p| std::fs::metadata(p).unwrap().len());
    let mut total_bytes = 0;
    let mut measures = Vec::new();
    let started = std::time::Instant::now();
    for path in &paths {
        let lang = crate::parser::detect_language(path).unwrap();
        let language = format!("{lang:?}").to_lowercase();
        let source = std::fs::read_to_string(path).unwrap();
        let file_started = std::time::Instant::now();
        let (blob, work) = measured(&source, &language);
        let elapsed = file_started.elapsed();
        let bytes = blob.to_bytes().unwrap();
        let relative = path.strip_prefix(&root).unwrap();
        let destination = output.join(relative);
        if compare {
            let baseline = std::fs::read(&destination).unwrap();
            if bytes != baseline {
                // Keep both payloads when a comparison fails so the differing
                // fields can be inspected without running another extraction.
                std::fs::write(destination.with_extension("head.json"), &bytes).unwrap();
            }
            assert_eq!(bytes.len(), baseline.len(), "length differs: {relative:?}");
            assert!(bytes == baseline, "serialized bytes differ: {relative:?}");
        } else {
            std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
            std::fs::write(&destination, &bytes).unwrap();
        }
        total_bytes += bytes.len();
        if Some(path) == largest_ts || relative.ends_with("crates/aft/src/subc/mod.rs") {
            measures.push(format!(
                "{relative:?}: source_bytes={} lines={} nodes={} elapsed={elapsed:?} whole_file_parses={} macro_range_parses={} unique_macro_ranges={} kind_copies={} child_vectors={} ordinal_visits={} dispatch_visits={} position_bytes={} call_kind_vectors={}",
                source.len(),
                source.lines().count(),
                blob.parse().unwrap().ast_nodes.len(),
                work.whole_file_parses, work.macro_ranges.len(), work.macro_ranges.iter().collect::<BTreeSet<_>>().len(),
                work.kind_copies, work.child_vectors, work.ordinal_visits, work.dispatch_visits, work.position_bytes, work.call_kind_vectors
            ));
        }
    }
    for measurement in measures {
        println!("{measurement}");
    }
    println!(
        "{} supported files; {total_bytes} serialized bytes; compared={compare}; elapsed={:?}",
        paths.len(),
        started.elapsed()
    );
    assert!(!paths.is_empty());
}
