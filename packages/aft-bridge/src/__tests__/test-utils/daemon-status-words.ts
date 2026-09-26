/**
 * The daemon's semantic-index vocabulary, read from the Rust source that
 * defines it.
 *
 * Parsed rather than copied: a second hand-written list of status words would
 * pass its own coverage check while the daemon emitted something new, which is
 * exactly how `backend_unavailable` reached users as unexplained grey text.
 * Shared from here because every reader of those words — the shared formatter
 * and each plugin host that renders it — has to be checked against the same
 * list.
 */

import { readFileSync } from "node:fs";
import { join } from "node:path";

const DAEMON_SEMANTIC_INDEX_SOURCE = join(
  import.meta.dir,
  "../../../../../crates/aft/src/semantic_index.rs",
);

function readDaemonSource(): string {
  const source = readFileSync(DAEMON_SEMANTIC_INDEX_SOURCE, "utf8");
  if (source.length === 0) {
    throw new Error(`empty daemon source at ${DAEMON_SEMANTIC_INDEX_SOURCE}`);
  }
  return source;
}

/** Every `semantic_index.status` word the daemon can put on the wire. */
export function daemonSemanticStatusWords(): string[] {
  const source = readDaemonSource();
  const declaration = source.indexOf("pub const SEMANTIC_INDEX_STATUS_WORDS");
  const open = source.indexOf("&[", declaration);
  const close = source.indexOf("];", open);
  if (declaration < 0 || open < 0 || close < 0) {
    throw new Error("SEMANTIC_INDEX_STATUS_WORDS not found in the daemon source");
  }
  const words = [...source.slice(open, close).matchAll(/"([a-z_]+)"/g)].map((match) => match[1]);
  // The daemon lists ready/building/failed and several more; a handful of
  // matches means the parse drifted, and an empty list would make a coverage
  // assertion pass without checking anything.
  if (words.length < 5) {
    throw new Error(`parsed only ${words.length} daemon status word(s): ${words.join(", ")}`);
  }
  return words;
}

/** Opening words of every missing-runtime message the daemon produces. */
export function daemonMissingRuntimePrefix(): string {
  const match = readDaemonSource().match(/ONNX_RUNTIME_MISSING_PREFIX: &str = "([^"]+)"/);
  if (!match) {
    throw new Error("ONNX_RUNTIME_MISSING_PREFIX not found in the daemon source");
  }
  return match[1];
}

/** Build stage the daemon reports while it waits for the ONNX Runtime download. */
export function daemonWaitingForOnnxDownloadStage(): string {
  const match = readDaemonSource().match(
    /WAITING_FOR_ONNX_RUNTIME_DOWNLOAD_STAGE: &str = "([^"]+)"/,
  );
  if (!match) {
    throw new Error("WAITING_FOR_ONNX_RUNTIME_DOWNLOAD_STAGE not found in the daemon source");
  }
  return match[1];
}
