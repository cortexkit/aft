import { expect, test } from "bun:test";
import { aftLiveConfigKeys, applyLiveConfigKeys } from "../config-watch.js";

test("gather config is live, including removal, without changing unrelated search settings", () => {
  const keys = aftLiveConfigKeys<Record<string, unknown>>(() => ({
    enabled: true,
    compress: true,
    background: true,
    host_fallback: false,
    subagent_background: true,
    foreground_wait_window_ms: 15000,
    watch_sync_max_ms: 120000,
    worker_wait_max_ms: 1800000,
    env: {},
  }));
  const current = { search: { gather: { enabled: false }, rerank: { backend: "off" } } };
  const next = { search: { gather: { enabled: true, max_nodes: 4 }, rerank: { backend: "off" } } };
  const result = applyLiveConfigKeys(current, next, keys);
  expect(result.applied).toEqual(["search.gather"]);
  expect(result.deferred).toEqual([]);
  expect(result.config).toEqual(next);
  expect(current.search.gather.enabled).toBe(false);
  const removed = applyLiveConfigKeys(
    result.config,
    { search: { rerank: { backend: "off" } } },
    keys,
  );
  expect(removed.applied).toEqual(["search.gather"]);
  expect(removed.deferred).toEqual([]);
  expect(removed.config).toEqual({ search: { rerank: { backend: "off" } } });
});
