# September 2026 roadmap

Status: implementation and hardware validation in progress. The priority path has
focused c8i evidence, but the September release gate is not complete and no open
security, durability, cross-node, SDK, or performance item is waived.

## Outcome

September should turn Tarit's existing VMM primitives into a tenant-safe sandbox
product surface while continuing to treat production security as the release
gate. The month has five deliverables:

1. sandbox fork and branch operations built on live snapshots, lazy memory
   restore, and private disk CoW overlays;
2. generated Python and TypeScript SDKs;
3. durable, auditable per-VM egress policy control;
4. scale-to-zero through a capacity-releasing hibernation lifecycle based on
   suspend and resume; and
5. evidence for all seven historical stop-ship security items, including
   completion of every aspect still marked partial below.

GPU support, aarch64 guests, and a public MCP server are explicitly deferred.
The release must keep Tarit a general-purpose OSS VMM/orchestrator rather than
embedding InstaVM Sandbox policy. The Sandbox integration should reduce to
user-specific CRUD and policy code over Tarit's public contract. Customer-facing
durable volumes are now in scope behind a provider-neutral attachment contract:
local/block and NFS-compatible backends first, with AWS, Azure, and object-store
adapters evaluated and tested without provider logic leaking into the VMM.

### P0 execution and acceptance order

1. Atomically fork a running VM at one RAM, device, and disk boundary.
2. Hibernate it to true scale-to-zero, releasing the VMM and scheduler capacity.
3. Single-flight resume on HTTP, connected PTY, SSH, and share ingress without
   allowing duplicate VMMs.
4. Treat artifact integrity, tenant isolation, ingress authentication, stale
   ownership, and every historical stop-ship security row as release blockers.
5. Add branch/SDK breadth and performance distributions only on top of the
   proven lifecycle path.

The public MCP server remains deferred. Local mocks, unit tests, and compilation
alone never satisfy an item that requires KVM evidence: test the exact candidate
revision on c8i with a compatible kernel and, wherever possible, an Ubuntu guest
produced through the OCI image pipeline.

## Current baseline

- Live snapshots are wire-compatible and include device-DMA dirty tracking,
  I/O-thread parking, and a measured 92-279 microsecond final stop on c8i.
- Every orchestrated VM receives a private sparse rootfs overlay. Restored warm
  clones already combine shared snapshot memory with a per-clone CoW disk.
- Fleet live fork is now a session-fenced cross-node operation. A request
  received by node B for a running source on node A takes the atomic snapshot
  on A, streams and authenticates the complete artifact over mTLS, requires the
  configured replica policy, and restores the isolated child on B. The public
  request cannot choose a host, peer URL, or storage path.
- Suspend releases resident guest memory but retains scheduler quota. The new
  hibernation path first satisfies the configured cross-failure-domain artifact
  policy, removes the VMM and releases capacity, then verifies and restores its
  artifact through the activation gate. A request received by a non-owner node
  is forwarded over the fenced peer lifecycle protocol. Hibernation now has a
  durable fleet artifact/policy binding; after the owner becomes stale, a
  surviving node with a verified replica may transactionally claim and restore
  the same logical VM identity. Healthy owners, wrong tenants, and stale target
  boot sessions cannot be claimed.
- Per-VM nftables egress policy is now a tenant-scoped, CAS-versioned durable
  desired resource. Live updates are atomic; hibernated updates serialize with
  activation; resume installs the normalized policy before guest startup; and
  signed peer GET/PUT forwarding preserves owner-node routing. The fleet
  hibernation binding carries the exact policy revision and canonical content,
  so stale-owner recovery on another node restores the same desired policy.
- Orchestrated snapshots are full-only. Incremental snapshot chains remain a
  local VMM facility until parent relocation and manifest integrity are durable.
- The shipped Linux 6.12 guest kernel is checksum-pinned and reproducibly built.
  Virtio balloon, virtio entropy, block, network, vsock, and serial drivers are
  built in; unused legacy hardware entropy drivers remain disabled.
- The protected-main KVM release workflow is specified but still needs a
  registered `self-hosted`, `linux`, `x64`, `kvm` runner.

## The seven stop-ship items

The original readiness list contained seven items. The current candidate
implements all seven foundations and has focused c8i evidence for each. They
remain mandatory regression gates; broader release qualification is tracked in
`PRODUCTION_READINESS.md` and is not implied by implementation status.

| Historical item | Current state | September evidence or deliverable |
| --- | --- | --- |
| Mandatory per-VM jail, identity, namespaces, cgroups, cleanup, and coordinator seccomp | Implemented | Production-mode fail-closed tests; namespace/cgroup identity inspection on KVM; partial-staging and cleanup fault injection |
| Separate peer listener with mandatory mTLS, rotation, and host-session fencing | Implemented | Dedicated public/internal routers and listener; mandatory fleet mTLS; leaf-certificate fingerprint-to-host binding; explicit CA overlap/rotation; request replay defense; and source/target boot-session fencing. A real two-process Postgres-backed c8i gate rejects absent, untrusted, stale-session, wrong-target, and trusted-but-wrong-host credentials, and proves same-host certificate rotation before removing the old CA. |
| Opaque snapshot handles, durable authenticated storage, fleet index, and integrity metadata | Implemented | Public snapshot/restore exposes only an opaque UUID; private SQLite/PostgreSQL locators retain host/path/tenant identity; a SHA-256-authenticated 64 KiB chunk manifest covers snapshot metadata, RAM, and disk; UFFD verifies each RAM chunk from a stable private buffer before `UFFDIO_COPY`; disk source and seeded destination are verified before guest use. Durable VM-to-artifact references are owner/incarnation fenced and acquired before lazy restore publication. Branch, hibernation, parent, and VM release cascade logical deletion only at zero references; exact owned replica RAM, integrity, and CoW files plus local metadata are then removed by retryable GC. |
| Typed guest network repair after restore | Implemented | Real-KVM restore proving address, route, gateway, DNS, and outbound connectivity; malformed repair data fails closed |
| Artifact replication across failure domains | Implemented | Snapshot RAM, disk upper, authenticated manifest, exact generated ext4 lower, and kernel stream over the dedicated mTLS peer listener into private staging files with exact length/digest bounds, full verification before rename, failure-domain-aware readiness, and disk reservation through publication. The injected agent is carried inside the exact authenticated ext4 and independently bound by its admission digest. A real three-node c8i test starts the targets with no admitted image and a deliberately mismatched kernel, reaches two verified zones, rejects tampered boot metadata without residue, loses the source node, restores the same hibernated VM identity and an independent branch on the survivor, automatically distributes boot inputs, repairs onto a third failure domain under a fenced lease, and proves physical replicas converge to deletion only after the last live lazy reader releases its durable reference. |
| Immutable image digests and signature/provenance admission | Implemented | `skopeo inspect` resolves a tag once; only the resulting digest reference reaches the pull. Admission stores the OCI manifest, generated ext4, injected agent, and trusted-key digests; `cosign verify` is mandatory in production; VM and warm-pool admission rehash content and fence legacy or policy-mismatched rows. The c8i gate rejected unsigned Ubuntu, admitted an actually signed OCI image, rejected it after trusted-key rotation, and revalidated the identical pinned digest after rollback. |
| Per-tenant/per-VM I/O and network limits | Implemented | Boot/restore/recovery enforcement; multi-device `io.max`; TAP ingress/egress shaping; stale-limit cleanup; tenant aggregate abuse test |

No hostile multi-tenant production claim is permitted until the complete release
gate, including the unresolved adversarial and durability work, has passing
evidence.

## API and data model decisions

These decisions should be reviewed before implementation so the SDKs do not
freeze unstable or unsafe concepts.

### Artifacts and lineage

- Introduce an opaque `artifact_id`; public APIs never accept or return host
  paths or physical host IDs.
- An artifact manifest records content digest, type, size, immutable image and
  agent digests, parent artifact when applicable, creation revision, integrity
  metadata, replication state, and reference count.
- A branch is a tenant-scoped lineage record, not a mutable snapshot filename:
  `branch_id`, `name`, `head_artifact_id`, source VM/branch identity, revision,
  and timestamps.
- Updates use compare-and-swap revisions and idempotency keys. Branch deletion
  removes a reference; artifact GC occurs only after all VM, branch, and replica
  references are gone.

Proposed public operations:

```text
POST   /v1/vms/{vm_id}/fork
POST   /v1/branches
GET    /v1/branches
GET    /v1/branches/{branch_id}
POST   /v1/branches/{branch_id}/fork
POST   /v1/branches/{branch_id}/restore
DELETE /v1/branches/{branch_id}
GET    /v1/operations/{operation_id}
```

Fork and restore are asynchronous operations with stable terminal errors and
idempotent replay. A fork of a running VM uses the live full-snapshot path. A
fork of a suspended/hibernated VM reuses its immutable artifact when safe.
Every child receives a new VM identity, vsock identity, network allocation,
private disk overlay, audit lineage, and ownership record.

### Egress policies

