/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { LSP_SYMBOL_KIND_MAP, queryLspHints } from "../lsp.js";

/**
 * Minimal mock for the OpenCode SDK client used by queryLspHints.
 * Only lsp.status() and find.symbols() are exercised.
 */
function createMockClient(options: {
  lspServers?: Array<{ id: string; name: string; root: string; status: string }>;
  symbols?: Array<{
    name: string;
    kind: number;
    location: {
      uri: string;
      range: {
        start: { line: number; character: number };
        end: { line: number; character: number };
      };
    };
  }>;
  lspError?: Error;
  symbolsError?: Error;
}): any {
  return {
    lsp: {
      status: async () => {
        if (options.lspError) throw options.lspError;
        return { data: options.lspServers ?? [] };
      },
    },
    find: {
      symbols: async (_opts: unknown) => {
        if (options.symbolsError) throw options.symbolsError;
        return { data: options.symbols ?? [] };
      },
    },
  };
}

describe("queryLspHints", () => {
  test("roots relative, absolute and URI hints without conflating same-name files", async () => {
    const root = process.cwd();
    const target = join(root, "src/left/util file.ts");
    const client = createMockClient({
      lspServers: [{ id: "ts", name: "typescript", root, status: "connected" }],
      symbols: [
        "src/left/util file.ts",
        target,
        pathToFileURL(target).href,
        "src/right/util file.ts",
      ].map((uri) => ({
        name: "handler",
        kind: 12,
        location: {
          uri,
          range: { start: { line: 1, character: 0 }, end: { line: 5, character: 0 } },
        },
      })),
    });
    const result = await queryLspHints(client, "handler", root);
    expect(result?.symbols.map((hint) => hint.file)).toEqual([
      pathToFileURL(target).href,
      pathToFileURL(target).href,
      pathToFileURL(target).href,
      pathToFileURL(join(root, "src/right/util file.ts")).href,
    ]);
  });

  test("omits hints outside the requested root instead of matching their suffix", async () => {
    const root = process.cwd();
    const client = createMockClient({
      lspServers: [{ id: "ts", name: "typescript", root, status: "connected" }],
      symbols: [
        "src/left/utils.ts",
        `${root}-outside/src/left/utils.ts`,
        `file://${root}-outside/src/left/utils.ts`,
        "../outside/src/left/utils.ts",
      ].map((uri) => ({
        name: "handler",
        kind: 12,
        location: {
          uri,
          range: { start: { line: 1, character: 0 }, end: { line: 5, character: 0 } },
        },
      })),
    });
    const result = await queryLspHints(client, "handler", root);
    expect(result?.symbols.map((hint) => hint.file)).toEqual([
      pathToFileURL(join(root, "src/left/utils.ts")).href,
    ]);
  });

  test("preserves literal percent and Unicode names in file URIs", async () => {
    const client = createMockClient({
      lspServers: [{ id: "ts", name: "typescript", root: "/project", status: "connected" }],
      symbols: ["data %20 café.ts", "file:///project/data%20%2520%20caf%C3%A9.ts"].map((uri) => ({
        name: "handler",
        kind: 12,
        location: {
          uri,
          range: { start: { line: 1, character: 0 }, end: { line: 5, character: 0 } },
        },
      })),
    });
    const result = await queryLspHints(client, "handler", "/project");
    expect(result?.symbols.map((hint) => hint.file)).toEqual([
      "file:///project/data%20%2520%20caf%C3%A9.ts",
      "file:///project/data%20%2520%20caf%C3%A9.ts",
    ]);
  });

  test("resolves Windows drive and UNC hints on any host", async () => {
    for (const [root, files, expected] of [
      [
        "C:\\project",
        ["src/utils.ts", "C:\\project\\src\\utils.ts", "file:///C:/project/src/utils.ts"],
        "file:///C:/project/src/utils.ts",
      ],
      [
        "\\\\server\\share\\project",
        [
          "src/utils.ts",
          "\\\\server\\share\\project\\src\\utils.ts",
          "file://server/share/project/src/utils.ts",
        ],
        "file://server/share/project/src/utils.ts",
      ],
    ] as const) {
      const client = createMockClient({
        lspServers: [{ id: "ts", name: "typescript", root, status: "connected" }],
        symbols: files.map((uri) => ({
          name: "handler",
          kind: 12,
          location: {
            uri,
            range: { start: { line: 1, character: 0 }, end: { line: 5, character: 0 } },
          },
        })),
      });
      const result = await queryLspHints(client, "handler", root);
      expect(result?.symbols.map((hint) => hint.file)).toEqual([expected, expected, expected]);
    }
  });

  test("omits relative hints without a root and non-file URIs", async () => {
    const client = createMockClient({
      lspServers: [{ id: "ts", name: "typescript", root: "/project", status: "connected" }],
      symbols: ["src/utils.ts", "https://example.com/utils.ts"].map((uri) => ({
        name: "handler",
        kind: 12,
        location: {
          uri,
          range: { start: { line: 1, character: 0 }, end: { line: 5, character: 0 } },
        },
      })),
    });
    expect(await queryLspHints(client, "handler")).toBeUndefined();
  });

  test("returns formatted hints when server is connected and symbols found", async () => {
    const client = createMockClient({
      lspServers: [{ id: "ts", name: "typescript", root: "/project", status: "connected" }],
      symbols: [
        {
          name: "processData",
          kind: 12, // Function
          location: {
            uri: "file:///project/src/utils.ts",
            range: { start: { line: 42, character: 0 }, end: { line: 55, character: 1 } },
          },
        },
        {
          name: "processData",
          kind: 6, // Method
          location: {
            uri: "file:///project/src/service.ts",
            range: { start: { line: 10, character: 2 }, end: { line: 20, character: 3 } },
          },
        },
      ],
    });

    const result = await queryLspHints(client, "processData");

    expect(result).toBeDefined();
    expect(result?.symbols).toHaveLength(2);

    expect(result?.symbols[0]).toEqual({
      name: "processData",
      file: "file:///project/src/utils.ts",
      line: 42,
      kind: "function",
    });

    expect(result?.symbols[1]).toEqual({
      name: "processData",
      file: "file:///project/src/service.ts",
      line: 10,
      kind: "method",
    });
  });

  test("returns undefined when no LSP server is connected", async () => {
    const client = createMockClient({
      lspServers: [{ id: "ts", name: "typescript", root: "/project", status: "error" }],
    });

    const result = await queryLspHints(client, "processData");
    expect(result).toBeUndefined();
  });

  test("returns undefined when no LSP servers exist", async () => {
    const client = createMockClient({ lspServers: [] });

    const result = await queryLspHints(client, "processData");
    expect(result).toBeUndefined();
  });

  test("returns undefined when API throws an error", async () => {
    const client = createMockClient({
      lspServers: [{ id: "ts", name: "typescript", root: "/project", status: "connected" }],
      symbolsError: new Error("LSP connection lost"),
    });

    const result = await queryLspHints(client, "processData");
    expect(result).toBeUndefined();
  });

  test("returns undefined when lsp.status() throws", async () => {
    const client = createMockClient({
      lspError: new Error("Network error"),
    });

    const result = await queryLspHints(client, "processData");
    expect(result).toBeUndefined();
  });

  test("returns undefined when symbols result is empty", async () => {
    const client = createMockClient({
      lspServers: [{ id: "ts", name: "typescript", root: "/project", status: "connected" }],
      symbols: [],
    });

    const result = await queryLspHints(client, "nonExistent");
    expect(result).toBeUndefined();
  });

  test("preserves absolute file URIs", async () => {
    const client = createMockClient({
      lspServers: [{ id: "ts", name: "typescript", root: "/project", status: "connected" }],
      symbols: [
        {
          name: "hello",
          kind: 12,
          location: {
            uri: "file:///home/user/project/src/main.ts",
            range: { start: { line: 0, character: 0 }, end: { line: 5, character: 1 } },
          },
        },
      ],
    });

    const result = await queryLspHints(client, "hello");
    expect(result).toBeDefined();
    expect(result?.symbols[0].file).toBe("file:///home/user/project/src/main.ts");
  });

  test("handles URIs without file:// prefix", async () => {
    const client = createMockClient({
      lspServers: [{ id: "ts", name: "typescript", root: "/project", status: "connected" }],
      symbols: [
        {
          name: "hello",
          kind: 12,
          location: {
            uri: "/project/src/main.ts",
            range: { start: { line: 0, character: 0 }, end: { line: 5, character: 1 } },
          },
        },
      ],
    });

    const result = await queryLspHints(client, "hello");
    expect(result).toBeDefined();
    expect(result?.symbols[0].file).toBe("file:///project/src/main.ts");
  });

  test("maps known SymbolKind numbers to AFT kind strings", async () => {
    const client = createMockClient({
      lspServers: [{ id: "ts", name: "typescript", root: "/project", status: "connected" }],
      symbols: [
        {
          name: "MyClass",
          kind: 5,
          location: {
            uri: "file:///a.ts",
            range: { start: { line: 0, character: 0 }, end: { line: 1, character: 0 } },
          },
        },
        {
          name: "myMethod",
          kind: 6,
          location: {
            uri: "file:///a.ts",
            range: { start: { line: 2, character: 0 }, end: { line: 3, character: 0 } },
          },
        },
        {
          name: "MyEnum",
          kind: 10,
          location: {
            uri: "file:///a.ts",
            range: { start: { line: 4, character: 0 }, end: { line: 5, character: 0 } },
          },
        },
        {
          name: "MyInterface",
          kind: 11,
          location: {
            uri: "file:///a.ts",
            range: { start: { line: 6, character: 0 }, end: { line: 7, character: 0 } },
          },
        },
        {
          name: "myFunc",
          kind: 12,
          location: {
            uri: "file:///a.ts",
            range: { start: { line: 8, character: 0 }, end: { line: 9, character: 0 } },
          },
        },
        {
          name: "MyStruct",
          kind: 23,
          location: {
            uri: "file:///a.ts",
            range: { start: { line: 10, character: 0 }, end: { line: 11, character: 0 } },
          },
        },
      ],
    });

    const result = await queryLspHints(client, "test");
    expect(result).toBeDefined();

    expect(result?.symbols[0].kind).toBe("class");
    expect(result?.symbols[1].kind).toBe("method");
    expect(result?.symbols[2].kind).toBe("enum");
    expect(result?.symbols[3].kind).toBe("interface");
    expect(result?.symbols[4].kind).toBe("function");
    expect(result?.symbols[5].kind).toBe("struct");
  });

  test("omits kind for unknown SymbolKind numbers", async () => {
    const client = createMockClient({
      lspServers: [{ id: "ts", name: "typescript", root: "/project", status: "connected" }],
      symbols: [
        {
          name: "MY_CONST",
          kind: 14, // Constant — not in our map
          location: {
            uri: "file:///a.ts",
            range: { start: { line: 0, character: 0 }, end: { line: 1, character: 0 } },
          },
        },
      ],
    });

    const result = await queryLspHints(client, "MY_CONST");
    expect(result).toBeDefined();
    expect(result?.symbols[0].kind).toBeUndefined();
    expect(result?.symbols[0].name).toBe("MY_CONST");
    expect(result?.symbols[0].file).toBe("file:///a.ts");
    expect(result?.symbols[0].line).toBe(0);
  });

  test("passes directory parameter when provided", async () => {
    let capturedQuery: Record<string, unknown> | null = null;
    const client: any = {
      lsp: {
        status: async () => ({
          data: [{ id: "ts", name: "typescript", root: "/project", status: "connected" }],
        }),
      },
      find: {
        symbols: async (opts: { query: { query: string; directory?: string } }) => {
          capturedQuery = opts;
          return {
            data: [
              {
                name: "hello",
                kind: 12,
                location: {
                  uri: "file:///a.ts",
                  range: { start: { line: 0, character: 0 }, end: { line: 1, character: 0 } },
                },
              },
            ],
          };
        },
      },
    };

    await queryLspHints(client, "hello", "/project/src");
    expect(capturedQuery).toEqual({
      query: { query: "hello", directory: "/project/src" },
    });
  });
});

describe("LSP_SYMBOL_KIND_MAP", () => {
  test("contains all expected mappings", () => {
    expect(LSP_SYMBOL_KIND_MAP[5]).toBe("class");
    expect(LSP_SYMBOL_KIND_MAP[6]).toBe("method");
    expect(LSP_SYMBOL_KIND_MAP[10]).toBe("enum");
    expect(LSP_SYMBOL_KIND_MAP[11]).toBe("interface");
    expect(LSP_SYMBOL_KIND_MAP[12]).toBe("function");
    expect(LSP_SYMBOL_KIND_MAP[23]).toBe("struct");
  });

  test("has exactly 6 entries", () => {
    expect(Object.keys(LSP_SYMBOL_KIND_MAP)).toHaveLength(6);
  });
});
