//! Shared local-path and remote-document resolution for read-only commands.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use crate::secret::SecretVault;
use crate::url_fetch::{
    fetch_url_to_cache, is_http_url, SecretFetch, UrlFetchError, UrlFetchOptions, URL_REPLY_MARGIN,
};

/// Refusal for a placeholder fetch made outside an outline or zoom call, which
/// would have no call end to delete its private document at.
const SECRET_FETCH_NEEDS_CALL: &str =
    "Secret placeholder URLs can only be fetched with aft_outline or aft_zoom; nothing was fetched.";

pub(super) fn resolve_file_or_url(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
) -> Result<PathBuf, Response> {
    resolve_with_fetch(req, ctx, file, fetch_url_to_cache)
}

/// Trusted configuration sets the private-network ceiling; a request can
/// narrow that permission but cannot grant it.
fn fetch_options(req: &RawRequest, ctx: &AppContext) -> UrlFetchOptions {
    let allow_private = ctx.config().url_fetch_allow_private
        && req
            .params
            .get("allow_private")
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
    UrlFetchOptions {
        allow_private,
        ..Default::default()
    }
}

fn resolve_with_fetch(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
    fetch: impl FnOnce(&str, &Path, UrlFetchOptions) -> Result<PathBuf, UrlFetchError>,
) -> Result<PathBuf, Response> {
    if is_http_url(file) {
        // A URL that mentions a secret placeholder never touches the URL
        // cache: not even while the cache location is unavailable.
        if let Some(labels) = crate::url_fetch::secret_labels_in_url(file) {
            return resolve_secret_fetch(req, ctx, file, &labels);
        }
        let storage_dir = ctx.cache_dir();
        let options = fetch_options(req, ctx);
        // While the cache location is unavailable (its volume is missing, or
        // the user config that names it is unreadable), `cache_dir()` falls
        // back to durable storage, which caches must not fill. Serve a fresh
        // copy already cached there, and refuse to download anything new.
        let fetched = match ctx.cache_unavailable_reason() {
            Some(reason) => {
                match crate::url_fetch::cached_url_document(file, &storage_dir, &options) {
                    Ok(Some(cached)) => Ok(cached),
                    Ok(None) => {
                        return Err(Response::error(
                            &req.id,
                            crate::root_cache::CACHE_DIR_UNAVAILABLE,
                            format!(
                                "URL download refused while the cache location is unavailable: {reason}"
                            ),
                        ))
                    }
                    Err(error) => Err(error),
                }
            }
            None => fetch(file, &storage_dir, options),
        };
        return fetched
            .map(|path| {
                // The download stopped early enough to leave time for reading the
                // document back. Those reads must not use the reply margin too: a
                // stalled read then fails while the reply still reaches the caller
                // before the call's deadline.
                crate::bounded_io::keep_reply_margin(URL_REPLY_MARGIN);
                path
            })
            .map_err(|error| url_fetch_error_response(&req.id, &error));
    }
    ctx.validate_read_path(&req.id, req.session(), Path::new(file))
}

/// A failed fetch keeps its typed code; a timeout also names its phase, and
/// a failure repeated from memory says so and when a new fetch is possible.
fn url_fetch_error_response(id: &str, error: &UrlFetchError) -> Response {
    let mut data = serde_json::Map::new();
    if let Some(phase) = error.phase() {
        data.insert("phase".into(), phase.as_str().into());
    }
    if let Some(retry_after) = error.retry_after() {
        data.insert("cached".into(), true.into());
        data.insert(
            "retry_after_ms".into(),
            (retry_after.as_millis() as u64).into(),
        );
    }
    if data.is_empty() {
        return Response::error(id, error.code(), error.to_string());
    }
    Response::error_with_data(
        id,
        error.code(),
        error.to_string(),
        serde_json::Value::Object(data),
    )
}

