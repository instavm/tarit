from __future__ import annotations

from collections.abc import Mapping
from typing import TYPE_CHECKING, Any, TypeVar

from attrs import define as _attrs_define

from ..models.fork_execution_path import ForkExecutionPath, check_fork_execution_path
from ..types import UNSET, Unset

if TYPE_CHECKING:
    from ..models.fork_snapshot_metrics import ForkSnapshotMetrics


T = TypeVar("T", bound="ForkMetrics")


@_attrs_define
class ForkMetrics:
    """
    Attributes:
        path (ForkExecutionPath):
        source_resolution_us (int):
        operation_claim_us (int):
        snapshot_artifact_us (int): Atomic snapshot plus durable local artifact publication, or peer replication and
            localization.
        child_ready_us (int): Restore through repaired guest readiness.
        operation_commit_us (int):
        total_us (int):
        live_snapshot (ForkSnapshotMetrics | Unset):
    """

    path: ForkExecutionPath
    source_resolution_us: int
    operation_claim_us: int
    snapshot_artifact_us: int
    child_ready_us: int
    operation_commit_us: int
    total_us: int
    live_snapshot: ForkSnapshotMetrics | Unset = UNSET

    def to_dict(self) -> dict[str, Any]:
        path: str = self.path

        source_resolution_us = self.source_resolution_us

        operation_claim_us = self.operation_claim_us

        snapshot_artifact_us = self.snapshot_artifact_us

        child_ready_us = self.child_ready_us

        operation_commit_us = self.operation_commit_us

        total_us = self.total_us

        live_snapshot: dict[str, Any] | Unset = UNSET
        if not isinstance(self.live_snapshot, Unset):
            live_snapshot = self.live_snapshot.to_dict()

        field_dict: dict[str, Any] = {}

        field_dict.update(
            {
                "path": path,
                "source_resolution_us": source_resolution_us,
                "operation_claim_us": operation_claim_us,
                "snapshot_artifact_us": snapshot_artifact_us,
                "child_ready_us": child_ready_us,
                "operation_commit_us": operation_commit_us,
                "total_us": total_us,
            }
        )
        if live_snapshot is not UNSET:
            field_dict["live_snapshot"] = live_snapshot

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.fork_snapshot_metrics import ForkSnapshotMetrics  # noqa: PLC0415

        d = dict(src_dict)
        path = check_fork_execution_path(d.pop("path"))

        source_resolution_us = d.pop("source_resolution_us")

        operation_claim_us = d.pop("operation_claim_us")

        snapshot_artifact_us = d.pop("snapshot_artifact_us")

        child_ready_us = d.pop("child_ready_us")

        operation_commit_us = d.pop("operation_commit_us")

        total_us = d.pop("total_us")

        _live_snapshot = d.pop("live_snapshot", UNSET)
        live_snapshot: ForkSnapshotMetrics | Unset
        if isinstance(_live_snapshot, Unset):
            live_snapshot = UNSET
        else:
            live_snapshot = ForkSnapshotMetrics.from_dict(_live_snapshot)

        fork_metrics = cls(
            path=path,
            source_resolution_us=source_resolution_us,
            operation_claim_us=operation_claim_us,
            snapshot_artifact_us=snapshot_artifact_us,
            child_ready_us=child_ready_us,
            operation_commit_us=operation_commit_us,
            total_us=total_us,
            live_snapshot=live_snapshot,
        )

        return fork_metrics
