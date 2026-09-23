use aft::callgraph_store::project_dead_code_snapshot;
use aft::config::Config;
use aft::inspect::scanners::dead_code::run_dead_code_scan;
use aft::inspect::{CallgraphSnapshot, InspectCategory, InspectJob, JobKey};
use aft::parser::SymbolCache;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

fn path_bytes(path: &Path) -> u64 { path.as_os_str().len() as u64 }
fn estimate(snapshot: &CallgraphSnapshot) -> u64 {
    let files = snapshot.files.iter().map(|p| std::mem::size_of::<PathBuf>() as u64 + path_bytes(p)).sum::<u64>();
    let exports = snapshot.exported_symbols.iter().map(|e| std::mem::size_of_val(e) as u64 + path_bytes(&e.file) + e.symbol.len() as u64 + e.kind.len() as u64).sum::<u64>();
    let calls = snapshot.outbound_calls.iter().map(|c| std::mem::size_of_val(c) as u64 + path_bytes(&c.caller_file) + c.caller_symbol.len() as u64 + c.target.len() as u64 + c.provenance.len() as u64).sum::<u64>();
    let entry_points = snapshot.entry_points.iter().map(|p| std::mem::size_of::<PathBuf>() as u64 + path_bytes(p)).sum::<u64>();
    let symbols = snapshot.entry_point_symbols.iter().map(|(p, ss)| std::mem::size_of::<(PathBuf,std::collections::BTreeSet<String>)>() as u64 + path_bytes(p) + ss.iter().map(|s| std::mem::size_of::<String>() as u64+s.len() as u64).sum::<u64>()).sum::<u64>();
    std::mem::size_of::<CallgraphSnapshot>() as u64 + files + exports + calls + entry_points + symbols
}
fn main() {
    let args=std::env::args().collect::<Vec<_>>();
    let db=PathBuf::from(&args[1]); let root=PathBuf::from(&args[2]);
    let started=Instant::now();
    let snapshot=project_dead_code_snapshot(&db).expect("project snapshot");
    let projection_ms=started.elapsed().as_millis();
    let estimate=estimate(&snapshot);
    eprintln!("issue330_harness snapshot_complete files={} exports={} edges={} entry_points={} estimate_bytes={} projection_ms={}", snapshot.files.len(), snapshot.exported_symbols.len(), snapshot.outbound_calls.len(), snapshot.entry_points.len(), estimate, projection_ms);
    let files=snapshot.files.clone();
    let job=InspectJob{job_id:1,key:JobKey::for_project_category(InspectCategory::DeadCode),category:InspectCategory::DeadCode,scope_files:files,project_root:root.clone(),inspect_dir:root.join(".aft-issue330-inspect"),config:Arc::new(Config{project_root:Some(root),..Config::default()}),symbol_cache:Arc::new(RwLock::new(SymbolCache::new())),inspect_writer:false,callgraph_writer:false,callgraph_snapshot:Some(Arc::new(snapshot))};
    let scan_started=Instant::now();
    let result=run_dead_code_scan(&job);
    let scan_ms=scan_started.elapsed().as_millis();
    eprintln!("issue330_harness scan_complete success={} contributions={} scan_ms={}",result.outcome.is_ok(),result.outcome.as_ref().map(|o|o.contributions.len()).unwrap_or(0),scan_ms);
    if let Ok(success) = result.outcome.as_ref() {
        let text = serde_json::to_string(&success.aggregate).unwrap_or_default();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&text, &mut hasher);
        eprintln!("issue330_harness aggregate_json_bytes={} aggregate_digest={:016x}", text.len(), std::hash::Hasher::finish(&hasher));
    }
    std::hint::black_box(&result);
}
