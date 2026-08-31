from __future__ import annotations

import datetime
from collections.abc import Mapping
from typing import Any, TypeVar
from uuid import UUID

from attrs import define as _attrs_define
from attrs import field as _attrs_field

T = TypeVar("T", bound="SshKeyResponse")


@_attrs_define
class SshKeyResponse:
    """
    Attributes:
        id (UUID):
        fingerprint (str): OpenSSH SHA256 fingerprint (`SHA256:` plus unpadded base64).
        key_type (str):
        created_at (datetime.datetime):
    """

    id: UUID
    fingerprint: str
    key_type: str
    created_at: datetime.datetime
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        id = str(self.id)

        fingerprint = self.fingerprint

        key_type = self.key_type

        created_at = self.created_at.isoformat()

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "id": id,
                "fingerprint": fingerprint,
                "key_type": key_type,
                "created_at": created_at,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        id = UUID(d.pop("id"))

        fingerprint = d.pop("fingerprint")

        key_type = d.pop("key_type")

        created_at = datetime.datetime.fromisoformat(d.pop("created_at"))

        ssh_key_response = cls(
            id=id,
            fingerprint=fingerprint,
            key_type=key_type,
            created_at=created_at,
        )

        ssh_key_response.additional_properties = d
        return ssh_key_response

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
