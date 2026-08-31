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
  const childExecution = await client.execute(childId, "printf typescript-fork-ok", { pollIntervalMs: 20 });
  if (childExecution.status !== "completed" || childExecution.stdout !== "typescript-fork-ok") {
    throw new Error(`unexpected child execution result: ${JSON.stringify(childExecution)}`);
  }

  const foreign = new TaritClient({ baseUrl, apiKey: foreignKey });
  try {
    await foreign.waitExecution(execution.id, { pollIntervalMs: 0 });
    throw new Error("foreign tenant read another tenant's execution");
  } catch (error) {
    if (!(error instanceof TaritApiError) || error.status !== 403) throw error;
  }

  console.log(`TYPESCRIPT_SDK_E2E_PASS source=${vmId} child=${childId}`);
}

void main();
