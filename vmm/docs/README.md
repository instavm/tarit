# Documentation index

Start with the [README quickstart](../README.md#quickstart-run-your-first-microvm)
if you want to build the binary, create a rootfs, start `vmm serve`, and run the
first `exec` command.

## Core docs

- [Build and API/CLI reference](BUILD-AND-API.md): build commands, subcommands,
  the length-prefixed JSON protocol, and request/response examples.
- [Design choices](DESIGN-CHOICES.md): rationale for major implementation
  choices.
- [Standalone usage](STANDALONE.md): running the VMM on its own and bringing your
  own orchestrator.
- [Integration](INTEGRATION.md): how an orchestrator drives the VMM over the
  control socket.

## Operations

- [SSH and PTY](ssh-pty.md): interactive PTY protocol notes.
