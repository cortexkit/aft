import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { relative, resolve } from "node:path";

import type { HarnessAdapter } from "../adapters/types.js";
import type { AftRequest } from "../lib/aft-bridge.js";
import { sendAftRequests } from "../lib/aft-bridge.js";
import { findAftBinary } from "../lib/binary-probe.js";
import { resolveAdaptersForCommand } from "../lib/harness-select.js";
import { log, selectMany } from "../lib/prompts.js";
import { getSelfVersion } from "../lib/self-version.js";

export interface DoctorFiltersOptions {
  argv: string[];
  sendRequests?: typeof sendAftRequests;
  findBinary?: typeof findAftBinary;
  resolveAdapters?: typeof resolveAdaptersForCommand;
  selectMany?: typeof selectMany;
}

export interface FilterEntry {
  name: string;
  source:
    | "builtin"
    | "user"
    | "project"
    | "builtin_invalid"
    | "user_invalid"
    | "project_invalid"
    | string;
  source_path: string | null;
  matches: string[];
  description: string | null;
  content: string;
  trusted: boolean | null;
  error?: string;
}

export interface ListFiltersResponse {
  success: boolean;
  code?: string;
  message?: string;
  filters?: FilterEntry[];
  user_dir?: string | null;
  project_dir?: string | null;
  trusted_projects?: string[];
  project_dir_exists?: boolean;
}

type Mode =
  | { kind: "list" }
  | { kind: "show"; name: string }
  | { kind: "trust"; list: boolean }
  | { kind: "untrust" }
  | { kind: "help" };

export function printDoctorFiltersHelp(): void {
  console.log("Usage: aft doctor filters [--show <name>] [trust|untrust]");
  console.log("");
  console.log("Inspect declarative TOML compression filters.");
  console.log("");
  console.log("Commands:");
  console.log("  aft doctor filters                 List built-in, user, and project filters");
  console.log("  aft doctor filters --show <name>   Show resolved TOML for a filter");
  console.log("  aft doctor filters trust           Trust current project's .aft/filters");
  console.log("  aft doctor filters trust --list    List trusted filter project paths");
  console.log("  aft doctor filters untrust         Remove trusted project paths");
}

