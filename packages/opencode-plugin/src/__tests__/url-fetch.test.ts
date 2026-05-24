/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, mock, test } from "bun:test";
import type { LookupAddress } from "node:dns";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Dispatcher } from "undici";

const tempRoots = new Set<string>();

function makeStorageDir(): string {
  const root = mkdtempSync(join(tmpdir(), "aft-url-fetch-"));
  tempRoots.add(root);
  return join(root, "storage");
}

afterEach(() => {
  for (const root of tempRoots) {
    rmSync(root, { recursive: true, force: true });
  }
  tempRoots.clear();
});

describe("fetchUrlToTempFile", () => {
  test("blocks redirects to private hosts by default", async () => {
    const { fetchUrlToTempFile } = await import("@cortexkit/aft-bridge");

    const fetchImpl = mock(
      async () =>
        new Response(null, { status: 302, headers: { location: "http://127.0.0.1/internal" } }),
    );

    await expect(
      fetchUrlToTempFile("http://93.184.216.34/start", makeStorageDir(), { fetchImpl }),
    ).rejects.toThrow("Blocked private URL host");
  });

  test("pins fetch dispatcher to the already validated DNS result", async () => {
    const { fetchUrlToTempFile } = await import("@cortexkit/aft-bridge");
    const dispatcher = {} as Dispatcher;
    const dispatcherIps: string[] = [];
    const lookups: string[] = [];
    const lookup = mock(async (hostname: string) => {
      lookups.push(hostname);
      return [{ address: hostname === "example.com" ? "93.184.216.34" : "8.8.8.8", family: 4 }];
    }) as unknown as typeof import("node:dns/promises").lookup;
    const fetchImpl = mock(async (_url: string, init: { dispatcher?: Dispatcher }) => {
      expect(init.dispatcher).toBe(dispatcher);
      return new Response("# ok", { headers: { "content-type": "text/markdown" } });
    });

    await fetchUrlToTempFile("http://example.com/readme", makeStorageDir(), {
      dispatcherFactory: (validatedIp) => {
        dispatcherIps.push(validatedIp);
        return dispatcher;
      },
      fetchImpl,
      lookup,
    });

    // assertPublicUrl runs once on the cache-check path and once again before
    // the actual fetch, so example.com is resolved twice. The fix in v0.26
    // ensures cache hits cannot bypass SSRF validation.
    expect(lookups).toEqual(["example.com", "example.com"]);
    expect(dispatcherIps).toEqual(["93.184.216.34"]);
    expect(fetchImpl).toHaveBeenCalledTimes(1);
  });

  test("re-validates and re-pins each redirect target", async () => {
    const { fetchUrlToTempFile } = await import("@cortexkit/aft-bridge");
    const dispatcher = {} as Dispatcher;
    const dispatcherIps: string[] = [];
    const lookups: string[] = [];
    const lookupResults: Record<string, LookupAddress[]> = {
      "docs.example.com": [{ address: "93.184.216.34", family: 4 }],
      "cdn.example.com": [{ address: "8.8.8.8", family: 4 }],
    };
    const lookup = mock(async (hostname: string) => {
      lookups.push(hostname);
      return lookupResults[hostname] ?? [];
    }) as unknown as typeof import("node:dns/promises").lookup;
    const fetchImpl = mock(async (url: string) => {
      if (url === "http://docs.example.com/start") {
        return new Response(null, {
          status: 302,
          headers: { location: "http://cdn.example.com/final" },
        });
      }
      return new Response("# final", { headers: { "content-type": "text/markdown" } });
    });

    await fetchUrlToTempFile("http://docs.example.com/start", makeStorageDir(), {
      dispatcherFactory: (validatedIp) => {
        dispatcherIps.push(validatedIp);
        return dispatcher;
      },
      fetchImpl,
      lookup,
    });

    // assertPublicUrl validates each redirect target both at cache-check time
    // and at fetch time per the v0.26 SSRF cache fix, so docs.example.com is
    // resolved twice (once for the original cache check, once for the actual
    // fetch). The redirect target cdn.example.com is resolved once.
    expect(lookups).toEqual(["docs.example.com", "docs.example.com", "cdn.example.com"]);
    expect(dispatcherIps).toEqual(["93.184.216.34", "8.8.8.8"]);
  });
});

describe("isPrivateIp", () => {
  test("blocks IPv4 private ranges", async () => {
    const { _isPrivateIpv4 } = await import("@cortexkit/aft-bridge");
    expect(_isPrivateIpv4("0.0.0.0")).toBe(true);
    expect(_isPrivateIpv4("10.0.0.1")).toBe(true);
    expect(_isPrivateIpv4("127.0.0.1")).toBe(true);
    expect(_isPrivateIpv4("169.254.169.254")).toBe(true); // AWS metadata
    expect(_isPrivateIpv4("172.16.0.1")).toBe(true);
    expect(_isPrivateIpv4("172.31.255.255")).toBe(true);
    expect(_isPrivateIpv4("192.168.1.1")).toBe(true);
    expect(_isPrivateIpv4("224.0.0.1")).toBe(true); // multicast
    expect(_isPrivateIpv4("255.255.255.255")).toBe(true);
  });

  test("allows IPv4 public addresses", async () => {
    const { _isPrivateIpv4 } = await import("@cortexkit/aft-bridge");
    expect(_isPrivateIpv4("8.8.8.8")).toBe(false);
    expect(_isPrivateIpv4("93.184.216.34")).toBe(false);
    expect(_isPrivateIpv4("172.15.0.1")).toBe(false); // just outside 172.16/12
    expect(_isPrivateIpv4("172.32.0.1")).toBe(false);
    expect(_isPrivateIpv4("169.253.0.1")).toBe(false);
  });

  test("rejects IPv4-mapped IPv6 loopback bypass via SSRF guard", async () => {
    const { fetchUrlToTempFile } = await import("@cortexkit/aft-bridge");

    // Redirect to IPv4-mapped IPv6 form of 127.0.0.1
    const fetchImpl = mock(
      async () =>
        new Response(null, {
          status: 302,
          headers: { location: "http://[::ffff:127.0.0.1]/internal" },
        }),
    );

    await expect(
      fetchUrlToTempFile("http://93.184.216.34/start", makeStorageDir(), { fetchImpl }),
    ).rejects.toThrow("Blocked private URL host");
  });

  test("rejects IPv4-compatible IPv6 loopback bypass via SSRF guard", async () => {
    const { fetchUrlToTempFile } = await import("@cortexkit/aft-bridge");

    // IPv4-compatible form: ::127.0.0.1 (last colon is at index 1)
    const fetchImpl = mock(
      async () =>
        new Response(null, {
          status: 302,
          headers: { location: "http://[::127.0.0.1]/internal" },
        }),
    );

    await expect(
      fetchUrlToTempFile("http://93.184.216.34/start", makeStorageDir(), { fetchImpl }),
    ).rejects.toThrow("Blocked private URL host");
  });

  test("rejects IPv6 unspecified address ::", async () => {
    const { fetchUrlToTempFile } = await import("@cortexkit/aft-bridge");

    const fetchImpl = mock(
      async () =>
        new Response(null, {
          status: 302,
          headers: { location: "http://[::]/internal" },
        }),
    );

    await expect(
      fetchUrlToTempFile("http://93.184.216.34/start", makeStorageDir(), { fetchImpl }),
    ).rejects.toThrow("Blocked private URL host");
  });
});
