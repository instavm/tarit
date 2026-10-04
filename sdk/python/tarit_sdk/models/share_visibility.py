from typing import Literal

ShareVisibility = Literal["private", "public"]

SHARE_VISIBILITY_VALUES: set[ShareVisibility] = {
    "private",
    "public",
}


def check_share_visibility(value: str) -> ShareVisibility:
    if value in SHARE_VISIBILITY_VALUES:
        return value
    raise TypeError(f"Unexpected value {value!r}. Expected one of {SHARE_VISIBILITY_VALUES!r}")
