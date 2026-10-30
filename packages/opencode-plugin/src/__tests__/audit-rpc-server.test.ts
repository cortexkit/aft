import { expect, test } from "bun:test";
import { EventEmitter } from "node:events";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  rmSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AftRpcServer } from "../shared/rpc-server.js";
import { rpcPortFileDir } from "../shared/rpc-utils.js";

test("Bun RPC caps streamed bytes before buffering the remaining body", async () => {
  const server = new AftRpcServer("/isolated", "/project");
  server.handle("echo", async () => ({}));
  let cancelled = false;
  let pulls = 0;
  const stream = new ReadableStream<Uint8Array>({
    pull(controller) {
      pulls++;
      if (pulls < 6) controller.enqueue(new TextEncoder().encode("é".repeat(300_000)));
      else controller.close();
    },
    cancel() {
      cancelled = true;
    },
  });
  const request = new Request("http://localhost/rpc/echo", {
    method: "POST",
    body: stream,
    duplex: "half",
  } as RequestInit);
  const response = await (server as any).handleFetch(request, {});
  expect(response.status).toBe(413);
  expect(cancelled).toBe(true);
  expect(pulls).toBeLessThan(5);
});

test("Bun RPC refuses oversized declared length without reading the stream", async () => {
  const server = new AftRpcServer("/isolated", "/project");
  server.handle("echo", async () => ({}));
  let reads = 0;
  const response = await (server as any).handleFetch(
    {
      url: "http://localhost/rpc/echo",
      method: "POST",
      headers: new Headers({ "content-length": "1048577" }),
      text: async () => {
        reads++;
        return "{}";
      },
      body: {
        getReader: () => {
          reads++;
          throw new Error("body was read");
        },
      },
    },
    {},
  );
  expect(response.status).toBe(413);
  expect(reads).toBe(0);
});

function nodeDispatch(chunks: Buffer[]) {
  const server = new AftRpcServer("/isolated", "/project");
  let echo: unknown;
  server.handle("echo", async (params) => {
    echo = params.value;
    return {};
  });
  const req = Object.assign(new EventEmitter(), {
    method: "POST",
    url: "/rpc/echo",
    headers: {},
    destroy: () => {},
  });
  let status = 0;
  const res = {
    writeHead: (code: number) => {
      status = code;
    },
    end: () => {},
  };
  (server as any).dispatch(req, res);
  for (const chunk of chunks) req.emit("data", chunk);
  req.emit("end");
  return { status: () => status, echo: () => echo };
}

test("Node RPC decodes split UTF-8 only after collecting bytes", async () => {
  const tokenServer = new AftRpcServer("/isolated", "/project");
  // Use the real dispatch instance's token in the JSON envelope.
  const req = Object.assign(new EventEmitter(), {
    method: "POST",
    url: "/rpc/echo",
    headers: {},
    destroy: () => {},
  });
  let echoed: unknown;
  tokenServer.handle("echo", async (params) => {
    echoed = params.value;
    return {};
  });
  (tokenServer as any).dispatch(req, { writeHead: () => {}, end: () => {} });
  const body = Buffer.from(JSON.stringify({ token: (tokenServer as any).token, value: "é" }));
  const split = body.indexOf(Buffer.from("é")) + 1;
  req.emit("data", body.subarray(0, split));
  req.emit("data", body.subarray(split));
  req.emit("end");
  await Promise.resolve();
  expect(echoed).toBe("é");
});

test("Node RPC limit counts bytes rather than UTF-16 characters", () => {
  const result = nodeDispatch([Buffer.from(JSON.stringify({ value: "é".repeat(600_000) }))]);
  expect(result.status()).toBe(413);
});

test("Node RPC stopped during listen publishes no discovery record", async () => {
  const root = mkdtempSync(join(tmpdir(), "aft-rpc-stop-"));
  const server = new AftRpcServer(root, root);
  try {
    const started = (server as any).startNode() as Promise<number>;
    server.stop();
    await expect(started).rejects.toThrow("stopped");
    const ports = rpcPortFileDir(root, root);
    expect(existsSync(ports) ? readdirSync(ports) : []).toEqual([]);
    expect((server as any).nodeServer).toBeNull();
  } finally {
    server.stop();
    rmSync(root, { recursive: true, force: true });
  }
});

test("RPC startup sweeps dead and old unreadable tmp records but preserves live writers", async () => {
  const root = mkdtempSync(join(tmpdir(), "aft-rpc-tmp-"));
  const ports = rpcPortFileDir(root, root);
  mkdirSync(ports, { recursive: true });
  const files = ["dead.json.tmp", "live.json.tmp", "old.json.tmp", "fresh.json.tmp"];
  writeFileSync(
    join(ports, files[0]!),
    JSON.stringify({ port: 1234, token: "t", pid: 2147483647 }),
  );
  writeFileSync(
    join(ports, files[1]!),
    JSON.stringify({ port: 1234, token: "t", pid: process.pid }),
  );
  writeFileSync(join(ports, files[2]!), "{");
  writeFileSync(join(ports, files[3]!), "{");
  const old = new Date(Date.now() - 2 * 24 * 60 * 60 * 1000);
  utimesSync(join(ports, files[2]!), old, old);
  const server = new AftRpcServer(root, root);
  try {
    await server.start();
    expect(files.map((file) => existsSync(join(ports, file)))).toEqual([false, true, false, true]);
  } finally {
    server.stop();
    rmSync(root, { recursive: true, force: true });
  }
});
