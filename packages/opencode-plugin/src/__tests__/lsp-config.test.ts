/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import { AftConfigSchema, resolveLspConfigForConfigure } from "../config.js";

describe("lsp configure forwarding", () => {
  test("converts object-map lsp.servers into Rust configure array and strips dots", () => {
    const config = AftConfigSchema.parse({
      lsp: {
        servers: {
          tinymist: {
            extensions: [".typ", "typ"],
            binary: "tinymist",
            args: ["serve"],
            root_markers: [".git", "typst.toml"],
            disabled: true,
          },
        },
      },
    });

    expect(resolveLspConfigForConfigure(config)).toEqual({
      lsp_servers: [
        {
          id: "tinymist",
          extensions: ["typ", "typ"],
          binary: "tinymist",
          args: ["serve"],
          root_markers: [".git", "typst.toml"],
          disabled: true,
        },
      ],
    });
  });

  test('python="ty" enables ty and disables built-in python (pyright)', () => {
    const config = AftConfigSchema.parse({
      lsp: {
        disabled: ["yamlls"],
        python: "ty",
      },
    });

    expect(resolveLspConfigForConfigure(config)).toEqual({
      experimental_lsp_ty: true,
      // "python" is the Rust-side ServerKind id for the built-in Pyright server.
      disabled_lsp: ["yamlls", "python"],
    });
  });

  test('python="pyright" disables ty even when ty was explicitly enabled', () => {
    const config = AftConfigSchema.parse({
      experimental: { lsp_ty: true },
      lsp: { python: "pyright" },
    });

    expect(resolveLspConfigForConfigure(config)).toEqual({
      experimental_lsp_ty: false,
      disabled_lsp: ["ty"],
    });
  });

  test('python="auto" leaves ty and disabled ids unchanged', () => {
    const config = AftConfigSchema.parse({
      experimental: { lsp_ty: false },
      lsp: {
        disabled: ["pyright"],
        python: "auto",
      },
    });

    expect(resolveLspConfigForConfigure(config)).toEqual({
      experimental_lsp_ty: false,
      disabled_lsp: ["pyright"],
    });
  });

  test("default python auto is a no-op", () => {
    const config = AftConfigSchema.parse({
      lsp: {},
    });

    expect(resolveLspConfigForConfigure(config)).toEqual({});
  });

  test("disabled ids union with python resolution", () => {
    const config = AftConfigSchema.parse({
      lsp: {
        disabled: ["yamlls"],
        python: "ty",
      },
    });

    // python="ty" adds the built-in python (Pyright) id to the user's disabled set.
    expect(resolveLspConfigForConfigure(config).disabled_lsp).toEqual(["yamlls", "python"]);
  });
});