- Model policy as a versioned resource attached to one VM, with default-deny,
  normalized CIDR/port/protocol rules, optional DNS-aware destinations, revision,
  and audit identity.
- Preserve the existing atomic quarantine-and-replace transaction. A failed
  update must leave the VM quarantined or on the previous verified policy, never
  partially open.
- Persist desired policy independently of ephemeral TAP/nft identifiers so boot,
  restore, migration-like re-placement, and recovery recompile it safely.

Proposed operations:

```text
GET /v1/vms/{vm_id}/egress-policy
PUT /v1/vms/{vm_id}/egress-policy
```

The existing `PATCH /v1/egress/vm/{id}` remains a compatibility surface until
the SDKs have migrated, then follows the normal deprecation policy.

### Scale-to-zero lifecycle

Do not redefine the current `suspended` state: it intentionally retains owner
and scheduler quota. Add an explicit `hibernated` state and operation.

```text
POST /v1/vms/{vm_id}/hibernate
POST /v1/vms/{vm_id}/resume
```

Hibernate creates or reuses a durable artifact, verifies replication policy,
stops the resident VMM, releases memory/CPU/network allocation, and retains a
fenced logical VM record. Resume performs normal placement, localizes and
verifies artifacts, restores through UFFD, allocates a new network identity,
applies typed network repair and the desired egress policy, then returns only
after readiness succeeds. A failed resume must leave a retryable hibernated VM
without leaked capacity.

### Fork, snapshot, and lazy-CoW invariants

The implementation must optimize the complete fork path, not only the API
acknowledgement. Every operation records queue, pre-copy, final-stop, artifact
commit, placement, UFFD registration, first-run, readiness, and total latency so
the slow phase is visible. Performance claims use p50, p95, and p99 from a named
host shape, storage class, guest size, dirtying workload, concurrency, and source
revision.

- A running-VM fork has one atomic fork point. During the live snapshot's final
  stop, all vCPUs and guest-memory-writing I/O threads are quiesced; residual KVM
  and host/device-DMA dirty pages, vCPU/device state, and the flushed disk upper
  are captured from that same boundary. Memory and disk artifacts from different
  boundaries must never be assembled into one fork.
- Pre-copy is adaptive and bounded. It uses measured copy bandwidth and guest
  dirty rate to stop adding rounds when they cannot reduce blackout. Convergent
  workloads retain the current 500 microsecond final-stop target. A divergent or
  timed-out workload takes a truthful bounded final stop, reports the measured
  blackout and termination reason, and never publishes a partial artifact.
- The committed parent memory image and disk base are immutable and pinned by
  reference until every child is gone. Each child gets an independent anonymous
  guest-memory mapping populated on demand with UFFD and a unique sparse writable
  disk upper; child writes can modify neither the artifact nor another child.
- Artifact metadata and lineage are authenticated before guest execution. Every
  lazily loaded memory or disk chunk is range-checked and verified against
  authenticated chunk/Merkle metadata before it becomes guest-visible. A missing,
  short, corrupt, relocated, or wrong-parent chunk fails the child closed instead
  of supplying zeroes or unverified bytes.
- The local fast path is constant in guest RAM and virtual-disk size until the
  child actually touches pages or blocks: mmap plus UFFD registration for memory,
  and FICLONE/reflink or a fresh sparse upper for disk. A host without UFFD may use
  verified eager restore, and storage without safe reflink may use the existing
  allocated-extent sparse copy, but both are reported as degraded paths and are
  excluded from low-latency claims. Dense virtual-disk copying is not an allowed
  implicit fallback.
- The source VM resumes after the minimal coherent final stop; artifact hashing,
  replication, child placement, and readiness continue asynchronously. An
  operation becomes successful only after durable artifact commit and child
  readiness. Cancellation or failure removes staging files, UFFD handlers,
  overlays, leases, and reference counts without affecting the source or siblings.
- Week 1 records same-hardware baselines and approves hard regression budgets for
  snapshot blackout, artifact commit, UFFD setup, first-fault service, fork-to-run,
  fork-to-ready, restore-to-ready, concurrent fan-out, bytes copied, allocated
  blocks, source throughput impact, and post-restore major-fault tail latency. The
  release gate fails on a correctness error, an unexplained latency regression, or
  any claimed fast-path run that silently used an eager or dense-copy fallback.

### SDK boundary

- Treat `orch/openapi.yaml` as the public contract source. Add an API-breaking
  compatibility check before generating clients.
- Generate Python and TypeScript models and low-level clients; maintain a small
  handwritten ergonomic layer for polling operations, async execution, PTY,
  retries, deadlines, and idempotency.
- SDK releases are versioned with the server compatibility range and tested
  against the same OpenAPI fixture.

### OSS boundary and persistent volumes

- Tarit owns VM lifecycle, snapshots, forks, networking, storage attachment,
  scheduling, security, and its provider-neutral API. InstaVM Sandbox owns only
  product-specific identity, billing, policy, and user CRUD composition. Tarit
  must not import Sandbox packages or encode InstaVM tenancy/product semantics.
- Define an opaque `volume_id` and versioned attachment resource with explicit
  backend capabilities: block versus filesystem, read-only versus writable,
  single- versus multi-writer, snapshot/clone support, durability class,
  availability zone, mount/attach options, and fencing generation.
- Keep provider credentials and physical locators private. AWS EBS/EFS, Azure
  Disk/Files, generic NFS, local block, and object-backed implementations plug
  into an orchestrator-side provider interface; the VMM receives only a safely
  opened block descriptor or a pre-mounted, jailed filesystem path.
- Object storage is not silently presented as POSIX storage. Use it for immutable
  artifact/blob semantics, or an explicitly degraded cache/filesystem adapter
  whose consistency, fsync, rename, locking, and failure behavior are declared.
- Volume attach/detach is idempotent, tenant scoped, generation fenced, audited,
  crash recoverable, and ordered with hibernate/resume and VM deletion. Forks do
  not clone external durable volumes unless the backend advertises and completes
  an atomic snapshot/clone operation.

## Work sequence

### Week 1: contracts and security foundations

- Approve artifact, branch, operation, egress-policy, and hibernation schemas.
- Approve the fork/snapshot correctness invariants, phase telemetry, qualifying
  workloads, and same-hardware p50/p95/p99 regression budgets.
- Add the artifact index and immutable manifest/digest model before adding fork
  routes.
- Split public and peer listeners; introduce mTLS identity and boot/session
  leases in heartbeat and ownership records.
- Add contract tests that reject public paths/host IDs and stale host sessions.
- Re-run the three resolved stop-ship gates on real KVM and record results.

Exit: public schemas are reviewable; peer traffic fails closed without a valid
current host session; artifact records contain no node-local public identity.

### Week 2: fork/branch MVP and egress policy

- Implement live full-snapshot fork to an artifact and restore to an isolated
  child overlay.
- Keep the fork point atomic across memory, device, and disk state; use UFFD lazy
  memory population and reflink/sparse disk CoW without size-proportional work on
  the qualifying fast path.
- Implement branch CRUD, lineage CAS, idempotent operations, authorization, and
  reference-safe GC.
- Stabilize per-VM egress policy resources and recovery behavior.
- Add OpenAPI paths only after tenant-safe representations and errors pass tests.

Exit: two children forked from one live VM have byte-consistent initial state,
independent subsequent memory/disk/network state, and no public host paths. Phase
telemetry proves the qualifying local fast path used UFFD and reflink/sparse CoW,
and its p50/p95/p99 results remain inside the approved regression budgets.

### Week 3: scale-to-zero and clients

- Implement hibernate/resume with capacity release, normal re-placement, artifact
  verification, network repair, and egress restoration.
- Generate Python and TypeScript clients and add ergonomic async operation
  polling.
- Complete immutable image digest and provenance admission.

Exit: a hibernated VM consumes no resident VMM allocation, resumes without data
loss on another eligible node, and is controllable through both SDKs.

### Week 4: replication, adversarial testing, and release evidence

- Complete artifact replication and node/failure-domain placement checks.
- Run fault injection, node-loss restore, certificate rotation, stale-session,
  corrupted-artifact, policy rollback, quota, and GC tests.
- Run all normal CI and protected-main KVM gates, followed by soak and performance
  sampling on appropriate hardware.
- Update `CHANGELOG.md`, `PRODUCTION_READINESS.md`, OpenAPI, operations,
  architecture, resilience, and SDK documentation from measured evidence.
- Prove the Sandbox integration uses only Tarit's public contract, implement the
  portable volume attachment core and qualifying backends, and run a diverse OCI
  compatibility matrix plus bounded soak and chaos campaigns.

Exit: every historical stop-ship row has linked evidence, all roadmap APIs are
contract-tested, and release documentation makes no claim broader than the
passing hardware and failure-domain tests.

## Test matrix

