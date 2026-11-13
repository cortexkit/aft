import { describe, expect, test } from "bun:test";
import { buildWorkflowHints as buildPiHints } from "../../../pi-plugin/src/workflow-hints.js";
import { buildWorkflowHints as buildOpenCodeHints } from "../workflow-hints.js";

// Literal expectations keep matching mistakes in the two builders from passing as parity.
const APPROVED_TEXT = `# Agent Foundational Toolkit

**Finding code**: start with aft_search; it takes concepts, identifiers, regex and literals. Then aft_outline for structure, aft_zoom for named symbols, read for line ranges. Don't find or read code with grep, find, sed or cat in bash.
**Relationships**: aft_callgraph for callers, impact, and execution or data-flow traces.
**Web pages**: aft_outline with a URL as target, then aft_zoom on a heading.
**Parallel calls**: send independent read-only calls in one response.
**After edits**: run aft_inspect for diagnostics; the project's typecheck and tests are the final check.
**Status bar**: \`[AFT E.. W.. | D.. U.. C.. | T..]\` appears when a count changes. E above 0 usually means your last edit broke something; \`?\` means unknown, not zero.
**Commands**: don't pipe test or build output into tail, grep or head; AFT already shortens it. Pass wait: true when you need a long command's result before anything else; a new message from the user detaches the wait. Otherwise a long command moves to the background and a reminder brings its result. Block on it with bash_watch only when nothing else is left; bash_watch with background: true and a pattern tells you when a line appears. Never loop bash_status.`;

const WITHOUT_SEARCH_OR_BASH = `# Agent Foundational Toolkit

**Finding code**: aft_outline for structure, aft_zoom for named symbols, read for line ranges.
**Relationships**: aft_callgraph for callers, impact, and execution or data-flow traces.
**Web pages**: aft_outline with a URL as target, then aft_zoom on a heading.
**Parallel calls**: send independent read-only calls in one response.
**After edits**: run aft_inspect for diagnostics; the project's typecheck and tests are the final check.
**Status bar**: \`[AFT E.. W.. | D.. U.. C.. | T..]\` appears when a count changes. E above 0 usually means your last edit broke something; \`?\` means unknown, not zero.`;

const defaults = { bashBackgroundEnabled: true, bashCompressionEnabled: true };
const builders = {
  OpenCode: (absent: Set<string>, flags: Partial<typeof defaults> = {}) =>
    buildOpenCodeHints({ ...defaults, ...flags, disabledTools: absent }),
  Pi: (absent: Set<string>, flags: Partial<typeof defaults> = {}) =>
    buildPiHints({ ...defaults, ...flags, absentTools: absent }),
};

for (const [host, build] of Object.entries(builders)) {
  describe(`${host} approved workflow guidance`, () => {
    test("matches the approved full-surface text word for word", () => {
      expect(build(new Set())).toBe(APPROVED_TEXT);
    });

    test("matches the approved fragments without search or bash", () => {
      expect(build(new Set(["aft_search", "bash"]))).toBe(WITHOUT_SEARCH_OR_BASH);
    });

    test("omits whole Finding code and Commands paragraphs when no fragment applies", () => {
      const out = build(new Set(["aft_search", "aft_zoom", "bash"]));
      expect(out).not.toContain("**Finding code**:");
      expect(out).not.toContain("**Commands**:");
      expect(out).toContain("**Relationships**:");
    });

    for (const absent of ["aft_outline", "aft_zoom", "read"]) {
      test(`navigation sentence requires ${absent}`, () => {
        expect(build(new Set([absent]))).not.toContain("Then aft_outline for structure");
      });
    }

    test("relationships and diagnostics require their registered tools", () => {
      const out = build(new Set(["aft_callgraph", "aft_inspect"]));
      expect(out).not.toContain("**Relationships**:");
      expect(out).not.toContain("**After edits**:");
      expect(out).not.toContain("**Status bar**:");
    });

    test("compression and background fragments have independent feature gates", () => {
      const noCompression = build(new Set(), { bashCompressionEnabled: false });
      expect(noCompression).not.toContain("don't pipe test or build output");
      expect(noCompression).toContain("**Commands**: Pass wait: true");
      const noBackground = build(new Set(), { bashBackgroundEnabled: false });
      expect(noBackground).toContain("**Commands**: don't pipe test or build output");
      expect(noBackground).not.toContain("Pass wait: true");
      expect(
        build(new Set(), { bashCompressionEnabled: false, bashBackgroundEnabled: false }),
      ).not.toContain("**Commands**:");
    });

    for (const absent of ["bash_watch", "bash_status"]) {
      test(`background sentences require ${absent}`, () => {
        const out = build(new Set([absent]));
        expect(out).not.toContain("Pass wait: true");
        expect(out).not.toContain("bash_watch");
        expect(out).not.toContain("bash_status");
        expect(out).toContain("**Commands**: don't pipe test or build output");
      });
    }
  });
}

test("both hosts render the same text for matching tool, bash and hashline facts", () => {
  const tools = [
    "aft_search",
    "aft_outline",
    "aft_zoom",
    "read",
    "aft_callgraph",
    "aft_inspect",
    "bash",
    "bash_watch",
    "bash_status",
    "bash_write",
    "grep",
  ];
  const surfaces = [[], ...tools.map((name) => [name]), ["aft_search", "bash"], tools];
  for (const surface of surfaces) {
    for (const bashBackgroundEnabled of [false, true]) {
      for (const bashCompressionEnabled of [false, true]) {
        for (const hashlineEffective of [false, true]) {
          for (const bashRewriteEnabled of [false, true]) {
            const opts = {
              bashBackgroundEnabled,
              bashCompressionEnabled,
              hashlineEffective,
              bashRewriteEnabled,
            };
            expect(buildOpenCodeHints({ ...opts, disabledTools: new Set(surface) })).toBe(
              buildPiHints({ ...opts, absentTools: new Set(surface) }),
            );
          }
        }
      }
    }
  }
});
