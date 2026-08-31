from __future__ import annotations

import datetime
from collections.abc import Mapping
from typing import Any, TypeVar, cast
from uuid import UUID

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..models.share_visibility import ShareVisibility, check_share_visibility

T = TypeVar("T", bound="ShareRecord")


@_attrs_define
class ShareRecord:
    """
    Attributes:
        id (UUID):
        slug (str): Lowercase DNS label used as `<slug>.<TARIT_SHARE_DOMAIN>`.
        owner_key (str): Tenant that owns the share.
        vm_id (UUID):
        guest_port (int):
        visibility (ShareVisibility): `private` requires a valid share token at the gateway; `public` does not.
        token_version (int): Version embedded in private-share tokens; it changes when an access-relevant share field
            changes or the share is revoked.
        revoked_at (datetime.datetime | None):
        created_at (datetime.datetime):
        updated_at (datetime.datetime):
    """

    id: UUID
    slug: str
    owner_key: str
    vm_id: UUID
    guest_port: int
    visibility: ShareVisibility
    token_version: int
    revoked_at: datetime.datetime | None
    created_at: datetime.datetime
    updated_at: datetime.datetime
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        id = str(self.id)

        slug = self.slug

        owner_key = self.owner_key

        vm_id = str(self.vm_id)

        guest_port = self.guest_port

        visibility: str = self.visibility

        token_version = self.token_version

        revoked_at: None | str
        if isinstance(self.revoked_at, datetime.datetime):
            revoked_at = self.revoked_at.isoformat()
        else:
            revoked_at = self.revoked_at

        created_at = self.created_at.isoformat()

        updated_at = self.updated_at.isoformat()

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "id": id,
                "slug": slug,
                "owner_key": owner_key,
                "vm_id": vm_id,
                "guest_port": guest_port,
                "visibility": visibility,
                "token_version": token_version,
                "revoked_at": revoked_at,
                "created_at": created_at,
                "updated_at": updated_at,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        id = UUID(d.pop("id"))

        slug = d.pop("slug")

        owner_key = d.pop("owner_key")

        vm_id = UUID(d.pop("vm_id"))

        guest_port = d.pop("guest_port")

        visibility = check_share_visibility(d.pop("visibility"))

        token_version = d.pop("token_version")

        def _parse_revoked_at(data: object) -> datetime.datetime | None:
            if data is None:
                return data
            try:
                if not isinstance(data, str):
                    raise TypeError()
                revoked_at_type_0 = datetime.datetime.fromisoformat(data)

                return revoked_at_type_0
            except (TypeError, ValueError, AttributeError, KeyError):
                pass
            return cast(datetime.datetime | None, data)

        revoked_at = _parse_revoked_at(d.pop("revoked_at"))

        created_at = datetime.datetime.fromisoformat(d.pop("created_at"))

        updated_at = datetime.datetime.fromisoformat(d.pop("updated_at"))

        share_record = cls(
            id=id,
            slug=slug,
            owner_key=owner_key,
            vm_id=vm_id,
            guest_port=guest_port,
            visibility=visibility,
            token_version=token_version,
            revoked_at=revoked_at,
            created_at=created_at,
            updated_at=updated_at,
        )

        share_record.additional_properties = d
        return share_record

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
