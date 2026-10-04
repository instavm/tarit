import assert from "node:assert/strict";
import test from "node:test";

import { TaritClient, TaritDeadlineExceeded } from "../../typescript/src/index.js";

const vmId = "11111111-1111-4111-8111-111111111111";
const executionId = "33333333-3333-4333-8333-333333333333";

function executionResponse(status: "pending" | "completed", httpStatus = 200): Response {
  return new Response(JSON.stringify({ id: executionId, vm_id: vmId, status }), {
    status: httpStatus,
    headers: { "Content-Type": "application/json" },
  });
}

function delayedResponse(request: Request, response: Response, delayMs: number): Promise<Response> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => resolve(response), delayMs);
    const onAbort = () => {
      clearTimeout(timer);
      reject(request.signal.reason);
    };
    if (request.signal.aborted) onAbort();
    else request.signal.addEventListener("abort", onAbort, { once: true });
  });
}

test("waitExecution aborts a stalled HTTP poll at its deadline", async () => {
  let request: Request | undefined;
  const fetch: typeof globalThis.fetch = async (input, init) => {
    request = input instanceof Request ? input : new Request(input, init);
    return delayedResponse(request, executionResponse("completed"), 100);
  };
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "key", fetch });
  await assert.rejects(() => client.waitExecution(executionId, { deadlineMs: 10 }), TaritDeadlineExceeded);
  assert.equal(request?.signal.aborted, true);
});

test("execute includes submission in its deadline and never resubmits", async () => {
  let requests = 0;
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const request = input instanceof Request ? input : new Request(input, init);
    requests += 1;
    return delayedResponse(request, executionResponse("pending", 202), 100);
  };
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "key", fetch });
  await assert.rejects(() => client.execute(vmId, "echo test", { deadlineMs: 10 }), TaritDeadlineExceeded);
  assert.equal(requests, 1);
});

test("waitExecution does not poll again after sleeping to the deadline", async () => {
  let requests = 0;
  const fetch: typeof globalThis.fetch = async () => {
    requests += 1;
    return executionResponse(requests === 1 ? "pending" : "completed");
  };
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "key", fetch });
  await assert.rejects(
    () => client.waitExecution(executionId, { deadlineMs: 10, pollIntervalMs: 100 }),
    TaritDeadlineExceeded,
  );
  assert.equal(requests, 1);
});

for (const boundary of [
  { name: "deadline", deadlineMs: 10, pollIntervalMs: 100, expires: true },
  { name: "poll interval", deadlineMs: 100, pollIntervalMs: 10, expires: false },
]) {
  test(`waitExecution rechecks its ${boundary.name} after an early wake`, async (context) => {
    let elapsed = 0;
    context.mock.method(performance, "now", () => elapsed);
    context.mock.timers.enable({ apis: ["setTimeout"] });
    const polls: number[] = [];
    const fetch: typeof globalThis.fetch = async () => {
      polls.push(elapsed);
      return executionResponse(polls.length === 1 ? "pending" : "completed");
    };
    const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "key", fetch });
    const outcome = client.waitExecution(executionId, {
      deadlineMs: boundary.deadlineMs,
      pollIntervalMs: boundary.pollIntervalMs,
    }).then(
      (record) => ({ record, error: undefined }),
      (error: unknown) => ({ record: undefined, error }),
    );
    await new Promise<void>((resolve) => setImmediate(resolve));
    assert.deepEqual(polls, [0]);

    // Timer callbacks can run before the requested monotonic time is reached.
    elapsed = 9.5;
    context.mock.timers.tick(10);
    await new Promise<void>((resolve) => setImmediate(resolve));
    assert.deepEqual(polls, [0]);

    elapsed = 10;
    context.mock.timers.tick(1);
    const result = await outcome;
    if (boundary.expires) {
      assert.ok(result.error instanceof TaritDeadlineExceeded);
      assert.deepEqual(polls, [0]);
    } else {
      assert.equal(result.record?.status, "completed");
      assert.deepEqual(polls, [0, 10]);
    }
  });
}

test("execute uses one budget across submission and polling", async (context) => {
  let elapsed = 0;
  context.mock.method(performance, "now", () => elapsed);
  const requests: Request[] = [];
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const request = input instanceof Request ? input : new Request(input, init);
    requests.push(request);
    if (request.method === "POST") {
      elapsed = 60;
      return executionResponse("pending", 202);
    }
    elapsed = 120;
    return executionResponse("completed");
  };
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "key", fetch });
  await assert.rejects(() => client.execute(vmId, "echo test", { deadlineMs: 100 }), TaritDeadlineExceeded);
  assert.equal(requests.length, 2);
});

test("execution deadline covers the response body as well as headers", async () => {
  let aborted = false;
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const request = input instanceof Request ? input : new Request(input, init);
    const body = new ReadableStream({
      start(controller) {
        const timer = setTimeout(() => {
          controller.enqueue(new TextEncoder().encode(JSON.stringify({ id: executionId, status: "completed" })));
          controller.close();
        }, 100);
        const onAbort = () => {
          clearTimeout(timer);
          aborted = true;
          controller.error(request.signal.reason);
        };
        if (request.signal.aborted) onAbort();
        else request.signal.addEventListener("abort", onAbort, { once: true });
      },
    });
    return new Response(body, { status: 200, headers: { "Content-Type": "application/json" } });
  };
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "key", fetch });
  await assert.rejects(() => client.waitExecution(executionId, { deadlineMs: 10 }), TaritDeadlineExceeded);
  assert.equal(aborted, true);
});

test("execution options are validated before submitting a command", async () => {
  let requests = 0;
  const fetch: typeof globalThis.fetch = async () => {
    requests += 1;
    return executionResponse("completed");
  };
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "key", fetch });
  for (const deadlineMs of [0, -1, NaN, Infinity]) {
    await assert.rejects(() => client.execute(vmId, "echo test", { deadlineMs }), RangeError);
    await assert.rejects(() => client.waitExecution(executionId, { deadlineMs }), RangeError);
  }
  for (const pollIntervalMs of [-1, NaN, Infinity]) {
    await assert.rejects(() => client.execute(vmId, "echo test", { pollIntervalMs }), RangeError);
    await assert.rejects(() => client.waitExecution(executionId, { pollIntervalMs }), RangeError);
  }
  assert.equal(requests, 0);
});

test("successful execution clears its deadline timer", async () => {
  let signal: AbortSignal | undefined;
  const fetch: typeof globalThis.fetch = async (input, init) => {
    signal = (input instanceof Request ? input : new Request(input, init)).signal;
    return executionResponse("completed");
  };
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "key", fetch });
  assert.equal((await client.waitExecution(executionId, { deadlineMs: 50 })).status, "completed");
  await new Promise((resolve) => setTimeout(resolve, 60));
  assert.equal(signal?.aborted, false);
});

test("execution deadlines do not replace unrelated transport errors", async () => {
  const failure = new TypeError("connection failed");
  let requests = 0;
  const fetch: typeof globalThis.fetch = async () => {
    requests += 1;
    throw failure;
  };
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "key", fetch });
  await assert.rejects(() => client.execute(vmId, "echo test"), (error) => error === failure);
  assert.equal(requests, 1);
});
