import modernV1VersionText from "../../../../.github/opencode-version.txt" with { type: "text" };

export const AFT_OPENCODE_PACKAGE = "@cortexkit/aft-opencode";
export const MODERN_V1_VERSION = modernV1VersionText.trim();

export function pinnedPluginEntry(version: string): string {
  return `${AFT_OPENCODE_PACKAGE}@${version}`;
}

export function isAftNpmEntry(entry: unknown): entry is string {
  return (
    typeof entry === "string" &&
    (entry === AFT_OPENCODE_PACKAGE || entry.startsWith(`${AFT_OPENCODE_PACKAGE}@`))
  );
}

export interface PluginConfigUpdate {
  action: "already_present" | "added" | "updated";
  changed: boolean;
  entry: string;
}

/**
 * Normalize an OpenCode server or TUI config without replacing its parsed object.
 * Keeping the object and its existing `plugin` array lets comment-json retain JSONC comments.
 */
export function ensurePinnedPluginConfig(
  value: Record<string | symbol, unknown>,
  version: string,
  hasLocalAftEntry: (entry: string) => boolean = () => false,
): PluginConfigUpdate {
  const entry = pinnedPluginEntry(version);
  const singular = Array.isArray(value.plugin) ? value.plugin : [];
  const plural = Array.isArray(value.plugins) ? value.plugins : [];
  const hadPluralKey = Object.hasOwn(value, "plugins");

  if (!Array.isArray(value.plugin)) value.plugin = singular;
  if (plural !== singular) singular.push(...plural);
  if (hadPluralKey) delete value.plugins;

  const npmIndexes: number[] = [];
  let hasLocalEntry = false;
  for (let index = 0; index < singular.length; index += 1) {
    const candidate = singular[index];
    if (isAftNpmEntry(candidate)) npmIndexes.push(index);
    else if (typeof candidate === "string" && hasLocalAftEntry(candidate)) hasLocalEntry = true;
  }

  if (npmIndexes.length > 0) {
    const firstIndex = npmIndexes[0] as number;
    const wasExact = singular[firstIndex] === entry;
    singular[firstIndex] = entry;
    for (let index = npmIndexes.length - 1; index >= 1; index -= 1) {
      singular.splice(npmIndexes[index] as number, 1);
    }
    const changed = hadPluralKey || !wasExact || npmIndexes.length > 1;
    return { action: changed ? "updated" : "already_present", changed, entry };
  }

  if (hasLocalEntry) {
    return {
      action: hadPluralKey ? "updated" : "already_present",
      changed: hadPluralKey,
      entry,
    };
  }

  singular.push(entry);
  return { action: "added", changed: true, entry };
}

export function pluginConfigNeedsUpdate(
  value: Record<string | symbol, unknown>,
  version: string,
  hasLocalAftEntry: (entry: string) => boolean = () => false,
): boolean {
  if (Object.hasOwn(value, "plugins")) return true;
  if (!Array.isArray(value.plugin)) return true;

  const npmEntries = value.plugin.filter(isAftNpmEntry);
  if (npmEntries.length > 0) {
    return npmEntries.length !== 1 || npmEntries[0] !== pinnedPluginEntry(version);
  }
  return !value.plugin.some((entry) => typeof entry === "string" && hasLocalAftEntry(entry));
}
