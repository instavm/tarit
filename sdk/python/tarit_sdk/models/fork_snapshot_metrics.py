from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar

from attrs import define as _attrs_define

from ..models.fork_snapshot_termination import ForkSnapshotTermination, check_fork_snapshot_termination

T = TypeVar("T", bound="ForkSnapshotMetrics")


@_attrs_define
class ForkSnapshotMetrics:
    """
    Attributes:
        rounds (int):
        pages_copied (int):
        final_dirty_pages (int):
        elapsed_us (int):
        downtime_us (int): Complete final-stop guest blackout, including pause and resume handshakes.
        termination (ForkSnapshotTermination):
    """

    rounds: int
    pages_copied: int
    final_dirty_pages: int
    elapsed_us: int
    downtime_us: int
    termination: ForkSnapshotTermination

    def to_dict(self) -> dict[str, Any]:
        rounds = self.rounds

        pages_copied = self.pages_copied

        final_dirty_pages = self.final_dirty_pages

        elapsed_us = self.elapsed_us

        downtime_us = self.downtime_us

        termination: str = self.termination

        field_dict: dict[str, Any] = {}

        field_dict.update(
            {
                "rounds": rounds,
                "pages_copied": pages_copied,
                "final_dirty_pages": final_dirty_pages,
                "elapsed_us": elapsed_us,
                "downtime_us": downtime_us,
                "termination": termination,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        rounds = d.pop("rounds")

        pages_copied = d.pop("pages_copied")

        final_dirty_pages = d.pop("final_dirty_pages")

        elapsed_us = d.pop("elapsed_us")

        downtime_us = d.pop("downtime_us")

        termination = check_fork_snapshot_termination(d.pop("termination"))

        fork_snapshot_metrics = cls(
            rounds=rounds,
            pages_copied=pages_copied,
            final_dirty_pages=final_dirty_pages,
            elapsed_us=elapsed_us,
            downtime_us=downtime_us,
            termination=termination,
        )

        return fork_snapshot_metrics