| Area | Required tests |
| --- | --- |
| Fork consistency | One atomic memory/device/disk boundary; live device-DMA writes survive; guest-visible blackout measured; child memory and filesystem hashes match the fork point; concurrent exec/write/fork is fenced |
| Lazy memory | Independent UFFD mappings; first-touch and write-after-fault isolation; randomized and concurrent fault order; short read, wrong offset, truncation, bit flip, handler death, and source deletion fail closed; eager fallback is explicit |
| Disk CoW | Independent sparse uppers; base unchanged; overlapping and unaligned writes; flush/FUA and reopen durability; reflink and allocated-extent fallback; no dense-copy fallback; parent/child deletion order cannot corrupt survivors |
| Fork performance | Per-phase p50/p95/p99; convergent and divergent dirtying; 1/N fan-out; multiple RAM/disk sizes; bytes copied and allocated blocks; source throughput impact; first-fault and post-restore tail latency; fast/degraded path labels |
| Artifact integrity | Truncation, bit flip, wrong manifest, wrong parent, symlink, path substitution, stale replica, and unauthorized tenant all fail closed |
| Hibernation | RSS and scheduler capacity released; state byte-identical; repeated hibernate/resume; restart during each transition; failed restore remains retryable |
| Clone uniqueness | VMGenID changes before cloned vCPUs resume; guest kernel reseeds; sibling kernel RNG/boot identity diverges; a post-fork userspace hook invalidates cached nonces, tokens, and sessions before HTTP/PTY/SSH/exec admission |
| Network/egress | Allow and deny TCP/UDP/DNS; lateral denial; spoofing; atomic live update; restore/restart recompilation; stale qdisc/nft cleanup |
| Peer security | Separate listener; mTLS required; certificate fingerprint bound to host heartbeat; trusted-member impersonation rejection; rotated cert overlap; expired cert; replay; stale boot session; host ID reuse; public route isolation |
| Image supply chain | Tag mutation after admission; unsigned/untrusted image; digest mismatch; provenance-policy change; rollback to an already admitted digest |
| SDK | OpenAPI compatibility; Python sync/async; TypeScript Node runtime; retries/idempotency; typed errors; tenant-negative tests |
| OSS boundary | Tarit builds/tests without Sandbox; Sandbox-specific code depends only on public API/SDK packages; dependency-direction check rejects reverse imports |
| Persistent volumes | Attach/detach/restart/hibernate/resume/delete ordering; tenant and generation fencing; read-only and writable block; NFS reconnect; provider timeout/retry; node loss; snapshot-capability negotiation; fsync durability; stale attachment cleanup |
| OCI compatibility | Ubuntu, Debian, Alpine, Fedora, Rocky, and a minimal/distroless-compatible workload where guest-agent requirements permit; tag-to-digest pinning; init/entrypoint/env/user/filesystem variants; expected unsupported cases reported explicitly |
| Soak and chaos | Repeated create/exec/fork/hibernate/resume/delete with zero leaked VMMs, mounts, TAPs, cgroups, leases, references, or files; kill taritd/VMM/peer/transfer/GC at transition boundaries; corrupt/truncate artifacts; database and network interruption; deterministic seeds and retained failure bundles |
| Existing regression | Workspace fmt/clippy/tests; KVM integration; lifecycle; multitenancy; shares; PTY; warm restore; cgroup; GC; live snapshot; suspend; network recovery |

The upstream failure mapping and the evidence required to close each row are
maintained in [`docs/LIVE_FORK_KNOWN_FAILURE_AUDIT.md`](docs/LIVE_FORK_KNOWN_FAILURE_AUDIT.md).
In particular, fresh virtio-rng bytes do not by themselves prove that an
already-cloned kernel or userspace PRNG has reseeded. Clone uniqueness remains
a stop-ship item until a real generation notification and pre-admission
userspace repair barrier pass on c8i.

The release gate retains the documented 100 cold and warm create-to-exec samples,
100 percent success requirement, p99 <= 5,000 ms cold and <= 1,000 ms warm, plus
strict cold, snapshot-restore, and suspend/resume measurements.

## c8i validation plan

The functional qualification worker is a `c8i.xlarge` with two vCPUs, 7.6 GiB
RAM, and read/write `/dev/kvm`. It currently exposes one 50 GiB root volume and
no independent reflink-capable test SSD. This host is suitable for functional
KVM and low-concurrency failure testing, but not for headline concurrency or
p99 release claims. Those gates require dedicated metal and an independent
storage fixture.

The guest kernel used for SSH, PTY, and share activation must enable
`CONFIG_VSOCKETS`, `CONFIG_VIRTIO_VSOCKETS`, and
`CONFIG_VIRTIO_VSOCKETS_COMMON`. A kernel with only the generic vsock option is
not a valid ingress test kernel. Ubuntu identity must be asserted in the guest;
an ext4 superblock or successful boot alone is insufficient evidence that the
OCI input was Ubuntu.

### Current qualification summary

| Capability | Current evidence | Remaining release work |
| --- | --- | --- |
| Live fork and lazy CoW | Atomic live snapshot, authenticated lazy RAM, private disk overlays, sibling isolation, high-dirty workloads, rollback failpoints, and fail-closed UFFD handler exit, descriptor loss, UNMAP, and REMAP handling pass on c8i | Cancellation across every distributed phase and 100-sample latency distributions |
| Scale-to-zero and activation | Hibernation releases the VMM and scheduler capacity; HTTP, PTY, SSH, and share activation single-flight through the same restore gate | Long-hibernation timer/watchdog/lease qualification and sustained contention testing |
| Clone identity | Linux 6.6 VMGenID notification and the mandatory userspace repair barrier pass; Linux 5.10 passes through the barrier-only compatibility path; a delayed application-token repair hook rejects or repairs eight concurrent exec requests without exposing inherited state | Concurrent PTY, SSH, HTTP-share, and mixed-ingress repair races for application-owned PRNG, nonce, token, and session caches |
| Security and isolation | Guest VMX/SVM and `/dev/kvm` are hidden while worker KVM remains enabled; jail, seccomp, cgroups, mTLS peer identity, opaque artifacts, signed-image admission, and tenant fences pass focused gates | Continued kernel/microcode qualification and the remaining adversarial release matrix |
| Persistent volumes | Local and NFS-backed raw block volumes pass attach, fsync, hibernate, cross-node recovery, busy-detach, and deletion across Ubuntu/Alpine and Linux 6.6/5.10 | Managed EFS/Azure qualification, protected NFS transport, physical host loss, and backend-native snapshot/clone support |
| OCI and kernels | Ubuntu 24.04 and Alpine 3.20 cover the lifecycle matrix on Linux 6.6 and 5.10; the broader seven-image compatibility gate passes on both kernels | Decompression/inode exhaustion and interrupted registry-transfer qualification |
| Release artifacts | The Linux 6.12 production guest kernel is checksum-pinned, config-verified, and reproducibly built with required virtio drivers | Protected-main KVM runner execution and retained release evidence |

The rows above distinguish implemented behavior from release qualification. A
passing focused gate does not waive an item in the final column.

### Detailed c8i qualification record through 2026-08-31

- The vCPU snapshot format no longer stops at the historical fixed 14-MSR
  architectural set. Tarit now filters `KVM_GET_MSR_INDEX_LIST` across KVM's
  full reserved custom range, serializes the selected indices, validates the
  destination before restore, chunks at the ioctl entry limit, and enforces
  async-PF INT-before-EN and TSC-before-deadline ordering. Real jailed testing
  caught two integration errors that unit tests could not: lazy policy
  initialization attempted an `fcntl` after the vCPU seccomp filter was active,
  and c8i's enumeration omitted readable IA32_EFER. Initialization now happens
  before confinement, while enumeration is used only for the dynamic custom
  range and strict GET/SET counts retain authority for architectural MSRs. The
  boot-enabled VMM workspace tests and `-D warnings` clippy pass, followed by a
  302-operation c8i lifecycle run covering snapshot, suspend/resume, live fork,
  concurrent fan-out/delete, scale-to-zero single-flight wake, and randomized
  transitions.

- Clone admission now combines the `VMGENCTR`/`VM_GEN_COUNTER` ACPI generation
  notification with a synchronous host-to-guest repair barrier. Every restore
  receives fresh host entropy, applies `RNDADDENTROPY` and `RNDRESEEDCRNG`,
  replaces the guest boot/clone identity, and runs the optional root-owned
  `/usr/libexec/tarit/post-fork` hook before readiness. Linux 6.6 tests observed
  one VMGenID reseed in each clone; Linux 5.10 observed none and still produced
  distinct generation IDs, boot IDs, clone IDs, and first random samples through
  the compatibility barrier. Ubuntu 24.04 and Alpine 3.20 OCI guests passed cold
  boot, live fork, private disk CoW, fail-closed hook admission, hibernate, and
  HTTP resume under the production jail and seccomp policy on both kernels.
- The model-based real-KVM lifecycle gate passed 693 API operations on each of
  Linux 5.10 and 6.6. It covered the deterministic transition table, concurrent
  live-fork fan-out, duplicate create, snapshot/delete and terminal-delete races,
  capacity rejection, single-flight resume, balloon state across descendants,
  three fixed randomized seeds, and process/jail/artifact cleanup invariants.
