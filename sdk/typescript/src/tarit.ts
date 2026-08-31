import createClient, { type Client } from "openapi-fetch";

import type { components, paths } from "./generated/schema.js";

type ExecutionRecord = components["schemas"]["ExecutionRecord"];
type ForkVmResponse = components["schemas"]["ForkVmResponse"];

const RETRYABLE_STATUS = new Set([429, 502, 503, 504]);
const TERMINAL_EXECUTION_STATUS = new Set<ExecutionRecord["status"]>(["completed", "failed"]);

export interface TaritClientOptions {
  baseUrl: string;
  apiKey: string;
  fetch?: typeof globalThis.fetch;
  webSocketFactory?: PtyWebSocketFactory;
}

export interface ForkOptions {
  childId?: string;
  deadlineMs?: number;
}

export interface ExecuteOptions {
  timeoutMs?: number;
  deadlineMs?: number;
  pollIntervalMs?: number;
}

export interface PtyOptions {
  cols?: number;
  rows?: number;
  shell?: string;
  deadlineMs?: number;
}

export type PtyMessage = { type: "data"; data: Uint8Array } | { type: "exit"; exitCode: number };

type PtySocketEventMap = {
  open: Event;
  message: MessageEvent<unknown>;
  close: CloseEvent;
  error: Event;
};

export interface PtyWebSocket {
  binaryType: BinaryType;
  readonly readyState: number;
  send(data: string | ArrayBufferLike | ArrayBufferView | Blob): void;
  close(code?: number, reason?: string): void;
  addEventListener<K extends keyof PtySocketEventMap>(
    type: K,
    listener: (event: PtySocketEventMap[K]) => void,
    options?: boolean | AddEventListenerOptions,
  ): void;
  removeEventListener<K extends keyof PtySocketEventMap>(
    type: K,
    listener: (event: PtySocketEventMap[K]) => void,
  ): void;
}

export type PtyWebSocketFactory = (url: string) => PtyWebSocket | Promise<PtyWebSocket>;

export class TaritApiError extends Error {
  readonly operation: string;
  readonly status: number;
  readonly detail: unknown;

  constructor(operation: string, status: number, message: string, detail: unknown) {
    super(`${operation} failed with HTTP ${status}: ${message}`);
    this.name = "TaritApiError";
    this.operation = operation;
    this.status = status;
    this.detail = detail;
  }
}

export class TaritDeadlineExceeded extends Error {
  constructor(message: string) {
    super(message);
    this.name = "TaritDeadlineExceeded";
  }
}

export class TaritPtyClosed extends Error {
  constructor(message: string) {
    super(message);
    this.name = "TaritPtyClosed";
  }
}

export class TaritPtyProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "TaritPtyProtocolError";
  }
}

export class TaritPtyConnectionError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "TaritPtyConnectionError";
  }
}

