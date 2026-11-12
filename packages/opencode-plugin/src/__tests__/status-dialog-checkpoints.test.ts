import { expect, test } from "bun:test";
import { join } from "node:path";

test("TUI renders unknown current and project checkpoint counts", async () => {
  const child = Bun.spawn(
    [
      process.execPath,
      "--preload",
      "@opentui/solid/preload",
      join(import.meta.dir, "fixtures/render-status-frame.tsx"),
    ],
    { cwd: join(import.meta.dir, "../.."), stdout: "pipe", stderr: "pipe" },
  );
  const [frame, stderr, exitCode] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ]);
  expect({ exitCode, stderr }).toEqual({ exitCode: 0, stderr: "" });
  expect(frame).toMatch(/Checkpoints\s+—/);
  expect(frame).toMatch(/All-session checkpoints\s+—/);
}, 30_000);