- An SSH-before-fork gate exposed a Unix-domain socket path at the Linux
  `sockaddr_un` limit: cold boot silently used serial fallback and the restored
  child correctly failed clone admission. Vsock control sockets now use compact
  per-process names, reject paths over 107 bytes before bind, and surface restore
  bind failures directly. The exact rebuilt VMM then passed live fork after an
  authenticated SSH PTY session, opaque restore, eight-way HTTP single-flight
  wake, PTY wake, SSH wake, corrupted-artifact rejection, and interrupted-
  hibernation recovery on both 5.10 and 6.6.

- Ubuntu 24.04 OCI pull and ext4 conversion completed in about 1.92 seconds and
  produced a valid 512 MiB filesystem.
- The lower-level atomic live-snapshot harness passed two restore rounds with an
  8 MiB RAM payload digest preserved. Final stops were about 25.36 ms and 15.35
  ms on this nested two-vCPU host; the 8,216,301-byte diff was materially smaller
  than the 268,443,904-byte full snapshot.
- The focused real-KVM lifecycle gate passed live fork state continuity and
  post-fork isolation, VMM removal on hibernate, eight concurrent HTTP wakeups
  producing exactly one VMM, connected PTY wake, SSH wake, corrupted-artifact
  fail-closed behavior, interrupted-hibernation startup recovery, and durable
  egress restoration. It rejected a stale revision, changed policy while no
  VMM or TAP existed, resumed through HTTP, and completed real guest HTTPS
  through the restored allow rule.
- With real TAP/nftables enabled, a public share request woke an Ubuntu guest and
  reached its guest HTTP service. This is focused public-share evidence, not a
  substitute for private-token and full share regression suites.
- The current c8i orchestrator workspace passed all 390 Linux taritd tests,
  20 SQLite store tests, 13 public-type contract tests, 10 VMM-client tests,
  10 fleet tests, and the 8 benchmark/report tests. The matching macOS workspace
  passed its 365 taritd tests and every supporting crate. Peer security coverage
  included a real TLS
  listener request with a trusted client, rejection without or with an
  untrusted client certificate, old/new CA overlap followed by rejection of the
  old certificate, private-key permission enforcement, public/internal route
  separation, and stale source/target boot-session rejection.
- A two-process c8i acceptance gate used isolated listeners and a temporary real
  PostgreSQL database. It proved that a correctly signed request with the
  registered source certificate reaches authorization/routing, while no
  certificate, a rogue CA, a trusted certificate registered to another host,
  a stale source process, and a stale target process all fail closed. It then
  restarted the same source host with a new-CA leaf, observed the atomic
  session/fingerprint replacement, rejected the old leaf, removed the old CA
  from the receiver, and completed an authenticated request with the new leaf.
- After the final source changes, the exact production release binaries were
  rebuilt and the privileged Ubuntu lifecycle gate was rerun with real KVM and
  TAP/nftables. It passed live fork continuity and isolation, opaque public
  snapshot/restore with raw paths rejected, true scale-to-zero, eight-way HTTP
  single-flight wake, PTY wake, SSH wake, public-share wake, corrupt-artifact
  failure, interrupted-hibernation recovery, and durable egress restoration.
  Fork readiness was 4.024 seconds and HTTP resume was 1.525 seconds on the
  final nested two-vCPU run.
- Nested virtualization remains enabled on the c8i worker: `/dev/kvm` is a
  usable character device and the L1 worker CPU exposes VMX. The VMM now masks
  the Intel VMX and AMD SVM CPUID capability bits from every fresh and restored
  guest vCPU while preserving the APIC and TSC-deadline boot features. The
  exact-release Ubuntu gate asserted both the absence of standalone `vmx`/`svm`
  flags in `/proc/cpuinfo` and the absence of `/dev/kvm` inside the customer VM
  after initial boot, routed owner resume, stale-owner same-ID recovery on node
  B, and independent branch restore. The exact KVM-enabled VMM regression and
  full VMM workspace clippy gate also passed on c8i.
- Snapshot publication now writes a private, versioned integrity manifest with
  SHA-256 hashes for every 64 KiB chunk of snapshot metadata, RAM, and the
  optional disk overlay. The trusted snapshot row pins the manifest root. The
  VMM authenticates the manifest and metadata before constructing the guest;
  on each lazy UFFD fault it copies the complete chunk into a stable private
  buffer, verifies that buffer, and supplies the same bytes to `UFFDIO_COPY`.
  This avoids a hash/copy race against a mutable mapping and fails the VMM
  closed on a mismatch rather than exposing corrupted guest memory.
- The adversarial real-KVM gate changed one byte in every 64 KiB RAM chunk, then
  triggered the first lazy fault. The guest did not resume, no VMM remained,
  the public API returned only a generic operation failure, and the private log
  recorded the integrity violation. Public snapshot handles were also verified
  as UUID-only, with a real temporary PostgreSQL locator test proving that host
  paths remain private.
- The exact final c8i VMM workspace passed all enabled tests, including 79
  `vmm-core`, 123 `vmm-devices`, and 24 `vmm-memory-backend` tests. The exact
  current orchestrator workspace passed all 390 Linux `taritd` tests and every
  supporting crate. Two Linux-only vsock defects exposed by release testing
  were fixed: response-spool coverage now stays within the JSON-safe API limit,
  and marker reads install their remaining deadline while preserving an
  unterminated final stdout fragment.
- The image-supply-chain gate used the exact release taritd and VMM binaries.
  It converted and booted Ubuntu 24.04 from an OCI tag while proving that the
  stored source reference was digest-pinned and that manifest, ext4, and agent
  SHA-256 metadata were present. A second gate used cosign 3.1.3 and its
  checksum-verified release binary: unsigned Ubuntu was rejected before an
  output appeared; an actually release-key-signed OCI image was admitted;
  changing to an unrelated trusted key produced a tenant-safe HTTP 422; and
  restoring the release key revalidated the same already-admitted digest.
- Restore route verification now selects exactly one strict `ip route` default
  record while ignoring unrelated serial-console lines. A c8i live fork exposed
  the former whole-output comparison; regression tests accept console
  interleaving but continue to reject stale or duplicate default routes.
- Artifact and branch persistence now has tenant-scoped SQLite and PostgreSQL
  transactions for immutable idempotent publication, branch-head CAS, and
  reference transfer. Physical replicas are tracked separately by private host,
  locator, and failure domain; an available replica must have a verification
  timestamp and exactly match the artifact content, size, and authenticated
  manifest digests. Readiness is recomputed from verified replica and distinct
  failure-domain counts. The real c8i PostgreSQL gate caught and fixed a
  nanosecond-versus-microsecond branch replay mismatch, then passed exact replay,
  cross-tenant denial, stale-CAS denial, and reference release.
- The exact-release two-node c8i artifact gate boots a real Ubuntu 24.04 OCI
  guest on node A, publishes a one-copy `degraded` artifact, and causes branch
  creation to stream RAM, disk, and integrity metadata to node B over mTLS.
  PostgreSQL then reports `ready:2:2`. The receiver uses 0600 `O_NOFOLLOW`
  staging files, enforces declared and authenticated size bounds, verifies the
  complete snapshot before atomic publication, and holds an exact filesystem
  reservation through fsync, rename, and metadata commit. The boot-manifest
  digest independently binds the exact kernel bytes, OCI and injected-agent
  digests, memory/vCPU shape, command line, and rootfs mode. Altering the source
  command line produced a tenant-safe 503 with no target row or staging residue;
  restoring the exact metadata allowed replication. The gate then hibernated
  through node B's public HTTP endpoint, verified two-zone durability before
  VMM/capacity release, resumed through the fenced owner route, terminated node
  A, restored the branch on node B, and reasserted Ubuntu identity inside the
  surviving guest. Fleet readiness now counts only replicas with a verification
  timestamp on healthy hosts with a fresh heartbeat, both during startup and
  periodic reconciliation; after node A disappeared the test observed the
  artifact become `degraded`, restored from node B's verified survivor, and
  confirmed that the state did not falsely return to `ready`. An initial run on
  the undersized 2 GiB test volume exposed
  delayed-allocation ENOSPC at fsync; exact preflight reservation was added and
  the qualifying run used an isolated 3.5 GiB compressed btrfs tmpfs-backed
  volume. The PostgreSQL service affected by that discarded infrastructure run
  was recovered and verified healthy before the qualifying rerun.
- The same exact-release gate then hibernated the Ubuntu VM a second time,
  changed its durable egress policy while no VMM existed, terminated its owner,
  and waited for the 15-second heartbeat fence. A normal HTTP exec sent to the
  survivor transactionally claimed the same VM UUID using the survivor's exact
  current boot session, restored and executed Ubuntu on node B, preserved policy
  revision 2, and removed the fleet hibernation binding. PostgreSQL negative
  tests reject a healthy-owner steal, a foreign tenant, and a stale claimant
  boot session; VM deletion releases any crash-left hibernation artifact
  reference in the same transaction.
