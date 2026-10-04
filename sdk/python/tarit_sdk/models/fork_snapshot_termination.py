from typing import Literal

ForkSnapshotTermination = Literal["converged", "diverging", "max_rounds", "timeout"]

FORK_SNAPSHOT_TERMINATION_VALUES: set[ForkSnapshotTermination] = {
    "converged",
    "diverging",
    "max_rounds",
    "timeout",
}


def check_fork_snapshot_termination(value: str) -> ForkSnapshotTermination:
    if value in FORK_SNAPSHOT_TERMINATION_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {FORK_SNAPSHOT_TERMINATION_VALUES!r}")
