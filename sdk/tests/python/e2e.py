from __future__ import annotations

import asyncio
import os
from uuid import UUID

from tarit_sdk.high_level import AsyncTaritClient, TaritApiError, TaritClient


def required(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise RuntimeError(f"{name} is required")
    return value


async def async_execution(base_url: str, api_key: str, vm_id: UUID) -> None:
    async with AsyncTaritClient(base_url, api_key) as client:
        record = await client.execute(vm_id, "printf python-async-ok", poll_interval=0.02)
        assert record.status == "completed" and record.exit_code == 0 and record.stdout == "python-async-ok", record


def main() -> None:
    base_url = required("TARIT_SDK_BASE_URL")
    tenant_key = required("TARIT_SDK_TENANT_KEY")
    foreign_key = required("TARIT_SDK_FOREIGN_KEY")
    vm_id = UUID(required("TARIT_SDK_VM_ID"))
    child_id = UUID(required("TARIT_SDK_PYTHON_CHILD_ID"))

    with TaritClient(base_url, tenant_key) as client:
        execution = client.execute(vm_id, "printf python-sync-ok", poll_interval=0.02)
        assert execution.status == "completed" and execution.exit_code == 0 and execution.stdout == "python-sync-ok", (
            execution
        )
        fork = client.fork(vm_id, child_id=child_id, deadline_seconds=30)
        assert fork.source_vm_id == vm_id and fork.vm.id == child_id and fork.vm.status == "running", fork
        child_execution = client.execute(child_id, "printf python-fork-ok", poll_interval=0.02)
        assert child_execution.status == "completed" and child_execution.stdout == "python-fork-ok", child_execution

    asyncio.run(async_execution(base_url, tenant_key, vm_id))

    with TaritClient(base_url, foreign_key) as foreign:
        try:
            foreign.wait_execution(execution.id, poll_interval=0)
        except TaritApiError as error:
            assert error.status_code == 403, error
        else:
            raise AssertionError("foreign tenant read another tenant's execution")

    print(f"PYTHON_SDK_E2E_PASS source={vm_id} child={child_id}")


if __name__ == "__main__":
    main()
