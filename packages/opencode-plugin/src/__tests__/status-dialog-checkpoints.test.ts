import { expect, test } from "bun:test";
import { join } from "node:path";

async function renderStatusFrame(extraEnv: Record<string, string> = {}): Promise<string> {
  const child = Bun.spawn(
    [
      process.execPath,
      "--preload",
      "@opentui/solid/preload",
      join(import.meta.dir, "fixtures/render-status-frame.tsx"),
    ],
    {
      cwd: join(import.meta.dir, "../.."),
      env: { ...process.env, ...extraEnv },
      stdout: "pipe",
      stderr: "pipe",
    },
  );
  const [frame, stderr, exitCode] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ]);
  expect({ exitCode, stderr }).toEqual({ exitCode: 0, stderr: "" });
  return frame;
}

test("TUI renders unknown current and project checkpoint counts", async () => {
  const frame = await renderStatusFrame();
  expect(frame).toMatch(/Checkpoints\s+—/);
  expect(frame).toMatch(/All-session checkpoints\s+—/);
}, 30_000);

test("TUI status dialog shows a single shared Index store size", async () => {
  const frame = await renderStatusFrame();
  expect(frame.match(/Index store\s+4\.0 KB/g)).toHaveLength(1);
  expect(frame).not.toMatch(/\bDisk\b/);
}, 30_000);

test("disk-floor OpenCode TUI status dialog labels both blocked planes and warns once", async () => {
  const frame = await renderStatusFrame({ AFT_STATUS_DISK_FLOOR: "1" });
  const compact = frame.replace(/\s+/g, " ");
  expect(compact.match(/paused \(low disk space\)/g)).toHaveLength(2);
  expect(compact).toContain(
    "Index store 4.0 KB ⚠ Index storage is paused: 3.7 GiB is free on this disk and AFT keeps 4.0 GiB free. Unused index data is being cleaned up; retry later.",
  );
  expect(compact.match(/Index storage is paused:/g)).toHaveLength(1);
}, 30_000);