- Without an API request or manual copy trigger, the same gate then registered
  node C in a third failure domain. Its background repair worker acquired an
  artifact-scoped lease fenced by host boot session and random token, renewed
  that lease during transfer, and reused the bounded mTLS staging, integrity,
  boot-metadata, disk-reservation, and atomic-publication path to copy the
  survivor's artifact from node B. PostgreSQL returned to `ready:2:2`, node C
  held a verified replica, and no repair lease remained. Concurrent, stale-
  session, wrong-token, and same-zone lease cases are covered by real
  PostgreSQL tests. Candidate selection is deliberately fail-closed when the
  immutable Ubuntu base image, kernel, or agent is not already admitted on the
  target. That fail-closed prerequisite was subsequently replaced by the
  authenticated automatic boot-input distribution described below.
- Cross-node localization now transfers the exact generated ext4 lower and
  kernel through the target-bound, replay-fenced mTLS peer channel when they are
  absent. Artifact boot metadata version 2 binds the generated rootfs SHA-256 in
  addition to the OCI manifest, injected-agent, kernel, shape, command line, and
  rootfs-mode inputs; this closes the gap where the same OCI input could produce
  different ext4 bytes. Receivers reserve disk space, use private no-follow
  staging files, enforce authenticated byte limits, verify full SHA-256 digests,
  publish read-only files atomically, and re-run the target node's provenance
  admission policy. The exact-release c8i gate removed every manual image copy,
  started nodes B and C with empty image registries and `/bin/true` as a
  deliberately wrong kernel, then proved automatic distribution to both nodes,
  Ubuntu boot/identity on B, stale-owner same-ID HTTP wake, independent branch
  restore, and background repair on C. All 390 Linux taritd tests, all supporting
  real-PostgreSQL suites, and strict workspace clippy passed before this gate.
  The database test harness was also fixed to serialize schema initialization
  after c8i exposed a genuine concurrent-DDL deadlock.
- Deliberately launching a non-boot-capable VMM exposed a PostgreSQL incarnation
  fencing defect in failed-boot cleanup: claims normalize chrono nanoseconds to
  PostgreSQL microseconds, but deletion previously compared the unnormalized
  timestamp and could retain a `Creating` lifecycle. Fenced deletion now applies
  the identical normalization without weakening host or creation-time checks.
  The focused real-PostgreSQL c8i test passed valid nanosecond-precision cleanup
  and continued to reject a stale host. A second deliberate VMM boot failure
  returned the expected operation error with no retained fleet ownership, and
  the full exact-release Ubuntu three-node lifecycle gate then passed again.
- The c8i Ubuntu 24.04 OCI gate now snapshots a VM only after revalidating the
  admitted source, generated rootfs, injected agent, and current provenance
  policy. The snapshot publishes an opaque artifact plus a private verified
  replica with authenticated 64 KiB chunk metadata. Through the public API the
  gate created and idempotently replayed a branch without exposing owner, host,
  failure-domain, or locator fields; SQLite retained exactly one reference. It
  restored the artifact through real KVM, reasserted Ubuntu identity in the
  guest, deleted the branch, and observed the reference return to zero. The
  qualifying run used the btrfs reflink volume bind-mounted at a short socket
  path; an initial ext4/root-volume placement correctly failed admission under
  disk pressure and was not counted.
- The qualifying disk fast path used btrfs reflink storage. ext4 correctly
  rejects `FICLONE`; an allocated-extent sparse-copy degraded fallback remains
  open and must not silently become a dense copy.
- Durable running-VM artifact pins are now acquired before a lazy restore can be
  published and are fenced by tenant, logical VM identity, and PostgreSQL-
  normalized incarnation time. Failed boot, terminal deletion, hibernation
  replacement, and cross-node recovery release idempotently. Zero-reference
  logical deletion cascades through parents; each surviving peer then deletes
  only the exact owned replica RAM, integrity, and CoW files before atomically
  releasing local metadata. The real PostgreSQL suite passed all 10 fleet tests.
- The exact release Ubuntu 24.04 three-node c8i gate deleted the final branch
  while a node-B VM was still lazily reading the artifact, observed the durable
  reference remain at one, executed successfully inside that guest, and only
  then deleted the VM. Global metadata, node-B/node-C local artifact and snapshot
  rows, and every exact replica RAM, integrity, and overlay path converged to
  absence. The complete c8i regression passed 390 taritd, 20 store, 13 types,
  and 10 VMM-client tests plus strict workspace clippy. PostgreSQL remained
  healthy, and the isolated btrfs mount/image and test processes were absent
  after cleanup.
- Direct cross-node live fork is now included in that exact-release gate. The
  public request was sent to node B for a running Ubuntu 24.04 OCI guest owned
  by node A. Source lookup and snapshot were bound to A's current boot session;
  node B localized the snapshot, RAM, disk upper, integrity manifest, generated
  ext4 lower, kernel, and agent through the mTLS artifact channel, verified the
  configured `ready:2:2` policy, and restored the child. Guest state written
  immediately before the fork was present on B, VMX/SVM and `/dev/kvm` remained
  hidden, PostgreSQL reported `source=node-a` and `child=node-b`, and deleting
  the child did not affect the source. The remainder of the three-node gate then
  passed hibernate, same-ID stale-owner wake, branch restore, repair, lazy-reader
  pinning, and physical GC. The qualifying taritd SHA-256 was
  `31757b4ecb6c8db5b91e975cb5ffa324fcf323528e006f6b24d6bbfc752c63c2`.
- Provider-neutral persistent block volumes now have tenant-scoped public and
  private records, capability and access-mode negotiation, generation-fenced
  attachments, atomic single-writer enforcement, deletion claims, and a secure
  local-block provider. Tarit opens each 0600 no-follow backing object once;
  only its verified descriptor crosses into the VMM, including a fresh exact
  descriptor replacement on restore. The VMM refuses stale serialized FDs,
  validates regular/block identity and access mode, and accounts the actual
  backing device in cgroup I/O limits. Ordinary snapshot/fork with an external
  data disk fails explicitly until provider clone semantics are selected.
- A privileged c8i gate attached a 64 MiB local-block volume as `/dev/vdb` to an
  unprivileged, PID/network-namespaced, seccomp-confined Ubuntu guest. It wrote
  and synced an ext4 proof, rejected physical deletion while attached, removed
  every VMM at hibernate, woke the same UUID through HTTP in 3,288 ms, and read
  the exact proof after restore before VM and physical-volume deletion. The
  worker retained usable KVM/VMX; the guest lacked VMX/SVM and `/dev/kvm`; the
  jail itself contained no KVM device node because taritd passes a verified
  root-opened KVM descriptor. All 79 Linux KVM `vmm-core` tests, strict Linux
  Clippy, all 390 serialized taritd tests, and post-gate mount/process/database
  cleanup passed. AWS/Azure/NFS/object adapters are not claimed complete.
- The exact-release c8i OCI compatibility gate now converts and verifies seven
  digest-pinned amd64 images sequentially: Ubuntu 24.04, Debian 12 slim, Alpine
  3.20, BusyBox 1.36, Rocky Linux 9 minimal, Fedora Minimal 42, and distroless
  static Debian 12. Every generated 1 GiB ext4 passed read-only admission,
  SHA-256 metadata checks, and offline `e2fsck`; all six shell-capable guests
  booted the injected static agent as PID 1, identified as the expected Linux
  family, executed commands, and lacked guest `/dev/kvm` and VMX/SVM. Alpine
  additionally hibernated to zero VMMs and resumed through an HTTP exec in
  1,108 ms. Distroless booted and passed the agent-level no-op readiness probe,
  while an ordinary shell command returned the intentional status 127.
- That matrix caught and fixed three real compatibility defects: image-provided
  `/sbin/init` previously displaced the agent on Alpine; Fedora's chained
  `/sbin -> usr/sbin -> bin` usrmerge layout was rejected; and readiness
  incorrectly depended on `/bin/sh`. Init replacement is now atomic and
  descriptor-relative, only exact safe in-root usrmerge targets are accepted,
  and an empty readiness command is answered inside the agent before any shell
  fork. Ten hostile-path/OCI injection tests, all 390 serialized Linux taritd
  tests, and strict Linux all-target Clippy passed. The qualifying hashes were
  VMM `1e5ecd21d341a586aef8067b3566249b1700c8357d03068f8cf5d763ead8f353`,
  taritd `74b5f1f468c2aa1a4d6fb3272b1ff5085551670903e24a8ae00c9795caceec19`,
  and agent `e36edc67a8caa5899e24ff7855ec48f9a64f72aab9eafad989d5b53189276a65`.
  Each case deleted its VM and admitted image before the next; the final audit
  found no VMM or matrix workspace, retained healthy worker KVM/VMX and
  PostgreSQL, removed the temporary short bind mount, and measured only 294,912
  bytes used inside the isolated btrfs fixture after stale-socket cleanup.
