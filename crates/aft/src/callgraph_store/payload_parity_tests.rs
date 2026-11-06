use super::*;
use crate::callgraph_store::join::{CallgraphBlob, ManifestBlobReader, ManifestJoinError};
use crate::views::{Manifest, ManifestEntry, RegularPlanes, RelPath};

struct FileBlobs(PathBuf);

fn cpu_ns() -> u128 {
    #[cfg(target_os = "linux")]
    {
        let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
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

impl ManifestBlobReader for FileBlobs {
    fn read_callgraph_blob(
        &self,
        key: &str,
    ) -> std::result::Result<Option<Vec<u8>>, ManifestJoinError> {
        Ok(Some(std::fs::read(self.0.join(key)).unwrap()))
    }
}

fn legacy_bytes(blob: &CallgraphBlob) -> Vec<u8> {
    let mut value = serde_json::to_value(blob).unwrap();
    if let CallgraphBlob::Parse(parse) = blob {
        value["ast_nodes"] = serde_json::to_value(&parse.ast_nodes).unwrap();
    }
    serde_json::to_vec(&value).unwrap()
}

fn query_answers(root: &Path, database: &Path) -> String {
    let connection = super::super::open_readonly_connection(database).unwrap();
    let symbols = {
        let mut statement = connection
            .prepare(
                "SELECT DISTINCT n.file_path, n.scoped_name, f.lang FROM nodes n JOIN files f ON f.path=n.file_path ORDER BY n.file_path, n.scoped_name",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let selected = query_sample(&connection, &symbols);
    let mut store = super::super::CallGraphStore::from_connection(
        root.to_path_buf(),
        "payload-parity".into(),
        database.to_path_buf(),
        database.parent().unwrap().to_path_buf(),
        false,
        None,
        None,
        None,
        connection,
    );
    store.manifest_view = true;
    store
        .database_ready
        .store(true, std::sync::atomic::Ordering::Release);
    let mut answers = String::new();
    for (index, selected_index) in selected.iter().enumerate() {
        let (file, symbol, _) = &symbols[*selected_index];
        let path = Path::new(file);
        if let Some(output) = std::env::var_os("AFT_PAYLOAD_DUMP") {
            std::fs::write(
                PathBuf::from(output).join("current-query.txt"),
                format!("{file} {symbol}"),
            )
            .unwrap();
        }
        answers.push_str(&format!(
            "{file} {symbol}\n{:?}\n{:?}\n{:?}\n",
            store.callers_of(path, symbol, 1),
            store.call_tree(path, symbol, 1),
            store.trace_to(path, symbol, 1)
        ));
        if index % 100 == 0 {
            println!("queried {index}/{} symbols", selected.len());
        }
    }
    println!(
        "aft_callgraph: {} symbols x callers/call_tree/trace_to depth=1/1/1",
        selected.len()
    );
    answers
}

/// Select a reproducible cross-file/language read-path check. Table and dead-code
/// comparisons remain exhaustive; the highest-edge files are never sampled down.
fn query_sample(connection: &Connection, symbols: &[(String, String, String)]) -> BTreeSet<usize> {
    const SEED: u64 = 0x5eed_c011_6a9f_0001;
    const SAMPLE: usize = 2000;
    let rank = |index: &usize| {
        let (file, symbol, _) = &symbols[*index];
        let mut hash = blake3::Hasher::new();
        hash.update(&SEED.to_le_bytes());
        hash.update(file.as_bytes());
        hash.update(&[0]);
        hash.update(symbol.as_bytes());
        *hash.finalize().as_bytes()
    };
    let mut files = BTreeMap::<&str, BTreeMap<&str, Vec<usize>>>::new();
    for (index, (file, _, language)) in symbols.iter().enumerate() {
        files
            .entry(language)
            .or_default()
            .entry(file)
            .or_default()
            .push(index);
    }
    let mut languages = Vec::new();
    for groups in files.values() {
        let mut representatives = groups
            .values()
            .map(|indices| *indices.iter().min_by_key(|i| rank(i)).unwrap())
            .collect::<Vec<_>>();
        representatives.sort_by_key(rank);
        languages.push(representatives.into_iter());
    }
    let mut selected = BTreeSet::new();
    while selected.len() < SAMPLE {
        let before = selected.len();
        for language in &mut languages {
            if selected.len() == SAMPLE {
                break;
            }
            if let Some(index) = language.next() {
                selected.insert(index);
            }
        }
        if before == selected.len() {
            break;
        }
    }
    let mut remaining = (0..symbols.len()).collect::<Vec<_>>();
    remaining.sort_by_key(rank);
    let mut remaining = remaining.into_iter();
    while selected.len() < SAMPLE {
        let Some(index) = remaining.next() else {
            break;
        };
        selected.insert(index);
    }
    let mut statement = connection
        .prepare(
            "SELECT file, COUNT(*) FROM (
             SELECT n.file_path AS file FROM edges e JOIN nodes n ON n.id=e.source_node
             UNION ALL SELECT target_file AS file FROM edges
         ) GROUP BY file ORDER BY COUNT(*) DESC, file LIMIT 10",
        )
        .unwrap();
    let mut forced = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<BTreeSet<_>>>()
        .unwrap();
    for (index, (file, _, _)) in symbols.iter().enumerate() {
        if file.ends_with("/protocol.rs") || file.ends_with("/context.rs") {
            forced.insert(file.clone());
        }
        if forced.contains(file) {
            selected.insert(index);
        }
    }
    let mut language_counts = BTreeMap::<&str, usize>::new();
    let mut file_counts = BTreeMap::<&str, usize>::new();
    for index in &selected {
        let (file, _, language) = &symbols[*index];
        *language_counts.entry(language).or_default() += 1;
        *file_counts.entry(file).or_default() += 1;
    }
    let sample = serde_json::json!({"seed": format!("{SEED:#x}"), "total_symbols": symbols.len(),
        "sample_size": selected.len(), "files": file_counts.len(), "languages": language_counts,
        "forced_files": forced, "selected": selected.iter().map(|i| &symbols[*i]).collect::<Vec<_>>()});
    println!("query sample: seed={SEED:#x} selected={} files={} languages={language_counts:?} forced={forced:?}", selected.len(), file_counts.len());
    if let Some(output) = std::env::var_os("AFT_PAYLOAD_DUMP") {
        let output = PathBuf::from(output);
        std::fs::create_dir_all(&output).unwrap();
        std::fs::write(
            output.join("sample.json"),
            serde_json::to_vec(&sample).unwrap(),
        )
        .unwrap();
    }
    selected
}

fn dead_code_answers(root: &Path, files: &[PathBuf], snapshot: CallgraphSnapshot) -> String {
    use crate::inspect::{InspectCategory, InspectJob, JobKey};
    let projection = format!("{snapshot:?}");
    let job = InspectJob {
        job_id: 1,
        key: JobKey::for_project_category(InspectCategory::DeadCode),
        category: InspectCategory::DeadCode,
        scope_files: files.to_vec(),
        project_root: root.to_path_buf(),
        inspect_dir: root.join(".inspect"),
        config: std::sync::Arc::new(crate::config::Config {
            project_root: Some(root.to_path_buf()),
            ..Default::default()
        }),
        symbol_cache: std::sync::Arc::new(
            std::sync::RwLock::new(crate::parser::SymbolCache::new()),
        ),
        inspect_writer: true,
        callgraph_writer: true,
        callgraph_snapshot: Some(std::sync::Arc::new(snapshot)),
    };
    let result = crate::inspect::scanners::dead_code::run_dead_code_scan(&job)
        .outcome
        .unwrap();
    let mut contributions = result
        .contributions
        .iter()
        .map(|c| format!("{} {:?}", c.file_path.display(), c.contribution))
        .collect::<Vec<_>>();
    contributions.sort();
    format!(
        "{projection}\n{}\n{}",
        result.aggregate,
        contributions.join("\n")
    )
}
