# taritd configuration reference

`taritd` loads daemon configuration from environment variables when `Config::from_env()` runs at server startup. `TARIT_CONFIG` is an optional TOML file for API keys and warm-pool policy; autoscaling is configured with environment variables. Single-host mode is selected when `TARIT_DATABASE_URL` is unset. Cluster mode is selected when `TARIT_DATABASE_URL` is set and `TARIT_PEER_SECRET` is a strong shared secret on every node.

## Required vs optional

At least one API key is required. Configure it with `TARIT_API_KEY`, `TARIT_API_KEYS`, or `[api_keys]` in `TARIT_CONFIG`. In cluster mode, also set `TARIT_PEER_SECRET` to the same strong value on every node. All other daemon settings have code defaults, although host paths such as the VMM binary, kernel, and rootfs must still point to usable files for VM creation to work.

Boolean environment variables accept `1`, `true`, `yes`, or `on` for true and `0`, `false`, `no`, or `off` for false. Invalid booleans fail startup.

## Core / identity

| Variable | Type | Default | Description |
| --- | --- | --- | --- |
| `TARIT_API_KEY` | string | unset | Single public API key. When set, it adds tenant `default`, role `admin`, and unlimited VM quota (`max_vms = 0`). Empty values are rejected. |
| `TARIT_API_KEYS` | comma-separated string | unset | Multi-key config. Format is `key:tenant:role[:max_vms]`. `role` is `admin` or `user`. Omitted or `0` `max_vms` means unlimited. Entries are added to any TOML keys. |
| `TARIT_LISTEN` | socket address | `0.0.0.0:8080` | HTTP bind address for public and internal routes. Invalid socket addresses are rejected. |
| `TARIT_HOST_ID` | string | output of `hostname`, else `localhost` | Stable node identity used in local records and fleet ownership. |
| `TARIT_RPC_ADDR` | string | `http://{listen.ip()}:{listen.port()}` | HTTP(S) origin advertised to peers. In cluster mode, set this to an address other nodes can reach; HTTPS is required unless insecure peer HTTP is explicitly enabled for development. |
| `TARIT_VMM_BIN` | path | `vmm` (looked up on `PATH`) | Path to the rust-vmm based `vmm` binary. `~/` is expanded. |
| `TARIT_KERNEL` | path | `/tmp/vmlinux.microvm` | Default guest kernel path used when a create request omits `kernel_path`. `~/` is expanded. |
| `TARIT_ROOTFS` | path | `/tmp/debian-rootfs.ext4` | Default rootfs path used when a create request omits `rootfs_path`. `~/` is expanded. |
| `TARIT_ROOTFS_READONLY` | bool | `false` | Request read-only guest mount semantics (`ro` kernel cmdline). The host base is always immutable and every VM gets a private CoW overlay. |
| `TARIT_SOCKET_DIR` | path | `~/.taritd/sockets` | Directory for per-VM VMM Unix domain sockets. `~/` is expanded. |
| `TARIT_DB` | path | `~/.taritd/fleet.db` | Node-local SQLite database path. `~/` is expanded. |
| `TARIT_IMAGES_DIR` | path | `~/.taritd/images` | Directory for registered rootfs images. `~/` is expanded. |
| `TARIT_CONFIG` | path | `~/.taritd/config.toml` | Optional TOML config file. Missing file is allowed. The default path expands `~/`; a path supplied in `TARIT_CONFIG` is used as given. |

## Cluster and durability

