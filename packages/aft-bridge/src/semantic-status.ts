/**
 * How the AFT daemon's semantic-index status words are read and rendered.
 *
 * Both plugin hosts show this status, and both used to carry their own copy of
 * the formatter. The copies drifted the moment one was fixed: the defect where
 * a dead index rendered as "Rebuilding (model changed)" was repaired in one
 * host and stayed live in the other. One implementation here, imported by both,
 * is what keeps that from happening a third time.
 */

/**
 * Opening words of every missing-runtime message the daemon produces
 * (`ONNX_RUNTIME_MISSING_PREFIX` in crates/aft/src/semantic_index.rs). It
 * arrives wrapped in other text — as a build stage, as a status error, inside a
 * search reply — so callers match it anywhere in the string.
 */
const MISSING_ONNX_RUNTIME_MARKER = "ONNX Runtime not found.";

/**
 * What a reader with no ONNX Runtime needs: the fact, and the one command that
 * fixes it. `doctor --fix` is also the command that knows whether AFT can
 * download the runtime for this platform — that question has a single owner
 * (`isOrtAutoDownloadSupported` in this same package), so neither the daemon
 * nor a plugin keeps its own copy of the platform table to guess at brew/apt
 * advice.
 */
const MISSING_ONNX_RUNTIME_LABEL =
  "unavailable — ONNX Runtime missing (npx @cortexkit/aft doctor --fix)";

const REBUILDING_LABEL = "Rebuilding (model changed)";

/**
 * Build stage the daemon reports while it waits for the plugin to finish
 * downloading ONNX Runtime (`WAITING_FOR_ONNX_RUNTIME_DOWNLOAD_STAGE` in
 * crates/aft/src/semantic_index.rs). The runtime is absent at that moment, but
 * it is on its way and the index builds as soon as it lands, so the reader is
 * told to wait rather than to run `doctor --fix`.
 */
const WAITING_FOR_ONNX_RUNTIME_DOWNLOAD_STAGE = "waiting_for_onnx_runtime_download";
const WAITING_FOR_ONNX_RUNTIME_DOWNLOAD_LABEL = "waiting for ONNX Runtime download";

/**
 * Stage the daemon reports while its first reachability check of a remote
 * embedding backend is still out (`CHECKING_EMBEDDING_BACKEND_STAGE` in
 * crates/aft/src/semantic_index.rs). The check runs off the status path, so
 * until it answers the honest reading is "being checked", not "building".
 */
const CHECKING_EMBEDDING_BACKEND_STAGE = "checking_embedding_backend";
const CHECKING_EMBEDDING_BACKEND_LABEL = "checking the backend";

/**
 * How a reader should treat each status word the daemon can emit.
 *
 * `progress` means an attempt is under way and waiting is the right response;
 * `failure` means it is not, and nothing in the failure family may borrow a
 * progress rendering. The daemon's own list of words is
 * `SEMANTIC_INDEX_STATUS_WORDS` in crates/aft/src/semantic_index.rs, and a test
 * checks this mapping covers all of it — so a word the daemon can send but a
 * plugin cannot classify is a failing test rather than raw text in the UI.
 */
export type SemanticIndexStatusKind =
  | "ready"
  | "progress"
  | "failure"
  | "inactive"
  | "unrecognized";

// `refreshing` is a queryable index with a large batch of files masked while
// they re-embed: an attempt is under way and waiting is the right response, so
// it is progress rather than ready or failure.
const SEMANTIC_PROGRESS_STATUSES = new Set(["building", "loading", "refreshing"]);
// `empty` is not in the daemon's current word list: it used to mean a loaded
// index holding nothing, and that index now reports `ready` like any other
// queryable one. It stays recognised here because a plugin can be talking to a
// daemon older than that change.
const SEMANTIC_READY_STATUSES = new Set(["ready", "empty"]);
const SEMANTIC_INACTIVE_STATUSES = new Set(["disabled", "busy"]);

