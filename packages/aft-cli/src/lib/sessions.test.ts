import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { listPiSessionsFromDir, mapOpenCodeSessionRows, parsePiSessionJsonl } from "./sessions";

describe("mapOpenCodeSessionRows", () => {
  test("maps OpenCode directory and creation time for session log scoping", () => {
    expect(
      mapOpenCodeSessionRows([
        {
          id: "ses_target",
          title: "target",
          time_updated: 200,
          time_created: 100,
          directory: "/projects/target",
        },
      ]),
    ).toEqual([
      {
        id: "ses_target",
        title: "target",
        lastActivity: 200,
        startedAt: 100,
        projectRoot: "/projects/target",
      },
    ]);
  });

  test("maps rows to RecentSession entries in newest-first order capped at five", () => {
    const rows = [
      { id: "ses_1", title: "one", time_updated: 100 },
      { id: "ses_6", title: "six", time_updated: 600 },
      { id: "ses_3", title: "three", time_updated: 300 },
      { id: "", title: "invalid", time_updated: 700 },
      { id: "ses_5", title: "five", time_updated: 500 },
      { id: "ses_2", title: "two", time_updated: 200 },
      { id: "ses_4", title: "four", time_updated: 400 },
    ];

    expect(mapOpenCodeSessionRows(rows)).toEqual([
      { id: "ses_6", title: "six", lastActivity: 600 },
      { id: "ses_5", title: "five", lastActivity: 500 },
      { id: "ses_4", title: "four", lastActivity: 400 },
      { id: "ses_3", title: "three", lastActivity: 300 },
      { id: "ses_2", title: "two", lastActivity: 200 },
    ]);
  });
});

describe("parsePiSessionJsonl", () => {
  test("extracts session id and first user message content-array text", () => {
    const jsonl = [
      JSON.stringify({ type: "session", id: "019e6307-0749-4000-9000-111111111111" }),
      JSON.stringify({ type: "model_change", modelId: "gpt" }),
      JSON.stringify({
        type: "message",
        message: {
          role: "user",
          content: [
            { type: "text", text: "  Explain this bug\nplease " },
            { type: "image", url: "ignored" },
          ],
        },
      }),
    ].join("\n");

    expect(parsePiSessionJsonl(jsonl)).toEqual({
      id: "019e6307-0749-4000-9000-111111111111",
      title: "Explain this bug please",
    });
  });

  test("reads session project root and start time from the Pi header", () => {
    expect(
      parsePiSessionJsonl(
        [
          JSON.stringify({
            type: "session",
            id: "pi-id",
            cwd: "/projects/target",
            timestamp: "2026-01-01T10:00:00.000Z",
          }),
          JSON.stringify({ type: "message", message: { role: "user", content: "hello" } }),
        ].join("\n"),
      ),
    ).toEqual({
      id: "pi-id",
      title: "hello",
      projectRoot: "/projects/target",
      startedAt: Date.parse("2026-01-01T10:00:00.000Z"),
    });
  });

  test("falls back to filename uuid and uses uuid as title when no user prompt exists", () => {
    const parsed = parsePiSessionJsonl(
      JSON.stringify({ type: "model_change", modelId: "gpt" }),
      "2026-01-01T00-00-00-000Z_019e6307-0749-4000-9000-222222222222.jsonl",
    );

    expect(parsed).toEqual({
      id: "019e6307-0749-4000-9000-222222222222",
      title: "019e6307-0749-4000-9000-222222222222",
    });
  });
});

describe("listPiSessionsFromDir", () => {
  test("reads JSONL fixtures recursively and returns five most recent sessions by mtime", () => {
    const root = mkdtempSync(join(tmpdir(), "aft-pi-sessions-"));
    try {
      const dir = join(root, "project");
      mkdirSync(dir, { recursive: true });
      for (let i = 0; i < 6; i += 1) {
        const id = `019e6307-0749-4000-9000-00000000000${i}`;
        const file = join(dir, `2026-01-01T00-00-0${i}-000Z_${id}.jsonl`);
        writeFileSync(
          file,
          [
            JSON.stringify({ type: "session", id }),
            JSON.stringify({
              type: "message",
              message: { role: "user", content: [{ type: "text", text: `prompt ${i}` }] },
            }),
          ].join("\n"),
        );
        const date = new Date(1_700_000_000_000 + i * 1000);
        utimesSync(file, date, date);
      }

      const sessions = listPiSessionsFromDir(root);

      expect(sessions.map((session) => session.title)).toEqual([
        "prompt 5",
        "prompt 4",
        "prompt 3",
        "prompt 2",
        "prompt 1",
      ]);
      expect(sessions).toHaveLength(5);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("continues past newer unusable candidates to find older valid sessions", () => {
    const root = mkdtempSync(join(tmpdir(), "aft-pi-sessions-invalid-"));
    try {
      for (let i = 0; i < 5; i += 1) {
        const id = `019e6307-0749-4000-9000-10000000000${i}`;
        const file = join(root, `valid-${id}.jsonl`);
        writeFileSync(
          file,
          [
            JSON.stringify({ type: "session", id }),
            JSON.stringify({ type: "message", message: { role: "user", content: `valid ${i}` } }),
          ].join("\n"),
        );
        const date = new Date(1_700_000_000_000 + i * 1000);
        utimesSync(file, date, date);
      }
      for (let i = 0; i < 20; i += 1) {
        const file = join(root, `incomplete-${i}.jsonl`);
        writeFileSync(file, JSON.stringify({ type: "model_change", modelId: "gpt" }));
        const date = new Date(1_800_000_000_000 + i * 1000);
        utimesSync(file, date, date);
      }

      expect(listPiSessionsFromDir(root).map((session) => session.title)).toEqual([
        "valid 4",
        "valid 3",
        "valid 2",
        "valid 1",
        "valid 0",
      ]);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});
