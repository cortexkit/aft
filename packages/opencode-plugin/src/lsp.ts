import { isAbsolute, relative, resolve, win32 } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import type { PluginInput } from "@opencode-ai/plugin";
import { sessionWarn } from "./logger.js";

/** Wire format for a single LSP symbol hint sent to the binary. */
interface LspSymbolHint {
  name: string;
  /** Absolute file URI, preserving percent escapes until filesystem decoding. */
  file: string;
  line: number;
  kind?: string;
}

/** Wire format for the lsp_hints field in bridge requests. */
export interface LspHints {
  symbols: LspSymbolHint[];
}

/**
 * Maps LSP SymbolKind numbers to AFT kind strings.
 * Only kinds relevant to AFT disambiguation are included.
 * @see https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#symbolKind
 */
export const LSP_SYMBOL_KIND_MAP: Record<number, string> = {
  5: "class",
  6: "method",
  10: "enum",
  11: "interface",
  12: "function",
  23: "struct",
};

function symbolFileUri(file: string, directory?: string): string | undefined {
  try {
    const windowsPath = (path: string) => /^[A-Za-z]:[\\/]/.test(path) || path.startsWith("\\\\");
    const uri = /^file:/i.test(file) ? new URL(file) : undefined;
    const windows =
      windowsPath(file) ||
      (uri !== undefined &&
        ((uri.hostname !== "" && uri.hostname !== "localhost") ||
          /^\/[A-Za-z]:\//.test(uri.pathname))) ||
      (directory !== undefined && windowsPath(directory));
    const paths = windows ? win32 : { isAbsolute, relative, resolve };
    if (/^file:/i.test(file)) {
      file = fileURLToPath(file, { windows });
    } else if (!windowsPath(file) && /^[A-Za-z][A-Za-z0-9+.-]*:/.test(file)) {
      return undefined;
    }
    if (!paths.isAbsolute(file)) {
      // Relative SDK paths belong to the explicit query root, never the plugin cwd.
      if (!directory || !paths.isAbsolute(directory)) return undefined;
      file = paths.resolve(directory, file);
    }
    if (directory) {
      if (!paths.isAbsolute(directory)) return undefined;
      const within = paths.relative(directory, file);
      if (
        within === ".." ||
        within.startsWith(`..${windows ? "\\" : "/"}`) ||
        paths.isAbsolute(within)
      ) {
        return undefined;
      }
    }
    return pathToFileURL(file, { windows }).href;
  } catch {
    return undefined;
  }
}

/**
 * Query the OpenCode LSP for workspace symbols matching `symbolName`.
 *
 * Returns formatted hints for the binary's `lsp_hints` field, or `undefined` if:
 * - No LSP server is connected
 * - The API call fails
 * - No symbols match
 *
 * Failures are silent at the caller level — the binary falls back to
 * tree-sitter-only disambiguation when `lsp_hints` is absent.
 */
export async function queryLspHints(
  client: PluginInput["client"],
  symbolName: string,
  directory?: string,
  sessionId?: string,
): Promise<LspHints | undefined> {
  try {
    // Check if any LSP server is connected
    const statusResult = await client.lsp.status();
    const servers = statusResult.data;
    if (!servers || !servers.some((s) => s.status === "connected")) {
      return undefined;
    }

    // Query workspace symbols
    const query: { query: string; directory?: string } = { query: symbolName };
    if (directory) {
      query.directory = directory;
    }
    const symbolsResult = await client.find.symbols({ query });
    const symbols = symbolsResult.data;
    if (!symbols || symbols.length === 0) {
      return undefined;
    }

    // Map to wire format
    const hints: LspSymbolHint[] = [];
    for (const sym of symbols) {
      const file = symbolFileUri(sym.location.uri, directory);
      if (!file) continue;

      const hint: LspSymbolHint = {
        name: sym.name,
        file,
        line: sym.location.range.start.line,
      };

      const kind = LSP_SYMBOL_KIND_MAP[sym.kind];
      if (kind) {
        hint.kind = kind;
      }

      hints.push(hint);
    }

    return hints.length > 0 ? { symbols: hints } : undefined;
  } catch (err) {
    sessionWarn(sessionId, `LSP query failed for "${symbolName}": ${(err as Error).message}`);
    return undefined;
  }
}
