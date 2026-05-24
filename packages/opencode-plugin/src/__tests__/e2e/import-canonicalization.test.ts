/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { writeFile } from "node:fs/promises";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type PreparedBinary,
  prepareBinary,
  readTextFile,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

maybeDescribe("e2e import canonicalization", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await cleanupHarnesses(harnesses);
  });

  async function harness(): Promise<E2EHarness> {
    const created = await createHarness(preparedBinary, { fixtureNames: [] });
    harnesses.push(created);
    return created;
  }

  test("organize preserves aliased named imports", async () => {
    const h = await harness();
    const filePath = h.path("aliased.ts");
    await writeFile(
      filePath,
      'import { foo as bar, baz } from "x";\nconsole.log(bar, baz);\n',
      "utf8",
    );

    const before = await readTextFile(filePath);
    expect(before).toContain("foo as bar");
    const response = await h.bridge.send("organize_imports", { file: filePath });

    expect(response.success).toBe(true);
    const after = await readTextFile(filePath);
    expect(after).toContain("foo as bar");
    expect(after).toContain("baz");
  });

  test("organize preserves per-name type markers", async () => {
    const h = await harness();
    const filePath = h.path("types.ts");
    await writeFile(
      filePath,
      'import { type Foo, Bar } from "x";\nconst value: Foo | Bar = {} as Foo;\n',
      "utf8",
    );

    expect(await readTextFile(filePath)).toContain("type Foo");
    const response = await h.bridge.send("organize_imports", { file: filePath });

    expect(response.success).toBe(true);
    const after = await readTextFile(filePath);
    expect(after).toContain("type Foo");
    expect(after).toContain("Bar");
  });

  test("organize preserves mixed aliases and type qualifiers", async () => {
    const h = await harness();
    const filePath = h.path("mixed.ts");
    await writeFile(
      filePath,
      'import { type FooT as F, bar } from "x";\nconst value: F = bar as F;\n',
      "utf8",
    );

    expect(await readTextFile(filePath)).toContain("type FooT as F");
    const response = await h.bridge.send("organize_imports", { file: filePath });

    expect(response.success).toBe(true);
    const after = await readTextFile(filePath);
    expect(after).toContain("type FooT as F");
    expect(after).toContain("bar");
  });

  test("organize keeps Rust re-export aliases distinct from plain re-exports", async () => {
    const h = await harness();
    const filePath = h.path("lib.rs");
    await writeFile(
      filePath,
      "mod foo { pub struct Bar; }\npub use foo::Bar as Baz;\npub use foo::Bar;\n",
      "utf8",
    );

    const before = await readTextFile(filePath);
    expect(before).toContain("pub use foo::Bar as Baz;");
    expect(before).toContain("pub use foo::Bar;");
    const response = await h.bridge.send("organize_imports", { file: filePath });

    expect(response.success).toBe(true);
    const after = await readTextFile(filePath);
    expect(after).toContain("Bar as Baz");
    expect(after).toMatch(/\bBar\b/);
    expect(after).not.toBe("mod foo { pub struct Bar; }\npub use foo::Bar;\n");
  });
});
