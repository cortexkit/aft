/// <reference path="../bun-test.d.ts" />

import { afterAll, describe, expect, mock, test } from "bun:test";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import type { StatusCompression } from "../shared/status";

// These module mocks are applied process-globally by Bun. Restore them after
// this file so the @opentui/solid and solid-js stubs do not leak into other
// test files in the same `bun test` run.
afterAll(() => {
  mock.restore();
});

// These stand in for a real OpenTUI renderer so sidebar.tsx can be imported
// without one. They must still behave like JSX: a factory that returns null
// without invoking its component makes every component in the process inert,
// and Bun's mock.module outlives mock.restore() (see the CONSTRAINTS note on
// cross-file mock leakage), so a dishonest factory here silently breaks any
// later file whose subject renders. Invoking the component and discarding its
// output keeps this file's isolation while leaving those tests true.
const renderComponent = (type: unknown, props: unknown): null => {
  if (typeof type === "function") (type as (p: unknown) => unknown)(props);
  return null;
};

mock.module("@opentui/solid/jsx-dev-runtime", () => ({
  Fragment: (props: { children?: unknown }) => props.children,
  jsxDEV: renderComponent,
}));
mock.module("@opentui/solid/jsx-runtime", () => ({
  Fragment: (props: { children?: unknown }) => props.children,
  jsx: renderComponent,
  jsxs: renderComponent,
}));
mock.module("solid-js", () => ({
  createEffect: () => undefined,
  createMemo: (fn: () => unknown) => fn,
  createSignal: (initial: unknown) => [() => initial, () => undefined],
  on: (_source: unknown, fn: unknown) => fn,
  onCleanup: () => undefined,
}));

const {
  collapsedCompressionValue,
  collapsedHealthLights,
  degradedReasonLabel,
  formatCompressionSidebarRows,
  isSnapshotForContext,
  resolveTuiStorageDir,
  scopedSidebarSnapshot,
  shouldSuppressUninitializedDowngrade,
} = await import("../tui/sidebar.tsx");
const { worktreeCacheRoleNote } = await import("../shared/status.js");

const compression = (overrides: Partial<StatusCompression> = {}): StatusCompression => ({
  project: {
    events: 1234,
    original_tokens: 567000,
    compressed_tokens: 234000,
    savings_tokens: 333000,
  },
  session: { events: 12, original_tokens: 5600, compressed_tokens: 2300, savings_tokens: 3300 },
  ...overrides,
});

describe("sidebar compression rows", () => {
  test("TUI storage resolution uses the shared CortexKit helper", async () => {
    await withEnv(
      {
        XDG_DATA_HOME: "/tmp/aft-tui-storage-test",
        AFT_STORAGE_DIR: "/tmp/aft-tui-storage-override",
      },
      () => {
        expect(resolveTuiStorageDir()).toBe("/tmp/aft-tui-storage-override");
      },
    );
  });

  test("sidebar snapshot is scoped to the current directory and session", () => {
    const snapshot = { version: "test" } as any;
    const scoped = { directory: "/project/a", sessionID: "session-a", snapshot };

    expect(scopedSidebarSnapshot(scoped, "/project/a", "session-a")).toBe(snapshot);
    expect(scopedSidebarSnapshot(scoped, "/project/b", "session-a")).toBeNull();
    expect(scopedSidebarSnapshot(scoped, "/project/a", "session-b")).toBeNull();
    expect(scopedSidebarSnapshot(null, "/project/a", "session-a")).toBeNull();
  });

  test("sidebar_renders_compression_when_project_events_present", () => {
    const rows = formatCompressionSidebarRows(compression());

    // Each scope expands to: scope-header + Tokens Saved + Compression
    // Ratio. With both Session and Project, six rows total. 333,000
    // savings / 567,000 original ≈ 59% reduction.
    expect(rows).toHaveLength(6);
    expect(rows[0]).toEqual({ kind: "scope", label: "Session" });
    expect(rows[1]).toEqual({ kind: "stat", label: "Tokens Saved", value: "3,300" });
    expect(rows[2]).toEqual({ kind: "stat", label: "Compression Ratio", value: "59%" });
    expect(rows[3]).toEqual({ kind: "scope", label: "Project" });
    expect(rows[4]).toEqual({ kind: "stat", label: "Tokens Saved", value: "333,000" });
    expect(rows[5]).toEqual({ kind: "stat", label: "Compression Ratio", value: "59%" });
  });

  test("sidebar_hides_compression_when_undefined", () => {
    expect(formatCompressionSidebarRows(undefined)).toEqual([]);
  });

  test("sidebar_hides_compression_when_zero_events", () => {
    expect(
      formatCompressionSidebarRows(
        compression({
          project: { events: 0, original_tokens: 0, compressed_tokens: 0, savings_tokens: 0 },
        }),
      ),
    ).toEqual([]);
  });

  test("sidebar_hides_session_scope_when_session_events_zero", () => {
    const rows = formatCompressionSidebarRows(
      compression({
        session: { events: 0, original_tokens: 0, compressed_tokens: 0, savings_tokens: 0 },
      }),
    );

    // Only the Project scope (header + 2 stats) when session.events === 0.
    expect(rows).toHaveLength(3);
    expect(rows[0]).toEqual({ kind: "scope", label: "Project" });
    expect(rows.some((row) => row.kind === "scope" && row.label === "Session")).toBe(false);
  });
});