- A new real-KVM model-based lifecycle gate runs an independent state machine
  against the public API and, after every operation, reconciles the tenant-safe
  API view with SQLite status, VMM supervisor/child process trees, jail UID and
  KVM-device isolation, hibernation bindings, snapshot/artifact integrity,
  replica state, proof-file persistence, and CoW isolation. Its deterministic
  path covers create, execute, idempotent and invalid pause/suspend/resume,
  paused and running snapshots, rejected differential snapshots, live fork and
  divergent parent/child writes, true hibernation, transparent HTTP wake,
  restore, restored-VM re-hibernation, deletion while still hibernated, and
  repeated deletion. The qualifying c8i run then executed 20 randomized
  transitions for each deterministic seed `7`, `202609`, and `424242` and
  passed `LIFECYCLE_STATE_MACHINE_PASS operations=366` with no modeled VM,
  resident VMM, or hibernation binding left behind.
- The state-machine gate caught and fixed three lifecycle defects rather than
  weakening its model. Invalid operations on an existing hibernated VM returned
  a false 404 because runtime-gate lookup preceded durable-state validation;
  lifecycle validation now occurs before lookup and again under the live gate,
  producing a state conflict without introducing a transition race. A restored
  jailed lazy-CoW upper was passed to the VMM as relative `assets/...`, so a
  later live snapshot was reflinked successfully inside the jail but taritd
  searched its own working directory during ownership handoff; restored guest
  overlay paths are now absolute `/assets/...` and have a regression test.
  Finally, terminal deletion now idempotently removes the local hibernation
  relation, matching the existing fleet transaction that releases its artifact
  reference. The focused restored-VM hibernate/resume and hibernated-delete
  regression passed 56 operations, all 391 serialized Linux taritd tests and
  strict all-target taritd Clippy passed, and the qualifying taritd SHA-256 was
  `b578b8c1d30cbbd49f4f51a4ab6e77a709238ab724148ac5f27992343078642e`.
- The expanded three-VM state-machine matrix then exposed and fixed four
  userfaultfd/balloon correctness failures that narrower happy paths missed.
  A restored child's first balloon operation was killed by seccomp because the
  resident-page guard used `mincore`; the vCPU profile now permits and tests
  both `mincore` and `madvise`. Linux requires pending `UFFD_EVENT_REMOVE`
  events to be drained before `UFFDIO_COPY`, and authenticated chunk prefetch
  can return `EEXIST` when any page in a mixed-residency chunk already exists;
  the handler now defers/retries `EAGAIN` and falls back to copying the actual
  verified faulting page. Finally, REMOVE is not itself treated as a balloon
  zero: explicit balloon discard is pre-marked zero-on-refault, whereas
  scale-to-zero eviction rehydrates from the new suspend image. A focused c8i
  resident-page -> balloon discard -> REMOVE -> zero-refault regression passes,
  as do all boot-enabled VMM workspace tests and strict all-target Clippy.
  Deterministic lifecycle/concurrency/single-flight gates pass, the exact
  formerly failing seed `42` passes through step 32, and all four isolated
  40-step deterministic seeds complete with three-VM capacity (`42`: 452
  operations, `1337`: 463, `202609`: 424, `424242`: 437). An earlier combined
  run correctly hit the 8 GiB fixture's projected-space guard during a later
  hibernate; clean-baseline runs keep that capacity rejection separate from the
  success matrix.
- The current c8i candidate passes the expanded virtio-balloon gate across four
  OCI/kernel cases: Ubuntu 24.04 and Alpine 3.20 on Linux 6.6.155 and 5.10.230.
  Each case completed 20 live-snapshot/lazy-restore/inflate/deflate cycles (80
  restores total), retained its guest workload digest, scanned guest output for
  stalls/panics/virtio failures, and ran inside a cgroup v2 memory boundary.
  Measured VMM RSS reclaim was 16,028/15,152 KiB for Ubuntu/Alpine on 6.6 and
  13,688/12,816 KiB on 5.10. Every guest had a working balloon driver but no `/dev/kvm`
  or VMX/SVM, while worker KVM remained available. The soak found and fixed a
  lost interrupt window between the balloon configuration and first 256-page
  used-ring completion: balloon now uses an active-high level GSI with KVM EOI
  resampling and reassertion. It also found that draining serial output replaced
  its preallocated buffer with a zero-capacity vector, allowing a later allocator
  `openat` to kill the seccomp-confined vCPU; drains now install a fresh bounded
  buffer before returning the old bytes. A separate state-machine gate then
  passed 40 fresh create/readiness/exec/security/stop cycles (10 per OCI/kernel
  case) without an abnormal vCPU exit or a new host seccomp audit event. The
  exact boot-enabled VMM SHA-256 for the file-backed live-snapshot candidate is
  `275f24d02d2790092c371d503b260467ab0d93ed20c8b3dc257abefacab2b8e3`.
- Live snapshot no longer allocates a second guest-sized anonymous RAM buffer.
  Bounded pre-copy rounds write pages directly into a private sparse staging
  file; the final dirty pages and device state share one paused cut, and every
  dirty bit consumed before an error is replayed into the source tracker before
  the source resumes. Snapshot assembly streams through a 4 MiB buffer, retains
  the existing VMSN layout, computes and patches the RAM CRC before atomic
  publication, and removes the raw staging file on both success and failure.
  The four-way 20-cycle OCI/kernel gate then passed under a 768 MiB cgroup limit
  with zero OOM kills: peak usage was 756,740,096 bytes (Ubuntu 6.6),
  750,739,456 (Alpine 6.6), 753,000,448 (Ubuntu 5.10), and 748,806,144
  (Alpine 5.10). All four cases also passed a one-cycle 640 MiB pressure run,
  reaching the limit without an OOM, and a post-error-guard two-cycle run passed
  at 768 MiB with peaks no higher than 757,813,248 bytes. This closes the
  measured full-buffer blocker for 512 MiB guests; larger-memory, high-dirty-rate,
  and phase-injected failure distributions remain required before claiming
  size-independent live-snapshot performance.
- Live artifact publication no longer rereads the complete RAM image to build
  the authenticated 64 KiB chunk manifest. The VMM computes SHA-256 chunk
  hashes while it is already streaming the finalized VMSN RAM payload, writes a
  private ownership-tracked sidecar, and returns that sidecar through the same
  identity-checked scratch protocol as RAM and the disk upper. Taritd requires
  the exact sidecar, bounds its length from the VMSN layout before allocation,
  decodes its fixed format, rehashes the snapshot metadata, verifies RAM and
  metadata lengths, and only then adopts the RAM hashes; the disk upper remains
  independently hashed. Unit coverage asserts that this path performs zero
  orchestrator RAM hash passes and rejects a metadata mismatch. The public
  live-fork gate now fails unless taritd records adoption of this path.
  The exact release binaries passed Ubuntu 24.04 and Alpine 3.20 on Linux
  6.6.155 and 5.10.230 through atomic RAM/disk fork, opaque restore,
  hibernation, eight-way HTTP single-flight wake, PTY wake, SSH wake, corrupted
  artifact rejection, and interrupted-hibernation recovery. Fork-to-ready was
  4,907/4,863 ms on 6.6 and 4,863/4,834 ms on 5.10 for Ubuntu/Alpine; HTTP wake
  was 1,144/1,114 ms on 6.6 and 1,127/1,108 ms on 5.10. A final direct-VMM
  pressure smoke also passed all four cases under a 768 MiB cgroup limit with
  zero OOM kills and peaks of 756,871,168, 749,522,944, 754,229,248, and
  748,957,696 bytes respectively. The direct caller requires and removes the
  transferred sidecar; post-run inspection found no VMM runtime files or
  cgroups.
  The qualifying taritd SHA-256 was
  `53b40895cd2b51b0c5dd27247e1c3de444ae39985717db13aed1c8983a5dd319`.
