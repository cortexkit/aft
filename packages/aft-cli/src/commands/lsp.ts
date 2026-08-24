import { existsSync, readdirSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { readConfigTiers } from "@cortexkit/aft-bridge";

import type { HarnessAdapter } from "../adapters/types.js";
import type { AftRequest, sendAftRequest } from "../lib/aft-bridge.js";
import { sendAftRequests } from "../lib/aft-bridge.js";
import { findAftBinary } from "../lib/binary-probe.js";
import { CLI } from "../lib/cli.js";
import { resolveAdaptersForCommand } from "../lib/harness-select.js";
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
  diagnostics_complete?: boolean;
  diagnostics_gaps?: Array<{ server_id: string; reason: string }>;
  diagnostics_count?: number;
  diagnostics?: LspDiagnostic[];
  typescript_package_warning?: string;
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

const PROJECT_ROOT_MARKERS = [
  ".git",
  "package.json",
  "Cargo.toml",
  "pyproject.toml",
  "requirements.txt",
  "setup.py",
  "go.mod",
  "deno.json",
  "bun.lock",
  "bun.lockb",
  "pnpm-lock.yaml",
  "yarn.lock",
  "tsconfig.json",
];

export function findProjectRootForFile(
  filePath: string,
  fallbackCwd: string = process.cwd(),
): string {
  const resolvedFile = resolve(fallbackCwd, filePath);
  let dir = dirname(resolvedFile);

  try {
    if (existsSync(resolvedFile) && statSync(resolvedFile).isDirectory()) {
      dir = resolvedFile;
    }
  } catch {
    dir = dirname(resolvedFile);
  }

  while (true) {
    if (PROJECT_ROOT_MARKERS.some((marker) => existsSync(join(dir, marker)))) {
      return dir;
    }
    const parent = dirname(dir);
    if (parent === dir) return resolve(fallbackCwd);
    dir = parent;
  }
}

export function printLspDoctorHelp(): void {
  console.log(`Usage: ${CLI} doctor lsp <file> [--harness opencode|pi]`);
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

  const resolvedFile = resolve(file);
  const projectRoot = findProjectRootForFile(resolvedFile);
  const config: AftRequest = buildConfigureParams(adapter, projectRoot);
  const inspectRequest: AftRequest = {
    id: "doctor-lsp-inspect",
    command: "lsp_inspect",
    file: resolvedFile,
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

  const inspection = inspect as LspInspectResponse;
  const typescriptWarning = typescriptPackageWarning(inspection);
  console.log(
    renderLspInspection(file, {
      ...inspection,
      ...(typescriptWarning ? { typescript_package_warning: typescriptWarning } : {}),
    }),
  );
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

  if (response.typescript_package_warning) {
    lines.push("");
    lines.push(`Warning: ${response.typescript_package_warning}`);
  }

  const diagnostics = response.diagnostics ?? [];
  lines.push("");
  const diagnosticsComplete = response.diagnostics_complete !== false;
  const observed = response.diagnostics_count ?? diagnostics.length;
  lines.push(
    diagnosticsComplete
      ? `Diagnostics (${observed} found):`
      : `Diagnostics incomplete (${observed} observed):`,
  );
  if (diagnostics.length === 0) {
    lines.push(diagnosticsComplete ? "  (none)" : "  (none observed yet)");
  }
  for (const diagnostic of diagnostics) {
    lines.push(
      `  ${diagnostic.file}:${diagnostic.line}:${diagnostic.column} [${diagnostic.severity}] ${diagnostic.message}`,
    );
  }
  for (const gap of response.diagnostics_gaps ?? []) {
    lines.push(`  Gap: ${gap.server_id}: ${gap.reason}`);
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
  // P1 config relocation: core config (incl. LSP servers/disabled/python) is now
  // resolved + trust-stripped in AFT-core from raw `config: [{tier, source, doc}]`
  // tiers — the flat lsp_servers/disabled_lsp/experimental_lsp_ty params are no
  // longer read by handle_configure. Send the raw user+project tiers so the user's
  // custom/disabled LSP settings are honored again (and project-tier LSP settings
  // are stripped by the resolver, same as the plugins). lsp_paths_extra is
  // process-state (the install cache dirs) and stays a flat param.
  const userConfigPath = adapter.detectConfigPaths().aftConfig;
  const dir = adapter.kind === "pi" ? ".pi" : ".opencode";
  const projectJsonc = join(projectRoot, dir, "aft.jsonc");
  const projectJson = join(projectRoot, dir, "aft.json");
  const projectConfigPath = existsSync(projectJsonc) ? projectJsonc : projectJson;
  return {
    id: "doctor-lsp-configure",
    command: "configure",
    project_root: projectRoot,
    harness: adapter.kind,
    config: readConfigTiers({ userConfigPath, projectConfigPath }),
    lsp_paths_extra: inferLspPathsExtra({}),
  };
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
    const locations =
      server.kind === "python" || server.kind === "ty"
        ? "workspace virtualenv, node_modules/.bin, lsp_paths_extra, or PATH"
        : "node_modules/.bin, lsp_paths_extra, or PATH";
    return `${server.binary_name} (NOT FOUND in ${locations})`;
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

export function typescriptPackageWarning(response: LspInspectResponse): string | null {
  const typescriptServerSpawned = response.matching_servers?.some(
    (server) => server.id === "typescript" && server.spawn_status === "ok",
  );
  const diagnosticsCount = response.diagnostics_count ?? response.diagnostics?.length ?? 0;
  if (!typescriptServerSpawned || !response.project_root || diagnosticsCount > 0) return null;

  try {
    createRequire(join(response.project_root, "package.json")).resolve("typescript");
    return null;
  } catch {
    return "typescript package not resolvable from project — server will produce no diagnostics";
  }
}

function installHint(binaryName: string): string {
  if (binaryName === "ty") return "Install with `uv tool install ty` or `pip install ty`.";
  if (binaryName === "pyright-langserver") return "Install with `npm install -g pyright`.";
  return `Install ${binaryName} and ensure it is on PATH or in lsp_paths_extra.`;
}
