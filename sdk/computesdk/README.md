# @computesdk/tarit

[ComputeSDK](https://github.com/computesdk/computesdk) provider for Tarit:
each sandbox is a KVM microVM run by a `taritd` host or fleet.

```ts
import { tarit } from '@computesdk/tarit';

const compute = tarit({
  baseUrl: 'https://tarit.example.com', // or TARIT_BASE_URL
  apiKey: process.env.TARIT_API_KEY,
});

const sandbox = await compute.sandbox.create({ image: 'node:22' });
const result = await sandbox.runCommand('node -v');
await sandbox.destroy();
```

| Option | Default | Meaning |
| --- | --- | --- |
| `baseUrl` | `TARIT_BASE_URL` | taritd API URL |
| `apiKey` | `TARIT_API_KEY` | API key, sent as `X-API-Key` |
| `image` | `TARIT_IMAGE`, else `node:22` (`python:3.12-slim` for `runtime: 'python'`) | Registered image for creates that name none |
| `shareDomain` | `TARIT_SHARE_DOMAIN` | Domain the host serves shared ports under, for `getUrl` |
| `commandTimeoutMs` | 120000 | Command timeout when a call sets none |

What maps to what:

- `create` is `POST /v1/vms`; with `snapshotId` it is `POST /v1/restore`.
  `vcpus` and `memMiB` are passed through when set. Leave them unset to
  get taritd's default shape, which its warm pools are built for.
- `runCommand` is one `POST /v1/execute`. A timeout past 170 s goes through
  `/v1/execute_async` and polling instead, since the synchronous call is
  capped at 180 s.
- `snapshot.create` takes an incremental checkpoint: the guest pauses only for
  memory changed since the last one, and the result restores on its own.
- `pause`/`resume` keep memory. `getUrl` creates a public share of the port.

## Fast creates

taritd keeps a warm pool for each registered image, so most creates are
served by restoring a clone from a snapshot. To have the image's runtime
already in memory for the first command, give the image a prewarm command
on the host:

```sh
taritd image prewarm node:22 'node -e 0'
```

On the reference host (6-core desktop, other load present), ComputeSDK's
time-to-interactive benchmark measured a 66 ms median and a 99.1 score with
10 sandboxes at once. Treat that as a lower bound for a remote host, which
adds the network round trips.

Command requests and polling share a total deadline of the guest command timeout plus 5 seconds. Command timeouts must be finite positive milliseconds at most 2,147,478,647. Transient polling responses (429/502/503/504) retry the same execution; submission is never replayed. `TaritDeadlineExceeded` retains `executionId` when submission returned it; an unavailable ID means submission may still have been accepted.
