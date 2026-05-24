export interface StatusCompressionAggregate {
  events: number;
  original_tokens: number;
  compressed_tokens: number;
  savings_tokens: number;
}

export interface StatusCompression {
  project: StatusCompressionAggregate;
  session: StatusCompressionAggregate;
}

export interface AftStatusSnapshot {
  version: string;
  project_root: string | null;
  canonical_root: string | null;
  cache_role: string;
  features: {
    format_on_edit: boolean;
    validate_on_edit: string;
    restrict_to_project_root: boolean;
    search_index: boolean;
    semantic_search: boolean;
  };
  search_index: {
    status: string;
    files: number | null;
    trigrams: number | null;
  };
  semantic_index: {
    status: string;
    backend?: string | null;
    model?: string | null;
    stage?: string | null;
    files?: number | null;
    entries_done?: number | null;
    entries_total?: number | null;
    entries: number | null;
    dimension: number | null;
    error?: string | null;
  };
  disk: {
    storage_dir: string | null;
    trigram_disk_bytes: number;
    semantic_disk_bytes: number;
  };
  lsp_servers: number;
  symbol_cache: {
    local_entries: number;
    warm_entries: number;
  };
  storage_dir: string | null;
  /** Total checkpoints across all sessions sharing this bridge. */
  checkpoints_total: number;
  /** Current session's own slice of undo/checkpoint state. */
  session: {
    id: string;
    tracked_files: number;
    checkpoints: number;
  };
  /** Compression aggregate passthrough; rendering is added separately. */
  compression?: StatusCompression;
  /**
   * Set on synthetic "not_initialized" snapshots when no bridge has spawned
   * yet. Renderers display this in place of empty status rows.
   */
  message?: string;
}

function asRecord(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null ? (value as Record<string, unknown>) : {};
}

function readString(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

function readNullableString(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

function readBoolean(value: unknown, fallback = false): boolean {
  return typeof value === "boolean" ? value : fallback;
}

function readNumber(value: unknown, fallback = 0): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function readOptionalNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function readCompressionAggregate(value: unknown): StatusCompressionAggregate {
  const aggregate = asRecord(value);
  return {
    events: readNumber(aggregate.events),
    original_tokens: readNumber(aggregate.original_tokens),
    compressed_tokens: readNumber(aggregate.compressed_tokens),
    savings_tokens: readNumber(aggregate.savings_tokens),
  };
}

function readCompression(value: unknown): StatusCompression | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  const compression = asRecord(value);
  return {
    project: readCompressionAggregate(compression.project),
    session: readCompressionAggregate(compression.session),
  };
}

function formatFlag(enabled: boolean): string {
  return enabled ? "enabled" : "disabled";
}

function formatCount(value: number | null): string {
  return value == null ? "—" : value.toLocaleString("en-US");
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unitIndex = 0;

  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex++;
  }

  const decimals = value >= 10 || unitIndex === 0 ? 0 : 1;
  return `${value.toFixed(decimals)} ${units[unitIndex]}`;
}

export function coerceAftStatus(response: Record<string, unknown>): AftStatusSnapshot {
  const features = asRecord(response.features);
  const searchIndex = asRecord(response.search_index);
  const semanticIndex = asRecord(response.semantic_index);
  const semanticConfig = {
    ...asRecord(response.semantic),
    ...asRecord((response as { semantic_config?: unknown }).semantic_config),
  };
  const disk = asRecord(response.disk);
  const symbolCache = asRecord(response.symbol_cache);
  const session = asRecord(response.session);

  return {
    version: readString(response.version, "unknown"),
    project_root: readNullableString(response.project_root),
    canonical_root: readNullableString(response.canonical_root),
    cache_role: readString(response.cache_role, "not_initialized"),
    message: typeof response.message === "string" ? response.message : undefined,
    features: {
      format_on_edit: readBoolean(features.format_on_edit),
      validate_on_edit: readString(features.validate_on_edit, "off"),
      restrict_to_project_root: readBoolean(features.restrict_to_project_root),
      search_index: readBoolean(features.search_index ?? features.experimental_search_index),
      semantic_search: readBoolean(
        features.semantic_search ?? features.experimental_semantic_search,
      ),
    },
    search_index: {
      status: readString(searchIndex.status, "unknown"),
      files: readOptionalNumber(searchIndex.files),
      trigrams: readOptionalNumber(searchIndex.trigrams),
    },
    semantic_index: {
      status: readString(semanticIndex.status, "unknown"),
      backend: readNullableString(semanticIndex.backend ?? semanticConfig.backend),
      model: readNullableString(semanticIndex.model ?? semanticConfig.model),
      stage: readNullableString(semanticIndex.stage),
      files: readOptionalNumber(semanticIndex.files),
      entries_done: readOptionalNumber(semanticIndex.entries_done),
      entries_total: readOptionalNumber(semanticIndex.entries_total),
      entries: readOptionalNumber(semanticIndex.entries),
      dimension: readOptionalNumber(semanticIndex.dimension),
      error: readNullableString(semanticIndex.error),
    },
    disk: {
      storage_dir: readNullableString(disk.storage_dir),
      trigram_disk_bytes: readNumber(disk.trigram_disk_bytes),
      semantic_disk_bytes: readNumber(disk.semantic_disk_bytes),
    },
    lsp_servers: readNumber(response.lsp_servers),
    symbol_cache: {
      local_entries: readNumber(symbolCache.local_entries),
      warm_entries: readNumber(symbolCache.warm_entries),
    },
    storage_dir: readNullableString(response.storage_dir),
    checkpoints_total: readNumber(response.checkpoints_total),
    session: {
      id: readString(session.id, "__default__"),
      tracked_files: readNumber(session.tracked_files),
      checkpoints: readNumber(session.checkpoints),
    },
    compression: readCompression(response.compression),
  };
}

