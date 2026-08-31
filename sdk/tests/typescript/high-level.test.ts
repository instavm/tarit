import assert from "node:assert/strict";
import test from "node:test";

import { TaritApiError, TaritClient, TaritDeadlineExceeded } from "../../typescript/src/index.js";

const vmId = "11111111-1111-4111-8111-111111111111";
const childId = "22222222-2222-4222-8222-222222222222";
const executionId = "33333333-3333-4333-8333-333333333333";
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