| Variable | Type | Default | Description |
| --- | --- | --- | --- |
| `TARIT_DATABASE_URL` | string | unset | PostgreSQL fleet registry URL. Empty string is treated as unset. Setting this enables distributed fleet behavior. |
| `TARIT_PEER_SECRET` | string | random local-only value | Key for short-lived, source/target-bound peer request HMACs. It is never sent on the wire. Explicit values must be at least 32 characters and cannot be `dev-peer-secret`; cluster mode requires this variable. |
| `TARIT_ALLOW_INSECURE_PEER_HTTP` | bool | `false` | Permit `http://` peer origins in cluster mode. Use only on an isolated development network; production mode forbids it. |
| `TARIT_PRODUCTION` | bool | `false` | Request the hostile-multi-tenant production profile. This currently fails closed because the orchestrated jail path is incomplete; see `PRODUCTION_READINESS.md`. |
| `TARIT_REAP_ON_SHUTDOWN` | bool | `true` | On SIGTERM or SIGINT, stop local `vmm serve` children after HTTP drain. |
| `TARIT_RDS_CA_FILE` | path | unset | Extra CA bundle for PostgreSQL TLS. This is read by the fleet connector, not by `Config::from_env()`. Empty string is ignored. |

`TARIT_RPC_ADDR` is listed in Core / identity because it is always present. In cluster mode it should be a stable private URL reachable by every peer.

## Capacity and admission limits

| Variable | Type | Default | Description |
| --- | --- | --- | --- |
| `TARIT_MAX_VMS` | usize | `32` | Maximum concurrent local VM slots, including warm VMs. |
| `TARIT_MAX_VCPUS` | u64 | `64` when warm pool is disabled; otherwise `ceil(available_parallelism * cpu_overcommit)` | Local vCPU placement ceiling. If set, it overrides the derived warm-pool ceiling. |
| `TARIT_MAX_MEMORY_MIB` | u64 | `65536` | Local memory placement ceiling in MiB. |
| `TARIT_ADMISSION_TIMEOUT_MS` | u64 | `60000` | Time a create operation may wait for capacity before admission gives up. |
| `TARIT_CPU_OVERCOMMIT` | f64 | `4.0` | Warm-pool CPU overcommit ratio. Also affects derived `TARIT_MAX_VCPUS` when the warm pool is enabled and `TARIT_MAX_VCPUS` is unset. |
| `TARIT_PTY_MAX_ACTIVE_CONNECTIONS` | positive usize | `1024` | Node-wide active guest PTY WebSocket limit. |
| `TARIT_PTY_MAX_ACTIVE_CONNECTIONS_PER_TENANT` | positive usize | `128` | Active guest PTY limit for one authenticated tenant. Must be less than the node-wide limit so one tenant cannot consume every slot. |
| `TARIT_PTY_MAX_ACTIVE_CONNECTIONS_PER_VM` | positive usize | `16` | Active guest PTY limit for one VM. Must not exceed the per-tenant limit. |

## Warm pool

| Variable | Type | Default | Description |
| --- | --- | --- | --- |
| `TARIT_WARM_POOL` | bool | `false` | Enable warm-pool replenishment. For this variable, only `1` or `true` enables it. |
| `TARIT_WARM_POOL_TARGET` | usize | `8` for the default class | Override the first warm-pool class target. |
| `TARIT_WARM_POOL_ROOTFS` | path | unset | Override the first warm-pool class rootfs when non-empty. `~/` is expanded. |
| `TARIT_WARM_POOL_IMAGE` | string | unset | Override the first warm-pool class image reference when non-empty after trimming. |
| `TARIT_WARM_POOL_RESTORE` | bool | `false` | Use restore-from-golden replenishment for the first class. For this variable, only `1` or `true` enables it. |
| `TARIT_WARM_POOL_LOW_WATERMARK` | usize | derived from target | Override the first class low watermark. Refill starts when depth is below this value. |
| `TARIT_WARM_POOL_HIGH_WATERMARK` | usize | derived from target | Override the first class high watermark. Refill does not intentionally grow past this ceiling. |
| `TARIT_WARM_POOL_HARD_FLOOR` | usize | derived from target | Override the first class emergency minimum. |
| `TARIT_REFILL_CGROUP` | path | unset | Optional cgroup v2 path for refill-spawned VMM children. Empty string clears the path. `~/` is expanded. |
| `TARIT_REFILL_CPU_WEIGHT` | u64 | `10` | cgroup v2 `cpu.weight` for refill children. Values are clamped to `1..=10000`. |

