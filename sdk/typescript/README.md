# `@tarit/sdk`

Typed Node.js and browser client for the Tarit orchestrator API.

```typescript
import { TaritClient } from "@tarit/sdk";

const tarit = new TaritClient({
  baseUrl: "https://tarit.example",
  apiKey: process.env.TARIT_API_KEY!,
});

const result = await tarit.execute(vmId, "uname -a");
const child = await tarit.fork(vmId);
const pty = await tarit.openPty(vmId, { shell: "/bin/sh" });
```

The package version matches the compatible Tarit server release. See the
[SDK guide](https://github.com/instavm/tarit/tree/main/sdk) for lifecycle,
deadline, and PTY cleanup examples.
