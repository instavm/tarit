from __future__ import annotations

import asyncio
import os
from uuid import UUID

from tarit_sdk.api.default import hibernate_vm
from tarit_sdk.high_level import (
    AsyncTaritClient,
    PtyData,
    PtyExit,
    TaritApiError,
    TaritClient,
)
from tarit_sdk.types import UNSET


def required(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise RuntimeError(f"{name} is required")
    return value


def guest_identity(client: TaritClient, vm_id: UUID) -> tuple[str, str]:
    result = client.execute(
        vm_id,
        "uname -r; . /etc/os-release; printf '%s\\n' \"$ID\"",
        poll_interval=0.02,
    )
    assert result.status == "completed" and result.exit_code == 0, result
    assert isinstance(result.stdout, str), result
    lines = result.stdout.splitlines()
    assert len(lines) == 2, result
    return lines[0], lines[1]


async def async_execution(
    base_url: str, api_key: str, vm_id: UUID, child_id: UUID
) -> None:
    async with AsyncTaritClient(base_url, api_key) as client:
        record = await client.execute(
            vm_id, "printf python-async-ok", poll_interval=0.02
        )
        assert (
            record.status == "completed"
            and record.exit_code == 0
            and record.stdout == "python-async-ok"
        ), record
        replay = await client.fork(vm_id, child_id=child_id, deadline_seconds=30)
        assert replay.vm.id == child_id and replay.vm.status == "running", replay
        assert replay.metrics is UNSET, replay

        output = bytearray()
        pty = await client.open_pty(
            vm_id, shell="/bin/sh", cols=80, rows=24, deadline_seconds=30
        )
        try:
            await pty.resize(cols=103, rows=33)
            await pty.write(b"stty size; printf python-async-pty-ok; exit 0\n")
            while True:
                message = await pty.read(timeout=30)
                if isinstance(message, PtyData):
                    output.extend(message.data)
                    continue
                assert isinstance(message, PtyExit) and message.exit_code == 0, message
                break
        finally:
            await pty.close()
        normalized_output = bytes(output).replace(b"\r", b"")
        assert (
            b"33 103" in normalized_output
            and b"python-async-pty-ok" in normalized_output
        ), normalized_output


def main() -> None:
    base_url = required("TARIT_SDK_BASE_URL")
    tenant_key = required("TARIT_SDK_TENANT_KEY")
    foreign_key = required("TARIT_SDK_FOREIGN_KEY")
    vm_id = UUID(required("TARIT_SDK_VM_ID"))
    child_id = UUID(required("TARIT_SDK_PYTHON_CHILD_ID"))
    expected_kernel_prefix = required("TARIT_SDK_EXPECTED_KERNEL_PREFIX")
    expected_os_id = required("TARIT_SDK_EXPECTED_OS_ID")

    with TaritClient(base_url, tenant_key) as client:
        kernel_release, os_id = guest_identity(client, vm_id)
        assert kernel_release.startswith(expected_kernel_prefix), (
            expected_kernel_prefix,
            kernel_release,
        )
        assert os_id == expected_os_id, (expected_os_id, os_id)
        execution = client.execute(vm_id, "printf python-sync-ok", poll_interval=0.02)
        assert (
            execution.status == "completed"
            and execution.exit_code == 0
            and execution.stdout == "python-sync-ok"
        ), execution
        fork = client.fork(vm_id, child_id=child_id, deadline_seconds=30)
        assert (
            fork.source_vm_id == vm_id
            and fork.vm.id == child_id
            and fork.vm.status == "running"
        ), fork
        replay = client.fork(vm_id, child_id=child_id, deadline_seconds=30)
        assert (
            replay.source_vm_id == vm_id
            and replay.vm.id == child_id
            and replay.vm.status == "running"
        ), replay
        assert replay.metrics is UNSET, replay
        child_execution = client.execute(
            child_id, "printf python-fork-ok", poll_interval=0.02
        )
        assert (
            child_execution.status == "completed"
            and child_execution.stdout == "python-fork-ok"
        ), child_execution
        child_kernel_release, child_os_id = guest_identity(client, child_id)
        assert (child_kernel_release, child_os_id) == (kernel_release, os_id), (
            (kernel_release, os_id),
            (child_kernel_release, child_os_id),
        )

        hibernated = hibernate_vm.sync_detailed(vm_id, client=client.raw)
        assert int(hibernated.status_code) == 200 and hibernated.parsed is not None, (
            hibernated
        )
        assert hibernated.parsed.status == "hibernated", hibernated.parsed

        output = bytearray()
        with client.open_pty(
            vm_id, shell="/bin/sh", cols=80, rows=24, deadline_seconds=30
        ) as pty:
            pty.resize(cols=101, rows=31)
            pty.write(b"stty size; printf python-pty-wake-ok; exit 0\n")
            while True:
                message = pty.read(timeout=30)
                if isinstance(message, PtyData):
                    output.extend(message.data)
                    continue
                assert isinstance(message, PtyExit) and message.exit_code == 0, message
                break
        normalized_output = bytes(output).replace(b"\r", b"")
        assert (
            b"31 101" in normalized_output
            and b"python-pty-wake-ok" in normalized_output
        ), normalized_output

    asyncio.run(async_execution(base_url, tenant_key, vm_id, child_id))

    with TaritClient(base_url, foreign_key) as foreign:
        denied = (
            (
                "read execution",
                lambda: foreign.wait_execution(execution.id, poll_interval=0),
            ),
            ("execute", lambda: foreign.execute(vm_id, "true", poll_interval=0)),
            (
                "fork",
                lambda: foreign.fork(
                    vm_id, child_id=UUID("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
                ),
            ),
            ("open PTY", lambda: foreign.open_pty(vm_id, deadline_seconds=5)),
        )
        for operation, call in denied:
            try:
                call()
            except TaritApiError as error:
                assert error.status_code == 403, (operation, error)
            else:
                raise AssertionError(f"foreign tenant could {operation}")

    print(
        f"PYTHON_SDK_E2E_PASS source={vm_id} child={child_id} async=execute,fork,pty "
        f"fork_replay=pass tenant_denials=4 hibernate_pty_wake=pass kernel={kernel_release} os={os_id}"
    )


if __name__ == "__main__":
    main()
