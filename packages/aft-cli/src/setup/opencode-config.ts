import modernV1VersionText from "../../../../.github/opencode-version.txt" with { type: "text" };

export const AFT_OPENCODE_PACKAGE = "@cortexkit/aft-opencode";
export const MODERN_V1_VERSION = modernV1VersionText.trim();

/**
 * The config key an OpenCode host reads its plugin list from.
 *
 * V1 reads `plugin`. V2 renamed it to `plugins` and reads that name, so the
 * key a CLI writes has to follow the host in front of it: an entry under the
 * name the running host does not read is an unregistered plugin.
 */
export type OpenCodePluginKey = "plugin" | "plugins";

/** Host generation as resolved by host-generation.ts (`OpenCodeHostDetection["status"]`). */
export type OpenCodeConfigGeneration = "v1" | "v2" | "ambiguous" | "unknown";

/** The key writes must use for a host of this generation. */
export function openCodePluginKey(generation: OpenCodeConfigGeneration): OpenCodePluginKey {
  return generation === "v2" ? "plugins" : "plugin";
}

/** The key belonging to the other generation. */
export function otherOpenCodePluginKey(key: OpenCodePluginKey): OpenCodePluginKey {
  return key === "plugins" ? "plugin" : "plugins";
}

/**
 * The keys to search when answering "is AFT registered?".
 *
 * A settled generation answers for the single key that host reads. While the
 * generation is unresolved neither key can be ruled out, so a registration
 * under either one counts.
 */
export function openCodePluginReadKeys(generation: OpenCodeConfigGeneration): OpenCodePluginKey[] {
  if (generation === "v1") return ["plugin"];
  if (generation === "v2") return ["plugins"];
  return ["plugin", "plugins"];
}

export function pinnedPluginEntry(version: string): string {
  return `${AFT_OPENCODE_PACKAGE}@${version}`;
}

const EXACT_VERSION =
  /^\d+\.\d+\.\d+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;

/**
 * `@latest` or an exact version: the OpenCode 1 registrations doctor does not
 * report as a problem, because the host loads them correctly. `doctor --fix`
 * and setup still rewrite them to this CLI's exact version (the exact-pin
 * rule); only an entry with no version at all is reported.
 */
export function acceptV1Entry(entry: string): boolean {
  if (!entry.startsWith(`${AFT_OPENCODE_PACKAGE}@`)) return false;
  const tag = entry.slice(AFT_OPENCODE_PACKAGE.length + 1);
  return tag === "latest" || EXACT_VERSION.test(tag);
}

export function isAftNpmEntry(entry: unknown): entry is string {
  return (
    typeof entry === "string" &&
    (entry === AFT_OPENCODE_PACKAGE || entry.startsWith(`${AFT_OPENCODE_PACKAGE}@`))
  );
}

/** `{ package, options }` — the shape V2 uses for an entry that carries options. */
interface PackageObjectEntry {
  package: string;
  options?: unknown;
}

function isPackageObjectEntry(entry: unknown): entry is PackageObjectEntry {
  return (
    typeof entry === "object" &&
    entry !== null &&
    !Array.isArray(entry) &&
    typeof (entry as PackageObjectEntry).package === "string"
  );
}

/** `["./p.ts", { … }]` — the shape V1 uses for an entry that carries options. */
function isPackageTupleEntry(entry: unknown): entry is [string, ...unknown[]] {
  return Array.isArray(entry) && typeof entry[0] === "string";
}

/** The package a config entry registers, in any shape either host accepts. */
export function pluginEntryPackage(entry: unknown): string | null {
  if (typeof entry === "string") return entry;
  if (isPackageTupleEntry(entry)) return entry[0];
  if (isPackageObjectEntry(entry)) return entry.package;
  return null;
}

function pluginEntryOptions(entry: unknown): unknown {
  if (isPackageTupleEntry(entry)) return entry.length > 1 ? entry[1] : undefined;
  if (isPackageObjectEntry(entry)) return entry.options;
  return undefined;
}

/**
 * True when the entry's shape is one the host reading `key` can load.
 *
 * Both hosts accept a bare package string. Options travel differently: V1
 * pairs them in a tuple, V2 in a `package`/`options` object, and neither host
 * understands the other's form.
 */
export function pluginEntryFitsKey(entry: unknown, key: OpenCodePluginKey): boolean {
  if (typeof entry === "string") return true;
  if (isPackageTupleEntry(entry)) return key === "plugin";
  if (isPackageObjectEntry(entry)) return key === "plugins";
  return false;
}

/** Build an entry for `key`, preserving any options the previous shape carried. */
function shapePluginEntry(key: OpenCodePluginKey, packageSpec: string, options: unknown): unknown {
  if (options === undefined) return packageSpec;
  return key === "plugins" ? { package: packageSpec, options } : [packageSpec, options];
}

function sameEntry(left: unknown, right: unknown): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

interface FoundEntry {
  index: number;
  packageSpec: string;
  options: unknown;
}

