/**
 * E2E coverage for aft_transform (5 ops).
 * Regression for wrong Rust command names (was sending "transform"; Rust
 * dispatches each op name directly: add_member, add_derive, wrap_try_catch,
 * add_decorator, add_struct_tags).
 */

/// <reference path="../../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import { createHarness, type Harness, prepareBinary, writeFixture } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = initialBinary.binaryPath ? describe : describe.skip;

maybeDescribe("aft_transform (real bridge)", () => {
  let harness: Harness;

  beforeAll(async () => {
    harness = await createHarness(initialBinary);
  });

  afterAll(async () => {
    if (harness) await harness.cleanup();
  });

  test("add_derive adds Rust derive attributes", async () => {
    await writeFixture(harness, "derives.rs", `pub struct Widget {\n    pub id: u32,\n}\n`);
    await harness.callTool("aft_transform", {
      op: "add_derive",
      filePath: "derives.rs",
      target: "Widget",
      derives: ["Clone", "Debug"],
    });
    const after = await readFile(harness.path("derives.rs"), "utf8");
    expect(after).toContain("#[derive(");
    expect(after).toContain("Clone");
    expect(after).toContain("Debug");
  });

  test("add_member inserts a method into a TypeScript class", async () => {
    await writeFixture(
      harness,
      "member.ts",
      `export class Calc {\n  add(a: number, b: number): number {\n    return a + b;\n  }\n}\n`,
    );
    await harness.callTool("aft_transform", {
      op: "add_member",
      filePath: "member.ts",
      container: "Calc",
      code: "sub(a: number, b: number): number {\n  return a - b;\n}",
    });
    const after = await readFile(harness.path("member.ts"), "utf8");
    expect(after).toContain("sub(");
    expect(after).toContain("a - b");
  });

  test("add_decorator adds a Python decorator", async () => {
    await writeFixture(harness, "decorated.py", `def greet(name):\n    return f"hello {name}"\n`);
    await harness.callTool("aft_transform", {
      op: "add_decorator",
      filePath: "decorated.py",
      target: "greet",
      decorator: "staticmethod",
    });
    const after = await readFile(harness.path("decorated.py"), "utf8");
    expect(after).toContain("@staticmethod");
    expect(after).toContain("def greet");
  });

  test("wrap_try_catch wraps a TS function body", async () => {
    await writeFixture(
      harness,
      "risky.ts",
      `export function risky(): number {\n  return JSON.parse("{}").value;\n}\n`,
    );
    await harness.callTool("aft_transform", {
      op: "wrap_try_catch",
      filePath: "risky.ts",
      target: "risky",
    });
    const after = await readFile(harness.path("risky.ts"), "utf8");
    expect(after).toContain("try");
    expect(after).toContain("catch");
  });

  test("add_struct_tags adds Go JSON tags", async () => {
    await writeFixture(
      harness,
      "tagged.go",
      `package sample\n\ntype User struct {\n\tName string\n}\n`,
    );
    await harness.callTool("aft_transform", {
      op: "add_struct_tags",
      filePath: "tagged.go",
      target: "User",
      field: "Name",
      tag: "json",
      value: "name",
    });
    const after = await readFile(harness.path("tagged.go"), "utf8");
    expect(after).toContain("json:");
    expect(after).toContain("name");
  });
});