/// Resolve a URL that mentions a secret placeholder (`labels` are the
/// well-formed ones it holds).
///
/// The order matters. A cache location that is unavailable refuses first, as
/// a plain fetch's would, but without serving a cached copy. Then every label
/// is resolved under the call's daemon-stamped authorization scope: an
/// ineligible scope refuses before the vault is asked, and the first label
/// that is pending consent or refused ends the call before anything is
/// fetched. Only then is the document fetched into a private folder outside
/// the URL cache, which the enclosing [`FetchTempScope`] deletes when the
/// call returns.
fn resolve_secret_fetch(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
    labels: &[String],
) -> Result<PathBuf, Response> {
    let vault = fetch_vault();
    if let Some(reason) = ctx.cache_unavailable_reason() {
        return Err(secret_fetch_refusal(
            &req.id,
            vault.as_ref(),
            labels,
            crate::root_cache::CACHE_DIR_UNAVAILABLE,
            &format!("URL download refused while the cache location is unavailable: {reason}"),
        ));
    }
    if !FetchTempScope::active() {
        return Err(secret_fetch_refusal(
            &req.id,
            vault.as_ref(),
            labels,
            "url_fetch_failed",
            SECRET_FETCH_NEEDS_CALL,
        ));
    }
    #[allow(unused_mut)]
    let mut options = fetch_options(req, ctx);
    #[cfg(any(test, feature = "test-support"))]
    TEST_FETCH_OPTIONS.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook(&mut options);
        }
    });

    let mut authorization = None;
    if let Some(first) = labels.first() {
        let scopes = match crate::context::call_context::current_call_scopes() {
            Ok((scopes, _)) => scopes,
            Err(reason) => {
                let outcome = crate::secret::refuse_ineligible(first, reason);
                return Err(secret_outcome_refusal(
                    &req.id,
                    vault.as_ref(),
                    labels,
                    outcome,
                ));
            }
        };
        authorization = Some(scopes.authorization);
    }

    let document_dir = crate::url_fetch::create_secret_fetch_dir(&ctx.cache_dir())
        .map_err(|error| url_fetch_error_response(&req.id, &error))?;
    // Owned by the enclosing call from here on, so every return below,
    // failures included, leaves the folder to be deleted at the call's end.
    FetchTempScope::adopt(document_dir.clone());
    let secrets = SecretFetch::new(
        vault.clone(),
        authorization,
        labels.to_vec(),
        {
            #[cfg(any(test, feature = "test-support"))]
            {
                // Only an installed fake vault maps a fixture's ephemeral port to HTTPS/443.
                if TEST_FETCH_VAULT.with(|slot| slot.borrow().is_some()) {
                    url::Url::parse(file).ok().and_then(|url| url.port())
                } else {
                    None
                }
            }
            #[cfg(not(any(test, feature = "test-support")))]
            {
                None
            }
        },
        req.id.clone(),
        crate::url_fetch::SecretFetchCancellation::for_current_call(&options),
    );
    let document = crate::url_fetch::fetch_url_with_secrets(file, &document_dir, options, &secrets)
        .map_err(|error| url_fetch_error_response(&req.id, &error))?;
    #[cfg(any(test, feature = "test-support"))]
    TEST_DOCUMENT_OBSERVER.with(|observer| {
        if let Some(observer) = observer.borrow().as_ref() {
            observer(&document);
        }
    });
    crate::bounded_io::keep_reply_margin(URL_REPLY_MARGIN);
    Ok(document)
}

/// The same per-call vault factory bash uses: the live scoped subc transport
/// under AFT's daemon identity, or UnavailableVault without a usable daemon.
fn fetch_vault() -> Arc<dyn SecretVault> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(vault) = TEST_FETCH_VAULT.with(|slot| slot.borrow().clone()) {
        return vault;
    }
    crate::bash_background::secrets::vault()
}

