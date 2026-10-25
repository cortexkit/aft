import { expect, test } from "bun:test";

test("gather documentation states endpoint egress", async () => {
  const root = new URL("../../../../", import.meta.url);
  const config = await Bun.file(new URL("docs/config.md", root)).text();
  const tools = await Bun.file(new URL("docs/tools.md", root)).text();
  const note = await Bun.file(new URL(".alfonso/release-notes/next/search-gather.md", root)).text();
  const render = await Bun.file(new URL("crates/aft/src/commands/semantic_search/gather/render.rs", root)).text();
  const snippetBounds = await Bun.file(new URL("crates/aft/src/commands/semantic_search/snippet_bounds.rs", root)).text();
  const walk = await Bun.file(new URL("crates/aft/src/commands/semantic_search/gather/walk.rs", root)).text();
  const gather = await Bun.file(new URL("crates/aft/src/commands/semantic_search/gather/mod.rs", root)).text();
  const decision = await Bun.file(new URL("crates/aft/src/commands/semantic_search/gather/decision.rs", root)).text();
  const endpoint = await Bun.file(new URL("crates/aft/src/commands/semantic_search/gather/endpoint.rs", root)).text();
  const configSource = await Bun.file(new URL("crates/aft/src/config.rs", root)).text();
  const bridgeContext = await Bun.file(new URL("packages/aft-bridge/src/search-context.ts", root)).text();
  const openCodeContext = await Bun.file(new URL("packages/opencode-plugin/src/shared/search-context.ts", root)).text();
  const piContext = await Bun.file(new URL("packages/pi-plugin/src/shared/search-context.ts", root)).text();

  expect(config).toContain('"enabled": false');
  expect(config).toContain("off by default");
  expect(config).toContain("Host redaction does not apply to this request.");
  expect(config).toContain("local endpoint");
  expect(config).toContain("api_key_env");
  expect(config).toContain("deterministic");
  expect(config).toContain("project config");
  expect(tools).toContain("Related code (gathered for this search");
  expect(tools).toContain("context: host trace");
  expect(tools).toContain("context: query only");
  expect(tools).toContain("callgraph=unavailable reason=other_project");
  expect(render).toContain("Related code (gathered for this search");
  expect(render).toContain("host trace");
  expect(render).toContain("query only");
  expect(render).toContain("callgraph=unavailable reason=other_project");
  expect(render).toContain("skipped output_budget");
  expect(render).toContain("MAX_ADDED_BYTES: usize = 12 * 1024");
  expect(render).toContain("MAX_NODES: usize = 12");
  expect(snippetBounds).toContain("SEARCH_MAX_OUTPUT_BYTES");
  for (const field of ["model=", "rounds=", "considered=", "kept=", "fallback=", "context:", "gaps=", "stop="]) {
    expect(tools).toContain(field === "context:" ? "context: host trace" : `\`${field}\``);
    expect(render).toContain(field);
  }
  for (const state of [
    "building",
    "suspended",
    "unavailable",
    "off",
    "error",
    "no_seeds",
    "configuration_failure",
    "walk_failed",
    "output_budget",
    "other_project",
  ]) {
    expect(tools).toContain(`\`${state}\``);
    expect(render).toContain(state);
  }
  expect(walk).toContain("DEFAULT_MAX_ROUNDS: usize = 3");
  expect(walk).toContain("DEFAULT_MAX_NODES: usize = 12");
  expect(walk).toContain("DEFAULT_DECISION_TIMEOUT_MS: u64 = 3_000");
  expect(walk).toContain("DEFAULT_DEADLINE_MS: u64 = 8_000");
  expect(walk).toContain("MAX_OUTPUT_BYTES: usize = 12 * 1024");
  expect(gather).toContain("MAX_FRONTIER: usize = 40");
  expect(decision).toContain("DECISION_TTL: Duration = Duration::from_secs(600)");
  expect(endpoint).toContain("/chat/completions");
  expect(configSource).toContain("gather_enabled");
  expect(configSource).toContain("enabled == Some(true)");
  expect(bridgeContext).toContain("slice(-6000)");
  expect(openCodeContext).toContain("gatherEnabled(ctx.config)");
  expect(piContext).toContain("gatherEnabled(config)");
  expect(note).toContain("prototype");
  expect(note).toContain("off by default");
  expect(note.trim().split("\n")).toHaveLength(1);
});
