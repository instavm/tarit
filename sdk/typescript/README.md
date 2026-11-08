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

`execute` accepts a total wait budget in its third argument:
`tarit.execute(vmId, command, { deadlineMs: 10_000 })`. The deadline covers
submission, HTTP response bodies, and polling. `waitExecution` applies the same
budget to polling an existing execution. Deadlines must be finite and positive;
poll intervals must be finite and nonnegative. Expiry raises
`TaritDeadlineExceeded` and stops waiting; an already submitted guest command
may continue running.

The error's `executionId` and message preserve the server-returned ID once
submission is decoded, or the ID passed to `waitExecution`. Use
`tarit.waitExecution(error.executionId)` to resume polling without resubmitting.
If submission times out before a usable response arrives, `executionId` is
`undefined` and the message says the ID is unavailable. The server may still
have accepted the command; the SDK does not invent an ID or retry submission.

`pollIntervalMs` is the minimum wait after a pending response before another
poll. If the deadline expires before that interval ends, no further poll is sent.