The default warm class is `vcpus = 1`, `memory_mib = 256`, `target = 8`, and `restore = false`. Watermarks derive from the target as follows: if target is `0`, all three watermarks are `0`; otherwise `buffer = max(target / 4, 1)`, `low_watermark = max(target - buffer, 1)`, `hard_floor = low_watermark.saturating_sub(buffer)`, and `high_watermark = target + buffer`. For the default target `8`, this gives hard floor `4`, low watermark `6`, and high watermark `10`.

## Autoscaling

| Variable | Type | Default | Description |
| --- | --- | --- | --- |
| `TARIT_AUTOSCALE` | bool | `false` | Enable the leader-elected autoscaler. |
| `TARIT_AUTOSCALE_MIN` | usize | `1` | Minimum healthy node count. |
| `TARIT_AUTOSCALE_MAX` | usize | `10` | Maximum healthy node count. |
| `TARIT_AUTOSCALE_OUT_FREE_VCPUS` | u64 | `2` | Scale out when aggregate free vCPUs drop below this threshold. |
| `TARIT_AUTOSCALE_IN_FREE_VCPUS` | u64 | `64` | Scale in when aggregate free vCPUs stay above this threshold. |
| `TARIT_AUTOSCALE_PROVIDER_CMD` | string | unset | Provider command invoked by the autoscaler. Empty string is treated as unset. The decision JSON is passed as `argv[1]`. |
| `TARIT_CLOUD` | string | `onprem` | Topology label used by autoscaling and placement decisions. |
| `TARIT_REGION` | string | `local` | Topology label used by autoscaling and placement decisions. |
| `TARIT_ZONE` | string | same as `TARIT_REGION` | Topology label used by autoscaling and placement decisions. |

The `TARIT_CONFIG` TOML schema does not define an `[autoscale]` table. Configure autoscaling with the environment variables above.

## Networking

| Variable | Type | Default | Description |
| --- | --- | --- | --- |
| `TARIT_ENABLE_NET` | bool | `false` | Enable per-VM tap devices, `/30` addressing, and NAT. Requires host networking privileges. |
| `TARIT_NET_STATE` | path | next to `TARIT_DB` with `.net.json` appended to the DB file name | Persistent per-VM tap/IP slot state. With the default DB, this is `~/.taritd/fleet.db.net.json`. `~/` is expanded when set. |

## Access / SSH gateway

| Variable | Type | Default | Description |
| --- | --- | --- | --- |
| `TARIT_SSH_GATEWAY` | bool | `false` | Enable the embedded SSH gateway. |
| `TARIT_SSH_GATEWAY_ADDR` | socket address | `127.0.0.1:2222` | SSH gateway bind address. Invalid socket addresses are rejected. |
| `TARIT_SSH_GATEWAY_HOST_KEY` | path | `~/.taritd/ssh_host_ed25519` | OpenSSH Ed25519 host key path. `~/` is expanded. |

## Port shares

Port-share guest traffic is disabled unless `TARIT_SHARE_LISTEN` is set. The
share listener is distinct from `TARIT_LISTEN`: it is private to a trusted
Caddy or Envoy edge, and direct public access is unsupported and must be
firewalled. Disabling only this listener does not disable the `/v1/shares`
control routes on `TARIT_LISTEN`. The edge must terminate public TLS, preserve
the incoming `Host`, support WebSocket upgrades, and never route
`/internal/v1/*` from the public edge.

The edge must overwrite—not append—`Forwarded` and `X-Forwarded-*` headers.
Tarit accepts one
`X-Forwarded-Proto` value only when it is exactly lowercase `http` or `https`;
missing, repeated, comma-separated, or other values are treated as `http`.
Tarit then rebuilds the forwarding headers sent to the guest. The checked-in
Caddy example removes `X-API-Key` and `Proxy-Authorization`, resets the
forwarding chain to one edge value, and preserves `Host` for share routing;
Tarit removes those headers plus the private-share token after gateway
authorization. Guest applications needing an application credential must use
`Authorization` or a different non-reserved header, never `X-API-Key`.

