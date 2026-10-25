import { expect, test } from "bun:test";

test("gather documentation states endpoint egress", async () => {
  const root = new URL("../../../../", import.meta.url);
  const config = await Bun.file(new URL("docs/config.md", root)).text();
  const tools = await Bun.file(new URL("docs/tools.md", root)).text();
  const note = await Bun.file(new URL(".alfonso/release-notes/next/search-gather.md", root)).text();

  expect(config).toContain('"enabled": false');
  expect(config).toContain("outside host redaction");
  expect(config).toContain("local endpoint");
  expect(config).toContain("api_key_env");
  expect(config).toContain("deterministic");
  expect(config).toContain("project config");
  expect(tools).toContain("Related code (gathered for this search");
  expect(tools).toContain("query-only");
  expect(note.trim().split("\n")).toHaveLength(1);
});
