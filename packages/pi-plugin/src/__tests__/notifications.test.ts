/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, mock, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { BinaryBridge } from "@cortexkit/aft-bridge";
import { type ConfigureWarning, deliverConfigureWarnings } from "../notifications.js";

const tempRoots = new Set<string>();

function createStorageDir(): string {
  const root = mkdtempSync(join(tmpdir(), "aft-pi-notifications-"));
  tempRoots.add(root);
  return root;
}

function createClient() {
  const messages: string[] = [];
  const client = {
    ui: {
      notify(message: string): void {
        messages.push(message);
      },
    },
  };
  return { client, messages };
}

function createStateBridge(initialValue: string | null = null) {
  let value = initialValue;
  const send = mock(async (command: string, params: Record<string, unknown>) => {
    if (command === "db_get_state") {
      return { success: true, data: { value } };
    }
    if (command === "db_set_state") {
      value = params.value as string;
      return { success: true };
    }
    return { success: false };
  });

  return {
    bridge: { send } as unknown as Pick<BinaryBridge, "send">,
    send,
    get value() {
      return value;
    },
  };
}

function createFailingBridge() {
  const send = mock(async () => {
    throw new Error("bridge unavailable");
  });
  return { bridge: { send } as unknown as Pick<BinaryBridge, "send">, send };
}

function dbSetCalls(send: ReturnType<typeof createStateBridge>["send"]) {
  return send.mock.calls
    .filter((call) => call[0] === "db_set_state")
    .map((call) => call[1] as { key: string; value: string });
}

function baseWarning(overrides: Partial<ConfigureWarning> = {}): ConfigureWarning {
  return {
    kind: "formatter_not_installed",
    language: "typescript",
    tool: "biome",
    hint: "Install biome with bun add -d @biomejs/biome.",
    ...overrides,
  };
}

afterEach(() => {
  for (const root of tempRoots) {
    rmSync(root, { recursive: true, force: true });
  }
  tempRoots.clear();
});

describe("deliverConfigureWarnings", () => {
  test("first-time warning delivers via sendIgnoredMessage", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge, send } = createStateBridge();

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [baseWarning()],
    );

    expect(messages).toHaveLength(1);
    expect(messages[0]).toContain("🔧 AFT: ⚠️");
    expect(messages[0]).toContain("Formatter is not installed");
    expect(messages[0]).toContain("Install biome");
    expect(dbSetCalls(send)).toHaveLength(1);
  });

  test("second call with same warning skips delivery", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge } = createStateBridge();
    const opts = {
      client,
      sessionId: "session-1",
      bridge,
      storageDir,
      pluginVersion: "1.0.0",
      projectRoot: "/repo",
    };

    await deliverConfigureWarnings(opts, [baseWarning()]);
    await deliverConfigureWarnings(opts, [baseWarning()]);

    expect(messages).toHaveLength(1);
  });

  test("different warnings deliver independently", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge } = createStateBridge();

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [
        baseWarning(),
        baseWarning({ kind: "checker_not_installed", tool: "tsc", hint: "Install typescript." }),
      ],
    );

    expect(messages).toHaveLength(2);
    expect(messages[0]).toContain("Formatter is not installed");
    expect(messages[1]).toContain("Checker is not installed");
  });

  test("plugin version bump does not re-fire stale warnings", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge, send } = createStateBridge();

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [baseWarning()],
    );
    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "2.0.0",
        projectRoot: "/repo",
      },
      [baseWarning()],
    );

    expect(messages).toHaveLength(1);
    expect(dbSetCalls(send)).toHaveLength(1);
  });

  test("corrupt bridge state and missing storage_dir are non-fatal", async () => {
    const storageDir = createStorageDir();
    const missingStorageDir = join(storageDir, "missing", "nested");
    const { client, messages } = createClient();
    const { bridge } = createStateBridge("not json");

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [baseWarning()],
    );
    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir: missingStorageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [baseWarning({ tool: "prettier", hint: "Install prettier." })],
    );

    expect(messages).toHaveLength(2);
  });

  test("lsp_binary_missing warnings dedup across project roots", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge } = createStateBridge();
    const warning = baseWarning({
      kind: "lsp_binary_missing",
      language: undefined,
      tool: undefined,
      server: "typescript-language-server",
      binary: "typescript-language-server",
      hint: "Install `typescript-language-server`.",
    });

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo-a",
      },
      [warning],
    );
    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo-b",
      },
      [warning],
    );

    expect(messages).toHaveLength(1);
  });

  test("formatter warnings remain project-scoped", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge } = createStateBridge();

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo-a",
      },
      [baseWarning()],
    );
    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo-b",
      },
      [baseWarning()],
    );

    expect(messages).toHaveLength(2);
  });

  test("recordWarning_sends_db_set_state_with_merged_map", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge, send } = createStateBridge();
    const opts = {
      client,
      sessionId: "session-1",
      bridge,
      storageDir,
      pluginVersion: "1.0.0",
      projectRoot: "/repo",
    };

    await deliverConfigureWarnings(opts, [baseWarning()]);
    await deliverConfigureWarnings(opts, [
      baseWarning({ kind: "checker_not_installed", tool: "tsc", hint: "Install typescript." }),
    ]);

    expect(messages).toHaveLength(2);
    const sets = dbSetCalls(send);
    expect(sets).toHaveLength(2);
    const first = JSON.parse(sets[0].value) as Record<string, boolean>;
    expect(sets[0].key).toBe("warned_tools");
    expect(Object.values(first)).toEqual([true]);
    const merged = JSON.parse(sets[1].value) as Record<string, boolean>;
    expect(Object.keys(merged)).toHaveLength(2);
    expect(Object.values(merged)).toEqual([true, true]);
  });

  test("hasWarnedFor_returns_false_when_bridge_returns_null", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge } = createStateBridge(null);

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [baseWarning()],
    );

    expect(messages).toHaveLength(1);
  });

  test("hasWarnedFor_returns_true_when_bridge_value_contains_key", async () => {
    const storageDir = createStorageDir();
    const firstClient = createClient();
    const first = createStateBridge();

    await deliverConfigureWarnings(
      {
        client: firstClient.client,
        sessionId: "session-1",
        bridge: first.bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [baseWarning()],
    );

    const { client, messages } = createClient();
    const { bridge } = createStateBridge(first.value);
    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [baseWarning()],
    );

    expect(messages).toHaveLength(0);
  });

  test("recordWarning_continues_on_bridge_error", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge } = createFailingBridge();

    await expect(
      deliverConfigureWarnings(
        {
          client,
          sessionId: "session-1",
          bridge,
          storageDir,
          pluginVersion: "1.0.0",
          projectRoot: "/repo",
        },
        [baseWarning()],
      ),
    ).resolves.toBeUndefined();
    expect(messages).toHaveLength(1);
  });
});