export async function runDoctorFilters(options: DoctorFiltersOptions): Promise<number> {
  const mode = parseMode(options.argv);
  if (mode.kind === "help") {
    printDoctorFiltersHelp();
    return 0;
  }

  const resolveAdapters = options.resolveAdapters ?? resolveAdaptersForCommand;
  const adapters = await resolveAdapters(options.argv, {
    allowMulti: false,
    verb: "inspect filters for",
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
  const list = await listFilters(
    binary,
    adapter,
    projectRoot,
    options.sendRequests ?? sendAftRequests,
  );
  if (!list.success) {
    log.error(list.message ?? list.code ?? "list_filters failed");
    return 1;
  }
  list.project_dir_exists = list.project_dir ? existsSync(list.project_dir) : false;

  if (mode.kind === "list") {
    console.log(renderFilterList(list, projectRoot));
    return 0;
  }
  if (mode.kind === "show") {
    const rendered = renderFilterShow(list, mode.name, projectRoot);
    if (!rendered) {
      log.error(`Filter not found: ${mode.name}`);
      return 1;
    }
    console.log(rendered);
    return 0;
  }
  if (mode.kind === "trust") {
    if (mode.list) {
      console.log(renderTrustedProjects(list.trusted_projects ?? []));
      return 0;
    }
    return runTrustFlow(binary, list, adapter, projectRoot, options);
  }
  return runUntrustFlow(binary, list, adapter, projectRoot, options);
}

async function listFilters(
  binary: string,
  adapter: HarnessAdapter,
  projectRoot: string,
  sendRequests: typeof sendAftRequests,
): Promise<ListFiltersResponse> {
  const responses = await sendRequests(binary, [
    buildConfigureRequest(adapter, projectRoot),
    { id: "doctor-filters-list", command: "list_filters" },
  ]);
  const configure = responses[0];
  if (configure && !configure.success) return configure as ListFiltersResponse;
  return (responses[1] ?? {
    success: false,
    message: "aft exited before list_filters",
  }) as ListFiltersResponse;
}

function buildConfigureRequest(adapter: HarnessAdapter, projectRoot: string): AftRequest {
  return {
    id: "doctor-filters-configure",
    command: "configure",
    project_root: projectRoot,
    storage_dir: adapter.getStorageDir(),
  };
}

async function runTrustFlow(
  binary: string,
  list: ListFiltersResponse,
  adapter: HarnessAdapter,
  projectRoot: string,
  options: DoctorFiltersOptions,
): Promise<number> {
  const filters = (list.filters ?? []).filter(
    (filter) => filter.source.startsWith("project") && filter.trusted === false,
  );
  if (filters.length === 0) {
    console.log(`No untrusted project filters in ${projectRoot}.`);
    return 0;
  }
  const prompt = options.selectMany ?? selectMany;
  const selected = await prompt(
    "Trust project filters?",
    filters.map((filter) => ({
      label: filter.error ? `${filter.name} (invalid: ${filter.error})` : filter.name,
      value: filter.name,
      hint: formatSourcePath(filter.source_path, projectRoot),
    })),
    filters.map((filter) => filter.name),
    false,
  );
  if (selected.length === 0) {
    console.log("Trusted 0 project(s). Restart AFT (or reconfigure) for filters to take effect.");
    return 0;
  }
  const sendRequests = options.sendRequests ?? sendAftRequests;
  const responses = await sendRequests(binary, [
    buildConfigureRequest(adapter, projectRoot),
    { id: "doctor-filters-trust", command: "trust_filter_project", project_root: projectRoot },
  ]);
  const trust = responses[responses.length - 1];
  if (!trust?.success) {
    log.error(trust?.message ?? trust?.code ?? "trust_filter_project failed");
    return 1;
  }
  console.log("Trusted 1 project(s). Restart AFT (or reconfigure) for filters to take effect.");
  return 0;
}

async function runUntrustFlow(
  binary: string,
  list: ListFiltersResponse,
  adapter: HarnessAdapter,
  projectRoot: string,
  options: DoctorFiltersOptions,
): Promise<number> {
  const trusted = list.trusted_projects ?? [];
  if (trusted.length === 0) {
    console.log("No trusted filter projects.");
    return 0;
  }
  const prompt = options.selectMany ?? selectMany;
  const selected = await prompt(
    "Untrust filter projects?",
    trusted.map((path) => ({ label: path, value: path })),
    undefined,
    false,
  );
  if (selected.length === 0) {
    console.log("Untrusted 0 project(s).");
    return 0;
  }
  const requests: AftRequest[] = [buildConfigureRequest(adapter, projectRoot)];
  for (const path of selected) {
    requests.push({
      id: `doctor-filters-untrust-${requests.length}`,
      command: "untrust_filter_project",
      project_root: path,
    });
  }
  const responses = await (options.sendRequests ?? sendAftRequests)(binary, requests);
  const failures = responses.slice(1).filter((response) => !response.success);
  if (failures.length > 0) {
    log.error(failures[0]?.message ?? failures[0]?.code ?? "untrust_filter_project failed");
    return 1;
  }
  console.log(
    `Untrusted ${selected.length} project(s). Restart AFT (or reconfigure) for filters to take effect.`,
  );
  return 0;
}

export function renderFilterList(
  response: ListFiltersResponse,
  projectRoot = process.cwd(),
): string {
  const filters = response.filters ?? [];
  const lines = ["TOML compression filters", ""];
  pushSection(
    lines,
    "Built-in",
    filters.filter((filter) => filter.source === "builtin" || filter.source === "builtin_invalid"),
  );
  lines.push("");
  pushSection(
    lines,
    `User (${formatHome(response.user_dir ?? "")}`,
    filters.filter((filter) => filter.source === "user" || filter.source === "user_invalid"),
    true,
  );
  const projectFilters = filters.filter(
    (filter) => filter.source === "project" || filter.source === "project_invalid",
  );
  if (response.project_dir_exists || projectFilters.length > 0) {
    lines.push("");
    pushSection(
      lines,
      `Project (${formatProjectPath(response.project_dir ?? "", projectRoot)}`,
      projectFilters,
      true,
    );
  }
  return lines.join("\n");
}

function pushSection(
  lines: string[],
  title: string,
  filters: FilterEntry[],
  titleHasOpenParen = false,
): void {
  lines.push(titleHasOpenParen ? `${title}, ${filters.length}):` : `${title} (${filters.length}):`);
  if (filters.length === 0) {
    lines.push("  (empty)");
    return;
  }
  for (const filter of filters) {
    const description = filter.error
      ? `invalid — ${filter.error}`
      : truncate(filter.description ?? "");
    const trust =
      filter.source.startsWith("project") && filter.trusted === false
        ? filter.error
          ? " (untrusted)"
          : " (untrusted — run `aft doctor filters trust` to enable)"
        : "";
    lines.push(`  ${filter.name.padEnd(20)} ${description}${trust}`.trimEnd());
  }
}

export function renderFilterShow(
  response: ListFiltersResponse,
  name: string,
  projectRoot = process.cwd(),
): string | null {
  const filter = (response.filters ?? []).find(
    (entry) => entry.name === name || entry.matches.includes(name),
  );
  if (!filter) return null;
  const lines = [`Filter: ${filter.name}`];
  if (filter.source === "builtin") {
    lines.push("Source: built-in");
  } else if (filter.source.startsWith("user")) {
    lines.push(`Source: user (${formatHome(filter.source_path ?? "")})`);
  } else if (filter.source.startsWith("project")) {
    lines.push(`Source: project (${formatProjectPath(filter.source_path ?? "", projectRoot)})`);
    lines.push(`Trust: ${filter.trusted ? "trusted" : "untrusted"}`);
  } else {
    lines.push(`Source: ${filter.source}`);
  }
  if (filter.error) lines.push(`Error: ${filter.error}`);
  lines.push("", filter.content.trimEnd());
  return lines.join("\n");
}

export function renderTrustedProjects(paths: string[]): string {
  return paths.length === 0 ? "(none)" : paths.join("\n");
}

function parseMode(argv: string[]): Mode {
  if (argv.includes("--help") || argv.includes("-h")) return { kind: "help" };
  const showIndex = argv.indexOf("--show");
  if (showIndex >= 0) return { kind: "show", name: argv[showIndex + 1] ?? "" };
  const positional = argv.filter((arg, index) => {
    if (arg === "--harness") return false;
    if (index > 0 && argv[index - 1] === "--harness") return false;
    return !arg.startsWith("--");
  });
  if (positional[0] === "trust") return { kind: "trust", list: argv.includes("--list") };
  if (positional[0] === "untrust") return { kind: "untrust" };
  return { kind: "list" };
}

function truncate(value: string): string {
  return value.length <= 80 ? value : `${value.slice(0, 77)}…`;
}

function formatHome(path: string): string {
  const home = homedir();
  return path.startsWith(home) ? `~${path.slice(home.length)}` : path;
}

function formatProjectPath(path: string, projectRoot: string): string {
  if (!path) return "";
  const rel = relative(projectRoot, path);
  return rel.startsWith("..") || rel === "" ? path : `./${rel}`;
}

function formatSourcePath(path: string | null, projectRoot: string): string | undefined {
  if (!path) return undefined;
  return formatProjectPath(path, projectRoot);
}
