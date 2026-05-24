import type { PluginInput } from "@opencode-ai/plugin";
import { sessionWarn } from "./logger.js";

/** Wire format for a single LSP symbol hint sent to the binary. */
interface LspSymbolHint {
  name: string;
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
      // Convert file URI to path (handles Windows file:///C:/path correctly)
      let file = sym.location.uri;
      if (file.startsWith("file://")) {
        try {
          file = new URL(file).pathname;
          // On Windows, URL.pathname gives /C:/path — strip leading slash
          if (process.platform === "win32" && /^\/[A-Za-z]:/.test(file)) {
            file = file.slice(1);
          }
        } catch {
          file = file.slice(7);
        }
      }

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

    return { symbols: hints };
  } catch (err) {
    sessionWarn(sessionId, `LSP query failed for "${symbolName}": ${(err as Error).message}`);
    return undefined;
  }
}
