# Support

This document explains where to get help with Tarit and what to expect.

## Read the docs first

Most questions about building, running, and integrating Tarit are answered in
the docs:

- Project overview and quickstart: [README.md](README.md)
- The VMM on its own: [vmm/README.md](vmm/README.md),
  [vmm/docs/STANDALONE.md](vmm/docs/STANDALONE.md)
- Driving the VMM from your own control plane:
  [vmm/docs/INTEGRATION.md](vmm/docs/INTEGRATION.md)
- Orchestrator quickstart, config, and API:
  [orch/docs/QUICKSTART.md](orch/docs/QUICKSTART.md),
  [orch/docs/CONFIGURATION.md](orch/docs/CONFIGURATION.md),
  [orch/docs/API.md](orch/docs/API.md)

## Asking questions

For open-ended questions and ideas ("how do I", "would this design work"),
use GitHub Discussions on the repository if it is enabled. Otherwise open an
issue and say it is a question.

## Reporting bugs and requesting features

Open a GitHub issue using the bug report or feature request template. For
bugs, include your host details (distro, kernel, `/dev/kvm` availability),
the component (`vmm/`, `orch/`, or `proto/`), the version or commit, and
reproduction steps. Logs from `vmm serve` or `taritd serve` help a lot.

## Security issues

Do not open a public issue for a vulnerability. Report it privately through
GitHub Security Advisories as described in [SECURITY.md](SECURITY.md).

## Scope and expectations

Tarit is pre-1.0 and under active development. Support is best-effort by the
maintainers (see [MAINTAINERS.md](MAINTAINERS.md)); there is no SLA and no
long-term support branch. Fixes land on `main`. Running microVMs requires an
x86_64 Linux host with KVM; that platform gets the most attention.
