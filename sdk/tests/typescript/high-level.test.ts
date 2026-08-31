import assert from "node:assert/strict";
import test from "node:test";

import {
  TaritApiError,
  TaritClient,
  TaritDeadlineExceeded,
  TaritPtyProtocolError,
  type PtyWebSocket,
} from "../../typescript/src/index.js";

const vmId = "11111111-1111-4111-8111-111111111111";
const childId = "22222222-2222-4222-8222-222222222222";
const executionId = "33333333-3333-4333-8333-333333333333";
const ptyId = "44444444-4444-4444-8444-444444444444";
const now = "2026-08-31T00:00:00Z";

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function execution(status: "pending" | "running" | "completed" | "failed", fields: Record<string, unknown> = {}) {
  return {
    id: executionId,
    vm_id: vmId,
    command: "echo sdk-ok",
    timeout_ms: 30_000,
    status,
    created_at: now,
    updated_at: now,
    ...fields,
  };
}

class FakeWebSocket extends EventTarget {
  binaryType: BinaryType = "blob";
  readyState = 0;
  readonly sent: (string | Uint8Array)[] = [];

  open(): void {
    this.readyState = 1;
    this.dispatchEvent(new Event("open"));
  }

  send(data: string | ArrayBufferLike | ArrayBufferView | Blob): void {
    if (typeof data === "string") this.sent.push(data);
    else if (ArrayBuffer.isView(data)) this.sent.push(new Uint8Array(data.buffer, data.byteOffset, data.byteLength).slice());
    else if (data instanceof ArrayBuffer) this.sent.push(new Uint8Array(data.slice(0)));
    else throw new Error("unexpected Blob in test");
  }

  close(): void {
    if (this.readyState === 3) return;
    this.readyState = 3;
    this.dispatchEvent(Object.assign(new Event("close"), { code: 1000 }));
  }

  message(data: unknown): void {
    this.dispatchEvent(new MessageEvent("message", { data }));
  }

  asPtySocket(): PtyWebSocket {
    return this as unknown as PtyWebSocket;
  }
}

test("execute polls to terminal and sends the API key", async () => {
  let polls = 0;
  const fetch: typeof globalThis.fetch = async (request) => {
    const incoming = request instanceof Request ? request : new Request(request);
    assert.equal(incoming.headers.get("X-API-Key"), "tenant-key");
    if (new URL(incoming.url).pathname === "/v1/execute_async") {
      return json(202, execution("pending"));
    }
    polls += 1;
    return polls === 1
      ? json(200, execution("running"))
      : json(200, execution("completed", { exit_code: 0, stdout: "sdk-ok\n", stderr: "" }));
  };
  const client = new TaritClient({ baseUrl: "https://tarit.test/", apiKey: "tenant-key", fetch });
  const result = await client.execute(vmId, "echo sdk-ok", { pollIntervalMs: 0 });
  assert.equal(result.status, "completed");
  assert.equal(result.stdout, "sdk-ok\n");
  assert.equal(polls, 2);
});

test("fork overload retry reuses one child id", async () => {
  const bodies: unknown[] = [];
  const fetch: typeof globalThis.fetch = async (request, init) => {
    const incoming = request instanceof Request ? request : new Request(request, init);
    bodies.push(await incoming.json());
    if (bodies.length === 1) return json(503, { error: "target temporarily unavailable" });
    return json(201, {
      source_vm_id: vmId,
      vm: {
        id: childId,
        status: "running",
        revision: 1,
        memory_mib: 256,
        vcpus: 1,
        created_at: now,
        updated_at: now,
      },
    });
  };
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "tenant-key", fetch });
  const result = await client.fork(vmId, { childId, deadlineMs: 1_000 });
  assert.equal(result.vm.id, childId);
  assert.deepEqual(bodies, [{ id: childId }, { id: childId }]);
});

test("tenant denial raises a typed error", async () => {
  const fetch: typeof globalThis.fetch = async () => json(403, { error: "VM belongs to another tenant" });
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "tenant-key", fetch });
  await assert.rejects(
    () => client.waitExecution(executionId, { pollIntervalMs: 0 }),
    (error: unknown) =>
      error instanceof TaritApiError && error.status === 403 && error.message.includes("another tenant"),
  );
});

test("fork aborts an in-flight request at its deadline", async () => {
  const fetch: typeof globalThis.fetch = async (request, init) => {
    const incoming = request instanceof Request ? request : new Request(request, init);
    return new Promise((_resolve, reject) => {
      incoming.signal.addEventListener("abort", () => reject(incoming.signal.reason), { once: true });
    });
  };
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "tenant-key", fetch });
  await assert.rejects(
    () => client.fork(vmId, { childId, deadlineMs: 5 }),
    (error: unknown) => error instanceof TaritDeadlineExceeded,
  );
});

test("fork does not hide programming errors as retries", async () => {
  const bug = new Error("client bug");
  const fetch: typeof globalThis.fetch = async () => {
    throw bug;
  };
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "tenant-key", fetch });
  await assert.rejects(() => client.fork(vmId, { childId, deadlineMs: 100 }), (error: unknown) => error === bug);
});