export function formatStatusDialogMessage(status: AftStatusSnapshot): string {
  const lines = [
    `AFT version: ${status.version}`,
    `Project root: ${status.project_root ?? "(not configured)"}`,
    `Canonical root: ${status.canonical_root ?? "(not configured)"}`,
    `Cache role: ${status.cache_role}`,
    "",
    "Enabled features",
    `- format_on_edit: ${formatFlag(status.features.format_on_edit)}`,
    `- search_index: ${formatFlag(status.features.search_index)}`,
    `- semantic_search: ${formatFlag(status.features.semantic_search)}`,
    "",
    "Search index",
    `- status: ${status.search_index.status}`,
    `- files: ${formatCount(status.search_index.files)}`,
    `- trigrams: ${formatCount(status.search_index.trigrams)}`,
    "",
    "Semantic index",
    `- status: ${status.semantic_index.status}`,
    `- entries: ${formatCount(status.semantic_index.entries)}`,
  ];
  if (status.semantic_index.backend) {
    lines.push(`- backend: ${status.semantic_index.backend}`);
  }
  if (status.semantic_index.model) {
    lines.push(`- model: ${status.semantic_index.model}`);
  }

  if (status.semantic_index.dimension != null) {
    lines.push(`- dimension: ${formatCount(status.semantic_index.dimension)}`);
  }

  lines.push(
    "",
    "Disk usage",
    `- trigram index: ${formatBytes(status.disk.trigram_disk_bytes)}`,
    `- semantic index: ${formatBytes(status.disk.semantic_disk_bytes)}`,
    "",
    "Runtime",
    `- LSP servers: ${formatCount(status.lsp_servers)}`,
    `- symbol cache: ${formatCount(status.symbol_cache.local_entries)} local / ${formatCount(status.symbol_cache.warm_entries)} warm`,
  );

  if (status.storage_dir ?? status.disk.storage_dir) {
    lines.push(`- storage dir: ${status.storage_dir ?? status.disk.storage_dir}`);
  }

  if (status.semantic_index.stage) {
    lines.push("", "Semantic stage", status.semantic_index.stage);
  }
  if (status.semantic_index.files != null) {
    lines.push(`- semantic files: ${formatCount(status.semantic_index.files)}`);
  }
  if (status.semantic_index.entries_done != null || status.semantic_index.entries_total != null) {
    lines.push(
      `- semantic progress: ${formatCount(status.semantic_index.entries_done ?? null)} / ${formatCount(status.semantic_index.entries_total ?? null)}`,
    );
  }
  if (status.semantic_index.error) {
    lines.push("", "Semantic error", status.semantic_index.error);
  }

  return lines.join("\n");
}

export function formatStatusMarkdown(status: AftStatusSnapshot): string {
  const lines = [
    "## AFT Status",
    "",
    `- **Version:** \`${status.version}\``,
    `- **Project root:** \`${status.project_root ?? "(not configured)"}\``,
    `- **Canonical root:** \`${status.canonical_root ?? "(not configured)"}\``,
    `- **Cache role:** \`${status.cache_role}\``,
    "",
    "### Enabled features",
    `- \`format_on_edit\`: ${formatFlag(status.features.format_on_edit)}`,
    `- \`search_index\`: ${formatFlag(status.features.search_index)}`,
    `- \`semantic_search\`: ${formatFlag(status.features.semantic_search)}`,
    "",
    "### Search index",
    `- **Status:** \`${status.search_index.status}\``,
    `- **Files:** ${formatCount(status.search_index.files)}`,
    `- **Trigrams:** ${formatCount(status.search_index.trigrams)}`,
    "",
    "### Semantic index",
    `- **Status:** \`${status.semantic_index.status}\``,
    `- **Entries:** ${formatCount(status.semantic_index.entries)}`,
  ];
  if (status.semantic_index.backend) {
    lines.push(`- **Backend:** ${status.semantic_index.backend}`);
  }
  if (status.semantic_index.model) {
    lines.push(`- **Model:** ${status.semantic_index.model}`);
  }

  if (status.semantic_index.dimension != null) {
    lines.push(`- **Dimension:** ${formatCount(status.semantic_index.dimension)}`);
  }
  if (status.semantic_index.stage) {
    lines.push(`- **Stage:** ${status.semantic_index.stage}`);
  }
  if (status.semantic_index.files != null) {
    lines.push(`- **Files:** ${formatCount(status.semantic_index.files)}`);
  }
  if (status.semantic_index.entries_done != null || status.semantic_index.entries_total != null) {
    lines.push(
      `- **Progress:** ${formatCount(status.semantic_index.entries_done ?? null)} / ${formatCount(status.semantic_index.entries_total ?? null)}`,
    );
  }

  if (status.semantic_index.error) {
    lines.push(`- **Error:** ${status.semantic_index.error}`);
  }

  lines.push(
    "",
    "### Disk usage",
    `- **Trigram index:** ${formatBytes(status.disk.trigram_disk_bytes)}`,
    `- **Semantic index:** ${formatBytes(status.disk.semantic_disk_bytes)}`,
    "",
    "### Runtime",
    `- **LSP servers:** ${formatCount(status.lsp_servers)}`,
    `- **Symbol cache:** ${formatCount(status.symbol_cache.local_entries)} local / ${formatCount(status.symbol_cache.warm_entries)} warm`,
  );

  if (status.storage_dir ?? status.disk.storage_dir) {
    lines.push(`- **Storage dir:** \`${status.storage_dir ?? status.disk.storage_dir}\``);
  }

  return lines.join("\n");
}
