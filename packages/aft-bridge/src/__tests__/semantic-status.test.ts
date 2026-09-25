import { describe, expect, test } from "bun:test";
import { formatSemanticIndexStatus, semanticIndexStatusKind } from "../semantic-status.js";
import {
  daemonMissingRuntimePrefix,
  daemonSemanticStatusWords,
} from "./test-utils/daemon-status-words.js";

describe("formatSemanticIndexStatus", () => {
  test("a model change still reports as a rebuild", () => {
    // The rebuild wording is correct when a rebuild is really running; the
    // failure handling below must not cost us this case.
    expect(formatSemanticIndexStatus("building", "fingerprint_change")).toBe(
      "Rebuilding (model changed)",
    );
    expect(formatSemanticIndexStatus("loading", "fingerprint_change")).toBe(
      "Rebuilding (model changed)",
    );
  });

  test("a failed index is never reported as a rebuild", () => {
    // A build that dies leaves its stage behind. Reading the stage alone turns
    // a dead attempt into "Rebuilding (model changed)" and tells the user to
    // wait for a build that stopped.
    const label = formatSemanticIndexStatus(
      "building",
      "fingerprint_change",
      `${daemonMissingRuntimePrefix()} dlopen('libonnxruntime.dylib') failed: image not found`,
    );

    expect(label).not.toBe("Rebuilding (model changed)");
    expect(label).toContain("ONNX Runtime");
  });

  test("names the missing runtime and the command that installs it", () => {
    const label = formatSemanticIndexStatus(
      "failed",
      null,
      `${daemonMissingRuntimePrefix()} Run \`npx @cortexkit/aft doctor --fix\``,
    );

    expect(label).toContain("ONNX Runtime");
    expect(label).toContain("doctor --fix");
    // No platform-specific install advice: whether AFT can download the runtime
    // here is answered by the downloader, and `doctor --fix` is what asks it.
    expect(label).not.toContain("brew");
    expect(label).not.toContain("apt");
  });

  test("a missing runtime carried in the build stage is not progress", () => {
    const label = formatSemanticIndexStatus(
      "loading",
      `waiting_for_embedding_backend: ${daemonMissingRuntimePrefix()} dlopen failed`,
    );

    expect(label).not.toBe("loading");
    expect(label).toContain("ONNX Runtime");
  });

  test("an ordinary failure keeps its word and gains no progress wording", () => {
    expect(formatSemanticIndexStatus("failed", "fingerprint_change")).toBe("failed");
    expect(formatSemanticIndexStatus("backend_unavailable", null)).toBe("backend unavailable");
    expect(formatSemanticIndexStatus("ready", null)).toBe("ready");
  });

  test("an unreachable backend carries its URL and the engine's reason", () => {
    expect(
      formatSemanticIndexStatus("backend_unavailable", null, null, {
        reason: "connection refused",
        backendUrl: "http://localhost:1234/v1",
      }),
    ).toBe("backend unavailable (http://localhost:1234/v1): connection refused");
    // A local backend has no URL; the reason still reaches the reader.
    expect(
      formatSemanticIndexStatus("backend_unavailable", null, null, { reason: "model load failed" }),
    ).toBe("backend unavailable: model load failed");
  });

  test("classifies every status word the daemon can emit", () => {
    const unrenderable = daemonSemanticStatusWords().filter(
      (word) => semanticIndexStatusKind(word) === "unrecognized",
    );

    expect(unrenderable).toEqual([]);
  });

  test("no daemon status word is classified as progress and failure at once", () => {
    // The two kinds decide opposite advice (wait vs act), so the sets that
    // define them must not overlap.
    const progress = daemonSemanticStatusWords().filter(
      (word) => semanticIndexStatusKind(word) === "progress",
    );

    expect(progress).toEqual(["building", "loading"]);
  });
});