test("PTY helper creates, connects, bridges frames, and deletes its lease", async () => {
  const requests: Request[] = [];
  const fetch: typeof globalThis.fetch = async (request, init) => {
    const incoming = request instanceof Request ? request : new Request(request, init);
    requests.push(incoming);
    assert.equal(incoming.headers.get("X-API-Key"), "tenant-key");
    if (incoming.method === "POST") {
      assert.deepEqual(await incoming.json(), { cols: 100, rows: 40, shell: "/bin/sh" });
      return json(201, { pty_id: ptyId, cols: 100, rows: 40, connect_token: "pty-secret" });
    }
    return new Response(null, { status: 204 });
  };
  const socket = new FakeWebSocket();
  let socketUrl = "";
  const client = new TaritClient({
    baseUrl: "https://tarit.test/base/",
    apiKey: "tenant-key",
    fetch,
    webSocketFactory: (url) => {
      socketUrl = url;
      queueMicrotask(() => socket.open());
      return socket.asPtySocket();
    },
  });
  const pty = await client.openPty(vmId, { cols: 100, rows: 40, shell: "/bin/sh" });
  assert.equal(pty.ptyId, ptyId);
  assert.equal(socketUrl, `wss://tarit.test/base/v1/vms/${vmId}/pty/${ptyId}/connect?token=pty-secret`);
  assert.equal(socketUrl.includes("tenant-key"), false);
  pty.write("echo sdk-pty\n");
  pty.resize(120, 50);
  socket.message(new Uint8Array([112, 114, 111, 109, 112, 116]));
  assert.deepEqual(await pty.read(), { type: "data", data: new TextEncoder().encode("prompt") });
  socket.message('{"type":"exit","exit_code":7}');
  assert.deepEqual(await pty.read(), { type: "exit", exitCode: 7 });
  await pty.close();
  assert.deepEqual(socket.sent, [new TextEncoder().encode("echo sdk-pty\n"), '{"type":"resize","cols":120,"rows":50}']);
  assert.deepEqual(
    requests.map((request) => [request.method, new URL(request.url).pathname]),
    [
      ["POST", `/base/v1/vms/${vmId}/pty/sessions`],
      ["DELETE", `/base/v1/vms/${vmId}/pty/sessions/${ptyId}`],
    ],
  );
});

test("PTY connect deadline deletes the server-side lease", async () => {
  const methods: string[] = [];
  const fetch: typeof globalThis.fetch = async (request, init) => {
    const incoming = request instanceof Request ? request : new Request(request, init);
    methods.push(incoming.method);
    if (incoming.method === "POST") {
      return json(201, { pty_id: ptyId, cols: 80, rows: 24, connect_token: "pty-secret" });
    }
    return new Response(null, { status: 204 });
  };
  const socket = new FakeWebSocket();
  const client = new TaritClient({
    baseUrl: "https://tarit.test",
    apiKey: "tenant-key",
    fetch,
    webSocketFactory: () => socket.asPtySocket(),
  });
  await assert.rejects(() => client.openPty(vmId, { deadlineMs: 5 }), TaritDeadlineExceeded);
  assert.deepEqual(methods, ["POST", "DELETE"]);
});

test("PTY deadline closes a socket returned late by the factory", async () => {
  const methods: string[] = [];
  const fetch: typeof globalThis.fetch = async (request, init) => {
    const incoming = request instanceof Request ? request : new Request(request, init);
    methods.push(incoming.method);
    if (incoming.method === "POST") {
      return json(201, { pty_id: ptyId, cols: 80, rows: 24, connect_token: "pty-secret" });
    }
    return new Response(null, { status: 204 });
  };
  const socket = new FakeWebSocket();
  const client = new TaritClient({
    baseUrl: "https://tarit.test",
    apiKey: "tenant-key",
    fetch,
    webSocketFactory: async () => {
      await new Promise((resolve) => setTimeout(resolve, 20));
      return socket.asPtySocket();
    },
  });
  await assert.rejects(() => client.openPty(vmId, { deadlineMs: 5 }), TaritDeadlineExceeded);
  await new Promise((resolve) => setTimeout(resolve, 25));
  assert.equal(socket.readyState, 3);
  assert.deepEqual(methods, ["POST", "DELETE"]);
});

test("PTY helper rejects malformed server controls", async () => {
  const fetch: typeof globalThis.fetch = async (request, init) => {
    const incoming = request instanceof Request ? request : new Request(request, init);
    if (incoming.method === "POST") {
      return json(201, { pty_id: ptyId, cols: 80, rows: 24, connect_token: "pty-secret" });
    }
    return new Response(null, { status: 204 });
  };
  const socket = new FakeWebSocket();
  const client = new TaritClient({
    baseUrl: "https://tarit.test",
    apiKey: "tenant-key",
    fetch,
    webSocketFactory: () => {
      queueMicrotask(() => socket.open());
      return socket.asPtySocket();
    },
  });
  const pty = await client.openPty(vmId);
  const read = pty.read();
  socket.message('{"type":"unknown"}');
  await assert.rejects(read, TaritPtyProtocolError);
  await pty.close();
});
