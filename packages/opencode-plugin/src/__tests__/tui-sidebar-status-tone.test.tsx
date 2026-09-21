/// <reference path="../bun-test.d.ts" />

import { afterAll, describe, expect, mock, test } from "bun:test";
import { daemonSemanticStatusWords } from "../../../aft-bridge/src/__tests__/test-utils/daemon-status-words.js";

// These module mocks are applied process-globally by Bun. Restore them after
// this file so the @opentui/solid and solid-js stubs do not leak into other
// test files in the same `bun test` run.
afterAll(() => {
  mock.restore();
});

// These stand in for a real OpenTUI renderer so the sidebar module can be
// imported without one. They must still behave like JSX: a factory that returns
// null without invoking its component makes every component in the process
// inert, and Bun's mock.module outlives mock.restore(), so a dishonest factory
// here silently breaks any later file whose subject renders.
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

const { statusDisplay } = await import("../tui/sidebar-view.tsx");
const { semanticIndexStatusKind } = await import("../shared/status.js");

/**
 * What each classification is worth on screen, and the deliberate part of this
 * test: a failure has to pull the eye, an attempt under way warns, and only the
 * words that mean "nothing is being attempted" are allowed to be grey.
 */
const TONE_FOR_KIND = {
  ready: "ok",
  progress: "warn",
  failure: "err",
  inactive: "muted",
} as const;

describe("sidebar status tone", () => {
  /**
   * Read against the daemon's own word list rather than a list retyped here:
   * a second hand-written list passes its own coverage check while the daemon
   * emits something the sidebar has no reading for. `backend_unavailable`
   * reached users exactly that way — a real embedding-backend outage drawn in
   * the muted grey reserved for words the renderer does not recognise.
   */
  test("every status word the daemon can emit has a deliberate tone", () => {
    const undecided = daemonSemanticStatusWords().flatMap((word) => {
      const kind = semanticIndexStatusKind(word);
      if (kind === "unrecognized") return [`${word}: no classification`];
      const expected = TONE_FOR_KIND[kind];
      const actual = statusDisplay(word).tone;
      return actual === expected ? [] : [`${word}: expected ${expected}, drawn as ${actual}`];
    });

    expect(undecided).toEqual([]);
  });

  test("a word the daemon cannot emit keeps its text and stays grey", () => {
    // Grey is honest for an unknown word: it says the renderer has no reading
    // to offer, which is why no word the daemon can actually send may land here.
    expect(statusDisplay("chartreuse")).toEqual({ label: "chartreuse", tone: "muted" });
    expect(statusDisplay("")).toEqual({ label: "unknown", tone: "muted" });
  });
});
