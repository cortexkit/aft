type ConfigObject = Record<string, unknown>;

function object(value: unknown): ConfigObject | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as ConfigObject)
    : undefined;
}

export type CacheDirChoice =
  | { kind: "default" }
  | { kind: "main_disk" }
  | { kind: "dir"; path: string };

export function isCacheDirChoice(value: unknown): value is CacheDirChoice {
  const choice = object(value);
  if (!choice) return false;
  const keys = Object.keys(choice);
  if (choice.kind === "dir") {
    return (
      keys.length === 2 &&
      Object.hasOwn(choice, "kind") &&
      Object.hasOwn(choice, "path") &&
      typeof choice.path === "string"
    );
  }
  return (
    (choice.kind === "default" || choice.kind === "main_disk") &&
    keys.length === 1 &&
    Object.hasOwn(choice, "kind")
  );
}

export function hasMalformedCacheDirChoice(input: unknown): boolean {
  const config = object(input);
  if (!config) return false;
  const malformed = (block: ConfigObject | undefined) => {
    const choice = object(block?.storage)?.cache_dir;
    return object(choice) !== undefined && !isCacheDirChoice(choice);
  };
  return (
    malformed(config) ||
    Object.values(object(config.harnesses) ?? {}).some((block) => malformed(object(block)))
  );
}

// Keep the compatibility translation in step with Rust's feature_config/settings_shape.rs.
export function translateSettingsShape(
  block: ConfigObject,
  tier: "user" | "project",
  isBase: boolean,
): string[] {
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
      : { enabled: false, rewrite: false, compress: false, background: false };
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
  const storage = object(block.storage);
  if (storage?.cache_dir === null && isBase && tier === "user") {
    storage.cache_dir = { kind: "main_disk" };
    notices.push("storage.cache_dir null → storage.cache_dir main_disk");
  } else if (typeof storage?.cache_dir === "string") {
    storage.cache_dir = { kind: "dir", path: storage.cache_dir };
    notices.push("storage.cache_dir string → storage.cache_dir dir");
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
    // The project-base legacy opt-out was accepted but ignored.
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