| Variable | Type | Default | Description |
| --- | --- | --- | --- |
| `TARIT_SHARE_LISTEN` | socket address | unset (disabled) | Bind address for the guest port-share gateway. It is required to be a valid socket address when set. Enabling it also requires `TARIT_SHARE_DOMAIN` and `TARIT_SHARE_TOKEN_KEY`. |
| `TARIT_SHARE_DOMAIN` | normalized DNS domain | unset | Base domain for guest hosts, so a share slug is served at `<slug>.<domain>`. Case is normalized to lowercase; a single trailing dot is removed. |
| `TARIT_SHARE_TOKEN_KEY` | canonical base64url | unset | Secret used to sign and verify private-share tokens. It must decode to exactly 32 bytes. Keep it out of source control and rotate it through the deployment secret mechanism. |
| `TARIT_SHARE_TOKEN_TTL_SECS` | positive integer seconds | `300` | Lifetime of an issued private-share token. |
| `TARIT_SHARE_CONNECT_TIMEOUT_MS` | positive integer milliseconds | `10000` | Maximum time to establish an upstream HTTP or WebSocket connection to the guest. |
| `TARIT_SHARE_IDLE_TIMEOUT_SECS` | positive integer seconds | `300` | Maximum idle period while proxying an HTTP body or WebSocket connection. |

Startup validation:

- `TARIT_SHARE_LISTEN` must parse as a socket address.
- When `TARIT_SHARE_LISTEN` is configured, `TARIT_SHARE_DOMAIN` must also be
  configured and normalized. A domain cannot be empty, contain whitespace,
  resolve syntactically as an IP address, exceed DNS label/domain limits, or
  contain labels other than lowercase ASCII letters, digits, and internal
  hyphens.
- When `TARIT_SHARE_LISTEN` is configured, `TARIT_SHARE_TOKEN_KEY` must be
  canonical unpadded base64url and decode to exactly 32 bytes.
- Each timeout and TTL setting must be a positive integer. The token TTL must
  also fit UTC timestamp arithmetic.

Use a wildcard DNS record and a wildcard certificate for
`*.${TARIT_SHARE_DOMAIN}`. The checked-in
[`deploy/Caddyfile.shares.example`](../deploy/Caddyfile.shares.example) accepts
certificate and key paths through environment variables, so no DNS-provider
credential is embedded in the example. The share listener is a guest data
plane; do not send it `/v1/*`, `/internal/v1/*`, or control-plane credentials.

## Usage stats and audit trail

See [USAGE-AND-AUDIT.md](USAGE-AND-AUDIT.md). Attribution and the query APIs need
cluster mode (`TARIT_DATABASE_URL` set); the meter always runs and buffers to
the local outbox otherwise.

| Variable | Type | Default | Description |
| --- | --- | --- | --- |
| `TARIT_USAGE_METER_INTERVAL_SECS` | u64 | `30` | How often the VM-runtime meter bills alive local VMs. |
| `TARIT_USAGE_FLUSH_INTERVAL_SECS` | u64 | `10` | How often the flusher pushes local usage/audit outboxes to PostgreSQL. |

## TOML config reference

`TARIT_CONFIG` defaults to `~/.taritd/config.toml`. If the file is missing, startup continues with defaults and environment variables. If the file exists, it must parse as the schema below.

Supported top-level TOML sections are `[api_keys]` and `[warm_pool]`. Other daemon settings, including autoscaling, networking, paths, and cluster settings, are environment-only in `crates/taritd/src/config.rs`.

