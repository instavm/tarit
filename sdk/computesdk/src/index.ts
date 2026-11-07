/**
 * Tarit provider for ComputeSDK: sandboxes are KVM microVMs run by a taritd
 * host or fleet, driven through its public HTTP API.
 *
 *   create   -> POST   /v1/vms        (or POST /v1/restore for a snapshot)
 *   command  -> POST   /v1/execute
 *   destroy  -> DELETE /v1/vms/{id}
 *
 * taritd serves creates from a warm pool of clones restored from a golden
 * snapshot, and keeps one for every registered image, so a create for an
 * image the host has seen returns in milliseconds.
 */
import { defineProvider, escapeShellArg } from '@computesdk/provider';

export interface TaritConfig {
  /** taritd base URL, e.g. https://tarit.example.com. Defaults to TARIT_BASE_URL. */
  baseUrl?: string;
  /** API key sent as X-API-Key. Defaults to TARIT_API_KEY. */
  apiKey?: string;
  /** Image used when a create names none. Defaults to TARIT_IMAGE, else by runtime. */
  image?: string;
  /** Domain taritd serves shared ports under (TARIT_SHARE_DOMAIN on the host). */
  shareDomain?: string;
  /** Per-command timeout when a call sets none, in milliseconds. */
  commandTimeoutMs?: number;
}

/** A taritd VM as this provider holds it. */
export interface TaritInstance {
  id: string;
  config: TaritConfig;
  createdAt: Date;
  timeoutMs: number;
}

interface VmRecord {
  id: string;
  status: string;
  vcpus?: number;
  memory_mib?: number;
  created_at?: string;
}

/** Within the synchronous limit, so a command is one round trip by default. */
const DEFAULT_COMMAND_TIMEOUT_MS = 120_000;
/** Longest command `/v1/execute` waits for; longer ones run asynchronously. */
const SYNC_EXECUTE_LIMIT_MS = 170_000;
const EXECUTION_POLL_FIRST_MS = 5;
const EXECUTION_POLL_MAX_MS = 250;

interface ExecutionRecord {
  id: string;
  status: 'pending' | 'running' | 'completed' | 'failed';
  exit_code?: number | null;
  stdout?: string | null;
  stderr?: string | null;
  error?: string | null;
}

function baseUrl(config: TaritConfig): string {
  const url = config.baseUrl ?? process.env.TARIT_BASE_URL;
  if (!url) {
    throw new Error("Missing taritd URL. Set 'baseUrl' in config or TARIT_BASE_URL.");
  }
  return url.replace(/\/+$/, '');
}

function apiKey(config: TaritConfig): string {
  const key = config.apiKey ?? process.env.TARIT_API_KEY;
  if (!key) {
    throw new Error("Missing Tarit API key. Set 'apiKey' in config or TARIT_API_KEY.");
  }
  return key;
}

class TaritApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

async function call<T>(
  config: TaritConfig,
  method: string,
  path: string,
  body?: unknown,
  signal?: AbortSignal,
): Promise<T> {
  const response = await fetch(baseUrl(config) + path, {
    method,
    headers: { 'X-API-Key': apiKey(config), 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal,
  });
  const text = await response.text();
  if (!response.ok) {
    throw new TaritApiError(`tarit ${method} ${path}: ${response.status} ${text}`, response.status);
  }
  return (text ? JSON.parse(text) : undefined) as T;
}

function defaultImage(config: TaritConfig, runtime?: string): string {
  return (
    config.image ??
    process.env.TARIT_IMAGE ??
    (runtime === 'python' ? 'python:3.12-slim' : 'node:22')
  );
}

function instance(config: TaritConfig, record: VmRecord, timeoutMs?: number): TaritInstance {
  return {
    id: record.id,
    config,
    createdAt: record.created_at ? new Date(record.created_at) : new Date(),
    timeoutMs: timeoutMs ?? config.commandTimeoutMs ?? DEFAULT_COMMAND_TIMEOUT_MS,
  };
}

/** Wrap a command with its environment, working directory and backgrounding. */
function shellCommand(command: string, env?: Record<string, string>, cwd?: string, background?: boolean): string {
  const lines: string[] = [];
  for (const [name, value] of Object.entries(env ?? {})) {
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name)) {
      throw new Error(`Invalid environment variable name: ${JSON.stringify(name)}`);
    }
    lines.push(`export ${name}="${escapeShellArg(String(value))}"`);
  }
  if (cwd) lines.push(`cd "${escapeShellArg(cwd)}" || exit $?`);
  lines.push(command);
  const script = lines.join('\n');
  const inner = lines.length === 1 && !background ? command : `sh -c "${escapeShellArg(script)}"`;
  return background ? `nohup ${inner} >/dev/null 2>&1 &` : inner;
}

