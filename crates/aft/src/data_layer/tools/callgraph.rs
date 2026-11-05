//! The call-graph commands' reads from the data layer: `callers`, `impact`,
//! `call_tree`, `trace_to`, `trace_to_symbol` and `trace_data` answer from
//! the checkout's view when the data layer is on, through the same result
//! builders that answer from today's store.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::callgraph_store::{CallGraphStore, CallGraphStoreError, EitherGraph};
use crate::context::AppContext;
use crate::data_layer::checkout::{CheckoutView, PlaneStatus};
use crate::data_layer::query::{SliceReader, SliceSource};

/// How long a call-graph command waits for recorded edits to be resolved
/// before it answers that the call graph is still building, the same bound
/// reads of today's views use.
const READ_WAIT: Duration = Duration::from_secs(3);

/// The data layer's call-graph reader.
pub(crate) type DataGraph = SliceReader<Arc<dyn SliceSource + Send + Sync>>;

/// The reader a call-graph command answers from.
pub(crate) type OpsGraph = EitherGraph<CallGraphStore, DataGraph>;

/// What the data layer offers a call-graph command.
pub(crate) enum Access {
    /// The data layer is off for this root: answer from today's store.
    Off,
    /// Edits are still being resolved after the wait.
    Building,
    /// Answer from this reader. Keep the view until the answer is built: it
    /// holds the publication the reader reads from.
    Ready(DataGraph, CheckoutView),
}

