/// <reference path="../bun-test.d.ts" />

import { beforeEach, describe, expect, spyOn, test } from "bun:test";
import {
  __resetRpcNotificationsForTest,
  drainNotifications,
  isTuiConnected,
  pushNotification,
  pushStatusChange,
  type RpcNotification,
  registerNotificationSink,
  registerStatusChangeSink,
} from "../shared/rpc-notifications.js";

beforeEach(() => {
  __resetRpcNotificationsForTest();
});

describe("rpc notifications", () => {
  test("notification overflow preserves session fairness without sorting monotonic ids", () => {
    const sort = spyOn(Array.prototype, "sort");
    try {
      pushNotification("quiet", {}, "quiet");
      for (let i = 0; i < 100; i++) pushNotification("noisy", {}, "noisy");
      expect(sort).not.toHaveBeenCalled();
      expect(drainNotifications().map((message) => message.id)).toEqual([
        1,
        ...Array.from({ length: 50 }, (_, i) => i + 52),
      ]);
    } finally {
      sort.mockRestore();
    }
  });
  test("keeps messages queued until the client acks their id", () => {
    pushNotification("one", { ok: true }, "ses_1");
    const firstPoll = drainNotifications();
    expect(firstPoll).toHaveLength(1);
    expect(firstPoll[0]?.type).toBe("one");

    const retryPoll = drainNotifications();
    expect(retryPoll.map((message) => message.id)).toEqual(firstPoll.map((message) => message.id));

    const lastReceivedId = Math.max(...firstPoll.map((message) => message.id));
    expect(drainNotifications(lastReceivedId)).toEqual([]);
  });

  test("fans out live notifications to matching session sinks only", () => {
    const a: RpcNotification[] = [];
    const b: RpcNotification[] = [];
    const global: RpcNotification[] = [];
    registerNotificationSink({ sessionId: "ses_A", send: (notification) => a.push(notification) });
    registerNotificationSink({ sessionId: "ses_B", send: (notification) => b.push(notification) });
    registerNotificationSink({ send: (notification) => global.push(notification) });

    pushNotification("for-a", { ok: true }, "ses_A");

    expect(a.map((notification) => notification.type)).toEqual(["for-a"]);
    expect(b).toEqual([]);
    expect(global.map((notification) => notification.type)).toEqual(["for-a"]);
  });

  test("dead sink send does not block other sinks", () => {
    const delivered: RpcNotification[] = [];
    registerNotificationSink({
      sessionId: "ses_A",
      send: () => {
        throw new Error("socket closed");
      },
    });
    registerNotificationSink({
      sessionId: "ses_A",
      send: (notification) => delivered.push(notification),
    });

    pushNotification("for-a", { ok: true }, "ses_A");

    expect(delivered.map((notification) => notification.type)).toEqual(["for-a"]);
  });

  test("scopes drain to the requesting session; other sessions' items survive", () => {
    pushNotification("for-a", { action: "show-status-dialog" }, "ses_A");
    pushNotification("for-b", { action: "show-status-dialog" }, "ses_B");
    pushNotification("global", { action: "show-status-dialog" });

    // Session A sees only its own item + the global one, never ses_B's.
    const aPoll = drainNotifications(0, "ses_A");
    expect(aPoll.map((message) => message.type).sort()).toEqual(["for-a", "global"]);

    // Acking session A must NOT prune session B's still-unseen notification.
    const ackId = Math.max(...aPoll.map((message) => message.id));
    drainNotifications(ackId, "ses_A");
    const bPoll = drainNotifications(0, "ses_B");
    expect(bPoll.map((message) => message.type)).toContain("for-b");
  });

  test("session-less drain still receives all items", () => {
    pushNotification("x", { ok: true }, "ses_1");
    pushNotification("y", { ok: true }, "ses_2");
    const poll = drainNotifications(0);
    expect(poll.map((message) => message.type).sort()).toEqual(["x", "y"]);
  });

  test("hello-style backlog replay returns unacked notifications", () => {
    pushNotification("first", { ok: true }, "ses_A");
    const first = drainNotifications(0, "ses_A");
    const firstId = first[0]!.id;
    pushNotification("second", { ok: true }, "ses_A");

    const replay = drainNotifications(firstId, "ses_A");

    expect(replay.map((message) => message.type)).toEqual(["second"]);
  });

  test("isTuiConnected is exact per-session socket liveness", () => {
    const unregister = registerNotificationSink({ sessionId: "ses_A", send: () => {} });

    expect(isTuiConnected("ses_A")).toBe(true);
    expect(isTuiConnected("ses_B")).toBe(false);
    expect(isTuiConnected()).toBe(true);

    unregister();
    expect(isTuiConnected("ses_A")).toBe(false);
  });

  test("status-change sinks are scoped like notification sinks", () => {
    const a: string[] = [];
    const b: string[] = [];
    registerStatusChangeSink({
      sessionId: "ses_A",
      send: (event) => a.push(event.sessionId ?? "*"),
    });
    registerStatusChangeSink({
      sessionId: "ses_B",
      send: (event) => b.push(event.sessionId ?? "*"),
    });

    pushStatusChange("ses_A");
    pushStatusChange();

    expect(a).toEqual(["ses_A", "*"]);
    expect(b).toEqual(["*"]);
  });

  test("queue-cap eviction is session-fair: a noisy session cannot evict another session's newest unseen item", () => {
    // One quiet session with a single pending dialog.
    pushNotification("quiet-dialog", { action: "show-status-dialog" }, "ses_quiet");
    // A noisy session floods well past the 100 cap.
    for (let i = 0; i < 200; i += 1) {
      pushNotification("noise", { i }, "ses_noisy");
    }
    // The quiet session's newest item must survive the eviction.
    const quietPoll = drainNotifications(0, "ses_quiet");
    expect(quietPoll.some((message) => message.type === "quiet-dialog")).toBe(true);
  });
});
