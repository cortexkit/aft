import { expect, test } from "bun:test";

function reportedGatherValues(render: string): Set<string> {
  const arms = render.match(/let state = match state \{([\s\S]*?)\n\s*\};/)?.[1];
  expect(arms).toBeDefined();
  const values = new Set<string>();
  for (const line of arms?.trim().split("\n") ?? []) {
    const arm = line.trim().match(/^((?:"[a-z_]+"|_|\s|\|)+)\s*=>\s*("[a-z_]+"|state),$/);
    expect(arm).not.toBeNull();
    if (!arm) continue;
    const literals = arm[2] === "state" ? arm[1] : arm[2];
    for (const literal of literals?.matchAll(/"([a-z_]+)"/g) ?? []) {
      values.add(literal[1]!);
    }
  }
  const disclosures = [...render.matchAll(/"[^"\n]*gather: skipped ([^"\n]*)"/g)];
  expect(disclosures.length).toBeGreaterThan(0);
  for (const disclosure of disclosures) {
    if (disclosure[1] === "callgraph={state}") continue;
    const value = disclosure[1]?.match(/^(?:callgraph=)?([a-z_]+)(?: reason=([a-z_]+))?$/);
    expect(value).not.toBeNull();
    if (value) values.add((value[2] ?? value[1])!);
  }
  return values;
}

test("gather documentation states endpoint egress", async () => {
  const root = new URL("../../../../", import.meta.url);
  const config = await Bun.file(new URL("docs/config.md", root)).text();
  const tools = await Bun.file(new URL("docs/tools.md", root)).text();
  const note = await Bun.file(new URL(".alfonso/release-notes/next/search-gather.md", root)).text();
  const render = await Bun.file(
    process.env.AFT_GATHER_RENDER_SOURCE ??
      new URL("crates/aft/src/commands/semantic_search/gather/render.rs", root),
  ).text();
  const snippetBounds = await Bun.file(
    new URL("crates/aft/src/commands/semantic_search/snippet_bounds.rs", root),
  ).text();
  const walk = await Bun.file(
    new URL("crates/aft/src/commands/semantic_search/gather/walk.rs", root),
  ).text();
  const gather = await Bun.file(
    new URL("crates/aft/src/commands/semantic_search/gather/mod.rs", root),
  ).text();
  const decision = await Bun.file(
    new URL("crates/aft/src/commands/semantic_search/gather/decision.rs", root),
  ).text();
  const endpoint = await Bun.file(
    new URL("crates/aft/src/commands/semantic_search/gather/endpoint.rs", root),
  ).text();
  const configSource = await Bun.file(new URL("crates/aft/src/config.rs", root)).text();
  const bridgeContext = await Bun.file(
    new URL("packages/aft-bridge/src/search-context.ts", root),
  ).text();
  const openCodeContext = await Bun.file(
    new URL("packages/opencode-plugin/src/shared/search-context.ts", root),
  ).text();
  const piContext = await Bun.file(
    new URL("packages/pi-plugin/src/shared/search-context.ts", root),
  ).text();
  const graph = await Bun.file(
    new URL("crates/aft/src/commands/semantic_search/gather/graph.rs", root),
  ).text();
  const grepSource = await Bun.file(new URL("crates/aft/src/commands/grep.rs", root)).text();

  expect(config).toContain('"enabled": false');
  expect(config).toContain("OMP uses the Pi collector");
  expect(config).toContain("No test checks this timing");
  expect(config).toContain("when the current turn is not stored");
  for (const doc of [config, tools]) {
    expect(doc).toContain("fallback=<round>:<reason>");
    expect(doc).not.toContain("It adds one state line.");
  }
  expect(tools).not.toContain("| `configuration_failure` |");
  expect(tools).toContain("`gaps=` counts graph gaps");
  expect(tools).toContain("`gather_notice`");
  for (const [limit, maximum] of [
    ["max_rounds", 3],
    ["max_nodes", 12],
    ["max_frontier", 40],
    ["decision_timeout_ms", 3000],
    ["deadline_ms", 8000],
    ["max_output_bytes", 12288],
  ]) {
    expect(config).toContain(`| \`${limit}\` | ${maximum} |`);
  }
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
  const reportedValues = reportedGatherValues(render);
  const table = tools.split("#### Related-code gathering")[1]?.split("#### ")[0] ?? "";
  const documentedValues = [...table.matchAll(/^\| `([^`]+)` \|/gm)].map((match) => match[1]);
  expect(new Set(documentedValues)).toEqual(reportedValues);
  expect(documentedValues).toHaveLength(reportedValues.size);
  expect(grepSource).toMatch(/GREP_MAX_OUTPUT_BYTES\s*:\s*usize\s*=\s*50\s*\*\s*1024\s*;/);
  expect(snippetBounds).toMatch(
    /SEARCH_MAX_OUTPUT_BYTES\s*:\s*usize\s*=\s*GREP_MAX_OUTPUT_BYTES\s*;/,
  );
  expect(render).toContain("snippet_bounds::SEARCH_MAX_OUTPUT_BYTES");
  const capKb = Number(
    grepSource.match(/GREP_MAX_OUTPUT_BYTES\s*:\s*usize\s*=\s*(\d+)\s*\*\s*1024\s*;/)?.[1],
  );
  expect(table).toContain(`${capKb} KB limit`);
  expect(config).toContain(`${capKb} KB limit`);
  expect(gather).toMatch(/MAX_SEEDS\s*:\s*usize\s*=\s*5\s*;/);
  expect(graph).toMatch(/\.take\(MAX_SEEDS\)/);
  const seeds = Number(gather.match(/MAX_SEEDS\s*:\s*usize\s*=\s*(\d+)\s*;/)?.[1]);
  expect(table).toContain(`at most ${seeds} seed symbols`);
  expect(config).toContain(`at most ${seeds} seed symbols`);
  expect(render).toContain("Related code (gathered for this search");
  expect(render).toContain("host trace");
  expect(render).toContain("query only");
  expect(render).toContain("callgraph=unavailable reason=other_project");
  expect(render).toContain("skipped output_budget");
  expect(gather).toContain("MAX_OUTPUT_BYTES: usize = 12 * 1024");
  expect(gather).toContain("MAX_NODES: usize = 12");
  expect(snippetBounds).toContain("SEARCH_MAX_OUTPUT_BYTES");
  for (const field of [
    "model=",
    "rounds=",
    "considered=",
    "kept=",
    "fallback=",
    "context:",
    "gaps=",
    "stop=",
  ]) {
    expect(tools).toContain(field === "context:" ? "context: host trace" : `\`${field}\``);
    expect(render).toContain(field);
  }
  expect(walk).toContain("DEFAULT_MAX_ROUNDS: usize = 3");
  expect(walk).toContain("count(raw.max_nodes, super::MAX_NODES).min(super::MAX_NODES)");
  expect(walk).toContain("DEFAULT_DECISION_TIMEOUT_MS: u64 = 3_000");
  expect(walk).toContain("DEFAULT_DEADLINE_MS: u64 = 8_000");
  expect(walk).toContain("count(raw.max_output_bytes, super::MAX_OUTPUT_BYTES)");
  expect(walk).toContain(".min(super::MAX_OUTPUT_BYTES)");
  expect(gather).toContain("MAX_FRONTIER: usize = 40");
  expect(decision).toContain("DECISION_TTL: Duration = Duration::from_secs(600)");
  expect(endpoint).toContain("/chat/completions");
  expect(configSource).toContain("gather_enabled");
  expect(configSource).toContain("enabled == Some(true)");
  expect(bridgeContext).toContain("SEARCH_CONTEXT_MAX_CHARACTERS = 6000");
  expect(bridgeContext).toContain("slice(-SEARCH_CONTEXT_MAX_CHARACTERS)");
  expect(openCodeContext).toContain("gatherEnabled(ctx.config)");
  expect(piContext).toContain("gatherEnabled(config)");
  expect(note).toContain("prototype");
  expect(note).toContain("off by default");
  expect(note.trim().split("\n")).toHaveLength(1);
});