describe("worktreeCacheRoleNote", () => {
  test("worktree is a muted shared-index note, not a degraded reason", () => {
    expect(worktreeCacheRoleNote("worktree")).toBe(
      "shared repo index (built by the main checkout)",
    );
    expect(worktreeCacheRoleNote("main")).toBeNull();
    expect(worktreeCacheRoleNote("read_only")).toBeNull();
    expect(worktreeCacheRoleNote(undefined)).toBeNull();
  });
});

describe("degradedReasonLabel", () => {
  test("renders watcher_unavailable as a non-alarming degraded-mode note", () => {
    expect(degradedReasonLabel("watcher_unavailable")).toBe(
      "file watcher unavailable; continuing without live external-change invalidation",
    );
  });

  test("keeps unknown degraded reason codes visible", () => {
    expect(degradedReasonLabel("custom_reason")).toBe("custom_reason");
  });
});

describe("shouldSuppressUninitializedDowngrade (sidebar flicker fix)", () => {
  test("suppresses a not_initialized downgrade when good data is already shown", () => {
    expect(shouldSuppressUninitializedDowngrade("not_initialized", true)).toBe(true);
  });

  test("allows the first not_initialized snapshot (no good data yet)", () => {
    expect(shouldSuppressUninitializedDowngrade("not_initialized", false)).toBe(false);
  });

  test("never suppresses a real (initialized) snapshot, even over good data", () => {
    expect(shouldSuppressUninitializedDowngrade("main", true)).toBe(false);
    expect(shouldSuppressUninitializedDowngrade("worktree", true)).toBe(false);
  });

  test("treats undefined cache_role as a real snapshot (does not suppress)", () => {
    expect(shouldSuppressUninitializedDowngrade(undefined, true)).toBe(false);
  });
});

describe("collapsedCompressionValue (collapsed sidebar row)", () => {
  test("returns null when no compression recorded (0 project events)", () => {
    expect(collapsedCompressionValue(undefined)).toBeNull();
    expect(
      collapsedCompressionValue(compression({ project: { ...compression().project, events: 0 } })),
    ).toBeNull();
  });

  test("shortens tokens and shows ratio — e.g. 7.6M / 64%", () => {
    const value = collapsedCompressionValue(
      compression({
        project: {
          events: 100,
          original_tokens: 11_900_000,
          compressed_tokens: 4_235_000,
          savings_tokens: 7_665_687,
        },
      }),
    );
    // 7,665,687 → 7.7M (formatCount rounds to one decimal); ratio 7.66M/11.9M ≈ 64%.
    expect(value).toBe("7.7M / 64%");
  });

  test("uses K scale for thousands", () => {
    const value = collapsedCompressionValue(
      compression({
        project: {
          events: 5,
          original_tokens: 10_000,
          compressed_tokens: 6_500,
          savings_tokens: 3_500,
        },
      }),
    );
    expect(value).toBe("4K / 35%");
  });

  test("0% ratio when original tokens is 0 (no divide-by-zero)", () => {
    const value = collapsedCompressionValue(
      compression({
        project: { events: 1, original_tokens: 0, compressed_tokens: 0, savings_tokens: 0 },
      }),
    );
    expect(value).toBe("0 / 0%");
  });
});

