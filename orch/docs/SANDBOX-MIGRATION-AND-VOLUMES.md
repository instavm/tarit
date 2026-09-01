# Sandbox migration and persistent storage

## Dependency direction

Tarit is the general-purpose, AGPL VMM and orchestrator. It owns KVM/VMM
lifecycle, image admission, networking, scheduling, fork/snapshot/hibernate,
artifact replication, storage attachment, security enforcement, and the public
API/SDK contract. Tarit must build and test without any InstaVM or Sandbox source
tree, package, database model, billing concept, or credential.

InstaVM Sandbox is a consumer of that contract. It may keep user/session CRUD,
authentication, billing, product quotas, templates, browser/agent workflows,
and product-specific policy. It must not spawn Firecracker, manage TAPs or
cgroups, open UFFD, create overlays, distribute snapshots, run a second warm
pool, or maintain an independent VM lifecycle state machine.

The migration target is therefore:

```text
InstaVM HTTP/SSH/product CRUD
        |
        | generated Tarit SDK (tenant credential + idempotency key)
        v
Tarit public API
        |
        +-- lifecycle / live fork / hibernate / wake
        +-- images and immutable artifacts
        +-- egress, PTY, SSH, shares
        +-- provider-neutral persistent volumes
        v
Tarit VMM
```

The current Sandbox checkout contains about 50,000 lines under
`app/domain/firecracker` and `infrastructure/instavm-vmd`. Those implementations
are migration inputs and compatibility fixtures, not libraries Tarit should
import. During migration, each Sandbox endpoint should become CRUD plus one
Tarit SDK call; dual lifecycle implementations must not remain as a fallback
after a route is cut over.

## Storage model

One backend cannot truthfully cover every persistent-storage workload. Tarit
uses explicit capabilities and exposes unsupported combinations before a VM is
mutated.

| Class | Initial implementations | Best use | Important semantics |
| --- | --- | --- | --- |
| Block | local sparse file, raw device, AWS EBS, Azure Disk | databases, durable workspace disks | filesystem chosen by guest; single-writer by default; zone constrained; flush/FUA required |
| Shared filesystem | NFSv4, AWS EFS, Azure Files | multi-node/shared workspaces | backend supplies POSIX semantics; reconnect and stale-handle behavior are explicit; credentials stay outside the guest where possible |
| Object | S3-compatible, Azure Blob | immutable artifacts, checkpoints, large blobs | never advertised as a normal POSIX disk; optional FUSE/cache adapters are a separately named degraded class |

JuiceFS backed by object storage plus durable metadata can implement a useful
shared-filesystem provider, but it is not the block-volume abstraction. Its
Redis/SQL metadata durability, object consistency, cache behavior, and NFS
bridge failure modes must be operated as part of that provider.

## Provider contract

Every provider declares:

- storage class (`block`, `filesystem`, or `object`);
- read-only/read-write and single-/multi-writer modes;
- snapshot and clone capabilities;
- durability and failure-domain constraints;
- whether attachment is a host block path, a safely opened descriptor, or a
  guest filesystem mount specification;
- whether hibernate, cross-node resume, fork, and live migration are supported.

Public records contain an opaque `volume_id`, desired size, capabilities,
revision, and attachment state. Provider IDs, device paths, endpoints, mount
credentials, and cloud account details remain private.

An attachment is tenant scoped, idempotent, generation fenced, and durable
before provider mutation. The controller reconciles `attaching`, `attached`,
`detaching`, and `error` states after crashes. A writable single-writer lease is
fenced by VM incarnation and host boot session. Delete refuses live attachments;
force deletion is not a public shortcut.

The implemented foundation currently includes the provider-neutral types,
durable volume/attachment tables, generation and single-writer fencing, atomic
delete claims, local-block provider, public CRUD, and FD-only VMM attachment.
Local-block storage is deliberately host pinned. It may hibernate and resume on
the same host, but it cannot recover on another node. AWS/Azure block, NFS/shared
filesystem, and object/checkpoint adapters remain separate qualifying backends;
the existence of the local provider must never be interpreted as those backends
being complete.

The provider library now also contains three deliberately separate adapter
foundations:

- `AttachedBlockProvider` prepares an already attached AWS EBS, Azure Disk, or
  raw Linux block device. Every preparation reopens the device, checks block
  type, exact byte size, attachment generation, and major/minor identity. AWS
  EBS additionally binds the Nitro NVMe serial to the expected `vol-*` identity;
  Azure Disk accepts only `/dev/disk/azure/data/by-lun/<lun>`, never unstable
  `sdX` or `nvmeX` enumeration. Cloud creation, attach/detach, and deletion are
  intentionally unsupported until the corresponding durable control-plane
  reconciler is configured.