- Larger-memory/high-dirty qualification found and fixed four additional
  live-fork defects. The x86 E820 map incorrectly reserved RAM above 256 MiB
  even though the virtio MMIO aperture starts at 3.25 GiB; guests now see all
  configured RAM below that aperture. An intermediate guard rejected sizes
  above 3,328 MiB instead of silently losing memory; the split-slot work below
  subsequently removed that temporary ceiling.
  Pre-copy now reports `converged`, `diverging`, `timeout`, or `max_rounds`
  explicitly through the VMM API, and it will not start a dirty round projected
  to exceed the remaining background budget; already-consumed dirty bits are
  carried into the final residual. A Rust owned-fd drop on the vCPU thread also
  exposed a missing seccomp rule: only `fcntl(F_GETFD)` is now admitted.
  Finally, live publication block-aligns the RAM extent and range-reflinks it
  from the private stage on CoW filesystems, Btrfs stages are created NOCOW
  before allocation, and adjacent dirty PFNs are copied as runs rather than
  one 4 KiB write per page.
  The c8i 1 GiB high-dirty gate passed Ubuntu and Alpine on Linux 6.6.155 and
  5.10.230 with explicit timeout/divergence outcomes, source liveness, and lazy
  restored-data verification. A 1,536 MiB Ubuntu/Linux 6.6 run then passed with
  994,401 pages copied, 200,274 final dirty pages (820,322,304 bytes), 648.5 ms
  blackout, and 30.44 s pre-copy elapsed. Deliberately staging that workload on
  the loop-backed Btrfs fixture first reproduced a duplicate-publication
  `ENOSPC`, then a 209 s CoW residual stall; NOCOW plus range reflink removed the
  duplicate allocation and reduced that device-limited path to 6.0 s, while the
  ext4 staging qualification met the under-2-second gate. These failed runs are
  retained as storage-placement evidence, not counted as passes.
  A test-only, production-disabled failpoint gate now interrupts live snapshot
  at dirty-log activation, bulk copy, a dirty round, final pause, state capture,
  pre-copy completion, snapshot write, snapshot publication, integrity write,
  and integrity publication. Every phase resumed the authoritative source and
  left both the runtime and overlay artifact counts unchanged. All ten phases
  passed with Ubuntu 24.04/Linux 6.6.155 and Alpine 3.20/Linux 5.10.230.
  Guest memory now retains one contiguous packed host/snapshot mapping while
  KVM receives a low slot below the 3.25 GiB MMIO aperture and a high slot at
  4 GiB. Dirty-log PFNs from both slots are translated back into packed
  snapshot offsets, and guest-physical DMA/balloon ranges are translated in
  the opposite direction. The E820 map advertises the complete aperture and
  relocates every configured byte above it rather than consuming RAM. A 4 GiB
  live-snapshot/lazy-restore matrix passed Ubuntu 24.04 and Alpine 3.20 on
  Linux 6.6.155 and 5.10.230. Every guest exposed the 768 MiB high slot as
  `System RAM` at `0x100000000-0x12fffffff`, retained an in-memory proof across
  lazy restore, and remained command-ready. Final-stop downtime was 17.7/15.3
  ms on 6.6 and 25.3/15.2 ms on 5.10 for Ubuntu/Alpine.
  The exact production release binaries subsequently passed the full public
  lifecycle gate for Ubuntu 24.04 and Alpine 3.20 on both kernels.
  Fork-to-ready measured 4,833/4,845 ms on 6.6 and 4,814/4,806 ms on 5.10;
  HTTP wake measured 1,093/1,093 ms and 1,099/1,082 ms. Each case covered
  atomic fork isolation,
  opaque restore, zero-resident-VMM hibernation, eight-way HTTP single-flight,
  PTY and SSH activation, corruption rejection, and interrupted-hibernation
  recovery. All VMM workspace tests (99 core tests), 397 serialized taritd
  tests, 30 protocol tests, and strict all-target Clippy passed. Qualifying
  SHA-256 values are VMM
  `a13f8d7e70112739a610432bb73ef3cd69eb2e5113ba6ec36b87b179bbd96528`
  and taritd
  `f1955ca1f2a07b576c47960d6e8d967984c35e726bf801cee31ffbb83203c746`.
  Orchestrator/process-death and lost-acknowledgement failure injection,
  source/output alias and swap negatives, cross-build/template compatibility,
  and 100-sample latency distributions remain open.
- The persistent local-block gate now passes a four-way OCI/kernel matrix:
  Ubuntu and Alpine-derived workloads on Linux 6.6 and 5.10. Each case creates
  a 64 MiB raw volume through the public API, observes it as `/dev/vdb`, formats
  and mounts ext4, fsyncs a proof, rejects deletion while attached, hibernates
  to zero resident VMMs, wakes through HTTP, verifies the proof, and deletes the
  VM and physical volume. Measured HTTP wake times were 1,378 ms and 1,308 ms
  for Ubuntu 6.6/5.10, and 1,050 ms and 1,047 ms for Alpine 6.6/5.10. The Alpine
  workload is the digest-pinned `alpine:3.20` OCI image with `e2fsprogs` added,
  because provider-created block volumes are intentionally raw just like cloud
  block devices.
- That mixed-image volume run found an OCI PID-1 defect: when the kernel had
  already mounted devtmpfs, the guest agent's second devtmpfs mount failed and
  its fallback over-mounted `/dev` with an empty tmpfs. Kernel logs registered
  `vda` and `vdb`, but neither node was visible to Alpine. The initial repair
  avoided the over-mount, but a later fresh Ubuntu image exposed that statfs
  cannot distinguish devtmpfs from an unpopulated ordinary tmpfs because both
  report `TMPFS_MAGIC`. PID-1 setup now creates any missing block nodes from the
  kernel-owned `/sys/class/block/*/dev` identities without exposing host devices.
  The OCI compatibility gate requires `/dev/vda` to be a block device. Fresh
  Tarit OCI ingestion with the current agent SHA-256
  `60d16845381469215c853517761570c0389271c76df1f14a6b62f4c7aee93213`
  passed Alpine boot, exec, live fork, hibernate, and HTTP wake on Linux 6.6 and
  5.10; live-fork readiness was 4,686/4,791 ms and HTTP wake was 1,752/1,766 ms.
  The subsequent full seven-image matrix passed independently on both kernels:
  Ubuntu 24.04, Debian 12 slim, Alpine 3.20, BusyBox 1.36, Rocky Linux 9,
  Fedora Minimal 42, and non-root distroless Debian 12. All six shell images
  retained a visible block root device and passed exec/security checks;
  distroless passed agent readiness and the intentional no-shell result.
- The model-based lifecycle gate also passed against a freshly ingested Alpine
  3.20 OCI rootfs on Linux 6.6 and 5.10. Each run reconciled 689 API, database,
  process, jail, artifact, proof-file, and CoW invariants across the deterministic
  lifecycle table, concurrent create/snapshot/delete, concurrent live-fork
  fan-out, scale-to-zero capacity release, single-flight wake, balloon changes,
  and 20 transitions for each seed `7`, `202609`, and `424242`. A first 5.10 run
  correctly failed closed on `ENOSPC` late in the last seed: deleted btrfs extents
  had not yet been discarded from the sparse loop image, filling the 50 GiB host
  root even though `/t` had little live data. After an explicit filesystem commit
  and trim, the clean-baseline rerun passed. CoW-heavy harness cleanup now syncs
  the filesystem before trimming so sequential kernel/image matrices do not
  inherit that false capacity pressure.
- The full ingress/activation gate now also passes with the Alpine 3.20 OCI
  guest on both kernels. Authenticated SSH PTY control works before fork; the
  live child preserves the atomic RAM/disk boundary and diverges independently;
  public restore accepts only an opaque handle; hibernate leaves zero VMMs;
  eight concurrent HTTP callers single-flight into one restored VMM; PTY and
  SSH each wake a hibernated guest; corrupt artifacts fail closed; and an
  interrupted hibernation is reconciled after taritd restart. Linux 6.6 measured
  3,191 ms fork-to-ready and 1,072 ms HTTP wake; Linux 5.10 measured 3,172 ms and
  1,079 ms. Share ingress is not claimed for these two Alpine runs because an
  unrelated host process owns the shared nftables tables; its earlier dedicated
  gate remains separate.
- Explicit live-fork ids are now durable operation identities rather than only
  caller-selected VM ids. SQLite and PostgreSQL persist the tenant, source VM,
  source host, target host, `preparing`/`committed` phase, and exact child
  `created_at` fence. VM-id reservations now protect unlimited tenants as well
  as quota-limited tenants. A committed retry returns HTTP 200 with the same
  child; another source or tenant cannot claim it. A production-disabled
  failpoint paused after the repaired child was durable and before operation
  commit. The c8i gate killed taritd with `SIGKILL`, restarted it, deleted the
  source, replayed the request, verified the child proof and incarnation fence,
  and rejected source confusion on Ubuntu and Alpine with Linux 6.6.155 and
  5.10.230. The first run found that restart reconciliation recomputed an
  unsuffixed restore-overlay name and killed a valid child whose persisted name
  contained the snapshot digest. Reconciliation now accepts only the exact
  persisted overlay when it remains inside the VM's expected private directory
  and has the strict digest-suffixed name. All four crash lanes then passed.
  The exact production taritd SHA-256
  `b327918b8f9dbf7f9787e418c34b0dbbe032509bfb0abb92b0074b9cae9e1fc7`
  passed fresh Ubuntu 24.04 and Alpine 3.20 OCI ingestion, live fork, immediate
  idempotent replay, wrong-source rejection, private disk isolation,
  hibernate, and HTTP wake on both kernels. Fork-to-ready was 6,420/6,359 ms on
  6.6 and 6,362/6,348 ms on 5.10; HTTP wake was 1,806/1,906 ms and
  1,792/1,777 ms. The 399-test taritd suite, all workspace tests, and strict
  all-target/all-feature Clippy pass. The same source-bound claim/resume/commit,
  wrong-source rejection, and terminal id-reuse fence also passed against a
  disposable real PostgreSQL database on c8i. The PostgreSQL+mTLS cross-node
  gate now kills node B after the child is durable and running but before the
  fork operation commits or acknowledges the caller. After restart, the exact
  retry returns HTTP 200 with the same child, an immediate replay remains
  idempotent, a different source receives HTTP 409, and the durable
  source/host/target binding and child-incarnation fence remain intact. The
  test then completes stale-owner wake, shared-volume recovery, independent
  branch restore, replica repair, and physical GC. This passed with the Ubuntu
  24.04 OCI fixture on Linux 6.6.155 and 5.10.230. The test-failpoint taritd
  SHA-256 was
  `76fef90dd6037b4e7c05f14458e1e5520e5216fcf8d3e40a1b674a84fa158777`;
  the VMM SHA-256 was
  `a13f8d7e70112739a610432bb73ef3cd69eb2e5113ba6ec36b87b179bbd96528`.
  Physical multi-host failure and additional transfer/publication phase
  injection remain open.
