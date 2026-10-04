# `tarit-sdk`

Typed synchronous and asynchronous Python client for the Tarit orchestrator API.

```python
from uuid import UUID

from tarit_sdk.high_level import TaritClient

with TaritClient("https://tarit.example", "tenant-api-key") as tarit:
    result = tarit.execute(UUID(vm_id), "uname -a")
    child = tarit.fork(UUID(vm_id))
    with tarit.open_pty(UUID(vm_id), shell="/bin/sh") as pty:
        pty.write("uname -a; exit 0\n")
```

`AsyncTaritClient` provides asynchronous execution, fork, and PTY helpers. The
package version matches the compatible Tarit server release. See the
[SDK guide](https://github.com/instavm/tarit/tree/main/sdk) for lifecycle,
deadline, and cleanup behavior.
