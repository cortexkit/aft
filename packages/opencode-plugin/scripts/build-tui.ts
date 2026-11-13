import { copyFile, mkdir, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { basename, dirname, join, relative } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const pluginRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const sourceRoot = join(pluginRoot, "src/tui");
const outputRoot = join(pluginRoot, "src/tui-compiled");
const sharedSidebarSource = fileURLToPath(import.meta.resolve("@cortexkit/opencode-sidebar"));
const sharedSidebarHost = join(outputRoot, "shared-sidebar-host.tsx");
const sharedSidebarLocal = join(outputRoot, "shared-sidebar-local.tsx");
const lockfile = Bun.JSONC.parse(await readFile(join(pluginRoot, "../../bun.lock"), "utf8")) as {
  packages: Record<string, string[]>;
};
const sharedSidebarVersion = (
  JSON.parse(await readFile(join(dirname(sharedSidebarSource), "../package.json"), "utf8")) as {
    version: string;
  }
).version;
if (
  lockfile.packages["@cortexkit/opencode-sidebar"]?.[0] !==
  `@cortexkit/opencode-sidebar@${sharedSidebarVersion}`
) {
  throw new Error("The installed shared sidebar differs from bun.lock; run `bun install` first.");
}
const runtimeSpecifiers = new Set([
  "@opentui/core",
  "@opentui/core/testing",
  "@opentui/solid",
  "@opentui/solid/components",
  "@opentui/solid/jsx-runtime",
  "@opentui/solid/jsx-dev-runtime",
  "solid-js",
  "solid-js/store",
]);

type TransformSolidSource = (
  code: string,
  options: {
    filename: string;
    moduleName: string;
    resolvePath: (specifier: string) => string | null;
  },
) => Promise<string>;

type SolidTransformModule = {
  transformSolidSource?: TransformSolidSource;
};

function runtimeModuleId(specifier: string): string {
  return `opentui:runtime-module:${encodeURIComponent(specifier)}`;
}

function asTransformSolidSource(mod: SolidTransformModule, from: string): TransformSolidSource {
  if (typeof mod.transformSolidSource !== "function") {
    throw new Error(`@opentui/solid transform loaded from ${from} without transformSolidSource`);
  }
  return mod.transformSolidSource;
}

async function importTransformModule(specifier: string): Promise<SolidTransformModule> {
  return (await import(specifier)) as SolidTransformModule;
}

async function resolveSolidTransformPath(): Promise<string> {
  const packageJsonSpecifier = "@opentui/solid/package.json";
  const errors: string[] = [];

  try {
    const packageJsonUrl = import.meta.resolve(packageJsonSpecifier);
    return join(dirname(fileURLToPath(packageJsonUrl)), "scripts/solid-transform.js");
  } catch (error) {
    errors.push(`import.meta.resolve: ${error instanceof Error ? error.message : String(error)}`);
  }

  try {
    const require = createRequire(import.meta.url);
    return join(dirname(require.resolve(packageJsonSpecifier)), "scripts/solid-transform.js");
  } catch (error) {
    errors.push(`require.resolve: ${error instanceof Error ? error.message : String(error)}`);
  }

  throw new Error(`Unable to resolve @opentui/solid transform (${errors.join("; ")})`);
}

async function loadTransformSolidSource(): Promise<TransformSolidSource> {
  const bareTransformSpecifier = "@opentui/solid/scripts/solid-transform.js";

  try {
    return asTransformSolidSource(
      await importTransformModule(bareTransformSpecifier),
      bareTransformSpecifier,
    );
  } catch {
    const transformPath = await resolveSolidTransformPath();
    return asTransformSolidSource(
      await importTransformModule(pathToFileURL(transformPath).href),
      transformPath,
    );
  }
}

function isShippedSourceFile(filePath: string): boolean {
  if (/\.test\.[cm]?tsx?$/.test(basename(filePath))) return false;
  return filePath.endsWith(".tsx") || filePath.endsWith(".ts");
}

async function listSourceFiles(dir: string): Promise<string[]> {
  const entries = await readdir(dir, { withFileTypes: true });
  entries.sort((a, b) => a.name.localeCompare(b.name));

  const files: string[] = [];
  for (const entry of entries) {
    const entryPath = join(dir, entry.name);
    if (entry.isDirectory()) {
      files.push(...(await listSourceFiles(entryPath)));
    } else if (entry.isFile() && isShippedSourceFile(entryPath)) {
      files.push(entryPath);
    }
  }
  return files;
}

async function copyPlainTypeScript(sourceFile: string, outputFile: string): Promise<void> {
  await mkdir(dirname(outputFile), { recursive: true });
  await copyFile(sourceFile, outputFile);
}

async function compileTsx(
  transformSolidSource: TransformSolidSource,
  sourceFile: string,
  outputFile: string,
  hostRuntime = true,
): Promise<void> {
  const code = await readFile(sourceFile, "utf8");
  const compiled = await transformSolidSource(code, {
    filename: sourceFile,
    moduleName: hostRuntime ? runtimeModuleId("@opentui/solid") : "@opentui/solid",
    resolvePath: (specifier: string) => {
      if (!hostRuntime) return null;
      if (specifier === "./shared-sidebar") {
        const path = relative(dirname(outputFile), sharedSidebarHost).replaceAll("\\", "/");
        return path.startsWith(".") ? path : `./${path}`;
      }
      return runtimeSpecifiers.has(specifier) ? runtimeModuleId(specifier) : null;
    },
  });

  await mkdir(dirname(outputFile), { recursive: true });
  await writeFile(outputFile, `// @ts-nocheck\n${compiled}`);
}

const transformSolidSource = await loadTransformSolidSource();
const files = await listSourceFiles(sourceRoot);

await rm(outputRoot, { recursive: true, force: true });

for (const sourceFile of files) {
  const relativePath = relative(sourceRoot, sourceFile);
  const outputFile = join(outputRoot, relativePath);

  if (sourceFile.endsWith(".tsx")) {
    // OpenTUI skips the Solid compile-time transform for packages loaded from
    // node_modules. Without this precompiled copy, JSX children such as
    // signal-derived counts are evaluated once during element creation and the
    // sidebar freezes on its first paint. The virtual ids make the compiled
    // package bind the host process's single OpenTUI/Solid runtime instead of
    // loading a second copy from the plugin package.
    await compileTsx(transformSolidSource, sourceFile, outputFile);
  } else {
    await copyPlainTypeScript(sourceFile, outputFile);
  }
}

// Both outputs come from the installed, lockfile-pinned package on every build.
// Neither transform bundles a runtime: one imports OpenTUI's virtual runtime
// module IDs, the other imports the raw-source TUI's ordinary Solid/OpenTUI modules.
await compileTsx(transformSolidSource, sharedSidebarSource, sharedSidebarHost);
await compileTsx(transformSolidSource, sharedSidebarSource, sharedSidebarLocal, false);
await copyFile(
  join(dirname(sharedSidebarSource), "../LICENSE"),
  join(outputRoot, "shared-sidebar.LICENSE"),
);

console.log(
  `build-tui: wrote ${files.length + 3} file(s) to ${relative(pluginRoot, outputRoot)}, including @cortexkit/opencode-sidebar@${sharedSidebarVersion} in both runtime paths and its license`,
);
