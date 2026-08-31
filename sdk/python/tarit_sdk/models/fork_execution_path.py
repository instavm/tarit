from typing import Literal

ForkExecutionPath = Literal["cross_node", "local"]

FORK_EXECUTION_PATH_VALUES: set[ForkExecutionPath] = {
    "cross_node",
    "local",
}


def check_fork_execution_path(value: str) -> ForkExecutionPath:
    if value in FORK_EXECUTION_PATH_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {FORK_EXECUTION_PATH_VALUES!r}")
