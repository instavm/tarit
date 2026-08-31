import { TaritApiError, TaritClient } from "../../typescript/src/index.js";

function required(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required`);
  return value;
}

async function main(): Promise<void> {
  const baseUrl = required("TARIT_SDK_BASE_URL");
  const tenantKey = required("TARIT_SDK_TENANT_KEY");
  const foreignKey = required("TARIT_SDK_FOREIGN_KEY");
  const vmId = required("TARIT_SDK_VM_ID");
  const childId = required("TARIT_SDK_TYPESCRIPT_CHILD_ID");

  const client = new TaritClient({ baseUrl, apiKey: tenantKey });
  const execution = await client.execute(vmId, "printf typescript-sync-ok", { pollIntervalMs: 20 });
  if (execution.status !== "completed" || execution.exit_code !== 0 || execution.stdout !== "typescript-sync-ok") {
    throw new Error(`unexpected execution result: ${JSON.stringify(execution)}`);
  }
  const fork = await client.fork(vmId, { childId, deadlineMs: 30_000 });
  if (fork.source_vm_id !== vmId || fork.vm.id !== childId || fork.vm.status !== "running") {
    throw new Error(`unexpected fork result: ${JSON.stringify(fork)}`);
  }
  const replay = await client.fork(vmId, { childId, deadlineMs: 30_000 });
  if (replay.source_vm_id !== vmId || replay.vm.id !== childId || replay.vm.status !== "running") {
    throw new Error(`unexpected fork replay result: ${JSON.stringify(replay)}`);
  }
  if (replay.metrics !== undefined) throw new Error(`fork replay fabricated metrics: ${JSON.stringify(replay)}`);
  const childExecution = await client.execute(childId, "printf typescript-fork-ok", { pollIntervalMs: 20 });
  if (childExecution.status !== "completed" || childExecution.stdout !== "typescript-fork-ok") {
    throw new Error(`unexpected child execution result: ${JSON.stringify(childExecution)}`);
  }

  const hibernated = await client.raw.POST("/v1/vms/{id}/hibernate", { params: { path: { id: vmId } } });
  if (hibernated.response.status !== 200 || hibernated.data?.status !== "hibernated") {
    throw new Error(`hibernate failed: ${hibernated.response.status} ${JSON.stringify(hibernated.error)}`);
  }

  const pty = await client.openPty(vmId, { shell: "/bin/sh", cols: 80, rows: 24, deadlineMs: 30_000 });
  const output: Uint8Array[] = [];
  try {
    pty.resize(102, 32);
    pty.write("stty size; printf typescript-pty-wake-ok; exit 0\n");
    for (;;) {
      const message = await pty.read({ deadlineMs: 30_000 });
      if (message.type === "data") {
        output.push(message.data);
        continue;
      }
      if (message.exitCode !== 0) throw new Error(`PTY exited with ${message.exitCode}`);
      break;
    }
  } finally {
    await pty.close();
  }
  const ptyOutput = new TextDecoder().decode(Buffer.concat(output)).replaceAll("\r", "");
  if (!ptyOutput.includes("32 102") || !ptyOutput.includes("typescript-pty-wake-ok")) {
    throw new Error(`unexpected PTY output: ${JSON.stringify(ptyOutput)}`);
  }

  const foreign = new TaritClient({ baseUrl, apiKey: foreignKey });
  const denied: [string, () => Promise<unknown>][] = [
    ["read execution", () => foreign.waitExecution(execution.id, { pollIntervalMs: 0 })],
    ["execute", () => foreign.execute(vmId, "true", { pollIntervalMs: 0 })],
    ["fork", () => foreign.fork(vmId, { childId: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb" })],
    ["open PTY", () => foreign.openPty(vmId, { deadlineMs: 5_000 })],
  ];
  for (const [operation, call] of denied) {
    try {
      await call();
      throw new Error(`foreign tenant could ${operation}`);
    } catch (error) {
      if (!(error instanceof TaritApiError) || error.status !== 403) throw error;
    }
  }

  console.log(
    `TYPESCRIPT_SDK_E2E_PASS source=${vmId} child=${childId} ` +
      "fork_replay=pass tenant_denials=4 hibernate_pty_wake=pass",
  );
}

void main();