/// The data layer's call graph for `ctx`'s root, once no recorded edit
/// waits for it, or [`Access::Building`] after [`READ_WAIT`].
pub(crate) fn access(ctx: &AppContext) -> Access {
    let Some(checkout) = ctx.data_checkout() else {
        return Access::Off;
    };
    checkout.request_refresh();
    let deadline = Instant::now() + READ_WAIT;
    loop {
        if let Some(view) = checkout.view() {
            if view.status().callgraph == PlaneStatus::Ready {
                return Access::Ready(view.callgraph(), view);
            }
        }
        if Instant::now() >= deadline {
            return Access::Building;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether the call graph in `view` covers `file`, a path inside the
/// checkout (absolute or relative to its root).
pub(crate) fn covers(view: &CheckoutView, file: &Path) -> bool {
    relative(view.root(), file).is_some_and(|path| view.callgraph_has_file(&path))
}

/// The error to answer with when a read of the call graph failed (a
/// checksum, a malformed record): such a file was seen as absent, so the
/// answer cannot be trusted.
pub(crate) fn read_failure(view: &CheckoutView) -> Option<CallGraphStoreError> {
    let failures = view.callgraph_failures();
    (!failures.is_empty()).then(|| {
        CallGraphStoreError::Unavailable(format!(
            "the data layer could not read part of the call graph: {}",
            failures.join("; ")
        ))
    })
}

fn relative(root: &Path, file: &Path) -> Option<String> {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = canonical(root);
    let absolute = if file.is_relative() {
        root.join(file)
    } else {
        canonical(file)
    };
    let relative = absolute.strip_prefix(&root).ok()?;
    Some(relative.to_str()?.replace('\\', "/"))
}

/// The reader a call-graph command answers from: the data layer's when it is
/// on for `ctx`'s root, otherwise a pinned snapshot of today's store. Each
/// of `files` must be in the call graph. The view, when the data layer
/// answers, must stay alive until the answer is built; pass it to
/// [`failed_reads`] afterwards.
///
/// # Errors
///
/// The refusal to send: the call graph is building, off or failed, or a
/// file is not in it.
pub(crate) fn ops_graph(
    req_id: &str,
    operation: &str,
    ctx: &AppContext,
    files: &[&Path],
) -> Result<(OpsGraph, Option<CheckoutView>), crate::protocol::Response> {
    use crate::commands::callgraph_store_adapter::{
        ensure_target_indexed, index_refusal_response, not_indexed_response, store_error_response,
    };
    use crate::context::CallgraphStoreAccess;
    match access(ctx) {
        Access::Building => Err(index_refusal_response(
            req_id,
            operation,
            ctx,
            &CallgraphStoreAccess::Building,
        )),
        Access::Ready(reader, view) => {
            for file in files {
                if !covers(&view, file) {
                    let path = if file.is_relative() {
                        view.root().join(file)
                    } else {
                        file.to_path_buf()
                    };
                    return Err(not_indexed_response(req_id, operation, ctx, &path));
                }
            }
            Ok((EitherGraph::Data(reader), Some(view)))
        }
        Access::Off => {
            let store = match ctx.callgraph_store_for_ops() {
                CallgraphStoreAccess::Ready(store) => store,
                CallgraphStoreAccess::Error(error) => {
                    return Err(store_error_response(req_id, operation, error))
                }
                other => return Err(index_refusal_response(req_id, operation, ctx, &other)),
            };
            for file in files {
                ensure_target_indexed(req_id, operation, ctx, &store, file)?;
            }
            // A pinned snapshot keeps the generation's files until the
            // answer is built.
            let snapshot = store
                .read_snapshot()
                .map_err(|error| store_error_response(req_id, operation, error))?;
            Ok((EitherGraph::Store(snapshot), None))
        }
    }
}

/// The error to send instead of an answer built while a read of the data
/// layer's call graph failed: such a file was seen as absent.
pub(crate) fn failed_reads(
    req_id: &str,
    operation: &str,
    view: Option<&CheckoutView>,
) -> Option<crate::protocol::Response> {
    let error = read_failure(view?)?;
    Some(crate::commands::callgraph_store_adapter::store_error_response(req_id, operation, error))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use crate::context::AppContext;
    use crate::protocol::{RawRequest, Response};

    fn write(root: &Path, path: &str, text: &str) -> PathBuf {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn callers(ctx: &AppContext, file: &Path, symbol: &str) -> Response {
        let request = RawRequest {
            id: "data-layer-callers".into(),
            command: "callers".into(),
            lsp_hints: None,
            session_id: None,
            params: serde_json::json!({"file": file, "symbol": symbol}),
        };
        crate::commands::callers::handle_callers(&request, ctx)
    }

    /// Asks until the answer is no longer "still building", for up to 60 s.
    fn settled(ask: impl Fn() -> Response) -> Response {
        let started = Instant::now();
        loop {
            let response = ask();
            if response.data["code"] != "callgraph_building" {
                return response;
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "never stopped building"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn callers_answers_from_the_data_layer_and_follows_an_edit() {
        crate::data_layer::runtime::force_on_for_test();
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        write(&root, "Cargo.toml", "[package]\nname = \"demo\"\n");
        write(
            &root,
            "src/lib.rs",
            "mod util;\npub fn caller() { util::callee(); }\n",
        );
        let util = write(&root, "src/util.rs", "pub fn callee() {}\n");
        let readme = write(&root, "README.md", "callee\n");
        let mut config = crate::config::Config::default();
        config.project_root = Some(root.clone());
        config.storage_dir = Some(storage.path().to_path_buf());
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        ctx.set_canonical_cache_root(root.clone());

        let first = settled(|| callers(&ctx, &util, "callee"));
        assert!(first.success, "{:?}", first.data);
        let text = first.data.to_string();
        assert!(
            text.contains("src/lib.rs") && text.contains("caller"),
            "{text}"
        );
        assert!(!text.contains("other"), "{text}");

        // An edit adds a caller in a new file; the watcher reports it.
        let extra = write(
            &root,
            "src/extra.rs",
            "use crate::util;\npub fn other() { util::callee(); }\n",
        );
        let lib = write(
            &root,
            "src/lib.rs",
            "mod util;\nmod extra;\npub fn caller() { util::callee(); }\n",
        );
        ctx.data_note_paths(&[extra, lib]);
        let after = settled(|| callers(&ctx, &util, "callee"));
        assert!(after.success, "{:?}", after.data);
        let text = after.data.to_string();
        assert!(
            text.contains("src/extra.rs") && text.contains("other"),
            "{text}"
        );

        // A file the call graph does not parse gets today's refusal.
        let refused = callers(&ctx, &readme, "callee");
        assert_eq!(refused.data["code"], "not_indexed", "{:?}", refused.data);
    }
}
