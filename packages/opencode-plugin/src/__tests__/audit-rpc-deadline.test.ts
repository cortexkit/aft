import { expect, spyOn, test } from "bun:test";
import { AftRpcClient } from "../shared/rpc-client.js";

for (const ok of [true, false]) {
  test(`RPC ${ok ? "JSON" : "error text"} body retains caller cancellation after headers`, async () => {
    let reading!: () => void;
    const started = new Promise<void>((resolve) => {
      reading = resolve;
    });
    let requestSignal: AbortSignal;
    let rejectBody!: (error: Error) => void;
    const fetchMock = spyOn(globalThis, "fetch").mockImplementation(async (_url, options) => {
      const consume = () =>
        new Promise<never>((_, reject) => {
          rejectBody = reject;
          requestSignal = options!.signal as AbortSignal;
          reading();
          options!.signal!.addEventListener("abort", () => reject(new Error("body aborted")), {
            once: true,
          });
        });
      return { ok, status: 500, json: consume, text: consume } as unknown as Response;
    });
    try {
      const controller = new AbortController();
      const client = new AftRpcClient("/isolated", "/project") as unknown as {
        callOne(
          method: string,
          params: object,
          info: object,
          signal: AbortSignal,
        ): Promise<unknown>;
      };
      const result = client.callOne("status", {}, { port: 1234, token: "test" }, controller.signal);
      await started;
      controller.abort();
      const forwarded = requestSignal!.aborted;
      rejectBody(new Error("body aborted"));
      await expect(result).rejects.toThrow("body aborted");
      expect(forwarded).toBe(true);
    } finally {
      fetchMock.mockRestore();
    }
  });
}

test("RPC deadline remains armed while consuming a response body", async () => {
  const realSetTimeout = globalThis.setTimeout;
  const timer = spyOn(globalThis, "setTimeout").mockImplementation(((callback, delay, ...args) =>
    realSetTimeout(callback, delay === 5000 ? 10 : delay, ...args)) as typeof setTimeout);
  let requestSignal: AbortSignal | undefined;
  let watchdog: ReturnType<typeof setTimeout> | undefined;
  const fetchMock = spyOn(globalThis, "fetch").mockImplementation(
    async (_url, options) =>
      ({
        ok: true,
        json: () =>
          new Promise((_, reject) => {
            requestSignal = options!.signal as AbortSignal;
            requestSignal.addEventListener("abort", () => reject(new Error("body deadline")), {
              once: true,
            });
            // Settle even with a broken deadline, so the regression reports an assertion, not a hang.
            watchdog = realSetTimeout(() => reject(new Error("body deadline")), 100);
          }),
      }) as Response,
  );
  try {
    const client = new AftRpcClient("/isolated", "/project") as any;
    await expect(client.callOne("status", {}, { port: 1234, token: "test" })).rejects.toThrow(
      "body deadline",
    );
    expect(requestSignal?.aborted).toBe(true);
  } finally {
    clearTimeout(watchdog);
    timer.mockRestore();
    fetchMock.mockRestore();
  }
});
