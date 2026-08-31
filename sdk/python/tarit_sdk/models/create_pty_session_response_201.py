from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar
from uuid import UUID

from attrs import define as _attrs_define
from attrs import field as _attrs_field

T = TypeVar("T", bound="CreatePtySessionResponse201")


@_attrs_define
class CreatePtySessionResponse201:
    """
    Attributes:
        pty_id (UUID):
        cols (int):
        rows (int):
        connect_token (str): Per-session token passed as the `token` query parameter on the WebSocket connect route.
    """

    pty_id: UUID
    cols: int
    rows: int
    connect_token: str
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        pty_id = str(self.pty_id)

        cols = self.cols

        rows = self.rows

        connect_token = self.connect_token

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "pty_id": pty_id,
                "cols": cols,
                "rows": rows,
                "connect_token": connect_token,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        pty_id = UUID(d.pop("pty_id"))

        cols = d.pop("cols")

        rows = d.pop("rows")

        connect_token = d.pop("connect_token")

        create_pty_session_response_201 = cls(
            pty_id=pty_id,
            cols=cols,
            rows=rows,
            connect_token=connect_token,
        )

        create_pty_session_response_201.additional_properties = d
        return create_pty_session_response_201

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