export const tarit = defineProvider<TaritInstance, TaritConfig>({
  name: 'tarit',
  methods: {
    instances: {
      create: async (config, options) => {
        if (options?.snapshotId) {
          const record = await call<VmRecord>(
            config,
            'POST',
            '/v1/restore',
            { snapshot_id: options.snapshotId },
            options.signal,
          );
          return { instance: instance(config, record, options.timeout), instanceId: record.id };
        }
        const body: Record<string, unknown> = {
          image: options?.image ?? options?.templateId ?? defaultImage(config, options?.runtime),
        };
        // Shape fields are sent only when asked for: taritd's defaults match
        // its warm pools, so a plain create is served from one.
        if (options?.vcpus !== undefined) body.vcpus = options.vcpus;
        const memory = options?.memMiB ?? options?.memoryMiB;
        if (memory !== undefined) body.memory_mib = memory;
        const record = await call<VmRecord>(config, 'POST', '/v1/vms', body, options?.signal);
        return { instance: instance(config, record, options?.timeout), instanceId: record.id };
      },

      getById: async (config, instanceId) => {
        try {
          const record = await call<VmRecord>(config, 'GET', `/v1/vms/${encodeURIComponent(instanceId)}`);
          return { instance: instance(config, record), instanceId: record.id };
        } catch (error) {
          if (error instanceof TaritApiError && error.status === 404) return null;
          throw error;
        }
      },

      list: async (config) => {
        const vms = await call<VmRecord[]>(config, 'GET', '/v1/vms');
        return vms.map((record) => ({ instance: instance(config, record), instanceId: record.id }));
      },

      destroy: async (config, instanceId) => {
        try {
          await call(config, 'DELETE', `/v1/vms/${encodeURIComponent(instanceId)}`);
        } catch (error) {
          if (!(error instanceof TaritApiError && error.status === 404)) throw error;
        }
      },

      runCommand: async (vm, command, options) => {
        const started = Date.now();
        const timeoutMs = options?.timeout ?? vm.timeoutMs;
        const request = {
          vm_id: vm.id,
          command: shellCommand(command, options?.env, options?.cwd, options?.background),
          timeout_ms: timeoutMs,
        };
        let result: ExecutionRecord;
        if (timeoutMs <= SYNC_EXECUTE_LIMIT_MS) {
          result = await call<ExecutionRecord>(vm.config, 'POST', '/v1/execute', request);
        } else {
          // Past the synchronous deadline: start it, then poll until it ends.
          result = await call<ExecutionRecord>(vm.config, 'POST', '/v1/execute_async', request);
          let pollMs = EXECUTION_POLL_FIRST_MS;
          while (result.status === 'pending' || result.status === 'running') {
            await new Promise((resolve) => setTimeout(resolve, pollMs));
            pollMs = Math.min(pollMs * 2, EXECUTION_POLL_MAX_MS);
            result = await call<ExecutionRecord>(
              vm.config,
              'GET',
              `/v1/executions/${encodeURIComponent(result.id)}`,
            );
          }
        }
        if (result.exit_code === null || result.exit_code === undefined) {
          throw new Error(`tarit command did not complete: ${result.error ?? result.status}`);
        }
        return {
          stdout: result.stdout ?? '',
          stderr: result.stderr ?? '',
          exitCode: result.exit_code,
          durationMs: Date.now() - started,
        };
      },

      getInfo: async (vm) => {
        const record = await call<VmRecord>(vm.config, 'GET', `/v1/vms/${encodeURIComponent(vm.id)}`);
        const status = record.status === 'running' ? 'running' : record.status === 'error' ? 'error' : 'stopped';
        return {
          id: vm.id,
          provider: 'tarit',
          status,
          createdAt: record.created_at ? new Date(record.created_at) : vm.createdAt,
          timeout: vm.timeoutMs,
          metadata: { taritStatus: record.status, vcpus: record.vcpus, memoryMiB: record.memory_mib },
        };
      },

      getUrl: async (vm, { port, protocol }) => {
        const domain = vm.config.shareDomain ?? process.env.TARIT_SHARE_DOMAIN;
        if (!domain) {
          throw new Error("Port URLs need the host's share domain: set 'shareDomain' or TARIT_SHARE_DOMAIN.");
        }
        const share = await call<{ slug: string }>(vm.config, 'POST', '/v1/shares', {
          vm_id: vm.id,
          guest_port: port,
          visibility: 'public',
        });
        return `${protocol ?? 'https'}://${share.slug}.${domain}`;
      },

      pause: async (vm) => {
        await call(vm.config, 'POST', `/v1/vms/${encodeURIComponent(vm.id)}/pause`);
      },

      resume: async (vm) => {
        await call(vm.config, 'POST', `/v1/vms/${encodeURIComponent(vm.id)}/resume`);
      },

      pauseCapabilities: { memory: true },
    },

    snapshot: {
      capabilities: { scope: 'rootfs', memory: true, destroysSource: false },

      create: async (config, instanceId) => {
        const { snapshot_id } = await call<{ snapshot_id: string }>(
          config,
          'POST',
          `/v1/vms/${encodeURIComponent(instanceId)}/snapshot`,
          { diff: true },
        );
        return { id: snapshot_id, provider: 'tarit', createdAt: new Date(), metadata: { instanceId } };
      },

      list: async (config) => {
        const snapshots = await call<Array<{ snapshot_id: string; source_vm_id: string; created_at: string }>>(
          config,
          'GET',
          '/v1/snapshots',
        );
        return snapshots.map((snapshot) => ({
          id: snapshot.snapshot_id,
          provider: 'tarit',
          createdAt: new Date(snapshot.created_at),
          metadata: { instanceId: snapshot.source_vm_id },
        }));
      },

      delete: async (config, snapshotId) => {
        await call(config, 'DELETE', `/v1/snapshots/${encodeURIComponent(snapshotId)}`);
      },
    },
  },
});

export default tarit;
