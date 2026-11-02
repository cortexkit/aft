import modernV1VersionText from "../../../../.github/opencode-version.txt" with { type: "text" };
import { compareSemver } from "../lib/semver.js";

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

// Approved by Ufuk. Keep the wording identical in diagnosis, planning, and repair.
export const DOCTOR_NO_DOWNGRADE =
  "This doctor is AFT <cli>; OpenCode is configured for AFT plugin <plugin>. Doctor will not downgrade that entry. Run doctor with a matching or newer CLI.";

export function preserveDoctorPluginEntry(
  cli: string,
  entry: string | undefined,
  installed?: string,
): boolean {
  const configured = entry ? exactPinnedVersion(entry) : null;
  const configuredOrder = configured ? compareSemver(configured, cli) : undefined;
  const installedOrder = installed ? compareSemver(installed, cli) : undefined;
  if (installed && (installedOrder === undefined || installedOrder > 0)) return true;
  if (configured && (configuredOrder === undefined || configuredOrder > 0)) return true;
  // A bare registration gains this CLI's exact pin unless installed evidence
  // above shows that pinning would replace a newer or unreadable installation.
  if (entry === AFT_OPENCODE_PACKAGE) return false;
  // A tag or range is not version evidence. An independently installed older
  // version can establish an upgrade; an unresolved entry alone cannot.
  return Boolean(entry && configuredOrder === undefined && installedOrder === undefined);
}

export function doctorNoDowngradeMessage(cli: string, entry?: string, installed?: string): string {
  const configured = entry ? exactPinnedVersion(entry) : null;
  const newerInstalled = installed && (compareSemver(installed, cli) ?? 1) > 0;
  const plugin = newerInstalled
    ? installed
    : (configured ?? entry?.slice(AFT_OPENCODE_PACKAGE.length + 1)) || "unknown";
  return DOCTOR_NO_DOWNGRADE.replace("<cli>", cli).replace("<plugin>", plugin);
}

export interface PluginVersionPolicy {
  doctor?: boolean;
  installedVersion?: string;
  fallbackEntry?: string;
}

/**
 * Why an npm plugin entry other than this CLI's exact pin is a problem, as a
 * clause that follows "the plugin entry <entry>". Doctor's OpenCode section and
 * its version-skew issue share this wording so the two never disagree.
 */
export function describePluginEntry(entry: string): string {
  if (entry === AFT_OPENCODE_PACKAGE) {
    return "has no version, so OpenCode may load any release of the plugin";
  }
  const tag = entry.slice(entry.lastIndexOf("@") + 1);
  return tag === "latest"
    ? "follows the newest release, which can differ from this CLI and its binary"
    : `asks for version ${tag}, not the version of this CLI and its binary`;
}

const EXACT_VERSION =
  /^\d+\.\d+\.\d+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;

/**
 * `@latest` or an exact version: the OpenCode 1 registrations doctor does not
 * report as a problem, because the host loads them correctly. `doctor --fix`
 * and setup can normalize their shape; doctor preserves a newer or unresolved
 * version rather than assuming the invoking CLI is the newest release.
 */
export function acceptV1Entry(entry: string): boolean {
  if (!entry.startsWith(`${AFT_OPENCODE_PACKAGE}@`)) return false;
  const tag = entry.slice(AFT_OPENCODE_PACKAGE.length + 1);
  return tag === "latest" || EXACT_VERSION.test(tag);
}

/**
 * The exact version an AFT npm entry is pinned to (`0.57.2` for
 * `@cortexkit/aft-opencode@0.57.2`), or null for a bare entry, a dist-tag such
 * as `@latest`, or a range.
 */
export function exactPinnedVersion(entry: string): string | null {
  if (!entry.startsWith(`${AFT_OPENCODE_PACKAGE}@`)) return null;
  const tag = entry.slice(AFT_OPENCODE_PACKAGE.length + 1);
  return EXACT_VERSION.test(tag) ? tag : null;
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
  policy: PluginVersionPolicy = {},
): PluginConfigUpdate {
  const entry = policy.fallbackEntry ?? pinnedPluginEntry(version);
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
    if (
      policy.doctor &&
      npmIndexes.some((index) =>
        preserveDoctorPluginEntry(
          version,
          pluginEntryPackage(list[index]) ?? undefined,
          policy.installedVersion,
        ),
      )
    ) {
      let changed = false;
      for (const index of npmIndexes) {
        const spec = pluginEntryPackage(list[index]) as string;
        const shaped = shapePluginEntry(key, spec, pluginEntryOptions(list[index]));
        changed ||= !sameEntry(list[index], shaped);
        list[index] = shaped;
      }
      return {
        action: changed ? "updated" : "already_present",
        changed,
        entry: pluginEntryPackage(list[firstIndex]) as string,
        key,
      };
    }
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
  if (policy.doctor && siblingList) {
    const preserved = siblingList.find((candidate) => {
      const spec = pluginEntryPackage(candidate);
      return (
        spec !== null &&
        isAftNpmEntry(spec) &&
        preserveDoctorPluginEntry(version, spec, policy.installedVersion)
      );
    });
    const spec = pluginEntryPackage(preserved);
    if (spec) {
      list.push(shapePluginEntry(key, spec, pluginEntryOptions(preserved)));
      return { action: "added", changed: true, entry: spec, key };
    }
  }
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
  policy: PluginVersionPolicy = {},
): boolean {
  const key = openCodePluginKey(generation);
  const list = value[key];
  if (!Array.isArray(list)) return true;

  const npmEntries = list.filter((candidate) => {
    const packageSpec = pluginEntryPackage(candidate);
    return packageSpec !== null && isAftNpmEntry(packageSpec);
  });
  if (npmEntries.length > 0) {
    if (
      policy.doctor &&
      npmEntries.some((candidate) =>
        preserveDoctorPluginEntry(
          version,
          pluginEntryPackage(candidate) ?? undefined,
          policy.installedVersion,
        ),
      )
    )
      return npmEntries.some((candidate) => !pluginEntryFitsKey(candidate, key));
    return (
      npmEntries.length !== 1 ||
      pluginEntryPackage(npmEntries[0]) !== (policy.fallbackEntry ?? pinnedPluginEntry(version)) ||
      !pluginEntryFitsKey(npmEntries[0], key)
    );
  }

  const local = findLocalAftEntry(list, hasLocalAftEntry);
  if (local) return !pluginEntryFitsKey(list[local.index], key);
  return true;
}

