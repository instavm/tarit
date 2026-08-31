# Tarit SDKs

`orch/openapi.yaml` is the public contract for both checked-in clients. The
generated surface covers every documented HTTP operation; the small handwritten
layers add API-key configuration, typed failures, deadline-bounded execution
polling, stable-child-id retry for live forks, and bounded PTY/WebSocket sessions.

## Regeneration

Run `./sdk/generate.sh` from the repository root. Generator versions are pinned
in that script. CI regenerates both clients and rejects any diff.

## Python

The package in `sdk/python` supports Python 3.11 and later.

```python
from uuid import UUID

from tarit_sdk.high_level import PtyExit, TaritClient

with TaritClient("https://tarit.example", "tenant-api-key") as tarit:
    result = tarit.execute(UUID(vm_id), "uname -a")
    child = tarit.fork(UUID(vm_id))
    with tarit.open_pty(UUID(vm_id), shell="/bin/sh") as pty:
        pty.resize(cols=120, rows=40)
        pty.write("uname -a; exit 0\n")
        while not isinstance(pty.read(timeout=30), PtyExit):
            pass
```

`AsyncTaritClient` provides the same execution, fork, and PTY helpers for asyncio.
Generated models and operation modules remain available under `tarit_sdk.models`
and `tarit_sdk.api`.

## TypeScript

The package in `sdk/typescript` supports Node.js 20 and later and browser fetch
runtimes.

```typescript
import { TaritClient } from "@tarit/sdk";

const tarit = new TaritClient({
  baseUrl: "https://tarit.example",
  apiKey: "tenant-api-key",
});
const result = await tarit.execute(vmId, "uname -a");
const child = await tarit.fork(vmId);
const pty = await tarit.openPty(vmId, { shell: "/bin/sh", deadlineMs: 30_000 });
try {
  pty.resize(120, 40);
  pty.write("uname -a; exit 0\n");
  while ((await pty.read({ deadlineMs: 30_000 })).type !== "exit") {}
} finally {
  await pty.close();
}
```

The generated `paths`, `operations`, and `components` types are exported for
callers that need an operation outside the ergonomic layer.

Creating a PTY activates a hibernated VM through the normal lifecycle gate. The
API key remains in the authenticated HTTP request; the WebSocket URL contains
only the short-lived, single-session connection token. Failed connections and
normal close both delete the server-side PTY lease.
