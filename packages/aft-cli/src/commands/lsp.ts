import { existsSync, readdirSync, statSync } from "node:fs";
import { join, resolve } from "node:path";

import type { HarnessAdapter } from "../adapters/types.js";
import type { AftRequest, sendAftRequest } from "../lib/aft-bridge.js";
import { sendAftRequests } from "../lib/aft-bridge.js";
import { findAftBinary } from "../lib/binary-probe.js";
import { resolveAdaptersForCommand } from "../lib/harness-select.js";
import { readJsoncFile } from "../lib/jsonc.js";
import { getAftLspBinariesDir, getAftLspPackagesDir } from "../lib/paths.js";
import { log } from "../lib/prompts.js";
import { getSelfVersion } from "../lib/self-version.js";

export interface LspDoctorOptions {
  argv: string[];
  sendRequest?: typeof sendAftRequest;
  sendRequests?: typeof sendAftRequests;
  findBinary?: typeof findAftBinary;
  resolveAdapters?: typeof resolveAdaptersForCommand;
}

export interface LspInspectResponse {
  success: boolean;
  code?: string;
  message?: string;
  file?: string;
  extension?: string;
  project_root?: string | null;
  experimental_lsp_ty?: boolean;
  disabled_lsp?: string[];
  lsp_paths_extra?: string[];
  matching_servers?: LspServerInspection[];
  diagnostics_count?: number;
  diagnostics?: LspDiagnostic[];
}

export interface LspServerInspection {
  id: string;
  name: string;
  kind: string;
  extensions: string[];
  root_markers: string[];
  binary_name: string;
  binary_path: string | null;
  binary_source: "path" | "lsp_paths_extra" | "project_node_modules" | "not_found" | string;
  workspace_root: string | null;
  spawn_status: string;
  args: string[];
}

export interface LspDiagnostic {
  file: string;
  line: number;
  column: number;
  end_line?: number;
  end_column?: number;
  severity: string;
  message: string;
  code?: string | null;
  source?: string | null;
}

export function printLspDoctorHelp(): void {
  console.log("Usage: aft doctor lsp <file> [--harness opencode|pi]");
  console.log("");
  console.log("Inspect what AFT's LSP layer would do for a file.");
}

export async function runLspDoctor(options: LspDoctorOptions): Promise<number> {
  const file = parseFileArg(options.argv);
  if (!file) {
    printLspDoctorHelp();
    return 1;
  }

  const resolveAdapters = options.resolveAdapters ?? resolveAdaptersForCommand;
  const adapters = await resolveAdapters(options.argv, {
    allowMulti: false,
    verb: "inspect LSP for",
  });
  const adapter = adapters[0];
  if (!adapter) {
    log.error("No harness selected.");
    return 1;
  }

  const findBinary = options.findBinary ?? findAftBinary;
  const binary = findBinary(getSelfVersion());
  if (!binary) {
    log.error(
      "Could not find the aft binary in the cache, platform package, PATH, or ~/.cargo/bin.",
    );
    return 1;
  }

  const projectRoot = resolve(process.cwd());
  const config: AftRequest = buildConfigureParams(adapter, projectRoot);
  const inspectRequest: AftRequest = {
    id: "doctor-lsp-inspect",
    command: "lsp_inspect",
    file: resolve(file),
  };
  const responses = options.sendRequests
    ? await options.sendRequests(binary, [config, inspectRequest])
    : options.sendRequest
      ? [await options.sendRequest(binary, inspectRequest)]
      : await sendAftRequests(binary, [config, inspectRequest]);
  const configure = responses.length > 1 ? responses[0] : undefined;
  if (configure && !configure.success) {
    log.error(configure.message ?? configure.code ?? "configure failed");
    return 1;
  }
  const inspect = responses[responses.length - 1];

  if (!inspect) {
    log.error("aft exited before returning lsp_inspect response");
    return 1;
  }

  if (!inspect.success) {
    log.error(inspect.message ?? inspect.code ?? "lsp_inspect failed");
    return 1;
  }

  console.log(renderLspInspection(file, inspect as LspInspectResponse));
  return 0;
}

