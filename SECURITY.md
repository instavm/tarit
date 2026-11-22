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

- The standard path checks Unix-socket peer credentials, confines vCPU and selected I/O worker threads with seccomp, and enforces configured cgroup and egress policies. The coordinator thread runs outside the worker seccomp filters.
- In production mode, taritd requires a per-VM UID/GID jail, staged assets, cgroups, PID and network namespaces and a signed-image policy. The VMM receives its TAP by descriptor. The standalone VMM also supports a jail. [Orchestrator isolation](orch/docs/ISOLATION.md)

Raw VMM snapshots have corruption checks. The orchestrator additionally authenticates artifacts and verifies RAM chunks during lazy restore. It repairs guest network configuration and waits for guest identity and entropy repair before admitting workload requests. Applications own the rotation of secrets cached in their processes. [Guest repair hook](vmm/guest/agent/README.md#post-fork-repair)

The [release checklist](PRODUCTION_READINESS.md) records the remaining gates for hostile multi-tenant workloads.

**Client to control plane (the orchestrator, `orch/`).** `taritd` exposes an HTTP
API and drives one VMM per microVM over a Unix-domain socket. It is responsible
for tenant isolation, API-key authentication, per-tenant network and egress
policy, and resource accounting across a fleet.

Reports about the guest-to-host boundary (seccomp or jailer escapes, egress
bypass, virtio parsing bugs) and cross-tenant isolation failures in the control
plane are the highest priority.