fn secret_outcome_refusal(
    id: &str,
    vault: &dyn SecretVault,
    labels: &[String],
    outcome: crate::secret::LabelOutcome,
) -> Response {
    match outcome {
        crate::secret::LabelOutcome::Refuse { code, message } => {
            secret_fetch_refusal(id, vault, labels, code, &message)
        }
        // Only a refusal ends a launch here; anything else reaching this point
        // is still not a grant.
        _ => secret_fetch_refusal(
            id,
            vault,
            labels,
            crate::secret::SECRET_REFUSED,
            crate::url_fetch::SECRET_FETCH_REFUSED,
        ),
    }
}

/// Refuse a placeholder fetch before any request is sent, with one audit
/// record per scanned label naming the call's scope.
fn secret_fetch_refusal(
    id: &str,
    vault: &dyn SecretVault,
    labels: &[String],
    code: &str,
    message: &str,
) -> Response {
    let scope = crate::context::call_context::current_secret_audit_scope()
        .map(|scope| scope.to_scope_ref());
    for label in labels {
        let record = secret_rules::audit::AuditRecordV1::new(
            crate::secret::now_ms(),
            label,
            secret_rules::audit::Decision::Refused,
            secret_rules::audit::AuditReason::Refused,
            id,
            id,
        )
        .with_scope(scope.clone());
        let _ = vault.audit(&record);
    }
    Response::error(id, code, message)
}

thread_local! {
    /// Private documents placeholder fetches wrote during the current outline
    /// or zoom call, and how many [`FetchTempScope`]s are open on this thread.
    static FETCH_TEMP: RefCell<(usize, Vec<PathBuf>)> = const { RefCell::new((0, Vec::new())) };
}

/// Keeps the private documents of placeholder fetches for exactly one outline
/// or zoom call.
///
/// A placeholder fetch hands its caller the path of a scrubbed document in a
/// private folder, which the caller reads until it builds its reply. The
/// command handler opens one of these scopes first; when it drops, on every
/// return path, the folders registered while it was open are deleted. Scopes
/// nest: an inner one leaves the documents to the outermost, because the
/// outer call may still be reading them. A placeholder fetch with no scope
/// open is refused, so no document is ever left for a later call to clean up.
pub(super) struct FetchTempScope {
    /// Thread-local state: the scope must drop on the thread that opened it.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl FetchTempScope {
    pub(super) fn enter() -> Self {
        FETCH_TEMP.with(|state| state.borrow_mut().0 += 1);
        Self {
            _not_send: std::marker::PhantomData,
        }
    }

    fn active() -> bool {
        FETCH_TEMP.with(|state| state.borrow().0 > 0)
    }

    /// Hand `dir` to the open scope. Callers check [`Self::active`] first; a
    /// folder adopted with no scope open is deleted at once.
    fn adopt(dir: PathBuf) {
        let orphan = FETCH_TEMP.with(|state| {
            let mut state = state.borrow_mut();
            if state.0 == 0 {
                return Some(dir);
            }
            state.1.push(dir);
            None
        });
        if let Some(dir) = orphan {
            remove_fetch_dir(&dir);
        }
    }
}

impl Drop for FetchTempScope {
    fn drop(&mut self) {
        let finished = FETCH_TEMP.with(|state| {
            let mut state = state.borrow_mut();
            state.0 = state.0.saturating_sub(1);
            if state.0 == 0 {
                std::mem::take(&mut state.1)
            } else {
                Vec::new()
            }
        });
        for dir in finished {
            remove_fetch_dir(&dir);
        }
    }
}

/// Delete one placeholder fetch's private folder. Deletion is best effort:
/// nothing retries a failure later, so a failure is logged as a warning that
/// names the folder left on disk, for an operator to remove.
fn remove_fetch_dir(dir: &Path) {
    if let Err(error) = std::fs::remove_dir_all(dir) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log::warn!(
                "placeholder fetch cleanup failed: the private folder {} and its scrubbed document remain on disk and must be removed by hand ({error})",
                dir.display()
            );
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
type FetchOptionsHook = Box<dyn Fn(&mut UrlFetchOptions)>;
#[cfg(any(test, feature = "test-support"))]
type DocumentObserver = Box<dyn Fn(&Path)>;

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static TEST_FETCH_VAULT: RefCell<Option<Arc<dyn SecretVault>>> = const { RefCell::new(None) };
    /// Adjusts a placeholder fetch's options (host overrides, test TLS roots).
    static TEST_FETCH_OPTIONS: RefCell<Option<FetchOptionsHook>> = const { RefCell::new(None) };
    /// Sees each placeholder fetch's document path while the call still runs.
    static TEST_DOCUMENT_OBSERVER: RefCell<Option<DocumentObserver>> = const { RefCell::new(None) };
}

