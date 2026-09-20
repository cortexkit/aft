import { readFileSync } from "node:fs";
import { describe, expect, test } from "bun:test";
import {
  InvalidRequestError,
  isWellFormedUnicodeString,
  prepareCanonicalEditArguments,
  prepareCanonicalPathArguments,
} from "../path-aliases.js";

const symbolModeValidationCases = JSON.parse(
  readFileSync(
    new URL("../../../../crates/aft/tests/fixtures/symbol-mode-validation.json", import.meta.url),
    "utf8",
  ),
) as Array<{
  label: string;
  arguments: Record<string, unknown>;
  message: string;
}>;

function expectInvalid(tool: string, args: unknown, fields: string[] = ["path", "filePath"]): void {
  try {
    prepareCanonicalPathArguments(tool, args);
    throw new Error("expected invalid_request");
  } catch (error) {
    expect(error).toBeInstanceOf(InvalidRequestError);
    expect((error as InvalidRequestError).code).toBe("invalid_request");
    for (const field of fields) expect((error as Error).message).toContain(field);
  }
}

describe("canonical path alias preparation", () => {
  test.each([
    ["canonical only", { path: "src/main.ts" }, { path: "src/main.ts" }],
    ["legacy only", { filePath: "src/main.ts" }, { path: "src/main.ts" }],
    [
      "equal dual spelling",
      { path: "src/main.ts", filePath: "src/main.ts" },
      { path: "src/main.ts" },
    ],
    [
      "equivalent JSON escapes",
      { path: "src/a.ts", filePath: "src/\u0061.ts" },
      { path: "src/a.ts" },
    ],
    [
      "supplementary character",
      { path: "src/😀.ts", filePath: "src/😀.ts" },
      { path: "src/😀.ts" },
    ],
  ])("accepts %s", (_label, input, expected) => {
    expect(prepareCanonicalPathArguments("read", input)).toEqual(expected);
  });

  test("compares decoded strings without path transformations", () => {
    const input = { path: " src\\main.ts ", filePath: "src/main.ts" };
    expectInvalid("read", input);
    expect(input).toEqual({ path: " src\\main.ts ", filePath: "src/main.ts" });
  });

  test("rejects unequal and incompatible dual spellings atomically", () => {
    expectInvalid("read", { path: "src/a.ts", filePath: "src/b.ts" });
    expectInvalid("read", { path: "src/a.ts", filePath: 42 });
    expectInvalid("read", { path: 42, filePath: "src/a.ts" });
  });

  test("does not normalize canonically distinct Unicode spellings", () => {
    expectInvalid("read", { path: "src/é.ts", filePath: "src/e\u0301.ts" });
  });

  test("requires a non-empty canonical path without trimming", () => {
    expectInvalid("read", { path: "" }, ["path"]);
    expectInvalid("read", { path: 42 }, ["path"]);
    expect(prepareCanonicalPathArguments("read", { path: " " }).path).toBe(" ");
  });

  test("rejects malformed UTF-16 at the preparation boundary", () => {
    expect(isWellFormedUnicodeString("😀")).toBe(true);
    expect(isWellFormedUnicodeString("\ud800")).toBe(false);
    expect(isWellFormedUnicodeString("\udc00")).toBe(false);
    expectInvalid("read", { path: "\ud800" }, ["path"]);
    expectInvalid("read", { path: "src/a.ts", filePath: "\ud800" });
  });

  test("normalizes nested zoom targets and callgraph target aliases", () => {
    expect(
      prepareCanonicalPathArguments("zoom", {
        targets: [{ filePath: "src/main.ts", symbol: "main" }],
      }),
    ).toEqual({ targets: [{ path: "src/main.ts", symbol: "main" }] });
    expect(
      prepareCanonicalPathArguments("callgraph", {
        path: "src/main.ts",
        toFile: "src/target.ts",
        op: "trace_to_symbol",
        symbol: "main",
        toSymbol: "target",
      }),
    ).toMatchObject({ path: "src/main.ts", toPath: "src/target.ts" });
    expectInvalid("callgraph", {
      path: "src/main.ts",
      filePath: "src/other.ts",
      symbol: "main",
      op: "callers",
    });
  });

  test("leaves role-specific collection and destination properties unchanged", () => {
    const input = {
      files: ["src/a.ts"],
      destination: "src/b.ts",
      target: "src/c.ts",
      path: "src/main.ts",
    };
    expect(prepareCanonicalPathArguments("move", input)).toEqual({
      ...input,
    });
  });

  test("strips empty or null optional path sentinels as absent", () => {
    // Optional-path tools: an empty-string or null `path` is treated as not
    // supplied, so the prepared record carries no `path` key at all.
    for (const tool of ["grep", "search", "conflicts", "zoom", "safety"]) {
      expect(prepareCanonicalPathArguments(tool, { path: "" })).toEqual({});
      expect(prepareCanonicalPathArguments(tool, { path: null })).toEqual({});
    }
    // callgraph's optional `toPath` alias is stripped the same way.
    expect(
      prepareCanonicalPathArguments("callgraph", {
        path: "src/main.ts",
        toPath: "",
        op: "callers",
        symbol: "main",
      }),
    ).toEqual({ path: "src/main.ts", op: "callers", symbol: "main" });
    // zoom's legacy optional `filePath` alias is stripped the same way.
    expect(prepareCanonicalPathArguments("zoom", { filePath: "" })).toEqual({});
  });

  test("required tools report empty path as missing, not malformed", () => {
    for (const tool of ["read", "write", "edit", "move", "import", "refactor"]) {
      try {
        prepareCanonicalPathArguments(tool, { path: "" });
        throw new Error("expected invalid_request");
      } catch (error) {
        expect(error).toBeInstanceOf(InvalidRequestError);
        expect((error as InvalidRequestError).code).toBe("invalid_request");
        expect((error as Error).message).toBe("'path' is required");
      }
    }
  });

  test("empty optional path sentinel does not mask a lone-surrogate error", () => {
    // The strip is bounded to empty/null, not "anything falsy": a lone
    // surrogate on an optional field still throws the well-formed error.
    for (const tool of ["grep", "search", "conflicts", "zoom", "safety"]) {
      try {
        prepareCanonicalPathArguments(tool, { path: "\ud800" });
        throw new Error("expected invalid_request");
      } catch (error) {
        expect(error).toBeInstanceOf(InvalidRequestError);
        expect((error as Error).message).toContain("well-formed Unicode");
      }
    }
  });
});