function sleep(milliseconds: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function retryDelay(attempt: number): number {
  return Math.min(100 * 2 ** attempt, 1_000);
}

function errorMessage(detail: unknown): string {
  if (typeof detail === "object" && detail !== null && "error" in detail) {
    const value = (detail as { error?: unknown }).error;
    if (typeof value === "string") return value.slice(0, 1_024);
  }
  if (typeof detail === "string") return detail.slice(0, 1_024);
  try {
    return JSON.stringify(detail).slice(0, 1_024);
  } catch {
    return "unparseable response";
  }
}

function validatePtyDimensions(cols: number, rows: number): void {
  if (!Number.isInteger(cols) || !Number.isInteger(rows) || cols < 1 || rows < 1 || cols > 65_535 || rows > 65_535) {
    throw new RangeError("PTY cols and rows must be integers between 1 and 65535");
  }
}

function ptyWebSocketUrl(baseUrl: string, vmId: string, ptyId: string, token: string): string {
  const url = new URL(baseUrl);
  if (url.protocol === "http:") url.protocol = "ws:";
  else if (url.protocol === "https:") url.protocol = "wss:";
  else throw new TypeError("baseUrl must use http or https");
  const basePath = url.pathname.replace(/\/$/, "");
  url.pathname = `${basePath}/v1/vms/${encodeURIComponent(vmId)}/pty/${encodeURIComponent(ptyId)}/connect`;
  url.search = new URLSearchParams({ token }).toString();
  url.hash = "";
  return url.toString();
}

async function defaultWebSocketFactory(url: string): Promise<PtyWebSocket> {
  if (typeof globalThis.WebSocket === "function") return new globalThis.WebSocket(url);
  const { default: NodeWebSocket } = await import("ws");
  return new NodeWebSocket(url) as unknown as PtyWebSocket;
}

function createWebSocketWithinDeadline(
  factory: PtyWebSocketFactory,
  url: string,
  timeoutMs: number,
): Promise<PtyWebSocket> {
  return new Promise((resolve, reject) => {
    let settled = false;
    const timer = setTimeout(() => {
      settled = true;
      reject(new TaritDeadlineExceeded("PTY WebSocket creation exceeded its deadline"));
    }, Math.max(1, timeoutMs));
    Promise.resolve()
      .then(() => factory(url))
      .then(
        (socket) => {
          if (settled) {
            try {
              socket.close(1000, "connect timeout");
            } catch {
              // The server-side lease is removed by the caller's timeout path.
            }
            return;
          }
          settled = true;
          clearTimeout(timer);
          resolve(socket);
        },
        (error: unknown) => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          reject(error);
        },
      );
  });
}

function waitForSocketOpen(socket: PtyWebSocket, timeoutMs: number): Promise<void> {
  if (socket.readyState === 1) return Promise.resolve();
  if (socket.readyState >= 2) return Promise.reject(new TaritPtyConnectionError("PTY WebSocket closed during connect"));
  return new Promise((resolve, reject) => {
    const cleanup = () => {
      clearTimeout(timer);
      socket.removeEventListener("open", onOpen);
      socket.removeEventListener("error", onError);
      socket.removeEventListener("close", onClose);
    };
    const onOpen = () => {
      cleanup();
      resolve();
    };
    const onError = () => {
      cleanup();
      reject(new TaritPtyConnectionError("PTY WebSocket connection failed"));
    };
    const onClose = () => {
      cleanup();
      reject(new TaritPtyConnectionError("PTY WebSocket closed during connect"));
    };
    const timer = setTimeout(() => {
      cleanup();
      socket.close(1000, "connect timeout");
      reject(new TaritDeadlineExceeded("PTY WebSocket connection exceeded its deadline"));
    }, Math.max(1, timeoutMs));
    socket.addEventListener("open", onOpen, { once: true });
    socket.addEventListener("error", onError, { once: true });
    socket.addEventListener("close", onClose, { once: true });
    if (socket.readyState === 1) queueMicrotask(onOpen);
    else if (socket.readyState >= 2) queueMicrotask(onClose);
  });
}

type PtyReadWaiter = {
  resolve: (message: PtyMessage) => void;
  reject: (error: Error) => void;
  timer?: ReturnType<typeof setTimeout>;
};

export class PtyConnection {
  readonly vmId: string;
  readonly ptyId: string;
  private readonly client: TaritClient;
  private readonly socket: PtyWebSocket;
  private readonly queued: PtyMessage[] = [];
  private readonly waiters: PtyReadWaiter[] = [];
  private terminalError: Error | undefined;
  private sawExit = false;
  private closedByClient = false;
  private messageChain: Promise<void> = Promise.resolve();

