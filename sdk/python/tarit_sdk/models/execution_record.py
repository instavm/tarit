from __future__ import annotations

import datetime
from collections.abc import Mapping
from typing import Any, TypeVar, cast
from uuid import UUID

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..models.execution_record_status import ExecutionRecordStatus, check_execution_record_status
from ..types import UNSET, Unset

T = TypeVar("T", bound="ExecutionRecord")


@_attrs_define
class ExecutionRecord:
    """
    Attributes:
        id (UUID):
        vm_id (UUID):
        command (str):
        timeout_ms (int):
        status (ExecutionRecordStatus):
        created_at (datetime.datetime):
        updated_at (datetime.datetime):
        exit_code (int | None | Unset):
        stdout (None | str | Unset):
        stderr (None | str | Unset):
        duration_ms (int | None | Unset):
        error (None | str | Unset):
    """

    id: UUID
    vm_id: UUID
    command: str
    timeout_ms: int
    status: ExecutionRecordStatus
    created_at: datetime.datetime
    updated_at: datetime.datetime
    exit_code: int | None | Unset = UNSET
    stdout: None | str | Unset = UNSET
    stderr: None | str | Unset = UNSET
    duration_ms: int | None | Unset = UNSET
    error: None | str | Unset = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        id = str(self.id)

        vm_id = str(self.vm_id)

        command = self.command

        timeout_ms = self.timeout_ms

        status: str = self.status

        created_at = self.created_at.isoformat()

        updated_at = self.updated_at.isoformat()

        exit_code: int | None | Unset
        if isinstance(self.exit_code, Unset):
            exit_code = UNSET
        else:
            exit_code = self.exit_code

        stdout: None | str | Unset
        if isinstance(self.stdout, Unset):
            stdout = UNSET
        else:
            stdout = self.stdout

        stderr: None | str | Unset
        if isinstance(self.stderr, Unset):
            stderr = UNSET
        else:
            stderr = self.stderr

        duration_ms: int | None | Unset
        if isinstance(self.duration_ms, Unset):
            duration_ms = UNSET
        else:
            duration_ms = self.duration_ms

        error: None | str | Unset
        if isinstance(self.error, Unset):
            error = UNSET
        else:
            error = self.error

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "id": id,
                "vm_id": vm_id,
                "command": command,
                "timeout_ms": timeout_ms,
                "status": status,
                "created_at": created_at,
                "updated_at": updated_at,
            }
        )
        if exit_code is not UNSET:
            field_dict["exit_code"] = exit_code
        if stdout is not UNSET:
            field_dict["stdout"] = stdout
        if stderr is not UNSET:
            field_dict["stderr"] = stderr
        if duration_ms is not UNSET:
            field_dict["duration_ms"] = duration_ms
        if error is not UNSET:
            field_dict["error"] = error

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        id = UUID(d.pop("id"))

        vm_id = UUID(d.pop("vm_id"))

        command = d.pop("command")

        timeout_ms = d.pop("timeout_ms")

        status = check_execution_record_status(d.pop("status"))

        created_at = datetime.datetime.fromisoformat(d.pop("created_at"))

        updated_at = datetime.datetime.fromisoformat(d.pop("updated_at"))

        def _parse_exit_code(data: object) -> int | None | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(int | None | Unset, data)

        exit_code = _parse_exit_code(d.pop("exit_code", UNSET))

        def _parse_stdout(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        stdout = _parse_stdout(d.pop("stdout", UNSET))

        def _parse_stderr(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        stderr = _parse_stderr(d.pop("stderr", UNSET))

        def _parse_duration_ms(data: object) -> int | None | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(int | None | Unset, data)

        duration_ms = _parse_duration_ms(d.pop("duration_ms", UNSET))

        def _parse_error(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        error = _parse_error(d.pop("error", UNSET))

        execution_record = cls(
            id=id,
            vm_id=vm_id,
            command=command,
            timeout_ms=timeout_ms,
            status=status,
            created_at=created_at,
            updated_at=updated_at,
            exit_code=exit_code,
            stdout=stdout,
            stderr=stderr,
            duration_ms=duration_ms,
            error=error,
        )

        execution_record.additional_properties = d
        return execution_record

    @property
    def additional_keys(self) -> list[str]:
        return list(self.additional_properties.keys())

    def __getitem__(self, key: str) -> Any:
        return self.additional_properties[key]

    def __setitem__(self, key: str, value: Any) -> None:
        self.additional_properties[key] = value

    def __delitem__(self, key: str) -> None:
        del self.additional_properties[key]

    def __contains__(self, key: str) -> bool:
        return key in self.additional_properties