/** First local (non-npm) AFT registration in a plugin list, if there is one. */
function findLocalAftEntry(
  list: readonly unknown[],
  hasLocalAftEntry: (entry: string) => boolean,
): FoundEntry | null {
  for (let index = 0; index < list.length; index += 1) {
    const packageSpec = pluginEntryPackage(list[index]);
    if (packageSpec === null || isAftNpmEntry(packageSpec)) continue;
    if (hasLocalAftEntry(packageSpec)) {
      return { index, packageSpec, options: pluginEntryOptions(list[index]) };
    }
  }
  return null;
}

export interface PluginConfigUpdate {
  action: "already_present" | "added" | "updated";
  changed: boolean;
  entry: string;
  /** Config key the entry was written under, so callers can name it in output. */
  key: OpenCodePluginKey;
}

/**
 * Normalize an OpenCode server or TUI config without replacing its parsed object.
 * Keeping the object and its existing plugin array lets comment-json retain JSONC comments.
 *
 * Only the key this host generation reads is edited, and only AFT's own
 * entries within it. The other generation's key is never rewritten or removed:
 * one machine can run both hosts against one config file, so dropping the
 * sibling key unregisters AFT on whichever host is not being configured.
 */
export function ensurePinnedPluginConfig(
  value: Record<string | symbol, unknown>,
  version: string,
  hasLocalAftEntry: (entry: string) => boolean = () => false,
  generation: OpenCodeConfigGeneration = "unknown",
): PluginConfigUpdate {
  const entry = pinnedPluginEntry(version);
  const key = openCodePluginKey(generation);
  const hadKey = Array.isArray(value[key]);
  const list = hadKey ? (value[key] as unknown[]) : [];
  if (!hadKey) value[key] = list;

  const npmIndexes: number[] = [];
  for (let index = 0; index < list.length; index += 1) {
    const packageSpec = pluginEntryPackage(list[index]);
    if (packageSpec !== null && isAftNpmEntry(packageSpec)) npmIndexes.push(index);
  }

  if (npmIndexes.length > 0) {
    const firstIndex = npmIndexes[0] as number;
    const shaped = shapePluginEntry(key, entry, pluginEntryOptions(list[firstIndex]));
    const wasExact = sameEntry(list[firstIndex], shaped);
    list[firstIndex] = shaped;
    // The later npm registrations are duplicates this CLI wrote, and this call
    // replaces them with the single pin above. Nothing else is removed.
    for (let index = npmIndexes.length - 1; index >= 1; index -= 1) {
      list.splice(npmIndexes[index] as number, 1);
    }
    const changed = !wasExact || npmIndexes.length > 1;
    return { action: changed ? "updated" : "already_present", changed, entry, key };
  }

  const local = findLocalAftEntry(list, hasLocalAftEntry);
  if (local) {
    // A developer's own checkout stays registered; only its shape is corrected
    // when the config was written for the other generation.
    const shaped = shapePluginEntry(key, local.packageSpec, local.options);
    const changed = !sameEntry(list[local.index], shaped);
    list[local.index] = shaped;
    return { action: changed ? "updated" : "already_present", changed, entry, key };
  }

  // Nothing AFT under the key this host reads. A local checkout registered
  // under the other generation's key MOVES into this key's shape rather than
  // being copied: GA folds a V1 `plugin` list into `plugins`, so the same
  // checkout under both keys is ambiguous — it either registers twice or lets
  // the converted V1 list stand in for the V2 one. Our own entry is ours to
  // relocate; every other plugin's entry stays where the user wrote it.
  const sibling = value[otherOpenCodePluginKey(key)];
  const siblingList = Array.isArray(sibling) ? sibling : null;
  const carried = siblingList ? findLocalAftEntry(siblingList, hasLocalAftEntry) : null;
  list.push(carried ? shapePluginEntry(key, carried.packageSpec, carried.options) : entry);
  if (carried && siblingList) siblingList.splice(carried.index, 1);
  return { action: "added", changed: true, entry, key };
}

export function pluginConfigNeedsUpdate(
  value: Record<string | symbol, unknown>,
  version: string,
  hasLocalAftEntry: (entry: string) => boolean = () => false,
  generation: OpenCodeConfigGeneration = "unknown",
): boolean {
  const key = openCodePluginKey(generation);
  const list = value[key];
  if (!Array.isArray(list)) return true;

  const npmEntries = list.filter((candidate) => {
    const packageSpec = pluginEntryPackage(candidate);
    return packageSpec !== null && isAftNpmEntry(packageSpec);
  });
  if (npmEntries.length > 0) {
    return (
      npmEntries.length !== 1 ||
      pluginEntryPackage(npmEntries[0]) !== pinnedPluginEntry(version) ||
      !pluginEntryFitsKey(npmEntries[0], key)
    );
  }

  const local = findLocalAftEntry(list, hasLocalAftEntry);
  if (local) return !pluginEntryFitsKey(list[local.index], key);
  return true;
}
