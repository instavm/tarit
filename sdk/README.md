# Tarit SDKs

`orch/openapi.yaml` is the public contract for both checked-in clients. The
generated surface covers every documented HTTP operation; the small handwritten
layers add API-key configuration, typed failures, deadline-bounded execution
polling, and stable-child-id retry for live forks.

## Regeneration

Run `./sdk/generate.sh` from the repository root. Generator versions are pinned
in that script. CI regenerates both clients and rejects any diff.

## Python

The package in `sdk/python` supports Python 3.11 and later.

```python
from uuid import UUID

from tarit_sdk.high_level import TaritClient

with TaritClient("https://tarit.example", "tenant-api-key") as tarit:
    result = tarit.execute(UUID(vm_id), "uname -a")
    child = tarit.fork(UUID(vm_id))
```

`AsyncTaritClient` provides the same execution and fork helpers for asyncio.
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
```

The generated `paths`, `operations`, and `components` types are exported for
callers that need an operation outside the ergonomic layer.
