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
  permissions with `SO_PEERCRED` checks on the VMM control socket, a seccomp
  allowlist on the VMM I/O worker threads, an nftables egress allowlist applied
  on the host for any VM that sets an egress policy, and per-VM cgroup v2 memory
  and PID limits when a parent cgroup is configured (`TARIT_VM_CGROUP_PARENT`).
- Available as an opt-in hardened mode (`vmm serve --jail`): chroot, privilege
  drop to an unprivileged uid/gid, mount and network namespaces, and cgroup
  placement. This mode fails closed if a required confinement step cannot be
  applied.

Snapshot and migration streams are treated as untrusted input when they can
originate off-node.

**Client to control plane (the orchestrator, `orch/`).** `taritd` exposes an HTTP
API and drives one VMM per microVM over a Unix-domain socket. It is responsible
for tenant isolation, API-key authentication, per-tenant network and egress
policy, and resource accounting across a fleet.

Reports about the guest-to-host boundary (seccomp or jailer escapes, egress
bypass, virtio parsing bugs) and cross-tenant isolation failures in the control
plane are the highest priority.
