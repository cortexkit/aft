/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import { homedir } from "node:os";
import { resolve, sep } from "node:path";
import { tool } from "@opencode-ai/plugin";
import {
  callBridge,
  callToolCall,
  coerceOptionalInt,
  decodeFileUrl,
  expandTilde,
  optionalInt,
  resolvePathFromProjectRoot,
} from "../tools/_shared.js";
import type { PluginContext } from "../types.js";

const z = tool.schema;

for (const dispatch of [callBridge, callToolCall]) {
  test(`generic ${dispatch.name} forwards V1 caller abort and honors explicit overrides`, async () => {
    const signals: unknown[] = [];
    const send = async (...args: any[]) => {
      signals.push(args[args.length - 1]?.abortSignal);
      return {};
    };
    const ctx = {
      client: {},
      config: {},
      pool: { getBridge: () => ({ send, toolCall: send }) },
    } as unknown as PluginContext;
    const caller = new AbortController().signal;
    const override = new AbortController().signal;
    const effect = new AbortController().signal;
    const runtime = { directory: process.cwd(), abort: caller };
    await dispatch(ctx, runtime, "read", {});
    await dispatch(ctx, runtime, "read", {}, { abortSignal: override });
    await dispatch(ctx, { ...runtime, effectAbort: effect }, "read", {});
    expect(signals).toEqual([caller, override, effect]);
  });
}

describe("optionalInt", () => {
  test("accepts integers in range and undefined", () => {
    const schema = optionalInt(1, 100);
    expect(schema.parse(undefined)).toBeUndefined();
    expect(schema.parse(24)).toBe(24);
    expect(schema.parse(1)).toBe(1);
    expect(schema.parse(100)).toBe(100);
  });

  test("rejects out-of-range and non-integers (Zod-level)", () => {
    const schema = optionalInt(1, 100);
    expect(() => schema.parse(0)).toThrow();
    expect(() => schema.parse(101)).toThrow();
    expect(() => schema.parse(24.5)).toThrow();
    expect(() => schema.parse("24")).toThrow();
    expect(() => schema.parse(null)).toThrow();
  });

  test("MUST be JSON-Schema-representable (regression guard for plugin-load failure)", () => {
    // This test would have caught the v0.30.1 regression where optionalInt
    // used `.transform()` and made every tool's args unconvertible by
    // OpenCode's host Zod, killing plugin load with:
    //   "Transforms cannot be represented in JSON Schema"
    //
    // OpenCode's tool/registry.ts calls `z.toJSONSchema(args, { io: "input" })`
    // on a freshly-wrapped `z.object(args)` at session start. Any throw here
    // crashes plugin load. Keep this guard.
    const wrapped = z.object({ x: optionalInt(1, 60) });
    expect(() => z.toJSONSchema(wrapped, { io: "input" })).not.toThrow();
    const jsonSchema = z.toJSONSchema(wrapped, { io: "input" }) as Record<string, unknown>;
    const props = jsonSchema.properties as Record<string, Record<string, unknown>>;
    // Plain integer schema with bounds — no transform, no anyOf-with-null mess.
    expect(props.x.type).toBe("integer");
    expect(props.x.minimum).toBe(1);
    expect(props.x.maximum).toBe(60);
  });
});

describe("coerceOptionalInt", () => {
  test("treats empty sentinels as undefined", () => {
    expect(coerceOptionalInt(undefined, "x", 1, 100)).toBeUndefined();
    expect(coerceOptionalInt(null, "x", 1, 100)).toBeUndefined();
    expect(coerceOptionalInt("", "x", 1, 100)).toBeUndefined();
    expect(coerceOptionalInt(0, "x", 1, 100)).toBeUndefined();
    expect(coerceOptionalInt(Number.NaN, "x", 1, 100)).toBeUndefined();
  });

  test("accepts numbers and integer strings", () => {
    expect(coerceOptionalInt(24, "x", 1, 100)).toBe(24);
    expect(coerceOptionalInt("24", "x", 1, 100)).toBe(24);
  });

  test("0 is a real value when min is 0 (edit occurrence regression)", () => {
    // occurrence: 0 selects the FIRST match — dropping it as a sentinel sent
    // agents into an ambiguous_match loop suggesting the param they just sent.
    expect(coerceOptionalInt(0, "occurrence", 0, Number.MAX_SAFE_INTEGER)).toBe(0);
    expect(coerceOptionalInt("0", "occurrence", 0, Number.MAX_SAFE_INTEGER)).toBe(0);
  });

  test("rejects out-of-bounds and non-integers with named errors", () => {
    expect(() => coerceOptionalInt(999, "x", 1, 100)).toThrow("x must be between 1 and 100");
    expect(() => coerceOptionalInt("abc", "x", 1, 100)).toThrow(
      "x must be an integer between 1 and 100",
    );
    expect(() => coerceOptionalInt(24.5, "x", 1, 100)).toThrow(
      "x must be an integer between 1 and 100",
    );
  });
});

