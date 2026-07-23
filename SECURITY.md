# Security Policy

Tarit runs untrusted guest workloads inside hardware-virtualized microVMs, and
the host is responsible for containing them. We take security reports seriously.

## Supported versions

Tarit is pre-1.0 and under active development. Security fixes land on the `main`
branch. There is no long-term support branch yet.

## Reporting a vulnerability

Please report vulnerabilities privately. Do not open a public issue, pull
request, or discussion for a security problem.

Use GitHub private vulnerability reporting: open the repository **Security** tab
and choose **Report a vulnerability**. This creates a private advisory visible
only to the maintainers.

Please include:

- the affected component (`vmm/`, `orch/`, or `proto/`, plus the crate and file)
  and the version or commit,
- a description of the issue and its impact,
- reproduction steps or a proof of concept if you have one.

We aim to acknowledge reports promptly and will coordinate a fix and a
disclosure timeline with you.

## Threat model

Tarit has two trust boundaries.

**Guest to host (the VMM, `vmm/`).** The guest is untrusted and assumed hostile.
The boundary is crossed via virtio devices (block, net, vsock, serial), MMIO, and
shared guest memory. KVM provides the CPU and memory boundary. On top of that the
host applies defense in depth, in two tiers:

- Enforced on the standard orchestrated path: restrictive Unix-socket
  permissions with `SO_PEERCRED` checks, fail-closed seccomp on vCPU and selected
  I/O worker threads, an nftables egress allowlist for VMs that set an egress
  policy, and per-VM cgroup v2 limits when a parent cgroup is configured. The
  coordinator thread is not seccomp-confined.
- Available as an opt-in hardened mode (`vmm serve --jail`): chroot, privilege
  drop to an unprivileged uid/gid, mount and network namespaces, and cgroup
  placement. This mode fails closed if a required confinement step cannot be
  applied.

The standard `taritd` launch path does not yet stage and start every VMM through
that jail. Production mode therefore remains disabled for hostile multi-tenant
workloads until unique per-VM uid/gid jails, private staged assets, namespaces,
and mandatory resource controls are wired end to end. KVM and the worker-thread
filters are important boundaries, but they are not a categorical guarantee that
a compromised guest cannot affect the host.

The evidence and remaining release blockers are tracked in
[PRODUCTION_READINESS.md](PRODUCTION_READINESS.md).

Snapshot and migration streams are treated as untrusted input when they can
originate off-node. Raw snapshot restore validates the current CRC-based format,
which detects corruption but is not cryptographic authenticity, and full restore
currently scans the memory payload before arming lazy paging. Production use of
off-node artifacts therefore requires trusted storage or an external
authenticated artifact layer. End-to-end fs-verity sealing, trusted measurement
metadata, and restore propagation are still required before that scan can be
safely skipped. Networked restore is not production-ready until the resumed
guest also rebinds its IP, routes, gateway, and DNS; replacing only the host TAP
is insufficient.

**Client to control plane (the orchestrator, `orch/`).** `taritd` exposes an HTTP
API and drives one VMM per microVM over a Unix-domain socket. It is responsible
for tenant isolation, API-key authentication, per-tenant network and egress
policy, and resource accounting across a fleet.

Reports about the guest-to-host boundary (seccomp or jailer escapes, egress
bypass, virtio parsing bugs) and cross-tenant isolation failures in the control
plane are the highest priority.