describe("collapsedHealthLights (collapsed Code Health traffic lights)", () => {
  const bar = (overrides = {}) => ({
    errors: 0,
    warnings: 0,
    dead_code: 0,
    unused_exports: 0,
    duplicates: 0,
    todos: 0,
    tier2_stale: false,
    ...overrides,
  });

  test("returns null when status bar is undefined (Tier-2 not populated)", () => {
    expect(collapsedHealthLights(undefined)).toBeNull();
  });

  test("all green on a fully proven clean bar", () => {
    expect(collapsedHealthLights(bar())).toEqual({
      diagnostics: "ok",
      code: "ok",
      todos: "ok",
    });
  });

  test("missing categories stay muted instead of appearing clean", () => {
    expect(collapsedHealthLights({ errors: 0 })).toEqual({
      diagnostics: "muted",
      code: "muted",
      todos: "muted",
    });
  });

  test("diagnostics light: red on errors (wins over warnings)", () => {
    expect(collapsedHealthLights(bar({ errors: 2, warnings: 5 }))?.diagnostics).toBe("err");
  });

  test("diagnostics light: yellow on warnings only", () => {
    expect(collapsedHealthLights(bar({ warnings: 3 }))?.diagnostics).toBe("warn");
  });

  test("code light: yellow when duplicates are non-zero", () => {
    expect(collapsedHealthLights(bar({ duplicates: 1167 }))?.code).toBe("warn");
  });

  test("code light: dead_code / unused_exports drive the light", () => {
    expect(collapsedHealthLights(bar({ dead_code: 999 }))?.code).toBe("warn");
    expect(collapsedHealthLights(bar({ unused_exports: 999 }))?.code).toBe("warn");
  });

  test("todos light: yellow when any todos, green otherwise", () => {
    expect(collapsedHealthLights(bar({ todos: 4 }))?.todos).toBe("warn");
    expect(collapsedHealthLights(bar({ todos: 0 }))?.todos).toBe("ok");
  });
});

describe("isSnapshotForContext (cross-project contamination belt)", () => {
  const snap = (overrides: Record<string, unknown> = {}) =>
    ({
      project_root: "/work/aft",
      canonical_root: "/work/aft",
      session: { id: "ses_a", tracked_files: 0, checkpoints: 0 },
      ...overrides,
    }) as any;

  test("accepts a snapshot whose project_root matches the sidebar directory", () => {
    expect(isSnapshotForContext(snap(), "/work/aft", undefined)).toBe(true);
    // trailing slash tolerance
    expect(isSnapshotForContext(snap(), "/work/aft/", undefined)).toBe(true);
  });

  test("accepts via canonical_root when project_root differs (symlink aliasing)", () => {
    const s = snap({ project_root: "/private/work/aft", canonical_root: "/work/aft" });
    expect(isSnapshotForContext(s, "/work/aft", undefined)).toBe(true);
  });

  test("REJECTS another project's snapshot (the magic-context contamination case)", () => {
    const stray = snap({
      project_root: "/work/magic-context",
      canonical_root: "/work/magic-context",
    });
    expect(isSnapshotForContext(stray, "/work/aft", undefined)).toBe(false);
  });

  test("REGRESSION: a stray snapshot echoing OUR session id is still rejected", () => {
    // Rust echoes the REQUESTED session id into snapshot.session.id, so a
    // snapshot computed against another project's bridge carries our id. The
    // belt's first version accepted that as a "resume case" — making it
    // vacuous (contamination recurred live). Session identity must never
    // grant cross-project acceptance.
    const stray = snap({
      project_root: "/work/magic-context",
      canonical_root: "/work/magic-context",
      session: { id: "ses_a", tracked_files: 0, checkpoints: 0 }, // our id, echoed
    });
    expect(isSnapshotForContext(stray, "/work/aft", undefined)).toBe(false);
  });

  test("accepts a mismatched root when the handler attached a matching served_directory (opencode -s resume)", () => {
    const resume = snap({
      project_root: "/real/project/elsewhere",
      canonical_root: "/real/project/elsewhere",
    });
    expect(isSnapshotForContext(resume, "/launch/cwd", "/real/project/elsewhere")).toBe(true);
  });

  test("REJECTS a mismatched root when served_directory names a different project", () => {
    // A stray server attaching its own (other-project) served_directory must
    // not be accepted just because the field is present.
    const stray = snap({
      project_root: "/work/magic-context",
      canonical_root: "/work/magic-context",
    });
    expect(isSnapshotForContext(stray, "/work/aft", "/work/aft")).toBe(false);
  });

  test("accepts placeholder/synthetic snapshots with no roots", () => {
    const placeholder = snap({ project_root: null, canonical_root: null });
    expect(isSnapshotForContext(placeholder, "/work/aft", undefined)).toBe(true);
  });
});
