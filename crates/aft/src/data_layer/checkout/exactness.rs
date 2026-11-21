//! Offline exactness gates. Compiled only with `test-hooks`, never into a
//! production build. Artifacts retain stored bytes, not a re-resolution of
//! the source, so a candidate cannot erase a bad recording by recomputing it.

use super::*;
use std::io::{Read, Seek, SeekFrom, Write};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::callgraph_store::CallGraphRead;
use crate::inspect::job::is_test_file;

type GateResult<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Serialize, Deserialize)]
struct FileEntry {
    path: String,
    record: String,
    fragment: Option<String>,
    offset: u64,
    len: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct QuerySpec {
    op: String,
    file: String,
    symbol: String,
    stratum: String,
    to_file: Option<String>,
    to_symbol: Option<String>,
    expression: Option<String>,
    positive: bool,
}

impl QuerySpec {
    fn requires_positive_path(&self, include_tests: bool) -> bool {
        self.positive
            && (include_tests
                || (!is_test_file(&self.file)
                    && !self.to_file.as_deref().is_some_and(is_test_file)))
    }
}

/// Negative control: change a real method-call target before the cold
/// builder hands the resolution to its publication writer. An explicit
/// path keeps this independent of parallel scheduling.
pub(crate) fn flip_site_for_control(path: &str, file: &mut FileResolution) {
    if std::env::var("AFT_EXACTNESS_FLIP_SITE").ok().as_deref() != Some(path) {
        return;
    }
    for site in &mut file.sites {
        let targets = site.resolution.targets_mut();
        if let Some(mut target) = targets.pop_first() {
            target.symbol.push_str("__exactness_broken");
            targets.insert(target);
            eprintln!("EXACTNESS CONTROL flipped site target in {path}");
            return;
        }
    }
    panic!("EXACTNESS CONTROL found no site target in {path}");
}

/// A site's targets, changeable whether the site holds its answer or shares
/// it: the exactness gates (`scripts/telemetry/callgraph-exactness`) build
/// this file into older source trees as well, where sites hold their own.
trait TargetsMut {
    fn targets_mut(
        &mut self,
    ) -> &mut std::collections::BTreeSet<crate::callgraph_store::join::dispatch::Target>;
}

impl TargetsMut for crate::callgraph_store::join::dispatch::Resolution {
    fn targets_mut(
        &mut self,
    ) -> &mut std::collections::BTreeSet<crate::callgraph_store::join::dispatch::Target> {
        &mut self.targets
    }
}

impl TargetsMut for std::sync::Arc<crate::callgraph_store::join::dispatch::Resolution> {
    fn targets_mut(
        &mut self,
    ) -> &mut std::collections::BTreeSet<crate::callgraph_store::join::dispatch::Target> {
        &mut std::sync::Arc::make_mut(self).targets
    }
}

fn save(path: &Path, value: &impl Serialize) -> GateResult<()> {
    std::fs::write(path, serde_json::to_vec(value)?)?;
    Ok(())
}

fn checked_failures(view: &CheckoutView) -> GateResult<()> {
    let failures = view.callgraph_failures();
    if !failures.is_empty() {
        return Err(format!("StoredSlices::failures(): {failures:?}").into());
    }
    Ok(())
}

fn export(checkout: &Checkout, out: &Path, reference: Option<&Path>) -> GateResult<usize> {
    std::fs::create_dir_all(out.join("sections"))?;
    let view = checkout.view().ok_or("no published view")?;
    let published = &view.published;
    if !published.graph_built {
        return Err("publication has no call graph".into());
    }
    let expected: Option<Vec<FileEntry>> = reference
        .map(|dir| -> GateResult<_> {
            Ok(serde_json::from_slice(&std::fs::read(
                dir.join("files.json"),
            )?)?)
        })
        .transpose()?;
    let mut expected_blob = reference
        .map(|dir| std::fs::File::open(dir.join("resolutions.bin")))
        .transpose()?;
    let expected = expected.map(|files| {
        files
            .into_iter()
            .map(|file| (file.path.clone(), file))
            .collect::<BTreeMap<_, _>>()
    });
    let mut blob = std::fs::File::create(out.join("resolutions.bin"))?;
    let mut files = Vec::new();
    let mut count = 0;
    let mut offset = 0;
    for (index, record) in published.state.paths().iter().enumerate() {
        let path = std::str::from_utf8(&record.path)?.to_string();
        let key = crate::data_layer::content::record_key(record.content, &record.language)
            .digest()
            .to_string();
        let fragment = published.state.fragment(index);
        let mut bytes = Vec::new();
        if fragment.is_some() {
            let resolution = published
                .graph
                .resolution_of(&path)
                .ok_or_else(|| format!("{path}: missing stored resolution"))?;
            bytes = super::super::fragment::encode(&resolution);
            if let (Some(expected), Some(blob)) = (&expected, &mut expected_blob) {
                let file = expected
                    .get(&path)
                    .ok_or_else(|| format!("{path}: absent from reference"))?;
                blob.seek(SeekFrom::Start(file.offset))?;
                let mut encoded = vec![0; usize::try_from(file.len)?];
                blob.read_exact(&mut encoded)?;
                let theirs = super::super::fragment::decode(&file.record, &path, &encoded)?;
                if resolution != theirs {
                    return Err(format!("{path}: full FileResolution differs (rows, sites/protected sets/order/provenance, binding, dispatch or site_dependencies)").into());
                }
            }
            count += 1;
        }
        blob.write_all(&bytes)?;
        files.push(FileEntry {
            path,
            record: key,
            fragment: fragment.map(|key| key.to_string()),
            offset,
            len: bytes.len() as u64,
        });
        offset += bytes.len() as u64;
    }
    if count == 0 {
        return Err("compared zero FileResolutions".into());
    }
    if let Some(expected) = &expected {
        let paths = files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<BTreeSet<_>>();
        if paths != expected.keys().map(String::as_str).collect() {
            return Err("publication path set differs from reference".into());
        }
    }
    save(&out.join("files.json"), &files)?;
    // Cold and folded publications have one active graph/content segment.
    // Match by section identity, not physical offset or section-table order.
    let mut sections = BTreeMap::new();
    for segment in &published.segments {
        for (handle, info) in segment.sections() {
            let kind = info.id.kind.get();
            if !(0x0200..=0x0306).contains(&kind) {
                continue;
            }
            let name = format!("{kind:04x}-{:04x}.bin", info.id.key_space.get());
            if sections.contains_key(&name) {
                return Err(format!("duplicate active section {name}; fold first").into());
            }
            let mut file = std::fs::File::create(out.join("sections").join(&name))?;
            let mut position = 0;
            while position < info.len {
                let len = usize::try_from((info.len - position).min(1 << 20))?;
                let mut bytes = vec![0; len];
                segment.read(handle, position, &mut bytes)?;
                file.write_all(&bytes)?;
                position += len as u64;
            }
            sections.insert(
                name,
                json!({"version": info.version, "items": info.item_count, "bytes": info.len}),
            );
        }
    }
    save(&out.join("sections.json"), &sections)?;
    checked_failures(&view)?;
    save(
        &out.join("health.json"),
        &json!({"stored_slices_failures": [], "full_resolutions": count}),
    )?;
    Ok(count)
}

fn query_plan(view: &CheckoutView, sample: usize) -> GateResult<Vec<QuerySpec>> {
    let mut symbols = BTreeSet::new();
    let mut incoming = BTreeMap::<(String, String), usize>::new();
    let mut callers = BTreeSet::new();
    for record in view.published.state.paths() {
        let path = std::str::from_utf8(&record.path)?;
        let Some(slice) = view.graph.slice(path) else {
            continue;
        };
        for node in slice.nodes() {
            symbols.insert((path.to_string(), node.scoped_name().to_string()));
        }
        for edge in slice.edges() {
            let target = (
                edge.target_file().to_string(),
                edge.target_symbol().to_string(),
            );
            *incoming.entry(target.clone()).or_default() += 1;
            let Some(caller) = slice.node(edge.source()) else {
                continue;
            };
            callers.insert((path.to_string(), caller.scoped_name().to_string()));
        }
    }
    let mut ranked = symbols.iter().cloned().collect::<Vec<_>>();
    ranked.sort_by(|a, b| {
        incoming
            .get(b)
            .unwrap_or(&0)
            .cmp(incoming.get(a).unwrap_or(&0))
            .then(a.cmp(b))
    });
    let mut selected = BTreeMap::new();
    for (stratum, start) in [("busy", 0), ("middle", ranked.len() / 2)] {
        for index in start..(start + sample).min(ranked.len()) {
            selected.insert(ranked[index].clone(), stratum);
        }
    }
    let mut leaves = 0;
    for symbol in &symbols {
        if !callers.contains(symbol) && leaves < sample {
            selected.insert(symbol.clone(), "leaf");
            leaves += 1;
        }
    }
    // A small mixed-language island ensures every edited project exercises
    // the same positive data-flow and inter-file path witnesses.
    selected.insert(("exactness_gate/main.ts".into(), "entry".into()), "witness");
    // Slices contain non-call edges as well. The trace's outgoing lookup
    // includes callable constructions but excludes type-only constructions
    // and callback references.
    let sources = selected.keys().cloned().collect::<Vec<_>>();
    let mut outgoing = view.callgraph().outgoing_calls_for_symbols(&sources)?;
    let mut plan = Vec::new();
    for (source, stratum) in selected {
        let target = outgoing.remove(&source).and_then(|calls| {
            calls
                .into_iter()
                .filter_map(|site| site.target)
                .map(|target| (target.file, target.symbol))
                .find(|target| target != &source)
        });
        let (file, symbol) = source;
        for op in ["callers", "impact", "trace_to", "call_tree", "trace_data"] {
            plan.push(QuerySpec {
                op: op.into(),
                file: file.clone(),
                symbol: symbol.clone(),
                stratum: stratum.into(),
                to_file: None,
                to_symbol: None,
                expression: (op == "trace_data").then(|| "value".into()),
                positive: false,
            });
        }
        if let Some((target_file, target_symbol)) = target {
            plan.push(QuerySpec {
                op: "trace_to_symbol".into(),
                file: file.clone(),
                symbol: symbol.clone(),
                stratum: stratum.into(),
                to_file: Some(target_file),
                to_symbol: Some(target_symbol),
                expression: None,
                positive: true,
            });
        }
    }
    for (to_file, to_symbol, positive) in [
        ("exactness_gate/lib.ts", "sink", true),
        ("exactness_gate/lib.ts", "orphan", false),
    ] {
        plan.push(QuerySpec {
            op: "trace_to_symbol".into(),
            file: "exactness_gate/main.ts".into(),
            symbol: "entry".into(),
            stratum: "witness".into(),
            to_file: Some(to_file.into()),
            to_symbol: Some(to_symbol.into()),
            expression: None,
            positive,
        });
    }
    checked_failures(view)?;
    Ok(plan)
}

fn native_plan(view: &CheckoutView, out: &Path) -> GateResult<()> {
    let mut members = BTreeMap::<String, usize>::new();
    let mut declarations = BTreeMap::<String, Vec<Value>>::new();
    let mut callees = BTreeMap::<(String, String), usize>::new();
    let mut native_files = Vec::new();
    let mut member_readers = BTreeMap::<String, BTreeSet<usize>>::new();
    let mut java_members = BTreeMap::<String, BTreeSet<String>>::new();
    let mut java_callers = BTreeMap::<String, BTreeSet<usize>>::new();
    for record in view.published.state.paths() {
        let path = std::str::from_utf8(&record.path)?;
        if path.starts_with("exactness_gate/") {
            continue;
        }
        let Some((_, parse)) = view.published.graph.parse_of(path) else {
            continue;
        };
        let reader = native_files.len();
        native_files.push(path.to_string());
        for site in &parse.dispatch.sites {
            if let Some(member) = &site.member {
                *members.entry(member.clone()).or_default() += 1;
                if parse.language == "java" {
                    member_readers
                        .entry(member.clone())
                        .or_default()
                        .insert(reader);
                }
            }
        }
        if parse.language == "java" {
            java_members.insert(
                path.to_string(),
                parse
                    .dispatch
                    .methods
                    .iter()
                    .map(|method| method.name.clone())
                    .collect(),
            );
        }
        for method in &parse.dispatch.methods {
            if let Some(symbol) = parse
                .symbols
                .iter()
                .find(|symbol| symbol.scoped_name == method.symbol)
            {
                declarations
                    .entry(method.name.clone())
                    .or_default()
                    .push(json!({"file": path, "language": parse.language, "owner": method.owner, "symbol": symbol}));
            }
        }
        if let Some(slice) = view.graph.slice(path) {
            for edge in slice.edges() {
                if edge.target_file().ends_with(".java") {
                    java_callers
                        .entry(edge.target_file().to_string())
                        .or_default()
                        .insert(reader);
                }
                *callees
                    .entry((
                        edge.target_file().to_string(),
                        edge.target_symbol().to_string(),
                    ))
                    .or_default() += 1;
            }
        }
    }
    // Editing a Java file can touch readers of other members of the same
    // type too. Count distinct files for the entire source file, in addition
    // to the individual method's site or caller count.
    let java_file_readers = java_members
        .into_iter()
        .map(|(path, names)| {
            let mut readers = java_callers.remove(&path).unwrap_or_default();
            for name in names {
                readers.extend(member_readers.get(&name).into_iter().flatten());
            }
            (path, readers.len())
        })
        .collect::<BTreeMap<_, _>>();
    let mut member_rows = declarations.into_iter().map(|(member, declarations)| json!({"sites": members.get(&member).copied().unwrap_or(0), "member": member, "declarations": declarations})).collect::<Vec<_>>();
    member_rows.sort_by(|a, b| {
        b["sites"]
            .as_u64()
            .cmp(&a["sites"].as_u64())
            .then(a["member"].as_str().cmp(&b["member"].as_str()))
    });
    let mut busiest = callees.into_iter().collect::<Vec<_>>();
    busiest.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    // RefreshBudget admits max(1,024, ceil(parsed / 10)) files. Reserve the
    // 13 paths the driver's first edit batch changes: eight in exactness_gate/
    // and five in the corpus (including both sides of the rename). Divide the
    // rest across the four original corpus files. Subtract two from the parsed
    // count to allow for the two deletions in that batch.
    let java_reader_limit = (view
        .published
        .graph
        .parsed_files()
        .count()
        .saturating_sub(2)
        .div_ceil(10)
        .max(1024)
        - 13)
        / 4;
    let mut top = Vec::new();
    // The driver renames a parameter of the first of these whose signature
    // it can edit; on some projects none of the ten busiest has a parameter
    // besides its receiver, so it gets more to choose from.
    for ((path, name), callers) in busiest {
        if top.len() == 40 {
            break;
        }
        if java_file_readers
            .get(&path)
            .is_some_and(|readers| *readers > java_reader_limit)
        {
            continue;
        }
        let Some((_, parse)) = view.published.graph.parse_of(&path) else {
            continue;
        };
        if let Some(symbol) = parse
            .symbols
            .iter()
            .find(|symbol| symbol.scoped_name == name)
        {
            top.push(json!({"file": path, "language": parse.language, "symbol": symbol, "callers": callers}));
        }
    }
    save(
        &out.join("native.json"),
        &json!({"files": native_files, "members": member_rows, "top_callees": top,
            "java_reader_limit": java_reader_limit, "java_file_readers": java_file_readers}),
    )
}

fn result<T: Serialize>(
    value: Result<T, crate::callgraph_store::CallGraphStoreError>,
) -> GateResult<Value> {
    match value {
        Ok(value) => Ok(json!({"ok": value})),
        Err(error) => Ok(json!({"error": format!("{error:?}")})),
    }
}

fn queries(
    checkout: &Checkout,
    out: &Path,
    reference: Option<&Path>,
    sample: usize,
) -> GateResult<()> {
    use crate::data_layer::callgraph_store_adapter as adapter;
    let view = checkout.view().ok_or("no published view")?;
    let plan: Vec<QuerySpec> = if let Some(reference) = reference {
        serde_json::from_slice(&std::fs::read(reference.join("plan.json"))?)?
    } else {
        query_plan(&view, sample)?
    };
    save(&out.join("plan.json"), &plan)?;
    let graph = view.callgraph();
    let cache = Arc::new(RwLock::new(crate::parser::SymbolCache::new()));
    let mut output = std::io::BufWriter::new(std::fs::File::create(out.join("queries.jsonl"))?);
    for spec in &plan {
        for tests in [false, true] {
            let path = view.root().join(&spec.file);
            let answer = match spec.op.as_str() {
                "callers" => result(adapter::callers_result(
                    &graph,
                    &path,
                    &spec.symbol,
                    1,
                    tests,
                )),
                "impact" => result(adapter::impact_result(
                    &graph,
                    &path,
                    &spec.symbol,
                    3,
                    tests,
                )),
                "trace_to" => result(adapter::trace_to_result(
                    &graph,
                    &path,
                    &spec.symbol,
                    10,
                    tests,
                )),
                "call_tree" => result(adapter::call_tree_result(
                    &graph,
                    &path,
                    &spec.symbol,
                    5,
                    tests,
                )),
                "trace_data" => result(adapter::trace_data_result(
                    &graph,
                    &path,
                    &spec.symbol,
                    spec.expression.as_deref().unwrap(),
                    5,
                    Arc::clone(&cache),
                )),
                "trace_to_symbol" => result(adapter::trace_to_symbol_result(
                    &graph,
                    &path,
                    &spec.symbol,
                    spec.to_symbol.as_deref().unwrap(),
                    spec.to_file
                        .as_ref()
                        .map(|file| view.root().join(file))
                        .as_deref(),
                    10,
                    tests,
                )),
                _ => return Err("unknown exactness query".into()),
            }?;
            if spec.requires_positive_path(tests)
                && answer.pointer("/ok/path").is_none_or(Value::is_null)
            {
                return Err(format!(
                    "positive path witness failed: {}:{} -> {}:{} (tests={tests}): {answer}",
                    spec.file,
                    spec.symbol,
                    spec.to_file.as_deref().unwrap_or("<any file>"),
                    spec.to_symbol.as_deref().unwrap_or("<unknown>"),
                )
                .into());
            }
            if spec.stratum == "witness"
                && spec.op == "trace_data"
                && answer
                    .pointer("/ok/hops")
                    .and_then(Value::as_array)
                    .is_none_or(Vec::is_empty)
            {
                return Err(format!("positive trace_data witness failed: {answer}").into());
            }
            writeln!(
                output,
                "{}",
                json!({"query": spec, "include_tests": tests, "answer": answer})
            )?;
        }
    }
    // Preserve every dead-code input row, including initializer groups and
    // protected roots. The timestamp is deliberately outside the comparison.
    let dead =
        super::super::dead_code::stream::Rows::new(&view, &crate::inspect::InspectEntryPoints)?;
    let snapshot = view.dead_code_snapshot_in(None, &crate::inspect::InspectEntryPoints)?;
    super::super::dead_code::stream::artifact::write(
        &out.join("dead-code.json"),
        &dead,
        &snapshot,
    )?;
    drop(dead);
    save(
        &out.join("dead-code-findings.json"),
        &crate::inspect::dead_code_gate::exactness_rows(&view, &out.join("dead-code-work"))?,
    )?;
    checked_failures(&view)?;
    save(&out.join("cache.json"), &super::super::slice_cache::shared().stats().map(|stats| json!({"budget": stats.budget_bytes, "evictions": stats.evictions, "misses": stats.misses})))?;
    Ok(())
}

fn java_edit_spans(
    path: &Path,
    source: &str,
    name: &str,
    end: tree_sitter::Point,
) -> GateResult<Value> {
    let tree =
        crate::parser::parse_source_with_cached_parser(path, source, crate::parser::LangId::Java)?;
    // Match the stored symbol's end point and name rather than copying whole
    // lines, which may also contain other declarations or a class brace.
    let previous = tree_sitter::Point::new(end.row, end.column.saturating_sub(1));
    let mut node = tree
        .root_node()
        .descendant_for_point_range(previous, previous);
    while let Some(candidate) = node {
        if matches!(
            candidate.kind(),
            "method_declaration" | "constructor_declaration"
        ) && candidate.end_position() == end
            && candidate
                .child_by_field_name("name")
                .is_some_and(|id| &source[id.byte_range()] == name)
            && !candidate.has_error()
        {
            let member = (candidate.kind() == "method_declaration"
                && candidate.child_by_field_name("body").is_some()
                && candidate.parent().is_some_and(|body| {
                    body.kind() == "class_body"
                        && body
                            .parent()
                            .is_some_and(|owner| owner.kind() == "class_declaration")
                }))
            .then(|| [candidate.start_byte(), candidate.end_byte()]);
            let parameter = candidate
                .child_by_field_name("parameters")
                .and_then(|parameters| {
                    parameters
                        .named_children(&mut parameters.walk())
                        .find_map(|param| match param.kind() {
                            "formal_parameter" => param.child_by_field_name("name"),
                            "spread_parameter" => param
                                .named_children(&mut param.walk())
                                .find(|child| child.kind() == "variable_declarator")
                                .and_then(|child| child.child_by_field_name("name")),
                            _ => None,
                        })
                })
                .map(|id| [id.start_byte(), id.end_byte()]);
            return Ok(json!({"member": member, "parameter": parameter}));
        }
        node = candidate.parent();
    }
    Ok(Value::Null)
}

/// Commands are deliberately separate processes: `refresh` must bind the
/// on-disk publication, never inherit the cold builder's engine.
pub fn run(args: Vec<String>) -> GateResult<()> {
    if let [mode, path, name, end_line, end_col] = args.as_slice() {
        if mode == "java-edit-spans" {
            let path = Path::new(path);
            let source = std::fs::read_to_string(path)?;
            let end = tree_sitter::Point::new(end_line.parse()?, end_col.parse()?);
            println!("{}", java_edit_spans(path, &source, name, end)?);
            return Ok(());
        }
    }
    if args.first().is_some_and(|mode| mode == "validate") {
        let path = Path::new(args.get(1).ok_or("validate requires a path")?);
        let source = std::fs::read_to_string(path)?;
        if crate::edit::validate_syntax_str(&source, path) != Some(true) {
            return Err(format!(
                "{}: edited source is invalid or unsupported",
                path.display()
            )
            .into());
        }
        println!("EXACTNESS syntax valid: {}", path.display());
        return Ok(());
    }
    if args.len() < 7 {
        return Err(
            "usage: MODE ROOT STORE OUT THREADS CACHE_BYTES SAMPLE [REFERENCE|-] [CHANGES_JSON]"
                .into(),
        );
    }
    let mode = &args[0];
    let root = std::fs::canonicalize(&args[1])?;
    let store = Path::new(&args[2]);
    let out = Path::new(&args[3]);
    let threads = args[4].parse()?;
    let cache = args[5].parse()?;
    let sample = args[6].parse()?;
    let reference = args.get(7).filter(|arg| arg.as_str() != "-").map(Path::new);
    std::fs::create_dir_all(out)?;
    let checkout = Checkout::bind(
        Arc::new(DataLayer::new(cache, threads)),
        store,
        FamilyId::from_digest(Digest128::of(b"callgraph exactness gate")),
        &root,
    )?;
    match mode.as_str() {
        "cold" => {
            checkout.build()?;
        }
        "refresh" => {
            if checkout.needs_build() {
                return Err(
                    "restart lost the publication; refusing a cold-build substitute".into(),
                );
            }
            let changes: Vec<String> = serde_json::from_slice(&std::fs::read(
                args.get(8).ok_or("refresh requires changes JSON")?,
            )?)?;
            checkout.record_paths(
                &changes
                    .into_iter()
                    .map(String::into_bytes)
                    .collect::<Vec<_>>(),
            )?;
            let update = checkout
                .refresh_callgraph()?
                .ok_or("no incremental refresh")?;
            if update.cold_build {
                return Err("refresh rebuilt cold; refusing a cold-build substitute".into());
            }
            checked_failures(&checkout.view().ok_or("no published view")?)?;
            save(
                &out.join("prepublish-health.json"),
                &json!({"stored_slices_failures": []}),
            )?;
            checkout.publish_changes()?.ok_or("no delta publication")?;
        }
        "snapshot" => {}
        _ => return Err("unknown exactness mode".into()),
    }
    if mode == "cold" {
        export(&checkout, &out.join("raw"), None)?;
    }
    // Fold copies active stored units and their term postings without
    // resolving again. Apply it to both cold and refreshed publications so
    // obsolete records and segment-local ordinals do not create false diffs.
    match checkout.fold()? {
        super::Folded::Done(_) => {}
        other => return Err(format!("fold did not finish: {other:?}").into()),
    }
    let count = export(&checkout, out, reference)?;
    if reference.is_none() {
        native_plan(&checkout.view().ok_or("no published view")?, out)?;
    }
    queries(&checkout, out, reference, sample)?;
    println!("EXACTNESS snapshot: {count} full FileResolutions; StoredSlices::failures()=[]");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_edit_spans_use_stored_symbol_coordinates() {
        use crate::callgraph_store::join::CallgraphBlob;

        let declaration = "@Marker public <T extends Number> T lookup(\n        @Marker java.util.List<T> input\n    ) { return input.get(0); }";
        let source = format!(
            "class Service {{ int café; {declaration} void keep() {{}} }}\ninterface API {{ void lookup(String... names); }}\n"
        );
        let blob = CallgraphBlob::extract(&source, "java", "test").unwrap();
        let spans = |symbol: &crate::callgraph_store::join::BlobSymbol| {
            java_edit_spans(
                Path::new("Service.java"),
                &source,
                &symbol.name,
                tree_sitter::Point::new(symbol.end_line as usize, symbol.end_col as usize),
            )
            .unwrap()
        };
        let text = |value: &Value| {
            let [start, end]: [usize; 2] = serde_json::from_value(value.clone()).unwrap();
            &source[start..end]
        };
        let symbols = &blob.parse().unwrap().symbols;
        let mut matches = symbols.iter().filter(|symbol| symbol.name == "lookup");
        let method = spans(matches.next().unwrap());
        assert_eq!(text(&method["member"]), declaration);
        assert_eq!(text(&method["parameter"]), "input");
        let interface = spans(matches.next().unwrap());
        assert!(interface["member"].is_null());
        assert_eq!(text(&interface["parameter"]), "names");
        assert!(matches.next().is_none());
    }

    #[test]
    fn positive_path_expectation_respects_test_endpoints() {
        for (file, to_file, required_without_tests) in [
            ("src/start.ts", "src/end.ts", true),
            ("tests/start.ts", "src/end.ts", false),
            ("src/start.ts", "src/end.test.ts", false),
            ("tests/start.ts", "tests/end.ts", false),
        ] {
            let mut spec = QuerySpec {
                op: "trace_to_symbol".into(),
                file: file.into(),
                symbol: "start".into(),
                stratum: "busy".into(),
                to_file: Some(to_file.into()),
                to_symbol: Some("end".into()),
                expression: None,
                positive: true,
            };
            assert_eq!(spec.requires_positive_path(false), required_without_tests);
            assert!(spec.requires_positive_path(true));
            spec.positive = false;
            assert!(!spec.requires_positive_path(false));
            assert!(!spec.requires_positive_path(true));
        }
    }

    #[test]
    fn plan_uses_traversable_calls_not_raw_reference_edges() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("main.ts"),
            "function callback() {}\n\
             function invoked() {}\n\
             class Service { constructor() {} }\n\
             function callbackOnly() { items.map(callback); }\n\
             function construct() { new Service(); }\n\
             function direct() { invoked(); }\n",
        )
        .unwrap();
        let checkout = Checkout::bind(
            Arc::new(DataLayer::new(8 << 20, 1)),
            store.path(),
            FamilyId::from_digest(Digest128::of(b"exactness plan test")),
            root.path(),
        )
        .unwrap();
        checkout.build().unwrap();
        let view = checkout.view().unwrap();
        let slice = view.graph.slice("main.ts").unwrap();
        assert!(slice.edges().any(|edge| {
            edge.kind() == "callback_ref"
                && edge.target_symbol() == "callback"
                && slice.node(edge.source()).unwrap().scoped_name() == "callbackOnly"
        }));

        let plan = query_plan(&view, 16).unwrap();
        assert!(plan.iter().any(|spec| {
            spec.file == "main.ts" && spec.symbol == "callbackOnly" && spec.op == "callers"
        }));
        let native_witnesses = plan
            .iter()
            .filter(|spec| spec.positive && spec.file == "main.ts")
            .map(|spec| (spec.symbol.as_str(), spec.to_symbol.as_deref().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(
            native_witnesses,
            [("construct", "Service::constructor"), ("direct", "invoked")]
        );
        for (target, positive) in [("sink", true), ("orphan", false)] {
            assert!(plan.iter().any(|spec| {
                spec.file == "exactness_gate/main.ts"
                    && spec.symbol == "entry"
                    && spec.to_file.as_deref() == Some("exactness_gate/lib.ts")
                    && spec.to_symbol.as_deref() == Some(target)
                    && spec.positive == positive
            }));
        }
    }
}