```toml
# Dynamic table. Each key is the plaintext API key string.
[api_keys]
"dev-admin-key" = { tenant = "default", role = "admin", max_vms = 0 }
"tenant-a-key" = { tenant = "tenant-a", role = "user", max_vms = 20 }

[warm_pool]
enabled = true                 # bool, default false
cpu_overcommit = 4.0           # f64, default 4.0
replenish_concurrency = 4      # usize, default 4; file value 0 becomes 1
refill_cgroup = "/sys/fs/cgroup/taritd-refill" # optional string path
refill_cpu_weight = 10         # u64, default 10, clamped to 1..=10000

[[warm_pool.class]]
vcpus = 1                      # u8, required
memory_mib = 256               # u64, required
target = 8                     # usize, required
hard_floor = 4                 # optional usize, derived when omitted
low_watermark = 6              # optional usize, derived when omitted
high_watermark = 10            # optional usize, derived when omitted
restore = true                 # optional bool, default false
image = "node20"               # optional registered image name[:tag]
# rootfs = "/var/lib/taritd/rootfs.ext4" # optional path; do not set with image

[[warm_pool.class]]
vcpus = 2
memory_mib = 512
target = 4
restore = false
rootfs = "/var/lib/taritd/rootfs-512.ext4"
```

TOML and environment variables interact as follows:

- The file loads first.
- `[api_keys]`, `TARIT_API_KEYS`, and `TARIT_API_KEY` all add entries to the same registry. Duplicate plaintext keys are rejected.
- For warm pool settings, environment variables override the file after it is read.
- Class-specific warm-pool environment overrides apply only to the first class. Use TOML for multiple classes.
- `TARIT_WARM_POOL_TARGET`, `TARIT_WARM_POOL_HARD_FLOOR`, `TARIT_WARM_POOL_LOW_WATERMARK`, `TARIT_WARM_POOL_HIGH_WATERMARK`, `TARIT_WARM_POOL_RESTORE`, `TARIT_WARM_POOL_ROOTFS`, and `TARIT_WARM_POOL_IMAGE` target the first class only.
- If `[warm_pool]` omits `class`, the built-in default class remains.
- A warm-pool class may set `image` or `rootfs`, not both.

## Example environment blocks

### Single-node dev

```sh
export TARIT_API_KEY='replace-with-a-long-random-token'
export TARIT_LISTEN='0.0.0.0:8080'
export TARIT_HOST_ID="$(hostname)"
export TARIT_RPC_ADDR='http://127.0.0.1:8080'
export TARIT_VMM_BIN='/path/to/tarit/vmm/target/release/vmm'
export TARIT_KERNEL='/var/lib/taritd/vmlinux.microvm'
export TARIT_ROOTFS='/var/lib/taritd/rootfs.ext4'
export TARIT_SOCKET_DIR="$HOME/.taritd/sockets"
export TARIT_DB="$HOME/.taritd/fleet.db"
export TARIT_IMAGES_DIR="$HOME/.taritd/images"
```

### One node of a three-node cluster with PostgreSQL

```sh
export TARIT_API_KEY='replace-with-a-long-random-token'
export TARIT_PEER_SECRET='replace-with-a-long-random-peer-secret'
export TARIT_DATABASE_URL='postgres://user:password@postgres.example:5432/taritd?sslmode=require'
export TARIT_HOST_ID='node-a'
export TARIT_LISTEN='0.0.0.0:8080'
# HTTPS endpoint supplied by the private peer TLS proxy.
export TARIT_RPC_ADDR='https://node-a.peer.example.com:8443'
export TARIT_VMM_BIN='/opt/taritd/bin/vmm'
export TARIT_KERNEL='/var/lib/taritd/vmlinux.microvm'
export TARIT_ROOTFS='/var/lib/taritd/rootfs.ext4'
export TARIT_SOCKET_DIR="$HOME/.taritd/node-a/sockets"
export TARIT_DB="$HOME/.taritd/node-a/fleet.db"
export TARIT_IMAGES_DIR="$HOME/.taritd/node-a/images"
export TARIT_MAX_VMS='32'
export TARIT_MAX_MEMORY_MIB='65536'
# Optional for PostgreSQL deployments that need an extra CA bundle:
# export TARIT_RDS_CA_FILE="$HOME/.taritd/rds-global-bundle.pem"
```

