import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { access, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { BinaryBridge } from "../../bridge.js";
import { inlineUserConfigTier } from "../../config-tiers.js";
import { isolatedAftEnvironment } from "../../test-child-environment.js";

interface FileResult {
  filePath: string;
  movePath?: string;
  type: string;
  additions: number;
  deletions: number;
  patch?: string;
  diff_omitted?: boolean;
}

interface MutationReply {
  success: boolean;
  complete?: boolean;
  partial?: boolean;
  all_failed?: boolean;
  preview?: boolean;
  diff_omitted?: boolean;
  diff_notice?: string;
  preview_diff?: string;
  output?: string;
  diff?: { additions: number; deletions: number; before?: string; after?: string };
  metadata?: { diff?: string; diff_omitted?: boolean; files?: FileResult[] };
  files?: FileResult[];
  failures?: { path: string; error: string }[];
}

const budget = 1024 * 1024;

describe("bounded mutation diffs through the real bridge", () => {
  let root: string;
  let project: string;
  let bridge: BinaryBridge;

  beforeEach(async () => {
    const binary =
      process.env.AFT_BINARY_PATH ?? resolve(import.meta.dir, "../../../../../target/debug/aft");
    await access(binary);
    const scratch = resolve(import.meta.dir, "../../../../../target/scratch");
    await mkdir(scratch, { recursive: true });
    root = await mkdtemp(join(scratch, "bounded-mutation-"));
    project = join(root, "project");
    await mkdir(project);
    bridge = new BinaryBridge(binary, project, {
      timeoutMs: 60_000,
      maxRestarts: 0,
      childEnv: isolatedAftEnvironment(root),
    });
    const configured = await bridge.send("configure", {
      project_root: project,
      storage_dir: join(root, "storage"),
      harness: "opencode",
      config: inlineUserConfigTier({
        indexes: { trigram: false, semantic: false },
        format_on_edit: false,
        lsp: { enabled: false },
      }),
    });
    expect(configured.success).toBe(true);
  });

  afterEach(async () => {
    await bridge?.shutdown();
    if (root) await rm(root, { recursive: true, force: true });
  });

  async function send(command: string, params: Record<string, unknown>): Promise<MutationReply> {
    return (await bridge.send(command, params)) as unknown as MutationReply;
  }

  function omitted(reply: MutationReply) {
    expect(reply.success).toBe(true);
    expect(reply.diff_omitted).toBe(true);
    expect(reply.metadata?.diff_omitted).toBe(true);
    expect(reply.diff_notice).toBeString();
    expect(reply.diff_notice?.length).toBeGreaterThan(0);
    expect(Buffer.byteLength(JSON.stringify(reply))).toBeLessThan(budget + 16_384);
    expect(bridge.isAlive()).toBe(true);
  }

  test("reported-size deletion, two moves and a failed hunk preserve the partial native outcome", async () => {
    // 380,960 LF-terminated rows totaling 33,608,233 bytes.
    const content = `${"x".repeat(88)}\n`.repeat(83_753) + `${"x".repeat(87)}\n`.repeat(297_207);
    expect(Buffer.byteLength(content)).toBe(33_608_233);
    await writeFile(join(project, "huge.csv"), content);
    for (const name of ["one.md", "two.md", "fail.md"])
      await writeFile(join(project, name), "old\n");
    const patch = [
      "*** Begin Patch",
      "*** Delete File: huge.csv",
      "*** Update File: one.md",
      "*** Move to: moved-one.md",
      "@@",
      "-old",
      "+new",
      "*** Update File: two.md",
      "*** Move to: moved-two.md",
      "@@",
      "-old",
      "+new",
      "*** Update File: fail.md",
      "@@",
      "-absent",
      "+wrong",
      "*** End Patch",
    ].join("\n");
    const reply = await send("apply_patch", { patch_text: patch });
    omitted(reply);
    expect(reply.complete).toBe(false);
    expect(reply.partial).toBe(true);
    expect(reply.all_failed).toBe(false);
    expect(reply.failures).toHaveLength(1);
    expect(JSON.stringify(reply.failures)).toContain("fail.md");
    expect(reply.output).toContain(reply.diff_notice as string);
    const files = reply.metadata?.files ?? [];
    expect(files).toHaveLength(3);
    expect(files[0].type).toBe("delete");
    expect(files[0].filePath).toEndWith("huge.csv");
    expect(files[0].additions).toBe(0);
    expect(files[0].deletions).toBe(380_960);
    expect(files[0].diff_omitted).toBe(true);
    expect(files[0].patch).toBeUndefined();
    for (const [index, name] of [
      [1, "one"],
      [2, "two"],
    ] as const) {
      expect(files[index].type).toBe("move");
      expect(files[index].movePath).toEndWith(`moved-${name}.md`);
      expect(files[index].additions).toBe(1);
      expect(files[index].deletions).toBe(1);
      expect(await readFile(join(project, `moved-${name}.md`), "utf8")).toBe("new\n");
      expect(await Bun.file(join(project, `${name}.md`)).exists()).toBe(false);
    }
    expect(await Bun.file(join(project, "huge.csv")).exists()).toBe(false);
    expect(await readFile(join(project, "fail.md"), "utf8")).toBe("old\n");
    expect((await bridge.send("status", {})).success).toBe(true);
  }, 120_000);

  test("huge edit and write retain counts, while permission previews never mutate", async () => {
    const original = "old\n".repeat(100_000);
    const replacement = "new\n".repeat(100_000);
    const file = join(project, "edit.txt");
    await writeFile(file, original);
    for (const command of ["edit_match", "write", "apply_patch"]) {
      const params =
        command === "edit_match"
          ? {
              file,
              match: "old",
              replacement: "new",
              replace_all: true,
              preview: true,
              include_diff_content: true,
            }
          : command === "write"
            ? { file, content: replacement, preview: true, include_diff_content: true }
            : {
                patch_text: "*** Begin Patch\n*** Delete File: edit.txt\n*** End Patch",
                preview: true,
              };
      const preview = await send(command, params);
      omitted(preview);
      expect(preview.preview).toBe(true);
      expect(preview.preview_diff).toBeUndefined();
      expect(preview.metadata?.diff).toBeUndefined();
      if (command !== "apply_patch") {
        expect(preview.diff?.additions).toBe(100_000);
        expect(preview.diff?.deletions).toBe(100_000);
        expect(preview.diff?.before).toBeUndefined();
        expect(preview.diff?.after).toBeUndefined();
      } else {
        expect(preview.files?.[0].deletions).toBe(100_000);
        expect(preview.files?.[0].type).toBe("delete");
      }
      expect(await readFile(file, "utf8")).toBe(original);
    }
    const edited = await send("edit_match", {
      file,
      match: "old",
      replacement: "new",
      replace_all: true,
    });
    omitted(edited);
    expect(edited.diff?.additions).toBe(100_000);
    expect(edited.diff?.deletions).toBe(100_000);
    expect(await readFile(file, "utf8")).toBe(replacement);
    const written = await send("write", { file, content: original });
    omitted(written);
    expect(written.diff?.additions).toBe(100_000);
    expect(written.diff?.deletions).toBe(100_000);
    expect(await readFile(file, "utf8")).toBe(original);
  }, 120_000);

  test("many small Unicode and escaped patches share one aggregate reply budget", async () => {
    const line = 'é🦀"\\\t\0'.repeat(3000);
    const hunks = Array.from(
      { length: 40 },
      (_, index) => `*** Add File: file-${index}.txt\n+${line}`,
    );
    const reply = await send("apply_patch", {
      patch_text: `*** Begin Patch\n${hunks.join("\n")}\n*** End Patch`,
    });
    omitted(reply);
    expect(reply.complete).toBe(true);
    const files = reply.metadata?.files ?? [];
    expect(files).toHaveLength(40);
    expect(files.some((file) => file.patch?.includes('é🦀"\\\t\0'))).toBe(true);
    expect(files.some((file) => file.diff_omitted)).toBe(true);
    for (const [index, file] of files.entries()) {
      expect(file.type).toBe("add");
      expect(file.additions).toBe(1);
      expect(file.deletions).toBe(0);
      expect(await readFile(join(project, `file-${index}.txt`), "utf8")).toBe(`${line}\n`);
    }
  }, 120_000);

  test("ordinary patch and preview diff bytes are unchanged", async () => {
    const file = join(project, "small.txt");
    await writeFile(file, "old\n");
    const expected = `Index: ${file}\n===================================================================\n--- ${file}\n+++ ${file}\n@@ -1 +1 @@\n-old\n+new\n`;
    const preview = await send("write", { file, content: "new\n", preview: true });
    expect(preview.metadata?.diff).toBe(expected);
    expect(preview.preview_diff).toBe(expected);
    expect(preview.diff_omitted).toBeUndefined();
    const applied = await send("apply_patch", {
      patch_text: "*** Begin Patch\n*** Update File: small.txt\n@@\n-old\n+new\n*** End Patch",
    });
    expect(applied.metadata?.diff).toBe(expected);
    expect(applied.metadata?.files?.[0].patch).toBe(expected);
    expect(applied.diff_omitted).toBeUndefined();
  });
});