/// Run a handler with a fake vault and fixed fetch options for this thread.
#[cfg(all(unix, any(test, feature = "test-support")))]
pub fn with_secret_fetch_for_test<T>(
    vault: Arc<dyn SecretVault>,
    options: UrlFetchOptions,
    run: impl FnOnce() -> T,
) -> T {
    with_fetch_hooks(
        vault,
        Box::new(move |current| *current = options.clone()),
        None,
        run,
    )
}

/// Run a handler with a fake vault, adjusting its options and observing each private document.
#[cfg(any(test, feature = "test-support"))]
pub fn with_secret_fetch_test_hooks<T>(
    vault: Arc<dyn SecretVault>,
    configure: impl Fn(&mut UrlFetchOptions) + 'static,
    observe: impl Fn(&Path) + 'static,
    run: impl FnOnce() -> T,
) -> T {
    with_fetch_hooks(vault, Box::new(configure), Some(Box::new(observe)), run)
}

#[cfg(any(test, feature = "test-support"))]
fn with_fetch_hooks<T>(
    vault: Arc<dyn SecretVault>,
    configure: FetchOptionsHook,
    observe: Option<DocumentObserver>,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(
        Option<Arc<dyn SecretVault>>,
        Option<FetchOptionsHook>,
        Option<Option<DocumentObserver>>,
    );
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_FETCH_VAULT.with(|slot| slot.replace(self.0.take()));
            TEST_FETCH_OPTIONS.with(|slot| slot.replace(self.1.take()));
            if let Some(previous) = self.2.take() {
                TEST_DOCUMENT_OBSERVER.with(|slot| slot.replace(previous));
            }
        }
    }
    let _restore = Restore(
        TEST_FETCH_VAULT.with(|slot| slot.replace(Some(vault))),
        TEST_FETCH_OPTIONS.with(|slot| slot.replace(Some(configure))),
        observe.map(|hook| TEST_DOCUMENT_OBSERVER.with(|slot| slot.replace(Some(hook)))),
    );
    run()
}

#[cfg(test)]
mod url_tests {
    use super::*;