- `NfsProvider` produces a validated, credential-free NFSv4.1 mount intent for
  generic NFS, AWS EFS, or Azure Files. It forces hard mounts plus explicit
  timeout/retransmission and `nosuid,nodev,noexec` policy, rejects endpoint,
  export, and option injection, and declares shared-writer/cross-node lifecycle
  capabilities. Generic NFS-backed raw block volumes are connected to the
  public volume lifecycle. Tarit mounts them only long enough to open the exact
  backing object, removes the host mount path, and passes the verified descriptor
  to the VMM. `TARIT_SHARED_BLOCK_SECURITY` selects the NFS security flavor;
  production requires `krb5p`. Admission verifies the kernel-reported NFS
  version, TCP and hard-mount policy, and security flavor. AWS EFS and Azure
  Files remain library profiles until their TLS mount helpers and real-service
  cleanup gates are integrated.
- `ImmutableObjectProvider` has only content-addressed put-if-absent,
  verified-get, and exact-delete operations. The local implementation uses
  private immutable files and rehashes content on every read. There is no mount,
  append, random-write, rename, or lock API, making it impossible to silently
  advertise object storage as POSIX storage. Remote transports use conditional
  publication, bounded streaming, and size plus SHA-256 verification. Runtime
  objects are isolated by tenant and artifact; immutable boot objects use a
  tenant-scoped cache. Reference-safe collection fences concurrent publication
  and never reclaims shared boot objects or legacy bundles.

For descriptor-backed block volumes, taritd opens and validates the private
provider path once with no-follow and exact owner/mode/size checks. The VMM gets
only an inherited descriptor plus an opaque diagnostic volume UUID. Hibernate
restore supplies fresh descriptors and verifies exact identity/access; a stale
numeric descriptor serialized in a snapshot is rejected. The customer guest
does not receive provider paths, cloud credentials, or `/dev/kvm`.

## Lifecycle ordering

1. Create records desired attachment and obtains the writer/placement fence.
2. Provider preparation completes and is verified before VMM publication.
3. A block device is attached after the root disk so guest names remain stable;
   filesystem mounts occur through the authenticated guest agent after network
   policy is installed and before readiness succeeds.
4. Hibernate flushes/fences external writable volumes before the coherent VM
   snapshot boundary. The durable attachment intent remains with the logical VM.
5. Resume first selects a host satisfying every volume failure-domain constraint,
   then reattaches the same fenced generation before restoring the VM.
6. VM deletion stops the VMM, detaches providers, releases writer leases, and
   only then commits terminal attachment state. Failures remain retryable and do
   not permit a second writer.

Fork does not clone an external writable volume implicitly. The current runtime
rejects ordinary snapshot/fork restore when external data disks are present
rather than sharing a writable backing object accidentally. The eventual public
operation must choose `none`, `read_only`, or `clone`; `clone` is accepted only
when the provider advertises an atomic snapshot/clone operation and returns a
new volume identity. Hibernate is different: it retains the same logical volume
attachment and replaces the VMM's inherited descriptor on same-host resume.

## Acceptance gates

- Tarit has a dependency-direction test that fails if its manifests or source
  mention Sandbox/InstaVM packages.
- Local-block KVM E2E proves guest `sync` persistence across hibernate/HTTP
  resume, attached-delete refusal, generation/single-writer unit fencing,
  tenant denial, unprivileged FD-only attachment, guest virtualization masking,
  and exact physical cleanup. A host-crash durability gate with explicit
  provider flush evidence remains required before claiming fsync durability.
- NFS E2E proves reconnect, server interruption, busy detach, durable block
  reopen, strict live-mount option verification, and that no cloud or
  metadata-store credential reaches the guest. Production parsing rejects
  generic `AUTH_SYS` and accepts only Kerberos privacy.
- AWS and Azure gates use real disposable volumes, exercise attach timeout/node
  loss/stale generation cleanup, and leave no billable resource behind.
- Object-provider gates cover conditional publication, interrupted transfer,
  checksum rejection, source-loss recovery, legacy-layout restore, and
  reference-safe namespace collection. They also reject requests that imply
  unsupported POSIX, locking, or block semantics.
- Sandbox cutover tests run product CRUD against Tarit's public API with the
  legacy Firecracker/VMD processes disabled and assert equivalent user-visible
  behavior.
