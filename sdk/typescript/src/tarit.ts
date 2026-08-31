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

export class TaritClient {
  readonly raw: Client<paths>;

  constructor(options: TaritClientOptions) {
    if (!options.apiKey) throw new TypeError("apiKey must not be empty");
    this.raw = createClient<paths>({
      baseUrl: options.baseUrl.replace(/\/$/, ""),
      headers: { "X-API-Key": options.apiKey },
      ...(options.fetch === undefined ? {} : { fetch: options.fetch }),
    });
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