Nodes B and C should use the same API key, peer secret, database URL, VMM binary, kernel, and rootfs, but distinct `TARIT_HOST_ID`, `TARIT_RPC_ADDR`, `TARIT_SOCKET_DIR`, `TARIT_DB`, and usually `TARIT_IMAGES_DIR`.
The HTTPS proxy must deny every route except `/internal/v1/*`, support WebSocket
upgrades, and preserve the method, path, query, body, and `X-Tarit-*` headers
exactly; `taritd` performs the request-HMAC authentication. Its certificate SAN
must match the `TARIT_RPC_ADDR` host and chain to the WebPKI roots used by the
built-in Rustls clients. There is no custom peer-CA setting yet. For an isolated
development cluster without a compatible TLS proxy, use a private `http://`
origin and set `TARIT_ALLOW_INSECURE_PEER_HTTP=1` explicitly.

## Startup validation rules

`Config::from_env()` is called only for server mode. The following checks fail startup with hard errors:

- `TARIT_LISTEN` must parse as a socket address: `TARIT_LISTEN must be a valid socket address`.
- `TARIT_SSH_GATEWAY_ADDR` must parse as a socket address: `TARIT_SSH_GATEWAY_ADDR must be a valid socket address`.
- If `TARIT_CONFIG` exists, it must be readable and valid TOML. Error contexts are `read config file {path}` and `parse config file {path}`.
- At least one API key must be configured: `configure at least one API key with TARIT_API_KEY, TARIT_API_KEYS, or [api_keys] in TARIT_CONFIG`.
- `TARIT_API_KEY` must not be empty: `TARIT_API_KEY must not be empty`.
- Cluster peer origins must use HTTPS unless
  `TARIT_ALLOW_INSECURE_PEER_HTTP=1`; production mode always forbids insecure
  peer HTTP.
- PTY active-connection limits must be positive; the per-tenant limit must be
  less than the global limit, and the per-VM limit must not exceed the
  per-tenant limit.
- `TARIT_API_KEYS` must not be empty when set: `TARIT_API_KEYS must not be empty when set`.
- Each `TARIT_API_KEYS` entry must have three or four fields: `TARIT_API_KEYS entries must be key:tenant:role[:max_vms]`.
- `TARIT_API_KEYS` entries reject empty keys and tenants: `TARIT_API_KEYS entries must not contain empty keys` and `TARIT_API_KEYS entries must not contain empty tenants`.
- `TARIT_API_KEYS` `max_vms` must parse as `usize`: `TARIT_API_KEYS max_vms must be a non-negative integer`.
- `TARIT_API_KEYS` must include at least one non-empty entry: `TARIT_API_KEYS must include at least one entry`.
- API key roles must be `admin` or `user`: `API key role must be 'admin' or 'user'`.
- API keys from all sources must not be empty or duplicated: `API keys must not be empty` and `duplicate API key configured`.
- Tenant IDs must be non-empty and contain only ASCII letters, digits, `.`, `_`, or `-`: `tenant id must not be empty` and `tenant id may only contain ASCII letters, digits, '.', '_', or '-'`.
- Cluster mode requires a strong peer secret: `TARIT_PEER_SECRET must be set to a strong value when TARIT_DATABASE_URL is configured for a fleet`.
- `TARIT_PRODUCTION=1` fails startup until the mandatory per-VM jail,
  namespace, cgroup, and path-staging boundary is implemented end to end.
- Warm-pool watermarks must be ordered: `warm-pool watermarks for {vcpus} vCPU/{memory_mib} MiB must satisfy hard_floor <= low_watermark <= target <= high_watermark (got {hard_floor} <= {low_watermark} <= {target} <= {high_watermark})`.
- A warm-pool class cannot set both `image` and `rootfs`: `warm-pool class for {vcpus} vCPU/{memory_mib} MiB cannot set both image and rootfs`.
