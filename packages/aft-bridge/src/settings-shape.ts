type ConfigObject = Record<string, unknown>;

function object(value: unknown): ConfigObject | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as ConfigObject)
    : undefined;
}

// Keep the compatibility translation in step with Rust's feature_config/settings_shape.rs.
export function translateSettingsShape(block: ConfigObject, tier: "user" | "project"): string[] {
  const notices: string[] = [];
  if (typeof block.bash === "boolean") {
    const projectTrue = block.bash && tier === "project";
    block.bash = block.bash
      ? {
          ...(projectTrue ? {} : { enabled: true }),
          rewrite: true,
          compress: true,
          background: true,
        }
      : { enabled: false };
    notices.push(
      projectTrue
        ? "bash boolean → bash.rewrite, bash.compress and bash.background"
        : "bash boolean → bash.enabled",
    );
  }
  const lsp = object(block.lsp);
  if (lsp?.idle_minutes === "never") {
    if (!Object.hasOwn(lsp, "idle_shutdown")) lsp.idle_shutdown = false;
    delete lsp.idle_minutes;
    notices.push('lsp.idle_minutes "never" → lsp.idle_shutdown');
  }
  const servers = object(lsp?.servers);
  for (const server of Object.values(servers ?? {})) {
    const settings = object(server);
    const value = settings?.initialization_options;
    if (settings && value !== undefined && value !== null && typeof value !== "string") {
      settings.initialization_options = JSON.stringify(value);
      notices.push("lsp.servers.<name>.initialization_options → JSON text");
    }
  }
  const duplicates = object(object(block.inspect)?.duplicates);
  if (duplicates && Array.isArray(duplicates.expected_mirrors)) {
    let changed = false;
    const mirrors = (duplicates.expected_mirrors as unknown[]).map((entry) => {
      if (Array.isArray(entry) && entry.length === 2) {
        const [source, mirror] = entry;
        if (typeof source === "string" && typeof mirror === "string") {
          changed = true;
          return { source, mirror };
        }
      }
      return entry;
    });
    if (changed) {
      duplicates.expected_mirrors = mirrors;
      notices.push("inspect.duplicates.expected_mirrors pairs → source and mirror objects");
    }
  }
  return notices;
}

export function nullSettingPath(value: unknown, path: string[] = []): string | undefined {
  if (value === null) {
    return path.length === 2 && path[0] === "storage" && path[1] === "cache_dir"
      ? undefined
      : path.join(".");
  }
  if (typeof value !== "object") return undefined;
  for (const key of Object.keys(value).sort()) {
    const invalid = nullSettingPath((value as ConfigObject)[key], [...path, key]);
    if (invalid !== undefined) return invalid;
  }
  return undefined;
}
