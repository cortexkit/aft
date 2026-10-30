//! Per-child search engines for a parent folder's `aft_search`.
//!
//! Each child gets a reader-only [`AppContext`] with its own search engine.
//! The worker installs the child's published text and semantic indexes into
//! that context. Queries search those indexes, or use the bounded disk scan
//! when no usable index is available. These query contexts never write a
//! store. The parent merges child results without applying a second set of
//! ranking rules.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

use super::query::{attach_gaps, Gaps};
use super::{display_relative, lock, read, Child, ParentSession, Plane};

/// One child's search engine context and what is installed in it.
pub(crate) struct ChildEngine {
    ctx: AppContext,
    /// Retain the installed allocation so retirement cannot reuse its address.
    trigram: Option<Arc<crate::search_index::SearchIndex>>,
    /// Fills and live edits can replace this index within the same generation.
    semantic: Option<Arc<crate::semantic_index::SemanticIndex>>,
}

impl ChildEngine {
    fn new(session: &ParentSession, child: &Child) -> Self {
        let mut config = crate::config::Config::default();
        config.project_root = Some(child.root.clone());
        // The engine reads the indexes installed below, never a view of its own.
        config.views.enabled = false;
        config.indexes.trigram = session.planes.trigram;
        config.indexes.semantic = session.planes.semantic;
        config.indexes.callgraph = false;
        config.semantic = session.semantic_config.clone();
        Self {
            ctx: AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config),
            trigram: None,
            semantic: None,
        }
    }

    fn install_trigram(&mut self, child: &Child) {
        let Some(trigram) = read(&child.trigram).ready().cloned() else {
            return;
        };
        self.install_trigram_index(&trigram.index);
    }

    fn install_trigram_index(&mut self, index: &Arc<crate::search_index::SearchIndex>) {
        if self
            .trigram
            .as_ref()
            .is_some_and(|installed| Arc::ptr_eq(installed, index))
        {
            return;
        }
        // `SearchIndex` shares its postings through `Arc`s, so the copy the
        // engine owns costs only the per-file tables.
        *self
            .ctx
            .search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((**index).clone());
        self.trigram = Some(index.clone());
    }

    fn install_semantic(&mut self, session: &ParentSession, child: &Child) {
        let Plane::Ready(semantic) = read(&session.semantic).clone() else {
            return;
        };
        let Some(view) = read(&child.semantic).ready().cloned() else {
            return;
        };
        // The overlay index scores the vectors admitted from the child's view
        // with the same function a session bound to the child uses.
        let overlay = match semantic
            .plane
            .overlay(&view.access, &child.root, &view.snapshot)
        {
            Ok(overlay) => overlay,
            Err(error) => {
                crate::slog_warn!(
                    "parent folder search engine has no semantic index for {}: {}",
                    child.root.display(),
                    error
                );
                return;
            }
        };
        if self
            .semantic
            .as_ref()
            .is_some_and(|installed| Arc::ptr_eq(installed, &overlay.index))
        {
            return;
        }
        *self
            .ctx
            .semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((*overlay.index).clone());
        *self
            .ctx
            .semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            crate::context::SemanticIndexStatus::ready();
        self.semantic = Some(overlay.index);
    }
}

/// Creates the child's engine once its trigram snapshot is loaded, and
/// installs the child's current snapshots into it. Runs on the worker.
pub(super) fn sync(session: &ParentSession, child: &Child) {
    if !session.planes.trigram && !session.planes.semantic {
        return;
    }
    let mut engine = lock(&child.engine);
    if engine.is_none() {
        *engine = Some(ChildEngine::new(session, child));
    }
    if let Some(engine) = engine.as_mut() {
        engine.install_trigram(child);
        engine.install_semantic(session, child);
    }
}

/// Installs the child's current trigram snapshot after file changes.
pub(super) fn sync_trigram(child: &Child) {
    if let Some(engine) = lock(&child.engine).as_mut() {
        engine.install_trigram(child);
    }
}

// ---------------------------------------------------------------------------
// The parent's aft_search
// ---------------------------------------------------------------------------

/// The page size `aft_search` uses when the request names none.
const DEFAULT_TOP_K: usize = 10;

fn wire_score(row: &Value) -> f64 {
    row["score"].as_f64().unwrap_or(f64::MIN)
}

