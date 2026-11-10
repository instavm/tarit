import assert from "node:assert/strict";
import test from "node:test";

import { TaritApiError, TaritClient, TaritDeadlineExceeded } from "../../typescript/src/index.js";

const vmId = "11111111-1111-4111-8111-111111111111";
const executionId = "33333333-3333-4333-8333-333333333333";

function deadlineWithId(expected: string | undefined): (error: unknown) => boolean {
  return (error) => {
    assert.ok(error instanceof TaritDeadlineExceeded);
    assert.equal(error.executionId, expected);
    if (expected === undefined) {
      assert.match(error.message, /execution ID unavailable/);
      assert.match(error.message, /submission may have been accepted/);
    } else {
      assert.ok(error.message.includes(expected));
    }
    return true;
  };
}

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
  await assert.rejects(() => client.waitExecution(executionId, { deadlineMs: 10 }), deadlineWithId(executionId));
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
  await assert.rejects(() => client.execute(vmId, "echo test", { deadlineMs: 10 }), deadlineWithId(undefined));
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
    deadlineWithId(executionId),
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
  await assert.rejects(() => client.execute(vmId, "echo test", { deadlineMs: 100 }), deadlineWithId(executionId));
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
  await assert.rejects(() => client.waitExecution(executionId, { deadlineMs: 10 }), deadlineWithId(executionId));
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

for (const stage of ["submission", "submission decode", "poll decode"] as const) {
  test(`execute retains its ID when ${stage} finishes late`, async (context) => {
    let elapsed = 0;
    context.mock.method(performance, "now", () => elapsed);
    const methods: string[] = [];
    const parse = JSON.parse;
    context.mock.method(JSON, "parse", (...args: Parameters<typeof JSON.parse>) => {
      const record = parse(...args);
      if (record?.id === executionId && stage === (methods.length === 1 ? "submission decode" : "poll decode")) {
        elapsed = 120;
      }
      return record;
    });
    const fetch: typeof globalThis.fetch = async (input, init) => {
      const request = input instanceof Request ? input : new Request(input, init);
      methods.push(request.method);
      const submitting = request.method === "POST";
      if (stage === "submission") elapsed = 120;
      return executionResponse(submitting ? "pending" : "completed", submitting ? 202 : 200);
    };
    const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "key", fetch });
    await assert.rejects(() => client.execute(vmId, "echo once", { deadlineMs: 100 }), deadlineWithId(executionId));
    assert.deepEqual(methods, stage.startsWith("submission") ? ["POST"] : ["POST", "GET"]);
  });
}

test("execute preserves its ID on poll cancellation and can resume without resubmitting", async () => {
  const methods: string[] = [];
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const request = input instanceof Request ? input : new Request(input, init);
    methods.push(request.method);
    if (request.method === "POST") return executionResponse("pending", 202);
    if (methods.length === 2) return delayedResponse(request, executionResponse("completed"), 100);
    return executionResponse("completed");
  };
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "key", fetch });
  let submittedId: string | undefined;
  await assert.rejects(() => client.execute(vmId, "echo once", { deadlineMs: 10 }), (error) => {
    deadlineWithId(executionId)(error);
    submittedId = (error as TaritDeadlineExceeded).executionId;
    return true;
  });
  assert.ok(submittedId);
  assert.equal((await client.waitExecution(submittedId)).status, "completed");
  assert.deepEqual(methods, ["POST", "GET", "GET"]);
});

test("submission body timeout cannot claim an execution ID from unread bytes", async () => {
  let aborted = false;
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const request = input instanceof Request ? input : new Request(input, init);
    const body = new ReadableStream({
      start(controller) {
        request.signal.addEventListener("abort", () => {
          aborted = true;
          controller.error(request.signal.reason);
        }, { once: true });
      },
    });
    return new Response(body, { status: 202, headers: { "Content-Type": "application/json" } });
  };
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "key", fetch });
  await assert.rejects(() => client.execute(vmId, "echo once", { deadlineMs: 10 }), deadlineWithId(undefined));
  assert.equal(aborted, true);
});

test("execution ID context preserves ordinary API failures", async () => {
  const fetch: typeof globalThis.fetch = async () => new Response(JSON.stringify({ error: "forbidden" }), {
    status: 403,
    headers: { "Content-Type": "application/json" },
  });
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "key", fetch });
  for (const operation of [() => client.execute(vmId, "echo once"), () => client.waitExecution(executionId)]) {
    await assert.rejects(operation, (error) => error instanceof TaritApiError && error.status === 403);
  }
});

test("execution polling retries transient HTTP responses without resubmitting", async () => {
  const statuses = [202, 429, 502, 503, 504, 200];
  const methods: string[] = [];
  const fetch: typeof globalThis.fetch = async (request, init) => {
    const incoming = request instanceof Request ? request : new Request(request, init);
    methods.push(incoming.method);
    const status = statuses.shift()!;
    return status === 202 ? executionResponse("pending", status)
      : status === 200 ? executionResponse("completed")
      : new Response(JSON.stringify({ error: "temporarily busy" }), { status });
  };
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "key", fetch });
  assert.equal((await client.execute(vmId, "once", { deadlineMs: 5000, pollIntervalMs: 0 })).status, "completed");
  assert.deepEqual(methods, ["POST", "GET", "GET", "GET", "GET", "GET"]);
});

test("persistent polling overload reaches the original deadline with execution ID", async () => {
  let calls = 0;
  const fetch: typeof globalThis.fetch = async () => {
    calls++;
    return new Response('{"error":"busy"}', { status: 503 });
  };
  const client = new TaritClient({ baseUrl: "https://tarit.test", apiKey: "key", fetch });
  await assert.rejects(() => client.waitExecution(executionId, { deadlineMs: 20 }), deadlineWithId(executionId));
  assert.equal(calls, 1);
});

test("PTY deletion aborts a real HTTP response stalled in its body", async () => {
  const { createServer } = await import("node:http");
  let received = false;
  const server = createServer((_request, response) => {
    received = true;
    response.writeHead(500, { "Content-Type": "application/json", "Content-Length": 100 });
    response.write('{"error":');
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  assert.ok(address && typeof address !== "string");
  try {
    const client = new TaritClient({ baseUrl: `http://127.0.0.1:${address.port}`, apiKey: "key" });
    await assert.rejects(() => client.deletePtySession(vmId, "pty", { deadlineMs: 200 }), (error) => {
      assert.ok(error instanceof TaritDeadlineExceeded);
      assert.equal(error.executionId, undefined);
      assert.match(error.message, /delete PTY/);
      assert.doesNotMatch(error.message, /execution ID unavailable/);
      return true;
    });
    assert.ok(received);
  } finally {
    server.closeAllConnections();
    await new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
  }
});