- A bounded mixed-OCI soak gate now runs isolated longer state-machine rounds
  across explicit kernel/rootfs cases, rotates deterministic seeds, retains the
  exact failing log, rejects leaked candidate VMM/taritd processes or lifecycle
  mounts, commits and trims CoW storage between rounds, and fails on free-space
  drift above 128 MiB. Its first c8i qualification completed four 40-step rounds:
  Ubuntu and Alpine on Linux 6.6 and 5.10 using seeds `7`, `202609`, `424242`,
  and `7`. Every inner lifecycle gate emitted its exact pass sentinel, and free
  space returned to 8,313,483,264 bytes after every round. This is bounded soak
  evidence, not yet the required long-duration or destructive chaos campaign.
- A bounded runtime-crash gate now covers fresh digest-pinned Ubuntu 24.04 and
  Alpine 3.20 OCI guests on Linux 6.6 and 5.10. It kills taritd without a drain,
  requires exact VMM PID and `/proc` start-time re-adoption with an unchanged
  jailed control-runtime process set, checks guest data and hidden VMX/SVM state,
  hibernates to zero VMMs, wakes through HTTP, then kills the resumed VMM and
  requires an `error` record plus released scheduler capacity before creating a
  replacement. The first qualifying run exposed terminal records that retained
  a dead PID, jail, overlay, and socket after successful cleanup. Terminal
  persistence now clears that runtime ownership; focused reconciliation tests
  and all four real-KVM OCI/kernel cases pass with no remaining candidate
  process, mount, or CoW-capacity leak. The resulting c8i workspace passed all
  393 serialized taritd tests and strict all-workspace, all-target Clippy. This
  is targeted transition-boundary crash evidence, not the remaining
  long-duration peer/transfer/GC, database, network, corruption, or
  resource-exhaustion chaos campaign.
- Generic NFSv4.1-backed raw block volumes are now wired through the public
  volume API, provider-neutral placement, VM attachment, hibernation, resume,
  and deletion paths. Tarit mounts the export only long enough to open and
  validate a private 0600 raw file, lazy-detaches that exact provider mount when
  the live descriptor makes an ordinary detach busy, and passes only the
  descriptor to the VMM. Endpoint, export, mount path, and credentials remain
  host-private. A disposable c8i provider gate passed exact mount validation,
  fsync durability across a deliberate NFS-server interruption and reconnect,
  busy-detach handling, durable block reopen, and cleanup with no live NFS mount.
- The public lifecycle gate passed eight fresh OCI/kernel/provider cases on the
  c8i: local block and NFS-backed block, each with digest-pinned Ubuntu 24.04 and
  Alpine 3.20 on Linux 6.6 and 5.10. Ubuntu formatted ext4, wrote and synced a
  proof, hibernated to zero VMMs, woke through HTTP, and reread the file. The
  unmodified Alpine base lacks filesystem formatting tools, so it used fsynced
  raw block I/O and verified the exact bytes after wake. Every case rejected
  deletion while attached, hid VMX/SVM, `/dev/kvm`, NFS mounts, and provider
  configuration from the guest, then deleted both VM and physical volume without
  a residual provider mount. Shared-volume HTTP wake measured 1,117/1,087 ms for
  Ubuntu 6.6/5.10 and 249/225 ms for Alpine 6.6/5.10 in the qualifying matrix.
  The complete workspace passed, including 396 serialized taritd tests and 13
  tarit-volume tests; strict workspace/all-target Clippy and the disposable NFS
  reconnect gate also pass. The qualifying hashes were taritd
  `74c2e42fa3281b86b6c12a006c38600157ebf753ee91f679c7e0c461cb0d3d7b`,
  VMM `244f2be7cf2e0803d258b991ed06d6be03a3c989778a0ff016d02dbedc5f89f1`,
  and agent `60d16845381469215c853517761570c0389271c76df1f14a6b62f4c7aee93213`.
  A separate two-worker Postgres+mTLS gate passed on both Linux 6.6 and 5.10:
  node A created and fsynced an ext4 shared-volume workload, hibernated it to
  zero, and was terminated. After its heartbeat became stale, HTTP exec on node
  B fenced the old owner, localized and authenticated the hibernation artifact,
  recovered the fleet attachment into node B's local store, reopened the shared
  block descriptor, and read the original proof under the same VM ID. Deleting
  the VM and volume through node B removed the shared backing object. The same
  gate retained its existing live cross-node fork, corrupted-boot-metadata,
  replica-repair, egress recovery, reference, and physical-GC checks. This is a
  same-c8i logical-worker loss test with separate stores, peer listeners, mTLS
  identities, zones, and process lifetimes; it is not evidence of a physical
  host or managed-service outage. The qualifying taritd SHA-256 was
  `fd8fa866414c9a5763580c4907c942727223655a449de52e1c86594e650dc48b`.
  `orch/tests/e2e_oci_kernel_volume_matrix.sh` is the durable release gate for
  these eight combinations; a single successful distribution, kernel, or
  provider case cannot satisfy it.
  Real AWS EFS and Azure Files qualification, physical multi-host outage tests,
  and backend-native volume snapshot/clone support remain open.
- The current c8i storage audit does not show the assumed 200 GiB device. It
  exposes one 50 GiB EBS NVMe disk with a 49 GiB root partition and no second
  NVMe block device; `/t` is a 12 GiB Btrfs loop image backed by
  `/home/ubuntu/tarit-sept-reflink.btrfs` on that same root disk. Rust
  incremental objects and stale generated 1 GiB rootfs fixtures explained the
  immediate root pressure; only unmounted, unopened generated fixtures and the
  incremental cache were removed, restoring 6 GiB of headroom before the
  larger-memory gate. The loop-backed Btrfs device is suitable for reflink and
  ENOSPC correctness but not a qualifying low-latency RAM-stage device. A real
  attached reflink-capable SSD remains desirable for aggregate soak and
  performance qualification.

These are functional measurements, not p50/p95/p99 release distributions. The
current publisher no longer performs a second size-proportional RAM hash pass,
but disk-upper integrity work, replication, and high-dirty-rate behavior can
still scale with touched content, so the full fork path has not yet proven the
roadmap's size-independent performance requirement. These results close
reference-counted logical and physical replica deletion, direct cross-node fork,
and the provider-neutral local/NFS-backed block volume foundation. They do not
close SDK generation/integration, managed EFS/Azure volume qualification,
Sandbox decoupling, OCI
container-config semantics beyond the declared PID-1-agent contract, soak/chaos,
performance distributions, or any other remaining release row.

Before any test deployment:

1. inventory old build/snapshot directories and agree on cleanup or volume
   expansion; do not delete shared test data implicitly;
2. reserve the host with the existing global test lock and verify no unrelated
   VMM, TAP, nftables, or taritd state;
3. sync an exact reviewed source revision into a new timestamped directory;
4. run unit/contract tests before privileged KVM suites;
5. run focused feature suites, then the regression matrix; and
6. copy structured results, logs, commit ID, host shape, kernel, and configuration
   off-host before cleanup.

Use dedicated metal or a protected KVM runner for the 100-sample and contention
performance gates. Privileged persistent-runner execution remains restricted to
protected `main`; pull-request code must use ephemeral or explicitly isolated
validation infrastructure.

## Definition of done

September scope is complete only when:

- fork/branch, egress-policy, hibernate/resume, Python, and TypeScript public
  contracts are documented and compatibility-tested;
- real-KVM tests prove fork consistency/isolation and hibernation capacity
  release/recovery;
- same-hardware performance gates prove the qualifying fork/snapshot path is
  lazy and size-independent before first touch, meets its approved phase budgets,
  and reports every fallback or convergence failure;
- SDK integration tests exercise a real taritd instance and tenant authorization
  boundaries;
- all seven historical stop-ship items have current passing evidence, with the
  remaining partial aspects implemented rather than waived;
- all required CI, KVM, soak, and hardware-appropriate performance gates pass;
- operational rollback and artifact/certificate/key rotation procedures are
  documented and tested; and
- `PRODUCTION_READINESS.md` and `CHANGELOG.md` are updated from the resulting
  evidence, not from implementation intent.