/// Rewrites a child row's paths to be relative to the parent folder. The
/// `location` field is rewritten only when it begins with the row's file:
/// some rows carry a description there instead of a path.
fn prefix_row(row: &mut Value, parent: &Path, child: &Child) {
    let Some(file) = row["file"].as_str().map(str::to_owned) else {
        return;
    };
    let path = Path::new(&file);
    let prefixed = if path.is_absolute() {
        display_relative(parent, path)
    } else {
        child
            .relative
            .join(&file)
            .to_string_lossy()
            .replace('\\', "/")
    };
    if let Some(Value::String(location)) = row.get_mut("location") {
        if let Some(rest) = location.strip_prefix(file.as_str()) {
            *location = format!("{prefixed}{rest}");
        }
    }
    row["file"] = Value::String(prefixed);
}

/// Merges the children's ranked lists without reordering any one of them:
/// at each step the child whose next row has the highest engine score
/// (fused score, or the lane score the engine reports instead) gives its
/// next row. Ties prefer the higher semantic similarity, which uses the same
/// model in every child, then the earlier child.
fn merge(mut lists: Vec<VecDeque<Value>>, limit: usize) -> Vec<Value> {
    let mut merged = Vec::new();
    while merged.len() < limit {
        let best = lists
            .iter()
            .enumerate()
            .filter_map(|(index, list)| list.front().map(|row| (index, row)))
            .max_by(|(left_index, left), (right_index, right)| {
                wire_score(left)
                    .total_cmp(&wire_score(right))
                    .then_with(|| {
                        left["semantic_score"]
                            .as_f64()
                            .unwrap_or(f64::MIN)
                            .total_cmp(&right["semantic_score"].as_f64().unwrap_or(f64::MIN))
                    })
                    .then_with(|| right_index.cmp(left_index))
            })
            .map(|(index, _)| index);
        let Some(index) = best else {
            break;
        };
        if let Some(row) = lists[index].pop_front() {
            merged.push(row);
        }
    }
    merged
}

fn render_text(query: &str, page: &[Value], more_available: bool) -> String {
    if page.is_empty() {
        return format!("No results for \"{query}\" in this parent folder's repositories.");
    }
    let mut lines = page
        .iter()
        .map(|row| {
            let location = format!(
                "{}:{}-{}",
                row["file"].as_str().unwrap_or_default(),
                row["start_line"]
                    .as_u64()
                    .or_else(|| row["line"].as_u64())
                    .unwrap_or(0),
                row["end_line"]
                    .as_u64()
                    .or_else(|| row["line"].as_u64())
                    .unwrap_or(0)
            );
            let mut line = format!("{location} {}", row["name"].as_str().unwrap_or_default());
            if let Some(first) = row["snippet"]
                .as_str()
                .or_else(|| row["line_text"].as_str())
                .and_then(|snippet| snippet.lines().map(str::trim).find(|line| !line.is_empty()))
            {
                line.push_str(&format!("\n  {first}"));
            }
            line
        })
        .collect::<Vec<_>>();
    if more_available {
        lines.push("(More results are available: raise topK or pass offset.)".into());
    }
    lines.join("\n")
}

