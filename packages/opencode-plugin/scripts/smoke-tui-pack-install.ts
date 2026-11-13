import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const pluginRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = join(pluginRoot, "../..");
const packageName = "@cortexkit/aft-opencode";
const runtimeSpecifiers = [
  "@opentui/core",
  "@opentui/core/testing",
  "@opentui/solid",
  "@opentui/solid/components",
  "@opentui/solid/jsx-runtime",
  "@opentui/solid/jsx-dev-runtime",
  "solid-js",
  "solid-js/store",
];

let failures = 0;
let checks = 0;
function check(name: string, cond: boolean, detail?: string): void {
  checks++;
  if (cond) {
    console.log(`  ok  ${name}`);
  } else {
    failures++;
    console.log(`FAIL  ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

function fail(name: string, detail: string): never {
  check(name, false, detail);
  throw new Error(`${name}: ${detail}`);
}

function run(command: string, args: string[], cwd: string): string {
  const result = spawnSync(command, args, {
    cwd,
    encoding: "utf8",
    env: process.env,
    stdio: ["ignore", "pipe", "pipe"],
    maxBuffer: 20 * 1024 * 1024,
  });

  if (result.stdout) process.stdout.write(result.stdout);
  if (result.stderr) process.stderr.write(result.stderr);

  if (result.status !== 0) {
    fail(`${command} ${args.join(" ")}`, `exited with ${result.status ?? "unknown status"}`);
  }
  return result.stdout;
}

function parsePackedFilename(stdout: string): string {
  try {
    const packed = JSON.parse(stdout.trim()) as Array<{ filename?: string }>;
    const filename = packed[0]?.filename;
    if (typeof filename === "string" && filename.length > 0) return filename;
  } catch {
    // npm can print warnings around the JSON on some versions; fall back to the
    // last tarball-looking line rather than making the smoke version-fragile.
  }

  const fallback = stdout
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.endsWith(".tgz"))
    .at(-1);
  if (fallback) return fallback;
  fail("npm pack reports a tarball", stdout.trim() || "no stdout");
}

const scratchRoot = join(repoRoot, "target/scratch");
await mkdir(scratchRoot, { recursive: true });
const tempRoot = await mkdtemp(join(scratchRoot, "aft-opencode-tui-pack-"));
const installRoot = join(tempRoot, "install");

try {
  run("bun", ["run", "--cwd", "packages/aft-bridge", "build"], repoRoot);
  run("bun", ["run", "build"], pluginRoot);

  const packStdout = run("npm", ["pack", "--json", "--pack-destination", tempRoot], pluginRoot);
  const tarball = join(tempRoot, parsePackedFilename(packStdout));
  check("npm pack produced a tarball", existsSync(tarball), tarball);

  await mkdir(installRoot, { recursive: true });
  await writeFile(
    join(installRoot, "package.json"),
    JSON.stringify(
      {
        private: true,
        type: "module",
        dependencies: {
          [packageName]: `file:${tarball}`,
        },
      },
      null,
      2,
    ),
  );
  run("bun", ["install", "--production"], installRoot);

  const installedPackageRoot = join(installRoot, "node_modules", "@cortexkit", "aft-opencode");
  const compiledIndex = join(installedPackageRoot, "src/tui-compiled/index.tsx");
  check("compiled TUI index ships in the packed package", existsSync(compiledIndex));

  for (const runtime of ["host", "local"]) {
    const framePath = join(installedPackageRoot, `src/tui-compiled/shared-sidebar-${runtime}.tsx`);
    const frameSource = await readFile(framePath, "utf8");
    check(
      `packed ${runtime} module contains the shared frame`,
      frameSource.includes("function SidebarFrame("),
    );
    check(
      `packed ${runtime} module is reactively compiled`,
      frameSource.includes("_$insert(") || frameSource.includes("_$effect("),
    );
    check(
      `packed ${runtime} module does not bundle a Solid runtime`,
      !frameSource.includes("function createSignal(") &&
        !frameSource.includes("function createContext("),
    );
    check(
      `packed ${runtime} module does not bundle an OpenTUI runtime`,
      !frameSource.includes("class CliRenderer"),
    );
    check(
      `packed ${runtime} module binds the expected runtime`,
      runtime === "host"
        ? frameSource.includes("opentui:runtime-module:") &&
            !/from\s+["'](?:solid-js|@opentui\/solid)["']/.test(frameSource)
        : !frameSource.includes("opentui:runtime-module:") &&
            /from\s+["']solid-js["']/.test(frameSource) &&
            /from\s+["']@opentui\/solid["']/.test(frameSource),
    );
    console.log(`  shared-sidebar-${runtime}.tsx: ${Buffer.byteLength(frameSource)} bytes`);
  }
  check(
    "shared sidebar license ships with the generated modules",
    existsSync(join(installedPackageRoot, "src/tui-compiled/shared-sidebar.LICENSE")),
  );
  const panelSource = await readFile(
    join(installedPackageRoot, "src/tui-compiled/sidebar-view.tsx"),
    "utf8",
  );
  check(
    "compiled panel uses the host shared-frame module",
    panelSource.includes("./shared-sidebar-host.tsx"),
  );

  const compiledIndexText = existsSync(compiledIndex) ? await readFile(compiledIndex, "utf8") : "";
  check(
    "compiled TUI imports the host runtime virtual modules",
    compiledIndexText.includes("opentui:runtime-module:"),
  );

  const stubbedRuntimeProbe = `
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const packageRoot = process.argv[1];
const runtimeSpecifiers = ${JSON.stringify(runtimeSpecifiers)};
const mid = (specifier) => "opentui:runtime-module:" + encodeURIComponent(specifier);
const resolved = new Map(runtimeSpecifiers.map((specifier) => [specifier, import.meta.resolve(specifier)]));

Bun.plugin({
  name: "opentui-runtime-module-stubs",
  setup(build) {
    for (const specifier of runtimeSpecifiers) {
      const target = resolved.get(specifier);
      build.module(mid(specifier), () => ({
        loader: "js",
        contents: "export * from " + JSON.stringify(target) + ";\\n" +
          "import * as namespace from " + JSON.stringify(target) + ";\\n" +
          "export default ('default' in namespace ? namespace.default : namespace);\\n",
      }));
    }
  },
});

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

const solid = await import(mid("solid-js"));
const ordinarySolid = await import(resolved.get("solid-js"));
assert(
  typeof solid.createSignal === "function" && typeof solid.createEffect === "function",
  "stubbed solid-js virtual module exposes reactive primitives",
);
assert(solid.createSignal === ordinarySolid.createSignal && solid.createContext === ordinarySolid.createContext,
  "virtual and ordinary runtime names must resolve to one physical Solid copy");

const compiledSidebar = await readFile(join(packageRoot, "src/tui-compiled/sidebar.tsx"), "utf8");
const rawSidebar = await readFile(join(packageRoot, "src/tui/sidebar.tsx"), "utf8");
assert(
  compiledSidebar.includes("_$effect(") || compiledSidebar.includes("_$insert("),
  "compiled sidebar lacks OpenTUI reactive wrapper calls",
);
assert(
  !rawSidebar.includes("_$effect(") && !rawSidebar.includes("_$insert("),
  "raw sidebar unexpectedly contains compiled wrapper calls",
);

const compiled = await import(pathToFileURL(join(packageRoot, "src/tui-compiled/index.tsx")).href);
assert(
  compiled.default?.id === "aft-opencode" && typeof compiled.default?.tui === "function" && typeof compiled.default?.setup === "function",
  "compiled TUI default export shape is invalid",
);
const entry = await import(pathToFileURL(join(packageRoot, "src/tui/entry.mjs")).href);
assert(
  entry.default === compiled.default,
  "loader entry default export shape is invalid",
);
const hybrid = await import(pathToFileURL(join(packageRoot, "src/entry/tui.mjs")).href);
const directory = await import(pathToFileURL(join(packageRoot, "tui.js")).href);
assert(hybrid.default?.tui === entry.default.tui && typeof hybrid.default?.setup === "function",
  "V2 hybrid entry must preserve the V1 initializer and expose setup");
assert(directory.default === hybrid.default, "directory TUI entry must load the V2 hybrid entry");

const { AftSidebarPanel } = await import(pathToFileURL(join(packageRoot, "src/tui-compiled/sidebar-view.tsx")).href);
const { DEFAULT_PREFS } = await import(pathToFileURL(join(packageRoot, "src/tui-compiled/preferences.ts")).href);
const { testRender } = await import(mid("@opentui/solid"));
const [collapsed, setCollapsed] = solid.createSignal(false);
const setup = await testRender(() => solid.createComponent(AftSidebarPanel, {
  palette: { text: "#ffffff", textMuted: "#888888", success: "#00ff00", warning: "#ffff00", error: "#ff0000", accent: "#0088ff", background: "#000000", border: "#444444" },
  snapshot: { version: "0.64.0", cache_role: "main", degraded: true, degraded_reasons: ["home_root"], search_index: { status: "ready", files: 12 }, semantic_index: { status: "ready" } },
  prefs: structuredClone(DEFAULT_PREFS),
  get collapsed() { return collapsed(); },
  onToggleCollapsed: () => setCollapsed(value => !value),
  pluginVersion: "0.0.0",
}), { width: 38, height: 80 });
try {
  await setup.renderOnce();
  const expanded = setup.captureCharFrame();
  assert(expanded.includes("▼ AFT   DEGRADED") && expanded.includes("Status") && expanded.includes("ready"),
    "host-virtual shared frame must render the expanded AFT content and header marker");
  setCollapsed(true);
  await setup.renderOnce();
  const compact = setup.captureCharFrame();
  assert(compact.includes("▶ AFT   DEGRADED") && !compact.includes("Status"),
    "host-virtual shared frame must react to collapse using the same Solid runtime");
} finally {
  setup.renderer.destroy();
}
console.log("host-virtual shared frame rendered expanded and collapsed through V1/V2 packed entries");
console.log("stubbed runtime probe loaded the compiled TUI path");
`;
  run("bun", ["-e", stubbedRuntimeProbe, installedPackageRoot], installRoot);
  check("stubbed virtual runtime probe imports the compiled TUI entry", true);

  const rawFallbackProbe = `
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const packageRoot = process.argv[1];
const raw = await import(pathToFileURL(join(packageRoot, "src/tui/index.tsx")).href);
const entry = await import(pathToFileURL(join(packageRoot, "src/tui/entry.mjs")).href);
if (entry.default !== raw.default) {
  throw new Error("loader entry did not fall back to the raw TSX module under bare Bun");
}
if (entry.default?.id !== "aft-opencode" || typeof entry.default?.tui !== "function") {
  throw new Error("raw TSX fallback default export shape is invalid");
}
const hybrid = await import(pathToFileURL(join(packageRoot, "src/entry/tui.mjs")).href);
if (hybrid.default?.tui !== raw.default.tui || typeof hybrid.default?.setup !== "function") {
  throw new Error("raw V2 hybrid entry did not preserve the V1 initializer and expose setup");
}
console.log("raw TSX fallback probe loaded the TUI entry");
`;
  run("bun", ["-e", rawFallbackProbe, installedPackageRoot], installRoot);
  check("bare Bun probe imports the raw-TSX fallback", true);
} finally {
  if (process.env.KEEP_TUI_PACK_SMOKE !== "1") {
    await rm(tempRoot, { recursive: true, force: true });
  } else {
    console.log(`kept smoke temp dir: ${tempRoot}`);
  }
}

if (failures > 0) {
  console.error(`\nsmoke-tui-pack-install: ${failures} check(s) failed`);
  process.exit(1);
}
console.log(`\nsmoke-tui-pack-install: all ${checks} checks passed`);