describe("edit boundary preparation", () => {
  const meaningfulModeCases: Array<{
    label: string;
    input: Record<string, unknown>;
    expected?: Record<string, unknown>;
    error?: string;
  }> = [
    {
      label: "edits ignores empty mode sentinels",
      input: {
        filePath: "src/example.ts",
        edits: [{ oldString: "old", newString: "new" }],
        appendContent: "",
        symbol: "",
        content: "",
      },
      expected: {
        path: "src/example.ts",
        edits: [{ oldString: "old", newString: "new" }],
      },
    },
    {
      label: "append ignores empty edits",
      input: {
        filePath: "src/example.ts",
        appendContent: "append",
        edits: [],
      },
      expected: { path: "src/example.ts", appendContent: "append" },
    },
    {
      label: "find and replace ignores null range sentinels",
      input: {
        filePath: "src/example.ts",
        edits: [
          {
            oldString: "gamma line three",
            newString: "GAMMA line three",
            replaceAll: false,
            occurrence: null,
            startLine: null,
            endLine: null,
            content: null,
          },
        ],
      },
      expected: {
        path: "src/example.ts",
        edits: [{ oldString: "gamma line three", newString: "GAMMA line three" }],
      },
    },
    {
      label: "symbol deletion keeps empty content",
      input: { filePath: "src/example.ts", symbol: "target", content: "" },
      expected: { path: "src/example.ts", symbol: "target", content: "" },
    },
    {
      label: "content without a symbol is rejected",
      input: { filePath: "src/example.ts", symbol: "", content: "replacement" },
      error:
        "edit: incomplete symbol mode: property 'symbol' must be a non-empty string. " +
        "Retry with `symbol` + `content`, or use `edits[]`.",
    },
    {
      label: "two real modes conflict",
      input: {
        filePath: "src/example.ts",
        appendContent: "append",
        edits: [{ oldString: "old", newString: "new" }],
      },
      error: "conflicting modes",
    },
    {
      label: "all empty fields have no mode",
      input: {
        filePath: "src/example.ts",
        appendContent: "",
        edits: [],
        symbol: "",
        content: "",
        oldString: "",
        newString: "",
        replaceAll: null,
        occurrence: null,
      },
      error: "exactly one of",
    },
    {
      label: "whole-schema sentinel report resolves to appendContent",
      input: {
        path: "src/example.ts",
        symbol: "",
        content: "",
        appendContent: "CONTENT IT APPENDS",
        edits: [
          {
            oldString: "",
            newString: "",
            replaceAll: false,
            occurrence: 1,
            startLine: 1,
            endLine: 1,
            content: "",
          },
        ],
      },
      expected: { path: "src/example.ts", appendContent: "CONTENT IT APPENDS" },
    },
    {
      label: "sentinel item alongside a real item keeps the real item",
      input: {
        path: "src/example.ts",
        edits: [
          { oldString: "", newString: "", content: "" },
          { oldString: "before", newString: "after" },
        ],
      },
      expected: { path: "src/example.ts", edits: [{ oldString: "before", newString: "after" }] },
    },
    {
      label: "line-range delete item is never treated as a sentinel",
      input: {
        path: "src/example.ts",
        edits: [{ startLine: 1, endLine: 1, content: "" }],
      },
      expected: { path: "src/example.ts", edits: [{ startLine: 1, endLine: 1, content: "" }] },
    },
    {
      label: "empty-old with real replacement is kept for the batch error",
      input: {
        path: "src/example.ts",
        edits: [{ oldString: "", newString: "real" }],
      },
      expected: { path: "src/example.ts", edits: [{ oldString: "", newString: "real" }] },
    },
    {
      label: "line-range edit removes embedded find/replace sentinels",
      input: {
        path: "src/example.ts",
        edits: [
          {
            content: "const value = new;",
            startLine: 14,
            endLine: 14,
            oldString: "",
            newString: "",
            replaceAll: false,
            occurrence: 1,
          },
        ],
      },
      expected: {
        path: "src/example.ts",
        edits: [{ content: "const value = new;", startLine: 14, endLine: 14 }],
      },
    },
  ];

  for (const { label, input, expected, error } of meaningfulModeCases) {
    test(label, () => {
      if (error) {
        expect(() => prepareCanonicalEditArguments("edit", input)).toThrow(error);
      } else {
        expect(prepareCanonicalEditArguments("edit", input)).toEqual(expected);
      }
    });
  }

  test("applies symmetric edit-item sentinel precedence", () => {
    const cases: Array<{
      label: string;
      item: Record<string, unknown>;
      expected?: Record<string, unknown>;
      error?: string;
    }> = [
      {
        label: "find/replace wins over empty range defaults",
        item: { oldString: "before", newString: "after", startLine: 1, endLine: 1, content: "" },
        expected: { oldString: "before", newString: "after" },
      },
      {
        label: "find/replace strips bare range boundaries",
        item: { oldString: "before", newString: "after", startLine: 1, endLine: 1 },
        expected: { oldString: "before", newString: "after" },
      },
      {
        label: "range replacement wins over empty find defaults",
        item: {
          startLine: 1,
          endLine: 1,
          content: "after",
          oldString: "",
          newString: "",
          replaceAll: false,
          occurrence: 1,
        },
        expected: { startLine: 1, endLine: 1, content: "after" },
      },
      {
        label: "bare range deletion remains a range edit",
        item: { startLine: 1, endLine: 1, content: "" },
        expected: { startLine: 1, endLine: 1, content: "" },
      },
      {
        label: "both meaningful payloads remain rejected",
        item: {
          oldString: "before",
          newString: "after",
          startLine: 1,
          endLine: 1,
          content: "after",
        },
        error: "mixes find/replace and line-range fields",
      },
      {
        label: "all default payloads are dropped",
        item: {
          oldString: "",
          newString: "",
          replaceAll: false,
          occurrence: 1,
          startLine: 1,
          endLine: 1,
          content: "",
        },
        error: "exactly one of",
      },
    ];

    for (const { label, item, expected, error } of cases) {
      const input = { path: "src/example.ts", edits: [item] };
      if (error) {
        expect(() => prepareCanonicalEditArguments("edit", input)).toThrow(error);
      } else {
        if (!expected) throw new Error(`${label}: expected normalized item is required`);
        expect(prepareCanonicalEditArguments("edit", input)).toEqual({
          path: "src/example.ts",
          edits: [expected],
        });
      }
    }
  });

  test("retains meaningful fields alongside line-range edits as mixed-mode errors", () => {
    const lineRange = { content: "replacement", startLine: 14, endLine: 14 };
    for (const findFields of [
      { oldString: "meaningful", newString: "" },
      { oldString: "", newString: "", replaceAll: true },
      { oldString: "", newString: "", occurrence: 2 },
    ]) {
      expect(() =>
        prepareCanonicalEditArguments("edit", {
          path: "src/example.ts",
          edits: [{ ...lineRange, ...findFields }],
        }),
      ).toThrow("mixes find/replace and line-range fields");
    }
  });

  test("treats null optional fields as absent at both edit boundaries", () => {
    const base = {
      path: "src/example.ts",
      edits: [{ oldString: "before", newString: "after" }],
    };
    for (const key of [
      "appendContent",
      "symbol",
      "content",
      "oldString",
      "newString",
      "replaceAll",
      "occurrence",
    ]) {
      expect(prepareCanonicalEditArguments("edit", { ...base, [key]: null })).toEqual(base);
    }
    expect(
      prepareCanonicalEditArguments("edit", {
        path: "src/example.ts",
        appendContent: "append",
        edits: null,
      }),
    ).toEqual({ path: "src/example.ts", appendContent: "append" });

    for (const key of [
      "newString",
      "replaceAll",
      "occurrence",
      "startLine",
      "endLine",
      "content",
    ]) {
      const input = {
        path: "src/example.ts",
        edits: [{ oldString: "before", newString: "after", [key]: null }],
      };
      const expectedItem =
        key === "newString" ? { oldString: "before" } : { oldString: "before", newString: "after" };
      expect(prepareCanonicalEditArguments("edit", input)).toEqual({
        path: "src/example.ts",
        edits: [expectedItem],
      });
    }
  });

  test("drops all-null edit items without hiding malformed non-null edits", () => {
    const nullItem = {
      oldString: null,
      newString: null,
      replaceAll: null,
      occurrence: null,
      startLine: null,
      endLine: null,
      content: null,
    };
    expect(() =>
      prepareCanonicalEditArguments("edit", { path: "src/example.ts", edits: [nullItem] }),
    ).toThrow("exactly one of");

    expect(
      prepareCanonicalEditArguments("edit", {
        path: "src/example.ts",
        edits: [nullItem, { oldString: "before", newString: "after" }],
      }),
    ).toEqual({
      path: "src/example.ts",
      edits: [{ oldString: "before", newString: "after" }],
    });

    expect(() =>
      prepareCanonicalEditArguments("edit", {
        path: "src/example.ts",
        edits: [{ oldString: null, newString: "replacement" }],
      }),
    ).toThrow("requires string 'oldString'");
  });

  test("strips null find fields from a legitimate line-range delete", () => {
    expect(
      prepareCanonicalEditArguments("edit", {
        path: "src/example.ts",
        edits: [
          {
            startLine: 1,
            endLine: 1,
            content: "",
            oldString: null,
            newString: null,
            replaceAll: null,
            occurrence: null,
          },
        ],
      }),
    ).toEqual({
      path: "src/example.ts",
      edits: [{ startLine: 1, endLine: 1, content: "" }],
    });
  });

  test.each(symbolModeValidationCases)(
    "matches Rust symbol-mode validation for $label",
    ({ arguments: rawArguments, message }) => {
      expect(() => prepareCanonicalEditArguments("edit", rawArguments)).toThrow(message);
    },
  );

  test("reports null symbol content with the property-specific steer", () => {
    expect(() =>
      prepareCanonicalEditArguments("edit", {
        path: "src/example.ts",
        symbol: "greetUser",
        content: null,
      }),
    ).toThrow(
      "edit: incomplete symbol mode: property 'content' is null. " +
        "Retry with `symbol` + `content`, or use `edits[]`.",
    );
  });

  test("applies mode conflict precedence before parsing stringified edits", () => {
    expect(() =>
      prepareCanonicalEditArguments("edit", {
        path: "src/main.ts",
        appendContent: "append",
        edits: "not-json",
      }),
    ).toThrow("conflicting modes");

    expect(() =>
      prepareCanonicalEditArguments("edit", {
        path: "src/main.ts",
        edits: "not-json",
      }),
    ).toThrow("valid JSON");
    expect(
      prepareCanonicalEditArguments("edit", {
        path: "src/main.ts",
        edits: '[{"oldString":"before","newString":"after"}]',
      }),
    ).toEqual({
      path: "src/main.ts",
      edits: [{ oldString: "before", newString: "after" }],
    });
    expect(() =>
      prepareCanonicalEditArguments("edit", {
        path: "src/main.ts",
        edits: "[]",
      }),
    ).toThrow("exactly one of");
  });

  test("keeps canonical-only path validation after edit contract validation", () => {
    expect(() =>
      prepareCanonicalEditArguments("edit", {
        path: 42,
        startLine: 1,
      }),
    ).toThrow("startLine");
    expect(() => prepareCanonicalEditArguments("edit", { path: 42 })).toThrow("exactly one of");
    expect(() =>
      prepareCanonicalEditArguments("edit", { path: 42, appendContent: "append" }),
    ).toThrow("'path'");
  });

  test("uses the retired-form error only at the OpenCode-prefixed raw boundary", () => {
    expect(() =>
      prepareCanonicalEditArguments("aft_edit", {
        mode: "write",
        file: "src/main.ts",
        content: "x",
      }),
    ).toThrow("retired");

    expect(() =>
      prepareCanonicalEditArguments("edit", { mode: "write", file: "src/main.ts" }),
    ).toThrow('Unrecognized keys: "file", "mode"');
  });

  test("normalizes item aliases and the complete scalar compatibility domains", () => {
    const replaceAllValues: unknown[] = [true, false, "true", "TRUE", "fAlSe", 1, 0, "1", "0"];
    for (const value of replaceAllValues) {
      const result = prepareCanonicalEditArguments("edit", {
        path: "src/main.ts",
        edits: [{ oldText: "before", newText: "after", replaceAll: value }],
      });
      const expected =
        value === true ||
        value === 1 ||
        value === "1" ||
        (typeof value === "string" && value.toLowerCase() === "true");
      expect(result.edits).toEqual([
        { oldString: "before", newString: "after", replaceAll: expected },
      ]);
    }

    const result = prepareCanonicalEditArguments("edit", {
      path: "src/main.ts",
      edits: [{ oldString: "before", oldText: "legacy", occurrence: " +01 " }],
    });
    expect(result.edits).toEqual([{ oldString: "before", occurrence: 1 }]);

    for (const value of [null, "", " \t"]) {
      const omitted = prepareCanonicalEditArguments("edit", {
        path: "src/main.ts",
        edits: [{ oldString: "before", occurrence: value }],
      });
      expect(omitted.edits).toEqual([{ oldString: "before" }]);
    }

    for (const value of ["0", "00", "+0", "1.0", "1e0", "0x1", "-1", "9007199254740992"]) {
      expect(() =>
        prepareCanonicalEditArguments("edit", {
          path: "src/main.ts",
          edits: [{ oldString: "before", occurrence: value }],
        }),
      ).toThrow("occurrence");
    }
  });
});