pub(super) fn search(req: &RawRequest, ctx: &AppContext, session: &ParentSession) -> Response {
    let Some(query) = req.params.get("query").and_then(Value::as_str) else {
        return Response::error(
            &req.id,
            "invalid_request",
            "semantic_search: missing required param 'query'",
        );
    };
    let top_k = req
        .params
        .get("top_k")
        .or_else(|| req.params.get("topK"))
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_TOP_K);
    let offset = req
        .params
        .get("offset")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(0);
    let include_tests = req
        .params
        .get("include_tests")
        .or_else(|| req.params.get("includeTests"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let scope = match req.params.get("path").and_then(Value::as_str) {
        Some(raw) => match ctx.validate_read_path(&req.id, req.session(), Path::new(raw)) {
            Ok(path) => {
                let path = if path.is_relative() {
                    session.root().join(path)
                } else {
                    path
                };
                std::fs::canonicalize(&path).unwrap_or(path)
            }
            Err(response) => return response,
        },
        None => session.root().to_path_buf(),
    };

    let mut gaps = Gaps::default();
    if !super::query::discovered(session, &mut gaps) {
        let mut body = json!({
            "status": "building",
            "complete": false,
            "text": "",
            "query": query,
            "include_tests": include_tests,
            "result_count": 0,
            "results": [],
            "more_available": false,
            "engine_capped": false,
            // No search ran yet, so no answer was read from files directly.
            "fully_degraded": false,
        });
        attach_gaps(&mut body, gaps.into_values());
        return Response::success(&req.id, body);
    }
    let wanted = offset.saturating_add(top_k);
    // With semantic search on, the session embeds the query once with its
    // one model; every child engine then runs in parallel with that vector in
    // place of its own embedding, so no child starts a model. If the query
    // cannot be embedded, the model is lent to each engine in turn instead,
    // which reports the failure the way a direct search would.
    let semantic = read(&session.semantic).clone();
    let mut model = semantic
        .ready()
        .and_then(|semantic| lock(&semantic.model).take());
    let vector = model.as_mut().and_then(|model| {
        model
            .embed_query_cached(
                query,
                crate::semantic_index::QueryBudget::from_config(&session.semantic_config),
            )
            .ok()
    });
    if vector.is_some() {
        if let Some(semantic) = semantic.ready() {
            *lock(&semantic.model) = model.take();
        }
    }
    if let Some(reason) = semantic.gap_reason("semantic") {
        // A normal answer without semantic search is incomplete too; the
        // parent says why once instead of once per child.
        gaps.push("parent_semantic_unavailable", ".".into(), reason);
    }
    let children = session
        .children()
        .into_iter()
        .filter(|child| child.root.starts_with(&scope) || scope.starts_with(&child.root))
        .collect::<Vec<_>>();
    let run = |child: &Child,
               lent: Option<crate::semantic_index::EmbeddingModel>,
               vector: Option<&Vec<f32>>| {
        let mut engine = lock(&child.engine);
        let engine = engine.get_or_insert_with(|| ChildEngine::new(session, child));
        let mut params = json!({
            "id": req.id,
            "command": "semantic_search",
            "query": query,
            "top_k": wanted,
            "include_tests": include_tests,
        });
        if scope != session.root() && scope.starts_with(&child.root) {
            params["path"] = json!(scope);
        }
        let child_req: RawRequest = match serde_json::from_value(params) {
            Ok(request) => request,
            Err(error) => return (Err(format!("aft_search: {error}")), lent),
        };
        let lending = lent.is_some();
        if lending {
            *engine.ctx.semantic_embedding_model().lock() = lent;
        }
        let search = || {
            crate::commands::semantic_search::handle_parent_child_search(&child_req, &engine.ctx)
        };
        let response = match vector {
            Some(vector) => crate::commands::semantic_search::with_precomputed_query_vector(
                query,
                vector.clone(),
                search,
            ),
            None => search(),
        };
        let returned = if lending {
            engine.ctx.semantic_embedding_model().lock().take()
        } else {
            None
        };
        if !response.success {
            return (
                Err(format!(
                    "aft_search failed in this repository: {}",
                    response.data["message"].as_str().unwrap_or("unknown error")
                )),
                returned,
            );
        }
        (Ok(response.data), returned)
    };
    let outcomes = if model.is_some() {
        let mut outcomes = Vec::new();
        for child in &children {
            let (outcome, returned) = run(child, model.take(), None);
            model = returned;
            outcomes.push(outcome);
        }
        if let Some(semantic) = semantic.ready() {
            *lock(&semantic.model) = model;
        }
        outcomes
    } else {
        use rayon::prelude::*;
        children
            .par_iter()
            .map(|child| run(child, None, vector.as_ref()).0)
            .collect::<Vec<_>>()
    };

    let mut lists = Vec::new();
    let mut first: Option<Value> = None;
    let mut more_available = false;
    let mut engine_capped = false;
    let mut fully_degraded = true;
    let mut lexical_only_fallback = false;
    let mut bounded_disk_fallback = false;
    let mut semantic_statuses = Vec::new();
    for (child, outcome) in children.iter().zip(outcomes) {
        if session.planes.trigram {
            if let Some(reason) = read(&child.trigram).gap_reason("trigram") {
                gaps.child(child, reason);
            }
        }
        if session.planes.semantic && semantic.ready().is_some() {
            if let Some(reason) = read(&child.semantic).gap_reason("semantic") {
                gaps.child(child, reason);
            }
        }
        let data = match outcome {
            Ok(data) => data,
            Err(reason) => {
                gaps.child(child, reason);
                continue;
            }
        };
        // Without semantic search every child's answer is incomplete for the
        // reason already named once above.
        if data["complete"] == false && semantic.ready().is_some() {
            gaps.child(
                child,
                format!(
                    "aft_search: this repository's answer is incomplete (semantic status: {})",
                    data["semantic_status"].as_str().unwrap_or("unknown")
                ),
            );
        }
        more_available |= data["more_available"].as_bool().unwrap_or(false);
        engine_capped |= data["engine_capped"].as_bool().unwrap_or(false);
        fully_degraded &= data["fully_degraded"].as_bool().unwrap_or(false);
        lexical_only_fallback |= data["lexical_only_fallback"].as_bool().unwrap_or(false);
        bounded_disk_fallback |=
            data["lexical_only_fallback"] == true && data["fully_degraded"] == true;
        if let Some(status) = data["semantic_status"].as_str() {
            semantic_statuses.push(status.to_owned());
        }
        let rows = data["results"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|mut row| {
                prefix_row(&mut row, session.root(), child);
                row
            })
            .collect::<VecDeque<_>>();
        lists.push(rows);
        first.get_or_insert(data);
    }
    gaps.scope(session, &scope);

    let total = lists.iter().map(VecDeque::len).sum::<usize>();
    let merged = merge(lists, wanted);
    more_available |= total > wanted;
    let page = merged.into_iter().skip(offset).collect::<Vec<_>>();
    let envelope = crate::list_envelope::ListEnvelope::new(
        page.len(),
        if more_available {
            crate::list_envelope::Total::AtLeast(offset + page.len() + 1)
        } else {
            crate::list_envelope::Total::Exact(total)
        },
        crate::list_surfaces::search::SEARCH_UNIT,
        if more_available {
            vec![crate::list_envelope::Reason::Cap]
        } else {
            Vec::new()
        },
        crate::list_surfaces::search::SEARCH_NARROW,
    );
    semantic_statuses.sort();
    semantic_statuses.dedup();
    let semantic_status = match semantic_statuses.as_slice() {
        [single] => single.clone(),
        [] => "unavailable".to_string(),
        _ => "partial".to_string(),
    };
    let searched = first.is_some();
    let first = first.unwrap_or(Value::Null);
    let mut text = if searched {
        render_text(query, &page, more_available)
    } else {
        format!("Search unavailable for \"{query}\" in this parent folder's repositories.")
    };
    if bounded_disk_fallback {
        text.push_str(
            "\n\n(Child indexes unavailable: literal matches were read from a bounded disk scan.)",
        );
    }
    let mut body = json!({
        "status": first["status"].as_str().unwrap_or("unavailable"),
        "complete": true,
        "text": text,
        "query": query,
        "include_tests": include_tests,
        "interpreted_as": first["interpreted_as"],
        "query_kind": first["query_kind"],
        "result_count": page.len(),
        "results": page,
        "more_available": more_available,
        "engine_capped": engine_capped,
        // Every child that answered read its files directly; with no child
        // answer at all, nothing was read.
        "fully_degraded": fully_degraded && searched,
        "lexical_only_fallback": lexical_only_fallback,
        "semantic_status": semantic_status,
    });
    if let Some(object) = body.as_object_mut() {
        crate::list_surfaces::search::attach_projected_search_envelope(object, &envelope);
    }
    attach_gaps(&mut body, gaps.into_values());
    Response::success(&req.id, body)
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::*;
    use super::*;
    use crate::blob_store::v2::{ContentHash, FamilyPlane};
    use crate::views::contracts::{PlaneAdapter, ViewAccess};
    use crate::views::manifest_v2::{EntryPlanes, EntryV2, ManifestHeader, ManifestV2, Producers};
    use crate::views::readiness::PlaneState;
    use crate::views::semantic::{FillBudget, SemanticPlane, SemanticProducer};
    use crate::views::snapshot::{LiveDelta, OpenGeneration};
    use crate::views::RelPath;

    pub(crate) fn session(root: &Path, storage: &Path) -> ParentSession {
        ParentSession {
            root: root.to_path_buf(),
            storage: storage.to_path_buf(),
            planes: RequestedPlanes {
                trigram: true,
                semantic: true,
                callgraph: false,
            },
            semantic_config: Default::default(),
            trigram_policy: crate::blob_store::v2::TrigramPolicy {
                max_file_size: crate::config::Config::default().search_index_max_file_size,
            },
            view_routes: Default::default(),
            discovery: RwLock::new(None),
            children: RwLock::new(Vec::new()),
            semantic: RwLock::new(Plane::Loading),
            holders: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            rounds: AtomicUsize::new(0),
            wake: (Mutex::new(false), Condvar::new()),
            pointer_interval: Duration::from_secs(1),
            backstop_interval: Duration::from_secs(1),
        }
    }

    #[test]
    fn artifact_read_parent_search() {
        crate::access_policy_tests::assert_artifact_read_policy(
            |req, ctx| {
                let config = ctx.config();
                let parent = session(
                    config.project_root.as_deref().unwrap(),
                    config.storage_dir.as_deref().unwrap(),
                );
                drop(config);
                search(req, ctx, &parent)
            },
            "semantic_search",
            |path| serde_json::json!({"query": "artifact", "path": path}),
        );
    }

    #[test]
    fn semantic_fill_refreshes_parent_engine_within_one_generation() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let bytes = b"pub fn alpha() -> u32 { 42 }";
        std::fs::write(root.path().join("a.rs"), bytes).unwrap();
        let producer =
            SemanticProducer::current("model", crate::semantic_index::EmbedTextCaps::default());
        let plane = Arc::new(SemanticPlane::new(
            storage.path().to_path_buf(),
            producer.clone(),
        ));
        let registry =
            crate::views::registry::FamilyRegistry::open(storage.path(), "family").unwrap();
        let owner = registry
            .register_view_with_storage(registry.storage(), "child", root.path())
            .unwrap();
        let access = ViewAccess::Owner(owner.clone());
        let mut manifest = ManifestV2::new(ManifestHeader {
            producers: Producers {
                trigram: "test".into(),
                semantic: Some(producer.id()),
                callgraph: "test".into(),
            },
            head_tree: None,
            ignore_fingerprint: None,
            segment: None,
        });
        manifest
            .insert(
                RelPath::new(b"a.rs".to_vec()).unwrap(),
                EntryV2::regular(
                    ContentHash::of(bytes),
                    bytes.len() as u64,
                    EntryPlanes {
                        semantic: Some(PlaneState::pending("queued")),
                        trigram: None,
                        callgraph: None,
                    },
                ),
            )
            .unwrap();
        let generation = Arc::new(OpenGeneration::new("same-generation", manifest, None));
        plane.open_generation(&access, &generation).unwrap();
        let snapshot = LiveDelta::new(generation.clone()).snapshot();
        let session = session(root.path(), storage.path());
        *write(&session.semantic) = Plane::Ready(Arc::new(ParentSemantic {
            model: Mutex::new(None),
            plane: plane.clone(),
            producer_id: producer.id(),
        }));
        let child = Child::new(root.path(), root.path().to_path_buf());
        *write(&child.semantic) = Plane::Ready(Arc::new(SemanticChild {
            access,
            generation,
            snapshot: snapshot.clone(),
        }));
        let mut engine = ChildEngine::new(&session, &child);
        engine.install_semantic(&session, &child);
        assert_eq!(read(engine.ctx.semantic_index()).as_ref().unwrap().len(), 0);
        let report = plane
            .fill(
                &owner,
                &snapshot,
                FillBudget::default(),
                &mut |texts| Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect()),
                &|| snapshot.clone(),
            )
            .unwrap();
        assert_eq!(report.embedded_keys, 1);
        engine.install_semantic(&session, &child);
        assert!(read(engine.ctx.semantic_index()).as_ref().unwrap().len() > 0);
        assert_eq!(
            child.semantic_generation().as_deref(),
            Some("same-generation")
        );
        assert_eq!(
            engine
                .ctx
                .search_index()
                .read()
                .unwrap()
                .as_ref()
                .map(|index| index.file_count()),
            None
        );
        assert_eq!(plane.plane(), FamilyPlane::Semantic);
    }

    #[test]
    fn trigram_engine_retains_index_identity_across_retirement() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let session = session(root.path(), storage.path());
        let child = Child::new(root.path(), root.path().to_path_buf());
        let mut engine = ChildEngine::new(&session, &child);
        let old = Arc::new(crate::search_index::SearchIndex::new());
        let retired = Arc::downgrade(&old);
        engine.install_trigram_index(&old);
        drop(old);
        assert!(
            retired.upgrade().is_some(),
            "the installed allocation must remain alive"
        );
        let mut replacement = crate::search_index::SearchIndex::new();
        replacement.index_file(&root.path().join("new.rs"), b"new_unique_term");
        engine.install_trigram_index(&Arc::new(replacement));
        assert_eq!(
            read(engine.ctx.search_index())
                .as_ref()
                .unwrap()
                .file_count(),
            1
        );
        assert!(
            retired.upgrade().is_none(),
            "replacement releases the retired identity"
        );
    }
}
