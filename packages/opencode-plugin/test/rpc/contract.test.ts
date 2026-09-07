import { describe, expect, test } from "bun:test";
import { Schema } from "effect";

import { AftRpc } from "../../src/rpc/contract.js";

describe("AFT V2 RPC contract", () => {
  test("keeps the chair-fixed method and event ids", () => {
    expect(AftRpc.id).toBe("aft");
    expect(Object.keys(AftRpc.methods)).toEqual(["getStatus"]);
    expect(Object.keys(AftRpc.events)).toEqual([
      "statusInvalidated",
      "showStatusDialog",
      "indexProgress",
    ]);
  });

  test("validates session-scoped notifications and index progress", () => {
    const decodeSession = Schema.decodeUnknownSync(AftRpc.events.statusInvalidated.schema);
    const decodeDialog = Schema.decodeUnknownSync(AftRpc.events.showStatusDialog.schema);
    const decodeProgress = Schema.decodeUnknownSync(AftRpc.events.indexProgress.schema);

    expect(decodeSession({ sessionID: "ses_tui" })).toEqual({ sessionID: "ses_tui" });
    expect(decodeDialog({})).toEqual({});
    expect(
      decodeProgress({
        index: "semantic",
        status: "loading",
        stage: "embedding",
        completed: 4,
        total: 10,
      }),
    ).toEqual({
      index: "semantic",
      status: "loading",
      stage: "embedding",
      completed: 4,
      total: 10,
    });
    expect(() => decodeProgress({ index: "other", status: "loading" })).toThrow();
  });
});