  constructor(client: TaritClient, vmId: string, ptyId: string, socket: PtyWebSocket) {
    this.client = client;
    this.vmId = vmId;
    this.ptyId = ptyId;
    this.socket = socket;
    this.socket.binaryType = "arraybuffer";
    this.socket.addEventListener("message", (event) => {
      this.messageChain = this.messageChain
        .then(() => this.acceptMessage(event.data))
        .catch((error: unknown) => {
          this.fail(error instanceof Error ? error : new TaritPtyProtocolError("invalid PTY message"));
          this.socket.close(1002, "protocol error");
        });
    });
    this.socket.addEventListener("error", () => this.fail(new TaritPtyConnectionError("PTY WebSocket failed")));
    this.socket.addEventListener("close", (event) => {
      if (!this.sawExit && !this.closedByClient) {
        this.fail(new TaritPtyClosed(`PTY session ${this.ptyId} closed before an exit frame (code ${event.code})`));
      } else if (this.closedByClient) {
        this.fail(new TaritPtyClosed(`PTY session ${this.ptyId} was closed by the client`));
      }
    });
  }

  async ready(timeoutMs: number): Promise<void> {
    await waitForSocketOpen(this.socket, timeoutMs);
  }

  write(data: string | Uint8Array): void {
    if (this.socket.readyState !== 1) throw new TaritPtyClosed(`PTY session ${this.ptyId} is not open`);
    this.socket.send(typeof data === "string" ? new TextEncoder().encode(data) : data);
  }

  resize(cols: number, rows: number): void {
    validatePtyDimensions(cols, rows);
    if (this.socket.readyState !== 1) throw new TaritPtyClosed(`PTY session ${this.ptyId} is not open`);
    this.socket.send(JSON.stringify({ type: "resize", cols, rows }));
  }

  read(options: { deadlineMs?: number } = {}): Promise<PtyMessage> {
    const queued = this.queued.shift();
    if (queued !== undefined) return Promise.resolve(queued);
    if (this.terminalError !== undefined) return Promise.reject(this.terminalError);
    const deadlineMs = options.deadlineMs ?? 30_000;
    if (!Number.isFinite(deadlineMs) || deadlineMs <= 0) {
      return Promise.reject(new TaritDeadlineExceeded(`PTY session ${this.ptyId} read exceeded its deadline`));
    }
    return new Promise((resolve, reject) => {
      const waiter: PtyReadWaiter = { resolve, reject };
      waiter.timer = setTimeout(() => {
        const index = this.waiters.indexOf(waiter);
        if (index >= 0) this.waiters.splice(index, 1);
        reject(new TaritDeadlineExceeded(`PTY session ${this.ptyId} read exceeded its deadline`));
      }, deadlineMs);
      this.waiters.push(waiter);
    });
  }

  async close(options: { deleteSession?: boolean } = {}): Promise<void> {
    if (this.closedByClient) return;
    this.closedByClient = true;
    let closeError: unknown;
    try {
      this.socket.close(1000, "client closed");
    } catch (error) {
      closeError = error;
    }
    this.fail(new TaritPtyClosed(`PTY session ${this.ptyId} was closed by the client`));
    if (options.deleteSession ?? true) await this.client.deletePtySession(this.vmId, this.ptyId);
    if (closeError !== undefined) throw new TaritPtyConnectionError("PTY WebSocket close failed");
  }

  private async acceptMessage(data: unknown): Promise<void> {
    let message: PtyMessage;
    if (typeof data === "string") {
      let control: unknown;
      try {
        control = JSON.parse(data);
      } catch (error) {
        throw new TaritPtyProtocolError(`PTY server sent malformed control JSON: ${String(error)}`);
      }
      if (typeof control !== "object" || control === null || !("type" in control) || !("exit_code" in control)) {
        throw new TaritPtyProtocolError("PTY server sent an unknown control message");
      }
      const { type, exit_code: exitCode } = control as { type?: unknown; exit_code?: unknown };
      if (type !== "exit" || !Number.isInteger(exitCode) || (exitCode as number) < -(2 ** 31) || (exitCode as number) >= 2 ** 31) {
        throw new TaritPtyProtocolError("PTY server sent an invalid exit control message");
      }
      this.sawExit = true;
      message = { type: "exit", exitCode: exitCode as number };
    } else if (data instanceof ArrayBuffer) {
      message = { type: "data", data: new Uint8Array(data.slice(0)) };
    } else if (ArrayBuffer.isView(data)) {
      message = { type: "data", data: new Uint8Array(data.buffer, data.byteOffset, data.byteLength).slice() };
    } else if (typeof Blob !== "undefined" && data instanceof Blob) {
      message = { type: "data", data: new Uint8Array(await data.arrayBuffer()) };
    } else {
      throw new TaritPtyProtocolError("PTY server sent an unsupported binary message");
    }
    const waiter = this.waiters.shift();
    if (waiter === undefined) this.queued.push(message);
    else {
      if (waiter.timer !== undefined) clearTimeout(waiter.timer);
      waiter.resolve(message);
    }
  }