    /// A context whose cache location is unknown because the user config was
    /// unreadable; `cache_dir()` then names the durable storage root.
    fn cache_unavailable_ctx(storage: &Path) -> AppContext {
        crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.to_path_buf()),
                cache_config_unknown: Some("user config is not valid JSONC".into()),
                ..Default::default()
            },
        )
    }

    fn outline_request(url: &str) -> RawRequest {
        serde_json::from_value(serde_json::json!({
            "id": "cache-off-url",
            "command": "outline",
            "file": url,
        }))
        .unwrap()
    }

    /// With `storage.cache_dir` set, every URL cache access goes through the
    /// cache root: the download, the fresh-cache lookup and the remembered
    /// failure check all run on the folder handed to the fetch, and a
    /// remembered failure is keyed by it. None may read or write under the
    /// durable storage root.
    #[test]
    fn url_fetches_and_their_cache_checks_use_the_cache_root() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let cache_parent = temp.path().join("cache-volume");
        std::fs::create_dir(&cache_parent).unwrap();
        let mut config = crate::config::Config {
            storage_dir: Some(storage.clone()),
            cache_dir: Some(cache_parent),
            ..Default::default()
        };
        assert!(
            aft_engine::test_bridge::display_data::reader_floor::prepare_cache_root_for_test(
                &mut config
            )
            .is_none()
        );
        let ctx = crate::test_context(Box::new(crate::parser::TreeSitterProvider::new()), config);
        assert!(ctx.cache_unavailable_reason().is_none());
        let cache_root = ctx.cache_dir();
        assert_ne!(cache_root, storage);
        let url = "https://93.184.216.34/docs.md";
        let document = cache_root.join("url_cache").join("fetched.md");
        let resolved = resolve_with_fetch(&outline_request(url), &ctx, url, |_, folder, _| {
            assert_eq!(
                folder,
                cache_root.as_path(),
                "URL cache outside the cache root"
            );
            Ok(document.clone())
        })
        .expect("the fetch succeeded");
        assert_eq!(resolved, document);
    }

    #[test]
    fn url_download_is_refused_without_writing_while_the_cache_location_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let ctx = cache_unavailable_ctx(&storage);
        assert!(ctx.cache_unavailable_reason().is_some());
        // An address literal keeps the check off the network.
        let url = "https://93.184.216.34/docs.md";
        let refused = resolve_with_fetch(&outline_request(url), &ctx, url, |_, _, _| {
            panic!("a download started while the cache location is unavailable")
        })
        .expect_err("a fresh download must be refused");
        assert_eq!(
            refused.data["code"],
            crate::root_cache::CACHE_DIR_UNAVAILABLE
        );
        let message = refused.data["message"].as_str().unwrap();
        assert!(message.contains("cache_dir_config_unknown"), "{message}");
        assert_eq!(ctx.cache_dir(), storage);
        assert!(
            !storage.join("url_cache").exists(),
            "the refusal wrote a URL cache under durable storage"
        );
    }

    #[test]
    fn url_already_cached_still_serves_while_the_cache_location_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let ctx = cache_unavailable_ctx(&storage);
        let url = "https://93.184.216.34/docs.md";
        let cache = ctx.cache_dir();
        let content = crate::url_fetch::cache_content_path_for_url(&cache, url, ".md");
        std::fs::create_dir_all(content.parent().unwrap()).unwrap();
        std::fs::write(&content, "# cached").unwrap();
        let fetched_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        std::fs::write(
            crate::url_fetch::cache_meta_path_for_url(&cache, url),
            serde_json::json!({
                "url": url,
                "contentType": "text/markdown",
                "extension": ".md",
                "fetchedAt": fetched_at,
                "downloaded_bytes": 8,
            })
            .to_string(),
        )
        .unwrap();
        let served = resolve_with_fetch(&outline_request(url), &ctx, url, |_, _, _| {
            panic!("a download started while the cache location is unavailable")
        })
        .expect("the cached copy must still serve");
        assert_eq!(served, content);
    }

    #[test]
    fn private_fetch_permission_cannot_widen_trusted_ceiling() {
        for trusted in [false, true] {
            let ctx = crate::test_context(
                Box::new(crate::parser::TreeSitterProvider::new()),
                crate::config::Config {
                    url_fetch_allow_private: trusted,
                    ..Default::default()
                },
            );
            for requested in [None, Some(false), Some(true)] {
                let mut value = serde_json::json!({"id": "ceiling", "command": "outline"});
                if let Some(requested) = requested {
                    value["allow_private"] = requested.into();
                }
                let req = serde_json::from_value(value).unwrap();
                let resolved = resolve_with_fetch(
                    &req,
                    &ctx,
                    "https://example.com/doc.md",
                    |_, _, options| {
                        assert_eq!(options.allow_private, trusted && requested.unwrap_or(true));
                        Ok(PathBuf::from("cached.md"))
                    },
                );
                assert!(resolved.is_ok());
            }
        }
    }

    #[test]
    fn a_fetched_document_is_read_back_without_using_the_reply_margin() {
        let storage = tempfile::tempdir().unwrap();
        let document = storage.path().join("fetched.md");
        let ctx = crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.path().to_path_buf()),
                ..Default::default()
            },
        );
        let req =
            serde_json::from_value(serde_json::json!({"id": "slow-fetch", "command": "outline"}))
                .unwrap();
        let budget = crate::bounded_io::Budget::after(std::time::Duration::from_secs(10));
        let call_deadline = budget.deadline;
        let _call = crate::bounded_io::enter(Some(budget));
        let fetched = || {
            resolve_with_fetch(&req, &ctx, "https://example.com/doc.md", |_, _, _| {
                std::fs::write(&document, "# Title\n").unwrap();
                Ok(document.clone())
            })
            .expect("the fetch succeeded")
        };
        let path = fetched();
        assert_eq!(
            crate::bounded_io::read_to_string(&path).unwrap(),
            "# Title\n"
        );
        let read_back_deadline = |budget: Option<crate::bounded_io::Budget>| {
            budget.expect("the call budget is still in force").deadline
        };
        assert_eq!(
            read_back_deadline(crate::bounded_io::current()),
            call_deadline - URL_REPLY_MARGIN
        );
        // A second document in the same call keeps the one margin.
        fetched();
        assert_eq!(
            read_back_deadline(crate::bounded_io::current()),
            call_deadline - URL_REPLY_MARGIN
        );
    }

    #[test]
    fn untrusted_private_flag_is_refused_by_outline_and_zoom() {
        for (trusted, requested) in [(false, true), (true, false)] {
            let ctx = crate::test_context(
                Box::new(crate::parser::TreeSitterProvider::new()),
                crate::config::Config {
                    url_fetch_allow_private: trusted,
                    ..Default::default()
                },
            );
            for command in ["outline", "zoom"] {
                let req = serde_json::from_value(serde_json::json!({"id": "private-denied", "command": command, "file": "http://127.0.0.1:0/doc.md", "symbol": "Heading", "allow_private": requested})).unwrap();
                let response = if command == "outline" {
                    super::super::outline::handle_outline(&req, &ctx)
                } else {
                    super::super::zoom::handle_zoom(&req, &ctx)
                };
                assert!(!response.success);
                assert_eq!(response.data["code"], "url_fetch_failed");
                assert!(
                    response.data["message"]
                        .as_str()
                        .unwrap()
                        .to_ascii_lowercase()
                        .contains("private"),
                    "refusal must be the network policy, not a failed connection: {:?}",
                    response.data
                );
            }
        }
    }

    mod secret_fetch {
        use super::super::*;

        #[test]
        fn a_placeholder_fetch_outside_an_outline_or_zoom_call_is_refused() {
            let root = tempfile::tempdir().unwrap();
            let ctx = crate::test_context(
                Box::new(crate::parser::TreeSitterProvider::new()),
                crate::config::Config {
                    project_root: Some(root.path().to_path_buf()),
                    storage_dir: Some(root.path().join("storage")),
                    ..Default::default()
                },
            );
            let url = "https://93.184.216.34/v1/doc.md?key={{ck-secret:test}}";
            let req = super::outline_request(url);
            let refused = resolve_with_fetch(&req, &ctx, url, |_, _, _| {
                panic!("a placeholder URL reached the cached fetch")
            })
            .expect_err("no call scope is open");
            assert_eq!(
                refused.data["message"],
                "Secret placeholder URLs can only be fetched with aft_outline or aft_zoom; nothing was fetched."
            );
            assert!(!ctx
                .cache_dir()
                .join(crate::url_fetch::SECRET_FETCH_DIR)
                .exists());
        }

        #[test]
        fn nested_scopes_keep_documents_until_the_outermost_drops() {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().join("doc");
            std::fs::create_dir(&dir).unwrap();
            let outer = FetchTempScope::enter();
            {
                let _inner = FetchTempScope::enter();
                FetchTempScope::adopt(dir.clone());
            }
            assert!(
                dir.exists(),
                "an inner scope deleted a document the outer call uses"
            );
            drop(outer);
            assert!(!dir.exists());
            let orphan = temp.path().join("orphan");
            std::fs::create_dir(&orphan).unwrap();
            FetchTempScope::adopt(orphan.clone());
            assert!(
                !orphan.exists(),
                "a folder adopted with no scope must not be kept"
            );
        }
    }
}