export function renderLspInspection(inputFile: string, response: LspInspectResponse): string {
  const lines: string[] = [];
  lines.push(`LSP inspection — ${inputFile}`);
  lines.push("");
  lines.push(`Resolved file: ${response.file ?? "(unknown)"}`);
  lines.push(`File extension: ${response.extension ? `.${response.extension}` : "(none)"}`);
  lines.push(`Project root: ${response.project_root ?? "(not configured)"}`);
  lines.push("");
  lines.push(
    `Active config: experimental_lsp_ty=${response.experimental_lsp_ty === true ? "true" : "false"}, disabled_lsp=${formatList(response.disabled_lsp ?? [])}`,
  );
  lines.push(`lsp_paths_extra: ${formatList(response.lsp_paths_extra ?? [])}`);
  lines.push("");
  lines.push("Server attempts:");

  const active = new Set(response.matching_servers?.map((server) => server.id) ?? []);
  for (const id of response.disabled_lsp ?? []) {
    if (!active.has(id)) {
      lines.push(`  • ${id}: disabled by config`);
    }
  }

  const servers = response.matching_servers ?? [];
  if (servers.length === 0) {
    lines.push("  (no registered LSP servers match this file extension)");
  }
  for (const server of servers) {
    const ok = server.spawn_status === "ok";
    lines.push(`  ${ok ? "✓" : "✗"} ${server.id}`);
    lines.push(`    Binary: ${formatBinary(server)}`);
    lines.push(`    Workspace root: ${formatWorkspaceRoot(server)}`);
    lines.push(`    Args: ${JSON.stringify(server.args)}`);
    lines.push(`    Status: ${formatSpawnStatus(server)}`);
    if (server.binary_source === "not_found") {
      lines.push(`    Action: ${installHint(server.binary_name)}`);
    }
  }

  const diagnostics = response.diagnostics ?? [];
  lines.push("");
  lines.push(`Diagnostics (${response.diagnostics_count ?? diagnostics.length} found):`);
  if (diagnostics.length === 0) {
    lines.push("  (none)");
  }
  for (const diagnostic of diagnostics) {
    lines.push(
      `  ${diagnostic.file}:${diagnostic.line}:${diagnostic.column} [${diagnostic.severity}] ${diagnostic.message}`,
    );
  }

  return lines.join("\n");
}

function parseFileArg(argv: string[]): string | null {
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--harness") {
      i += 1;
      continue;
    }
    if (arg.startsWith("--")) continue;
    return arg;
  }
  return null;
}

function buildConfigureParams(adapter: HarnessAdapter, projectRoot: string): AftRequest {
  const paths = adapter.detectConfigPaths();
  const userConfig = readJsoncFile(paths.aftConfig).value ?? {};
  const projectConfig = readProjectConfig(adapter.kind, projectRoot);
  const merged = mergeConfig(userConfig, projectConfig);
  const lsp = isRecord(merged.lsp) ? merged.lsp : {};
  const lspConfig = resolveLspConfig(merged);
  return {
    id: "doctor-lsp-configure",
    command: "configure",
    project_root: projectRoot,
    ...lspConfig,
    lsp_paths_extra: inferLspPathsExtra(lsp),
  };
}

function readProjectConfig(kind: string, projectRoot: string): Record<string, unknown> {
  const dir = kind === "pi" ? ".pi" : ".opencode";
  const jsonc = join(projectRoot, dir, "aft.jsonc");
  const json = join(projectRoot, dir, "aft.json");
  if (existsSync(jsonc)) return readJsoncFile(jsonc).value ?? {};
  if (existsSync(json)) return readJsoncFile(json).value ?? {};
  return {};
}

function mergeConfig(
  userConfig: Record<string, unknown>,
  projectConfig: Record<string, unknown>,
): Record<string, unknown> {
  const userLsp = isRecord(userConfig.lsp) ? userConfig.lsp : {};
  const projectLsp = isRecord(projectConfig.lsp) ? projectConfig.lsp : {};
  return {
    ...userConfig,
    ...projectConfig,
    lsp: {
      ...userLsp,
      ...projectLsp,
      ...(isRecord(userLsp.servers) ? { servers: userLsp.servers } : {}),
      ...(Array.isArray(userLsp.disabled) ? { disabled: userLsp.disabled } : {}),
    },
  };
}

