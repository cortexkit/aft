import { describe, expect, test } from "bun:test";
import {
  __resetBgNotificationStateForTests,
  cleanupIdleSessionStates,
  SESSION_BG_STATE_IDLE_TTL_MS,
  __bgLogSuppressionForTests as suppression,
} from "../bg-notifications.js";

describe("background log retention", () => {
  test("idle log keys expire even without a session state", () => {
    __resetBgNotificationStateForTests();
    const context = {
      ctx: {} as Parameters<typeof suppression.foreign>[0]["ctx"],
      directory: "/fixture",
      sessionID: "live",
    };
    try {
      suppression.foreign(context, "foreign");
      suppression.hop(context, "ack", "fixture", { task_id: "task" });
      expect(suppression.sizes()).toEqual({ foreign: 1, hop: 1 });
      cleanupIdleSessionStates(Date.now() + SESSION_BG_STATE_IDLE_TTL_MS + 1);
      expect(suppression.sizes()).toEqual({ foreign: 0, hop: 0 });
    } finally {
      __resetBgNotificationStateForTests();
    }
  });
});