/**
 * Failure words and what a reader shows for each. The labels stay close to the
 * wire words so a bug report and the daemon log still line up; the point of the
 * table is that every word in it is known to be a failure, so none of them can
 * be rendered as progress.
 */
const SEMANTIC_FAILURE_LABELS: Record<string, string> = {
  backend_unavailable: "backend unavailable",
  degraded: "degraded",
  error: "error",
  failed: "failed",
  unavailable: "unavailable",
};

export function semanticIndexStatusKind(status: string): SemanticIndexStatusKind {
  if (SEMANTIC_READY_STATUSES.has(status)) return "ready";
  if (SEMANTIC_PROGRESS_STATUSES.has(status)) return "progress";
  if (status in SEMANTIC_FAILURE_LABELS) return "failure";
  if (SEMANTIC_INACTIVE_STATUSES.has(status)) return "inactive";
  return "unrecognized";
}

function mentionsMissingOnnxRuntime(...values: Array<string | null | undefined>): boolean {
  return values.some(
    (value) => typeof value === "string" && value.includes(MISSING_ONNX_RUNTIME_MARKER),
  );
}

/**
 * The label for a semantic index that is not going to serve, or null when the
 * snapshot describes no failure.
 *
 * The missing-runtime check reads the stage and the error, not just the status
 * word: a build that died still carries the stage it died in, and a reader that
 * looked only at the status word would report that dead attempt as progress.
 */
function semanticFailureLabel(
  status: string,
  stage?: string | null,
  error?: string | null,
): string | null {
  if (mentionsMissingOnnxRuntime(error, stage)) return MISSING_ONNX_RUNTIME_LABEL;
  return SEMANTIC_FAILURE_LABELS[status] ?? null;
}

/**
 * What the daemon reports alongside a `backend_unavailable` status: the
 * engine's own reason (for example "connection refused") and the configured
 * backend URL when the backend is remote.
 */
export interface SemanticBackendDetail {
  reason?: string | null;
  backendUrl?: string | null;
}

/**
 * `backend unavailable (<url>): <reason>`. The daemon's search reply opens
 * with the same words ("Semantic backend unavailable (<url>): <reason>"), so
 * the sidebar, the status dialog and an aft_search result describe one outage
 * in one vocabulary.
 */
function backendUnavailableLabel(detail?: SemanticBackendDetail): string {
  const url = detail?.backendUrl?.trim();
  const reason = detail?.reason?.trim();
  let label = SEMANTIC_FAILURE_LABELS.backend_unavailable;
  if (url) label += ` (${url})`;
  if (reason) label += `: ${reason}`;
  return label;
}

export function formatSemanticIndexStatus(
  status: string,
  stage?: string | null,
  error?: string | null,
  backend?: SemanticBackendDetail,
): string {
  // A failure outranks any progress stage. Telling the reader the index is
  // rebuilding when the build cannot start asks them to wait for something that
  // will never finish.
  if (status === "backend_unavailable" && !mentionsMissingOnnxRuntime(error, stage)) {
    return backendUnavailableLabel(backend);
  }
  const failure = semanticFailureLabel(status, stage, error);
  if (failure) return failure;

  if (semanticIndexStatusKind(status) === "progress" && stage === "fingerprint_change") {
    return REBUILDING_LABEL;
  }
  if (
    semanticIndexStatusKind(status) === "progress" &&
    stage === WAITING_FOR_ONNX_RUNTIME_DOWNLOAD_STAGE
  ) {
    return WAITING_FOR_ONNX_RUNTIME_DOWNLOAD_LABEL;
  }
  if (
    semanticIndexStatusKind(status) === "progress" &&
    stage === CHECKING_EMBEDDING_BACKEND_STAGE
  ) {
    const url = backend?.backendUrl?.trim();
    return url ? `${CHECKING_EMBEDDING_BACKEND_LABEL} (${url})` : CHECKING_EMBEDDING_BACKEND_LABEL;
  }
  return status;
}