  private fail(error: Error): void {
    if (this.terminalError === undefined) this.terminalError = error;
    for (const waiter of this.waiters.splice(0)) {
      if (waiter.timer !== undefined) clearTimeout(waiter.timer);
      waiter.reject(this.terminalError);
    }
  }
}

export class TaritClient {
  readonly raw: Client<paths>;
  private readonly baseUrl: string;
  private readonly webSocketFactory: PtyWebSocketFactory;

  constructor(options: TaritClientOptions) {
    if (!options.apiKey) throw new TypeError("apiKey must not be empty");
    this.baseUrl = options.baseUrl.replace(/\/$/, "");
    this.webSocketFactory = options.webSocketFactory ?? defaultWebSocketFactory;
    this.raw = createClient<paths>({
      baseUrl: this.baseUrl,
      headers: { "X-API-Key": options.apiKey },
      ...(options.fetch === undefined ? {} : { fetch: options.fetch }),
    });
  }

  async openPty(vmId: string, options: PtyOptions = {}): Promise<PtyConnection> {
    const cols = options.cols ?? 80;
    const rows = options.rows ?? 24;
    validatePtyDimensions(cols, rows);
    const deadlineMs = options.deadlineMs ?? 30_000;
    if (!Number.isFinite(deadlineMs) || deadlineMs <= 0) {
      throw new RangeError("PTY deadlineMs must be a finite positive number");
    }
    const deadline = Date.now() + deadlineMs;
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), deadlineMs);
    let created: { pty_id: string; connect_token: string };
    try {
      const result = await this.raw.POST("/v1/vms/{id}/pty/sessions", {
        params: { path: { id: vmId } },
        body: { cols, rows, ...(options.shell === undefined ? {} : { shell: options.shell }) },
        signal: controller.signal,
      });
      if (result.response.status !== 201 || result.data === undefined) {
        throw new TaritApiError(
          "create PTY session",
          result.response.status,
          errorMessage(result.error),
          result.error,
        );
      }
      created = result.data;
    } catch (error) {
      if (controller.signal.aborted) throw new TaritDeadlineExceeded(`open PTY for VM ${vmId} exceeded its deadline`);
      throw error;
    } finally {
      clearTimeout(timer);
    }
    const remaining = deadline - Date.now();
    if (remaining <= 0) {
      await this.deletePtySession(vmId, created.pty_id);
      throw new TaritDeadlineExceeded(`open PTY for VM ${vmId} exceeded its deadline`);
    }
    let connection: PtyConnection | undefined;
    try {
      const socket = await createWebSocketWithinDeadline(
        this.webSocketFactory,
        ptyWebSocketUrl(this.baseUrl, vmId, created.pty_id, created.connect_token),
        remaining,
      );
      connection = new PtyConnection(this, vmId, created.pty_id, socket);
      const attachRemaining = deadline - Date.now();
      if (attachRemaining <= 0) {
        throw new TaritDeadlineExceeded(`open PTY for VM ${vmId} exceeded its deadline`);
      }
      await connection.ready(attachRemaining);
      return connection;
    } catch (error) {
      if (connection !== undefined) await connection.close({ deleteSession: false }).catch(() => undefined);
      await this.deletePtySession(vmId, created.pty_id).catch(() => undefined);
      if (error instanceof TaritDeadlineExceeded || error instanceof TaritPtyConnectionError) throw error;
      throw new TaritPtyConnectionError("PTY WebSocket connection failed");
    }
  }

  async deletePtySession(vmId: string, ptyId: string): Promise<void> {
    const result = await this.raw.DELETE("/v1/vms/{id}/pty/sessions/{pty_id}", {
      params: { path: { id: vmId, pty_id: ptyId } },
    });
    if (result.response.status === 204 || result.response.status === 404) return;
    throw new TaritApiError(
      "delete PTY session",
      result.response.status,
      errorMessage(result.error),
      result.error,
    );
  }

  async fork(vmId: string, options: ForkOptions = {}): Promise<ForkVmResponse> {
    const childId = options.childId ?? crypto.randomUUID();
    const deadline = Date.now() + (options.deadlineMs ?? 30_000);
    let attempt = 0;
    while (true) {
      const requestBudget = deadline - Date.now();
      if (requestBudget <= 0) throw new TaritDeadlineExceeded(`fork VM ${vmId} exceeded its deadline`);
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), Math.max(1, requestBudget));
      try {
        const result = await this.raw.POST("/v1/vms/{id}/fork", {
          params: { path: { id: vmId } },
          body: { id: childId },
          signal: controller.signal,
        });
        if (result.response.status === 201 && result.data !== undefined) return result.data;
        if (!RETRYABLE_STATUS.has(result.response.status)) {
          throw new TaritApiError(
            "fork VM",
            result.response.status,
            errorMessage(result.error),
            result.error,
          );
        }
      } catch (error) {
        if (error instanceof TaritApiError) throw error;
        if (!(error instanceof TypeError) && !(error instanceof DOMException)) throw error;
      } finally {
        clearTimeout(timer);
      }
      const remaining = deadline - Date.now();
      if (remaining <= 0) throw new TaritDeadlineExceeded(`fork VM ${vmId} exceeded its deadline`);
      await sleep(Math.min(retryDelay(attempt), remaining));
      attempt += 1;
    }
  }

  async execute(vmId: string, command: string, options: ExecuteOptions = {}): Promise<ExecutionRecord> {
    const timeoutMs = options.timeoutMs ?? 30_000;
    const result = await this.raw.POST("/v1/execute_async", {
      body: { vm_id: vmId, command, timeout_ms: timeoutMs },
    });
    if (result.response.status !== 202 || result.data === undefined) {
      throw new TaritApiError(
        "execute command",
        result.response.status,
        errorMessage(result.error),
        result.error,
      );
    }
    return this.waitExecution(result.data.id, {
      deadlineMs: options.deadlineMs ?? Math.max(timeoutMs + 5_000, 5_000),
      ...(options.pollIntervalMs === undefined ? {} : { pollIntervalMs: options.pollIntervalMs }),
    });
  }

  async waitExecution(executionId: string, options: Omit<ExecuteOptions, "timeoutMs"> = {}): Promise<ExecutionRecord> {
    const deadline = Date.now() + (options.deadlineMs ?? 35_000);
    const pollInterval = options.pollIntervalMs ?? 100;
    while (true) {
      const result = await this.raw.GET("/v1/executions/{id}", {
        params: { path: { id: executionId } },
      });
      if (result.response.status !== 200 || result.data === undefined) {
        throw new TaritApiError(
          "get execution",
          result.response.status,
          errorMessage(result.error),
          result.error,
        );
      }
      if (TERMINAL_EXECUTION_STATUS.has(result.data.status)) return result.data;
      const remaining = deadline - Date.now();
      if (remaining <= 0) throw new TaritDeadlineExceeded(`execution ${executionId} exceeded its deadline`);
      await sleep(Math.min(pollInterval, remaining));
    }
  }
}