describe("expandTilde", () => {
  test("expands a bare ~ to home", () => {
    expect(expandTilde("~")).toBe(homedir());
  });

  test("expands ~/path to home-rooted path", () => {
    expect(expandTilde("~/Work/OSS/toon/SPEC.md")).toBe(
      resolve(homedir(), "Work/OSS/toon/SPEC.md"),
    );
  });

  test("expands ~<sep>path on the platform separator", () => {
    expect(expandTilde(`~${sep}Work`)).toBe(resolve(homedir(), "Work"));
  });

  test("leaves absolute, relative, and ~user paths untouched", () => {
    expect(expandTilde("/abs/path")).toBe("/abs/path");
    expect(expandTilde("rel/path")).toBe("rel/path");
    expect(expandTilde("~otheruser/foo")).toBe("~otheruser/foo");
    expect(expandTilde("")).toBe("");
  });
});

describe("resolvePathFromProjectRoot", () => {
  const root = "/tmp/project-root";

  test("expands ~/ before resolving (regression: ~ was joined as a literal segment)", () => {
    // The bug: resolvePathFromProjectRoot("~/Work/...") produced
    // "<projectRoot>/~/Work/..." because path.resolve treats ~ literally.
    expect(resolvePathFromProjectRoot(root, "~/Work/OSS/toon/SPEC.md")).toBe(
      resolve(homedir(), "Work/OSS/toon/SPEC.md"),
    );
    expect(resolvePathFromProjectRoot(root, "~/Work/OSS/toon/SPEC.md")).not.toContain(`${root}/~`);
  });

  test("preserves absolute paths", () => {
    expect(resolvePathFromProjectRoot(root, "/abs/file.ts")).toBe("/abs/file.ts");
  });

  test("roots relative paths at the project root", () => {
    expect(resolvePathFromProjectRoot(root, "src/file.ts")).toBe(resolve(root, "src/file.ts"));
  });

  test("decodes RFC 8089 file: URLs to local paths", () => {
    expect(resolvePathFromProjectRoot(root, "file:///abs/file.ts")).toBe("/abs/file.ts");
    expect(resolvePathFromProjectRoot(root, "file:/abs/file.ts")).toBe("/abs/file.ts");
    expect(resolvePathFromProjectRoot(root, "file://localhost/abs/file.ts")).toBe("/abs/file.ts");
    expect(resolvePathFromProjectRoot(root, "file:///tmp/a%20b.md")).toBe("/tmp/a b.md");
  });
});

// This decoder MUST agree with the Rust percent_decode (subc_translate.rs):
// each valid %HH decodes independently, malformed escapes stay literal. A
// decodeURIComponent-based decoder throws on one malformed %ZZ and leaves the
// WHOLE string encoded, so valid %2e%2e stayed encoded here while Rust
// decoded ../ out of the project — a permission-vs-filesystem split.
describe("decodeFileUrl parity with the Rust decoder", () => {
  test("valid escapes decode even when a later escape is malformed", () => {
    // %2e%2e -> ".." must be produced despite the trailing malformed %ZZ, so
    // downstream path resolution escapes the project exactly as Rust does.
    expect(decodeFileUrl("file:///proj/%2e%2e/%ZZ/../secret")).toBe("/proj/../%ZZ/../secret");
  });

  test("non-file targets pass through untouched", () => {
    expect(decodeFileUrl("/plain/abs")).toBe("/plain/abs");
    expect(decodeFileUrl("relative/path")).toBe("relative/path");
  });

  test("non-local authority is not treated as a local path", () => {
    // On unix this is left as the original URL (no decode) so it never
    // silently points at some other local file.
    if (process.platform !== "win32") {
      expect(decodeFileUrl("file://server/share/x.txt")).toBe("file://server/share/x.txt");
    }
  });
});
