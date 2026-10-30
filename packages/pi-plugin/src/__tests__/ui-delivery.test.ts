import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { sendFeatureAnnouncement, sendLspInstallSummary } from "../notifications.js";

function upgradeFixture(fn: (root: string, marker: string) => void): void {
  const root = mkdtempSync(join(tmpdir(), "aft-pi-ui-upgrade-"));
  const marker = join(root, "pi", "last_announced_version");
  mkdirSync(join(root, "pi"));
  writeFileSync(marker, "0.1.0");
  try {
    fn(root, marker);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

describe("approved Pi UI delivery", () => {
  test("upgrade announcement reaches UI once before it is marked seen", () => {
    upgradeFixture((root, marker) => {
      const messages: string[] = [];
      const client = {
        hasUI: true,
        ui: {
          notify(message: string) {
            expect(readFileSync(marker, "utf8")).toBe("0.1.0");
            messages.push(message);
          },
        },
      };
      sendFeatureAnnouncement("1.0.0", ["Feature"], "Footer", root, client);
      sendFeatureAnnouncement("1.0.0", ["Feature"], "Footer", root, client);
      expect(messages).toEqual(["🔧 AFT: ✨ v1.0.0:\n  • Feature\n\nFooter"]);
      expect(readFileSync(marker, "utf8")).toBe("1.0.0");
    });
  });
  test("headless no-op UI does not consume an upgrade announcement", () => {
    upgradeFixture((root, marker) => {
      let calls = 0;
      sendFeatureAnnouncement("1.0.0", ["Feature"], "", root, {
        hasUI: false,
        ui: {
          notify: () => {
            calls++;
          },
        },
      });
      expect(calls).toBe(0);
      expect(readFileSync(marker, "utf8")).toBe("0.1.0");
    });
  });
  test("failed notification leaves the announcement eligible for retry", () => {
    upgradeFixture((root, marker) => {
      sendFeatureAnnouncement("1.0.0", ["Feature"], "", root, {
        hasUI: true,
        get ui() {
          throw new Error("UI not ready");
        },
      });
      expect(readFileSync(marker, "utf8")).toBe("0.1.0");
      const messages: string[] = [];
      expect(
        sendFeatureAnnouncement("1.0.0", ["Feature"], "", root, {
          hasUI: true,
          ui: { notify: (message: string) => messages.push(message) },
        }),
      ).toBe(true);
      expect(messages).toHaveLength(1);
      expect(readFileSync(marker, "utf8")).toBe("1.0.0");
    });
  });
  test("actionable install summary reaches UI without routine skips", () => {
    const messages: Array<{ message: string; type: string | undefined }> = [];
    const client = {
      hasUI: true,
      ui: { notify: (message: string, type?: string) => messages.push({ message, type }) },
    };
    expect(
      sendLspInstallSummary(
        [
          { id: "routine", reason: "already installed" },
          { id: "clangd", reason: "install failed" },
        ],
        client,
      ),
    ).toBe(true);
    expect(messages).toHaveLength(1);
    expect(messages[0]?.message).toContain("clangd: install failed");
    expect(messages[0]?.message).not.toContain("routine");
    expect(messages[0]?.type).toBe("warning");
  });
});