/**
 * The `plugins` entry that removes OpenCode 2's built-in shell tool plugin.
 *
 * OpenCode 2 ships its own command tool, `shell`, and its plugin rewrites the
 * description of whatever tool is named `shell` on every request. With AFT's
 * `bash` registered beside it the agent is offered two command tools. A
 * `plugins` entry starting with `-` removes a plugin by id, built-ins included,
 * so this one entry removes the host's shell tool and its hooks together and
 * leaves AFT's `bash` as the only command tool. OpenCode 1 has no such plugin.
 */
export const OPENCODE_HOST_SHELL_DISABLE_ENTRY = "-opencode.tool.shell";
export const OPENCODE_HOST_PATCH_DISABLE_ENTRY = "-opencode.tool.patch";

/** Removing a host plugin removes its tool and its tool-name-specific hooks together. */
export const OPENCODE_HOST_TOOLS = {
  shell: { aftTool: "bash", disableEntry: OPENCODE_HOST_SHELL_DISABLE_ENTRY },
  patch: { aftTool: "apply_patch", disableEntry: OPENCODE_HOST_PATCH_DISABLE_ENTRY },
} as const;

export type OpenCodeHostTool = keyof typeof OPENCODE_HOST_TOOLS;
export const OPENCODE_HOST_TOOL_NAMES = Object.keys(OPENCODE_HOST_TOOLS) as OpenCodeHostTool[];

export function hostToolPluginDisabled(
  value: Record<string | symbol, unknown> | null,
  hostTool: OpenCodeHostTool,
): boolean {
  const list = value?.plugins;
  return Array.isArray(list) && list.includes(OPENCODE_HOST_TOOLS[hostTool].disableEntry);
}

/** True when the OpenCode 2 `plugins` list removes the host's shell tool plugin. */
export function hostShellPluginDisabled(value: Record<string | symbol, unknown> | null): boolean {
  return hostToolPluginDisabled(value, "shell");
}

/**
 * Add or remove the host-shell removal entry in the OpenCode 2 `plugins` list.
 *
 * Idempotent in both directions: enabling leaves exactly one entry, disabling
 * leaves none, and a list already in the requested state is not touched.
 * Every other entry keeps its position. The array is edited in place so the
 * JSONC writer keeps the file's comments.
 */
export function setHostShellPluginDisabled(
  value: Record<string | symbol, unknown>,
  disabled: boolean,
): { changed: boolean } {
  return setHostToolPluginDisabled(value, "shell", disabled);
}

/** Edit only this host tool's removal entries, retaining JSONC comments and other plugins. */
export function setHostToolPluginDisabled(
  value: Record<string | symbol, unknown>,
  hostTool: OpenCodeHostTool,
  disabled: boolean,
): { changed: boolean } {
  const entry = OPENCODE_HOST_TOOLS[hostTool].disableEntry;
  const existing = Array.isArray(value.plugins) ? (value.plugins as unknown[]) : null;
  const indexes: number[] = [];
  existing?.forEach((entry, index) => {
    if (entry === OPENCODE_HOST_TOOLS[hostTool].disableEntry) indexes.push(index);
  });
  if (disabled ? indexes.length === 1 : indexes.length === 0) return { changed: false };

  const list = existing ?? [];
  if (!existing) value.plugins = list;
  // Enabling keeps the first copy (if any) and removes the rest; disabling
  // removes every copy. Removal runs from the end so indexes stay valid.
  const remove = disabled ? indexes.slice(1) : indexes;
  for (let index = remove.length - 1; index >= 0; index -= 1) {
    list.splice(remove[index] as number, 1);
  }
  if (disabled && indexes.length === 0) list.push(entry);
  return { changed: true };
}

/**
 * Whether the user config leaves AFT's `bash` tool registered and runnable.
 *
 * The tool is registered unless `disabled_tools` names it, and a registered
 * tool refuses every command when the `bash` runtime gate is off (`bash:
 * false` or `bash.enabled: false`). Disabling the host's shell only makes
 * sense when AFT's bash can take its place, so this is the offer's default.
 * AFT disabled as a whole (`enabled: false`) counts as bash disabled.
 */
export function aftBashEnabled(userConfig: Record<string, unknown> | null): boolean {
  return aftHostReplacementEnabled(userConfig, "shell");
}

/** Whether AFT can replace the host tool; shell also has a separate runtime gate. */
export function aftHostReplacementEnabled(
  userConfig: Record<string, unknown> | null,
  hostTool: OpenCodeHostTool,
): boolean {
  if (!userConfig) return true;
  if (userConfig.enabled === false) return false;
  const disabled = userConfig.disabled_tools;
  if (Array.isArray(disabled) && disabled.includes(OPENCODE_HOST_TOOLS[hostTool].aftTool))
    return false;
  if (hostTool !== "shell") return true;
  const bash = userConfig.bash;
  if (bash === false) return false;
  if (
    typeof bash === "object" &&
    bash !== null &&
    (bash as { enabled?: unknown }).enabled === false
  ) {
    return false;
  }
  return true;
}