function resolveLspConfig(config: Record<string, unknown>): Record<string, unknown> {
  const lsp = isRecord(config.lsp) ? config.lsp : {};
  const disabled = new Set<string>();
  for (const entry of Array.isArray(lsp.disabled) ? lsp.disabled : []) {
    if (typeof entry === "string") disabled.add(entry.toLowerCase());
  }

  let experimentalTy =
    typeof config.experimental_lsp_ty === "boolean" ? config.experimental_lsp_ty : undefined;
  if (lsp.python === "ty") {
    experimentalTy = true;
    disabled.add("python");
  } else if (lsp.python === "pyright") {
    experimentalTy = false;
    disabled.add("ty");
  }

  const result: Record<string, unknown> = {};
  if (experimentalTy !== undefined) result.experimental_lsp_ty = experimentalTy;
  if (disabled.size > 0) result.disabled_lsp = [...disabled];
  const servers = resolveCustomServers(lsp.servers);
  if (servers.length > 0) result.lsp_servers = servers;
  return result;
}

function resolveCustomServers(servers: unknown): Record<string, unknown>[] {
  if (!isRecord(servers)) return [];
  return Object.entries(servers)
    .filter(([, server]) => isRecord(server))
    .map(([id, server]) => {
      const entry = server as Record<string, unknown>;
      return {
        id,
        extensions: Array.isArray(entry.extensions) ? entry.extensions : [],
        binary: typeof entry.binary === "string" ? entry.binary : "",
        args: Array.isArray(entry.args) ? entry.args : [],
        root_markers: Array.isArray(entry.root_markers) ? entry.root_markers : [".git"],
        disabled: entry.disabled === true,
        ...(isRecord(entry.env) ? { env: entry.env } : {}),
        ...(entry.initialization_options !== undefined
          ? { initialization_options: entry.initialization_options }
          : {}),
      };
    })
    .filter((server) => typeof server.binary === "string" && server.binary.length > 0);
}

function inferLspPathsExtra(_lsp: Record<string, unknown>): string[] {
  const paths = new Set<string>();
  for (const entry of childDirs(getAftLspPackagesDir())) {
    paths.add(join(entry, "node_modules", ".bin"));
  }
  for (const entry of childDirs(getAftLspBinariesDir())) {
    paths.add(join(entry, "bin"));
  }
  return [...paths];
}

function childDirs(path: string): string[] {
  if (!existsSync(path)) return [];
  try {
    return readdirSync(path)
      .map((entry) => join(path, entry))
      .filter((entry) => {
        try {
          return statSync(entry).isDirectory();
        } catch {
          return false;
        }
      });
  } catch {
    return [];
  }
}

function formatBinary(server: LspServerInspection): string {
  if (!server.binary_path) {
    return `${server.binary_name} (NOT FOUND on PATH or in lsp_paths_extra)`;
  }
  return `${server.binary_path} (found via ${server.binary_source})`;
}

function formatWorkspaceRoot(server: LspServerInspection): string {
  if (!server.workspace_root) {
    return `(not found; markers: ${server.root_markers.join(", ") || "none"})`;
  }
  return `${server.workspace_root} (markers: ${server.root_markers.join(", ") || "none"})`;
}

function formatSpawnStatus(server: LspServerInspection): string {
  if (server.spawn_status === "ok") return "spawned successfully";
  if (server.spawn_status === "binary_not_installed") return "binary not installed";
  if (server.spawn_status === "no_root_marker") return "no workspace root marker found";
  return server.spawn_status;
}

function formatList(values: string[]): string {
  return values.length === 0 ? "(none)" : values.join(", ");
}

function installHint(binaryName: string): string {
  if (binaryName === "ty") return "Install with `uv tool install ty` or `pip install ty`.";
  if (binaryName === "pyright-langserver") return "Install with `npm install -g pyright`.";
  return `Install ${binaryName} and ensure it is on PATH or in lsp_paths_extra.`;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
