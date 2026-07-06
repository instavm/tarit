# Documentation index

Start with the [README quickstart](../README.md#quickstart-run-your-first-microvm)
if you want to build the binary, create a rootfs, start `vmm serve`, and run the
first `exec` command.

## Core docs

- [Build and API/CLI reference](BUILD-AND-API.md): build commands, subcommands,
  the length-prefixed JSON protocol, and request/response examples.
- [PRD](VMM-PRD.md): product requirements, architecture, implementation plan, and
  test strategy.
- [Design choices](DESIGN-CHOICES.md): rationale for major implementation
  choices.
- [Feature status](FEATURE-STATUS.md): current feature coverage and validation
  notes.
- [Standalone usage](STANDALONE.md): running the VMM on its own and bringing your
  own orchestrator.
- [Integration](INTEGRATION.md): how an orchestrator drives the VMM over the
  control socket.
- [Remaining work](remaining_work.md): current backlog and known gaps.
- [PRD gap analysis](PRD-GAP-ANALYSIS.md): requirement-by-requirement status.

## Operations and performance

- [Cold boot exec](cold-boot-exec.md): cold create-to-first-exec measurements
  and kernel tuning notes.
- [Cold boot benchmark](cold-boot-bench.md): boot benchmark details.
- [Bare-metal benchmarks](METAL-BENCHMARKS.md): methodology and results from a
  bare-metal KVM run.
- [Performance analysis](PERF-ANALYSIS.md): performance budget and bottlenecks.
- [Stress test results](stress-test-results.md): stress and soak results.
- [SSH and PTY](ssh-pty.md): interactive PTY protocol notes.
- [OCI boot probe](oci-boot-probe.md): OCI image boot notes.
- [Full boot problem](full-boot-problem.md): historical boot debugging notes.

## Journal and long-form writeups

- [Build journal index](journal.md): chronological milestone journal.
- [Journal posts](journal/): per-milestone notes.
- [Blog itinerary](blog/build-itinerary.md) and
  [long-form build article](blog/building-a-minimal-rust-vmm.md).
