# tarit-proto

The wire protocol for talking to a Tarit VMM (`vmm serve`) over its Unix domain
socket. This crate is dependency-light (serde only, no KVM) and is the single
source of truth for the types on the wire, so any orchestrator can drive the VMM
without hand-copying types.

`vmm-core` and `vmm-api` (the VMM side) re-export these types, and `taritd` (the
orchestrator) consumes them. If you build your own orchestrator, depend on this
crate.

## What it contains

- `api`: `ApiRequest`, `ApiResponse`, `VmSpec`. The request/response enums sent
  over the socket. One VMM process manages one microVM, so requests carry no VM
  id.
- `config`: `VmConfig` and its parts (`KernelConfig`, `MemoryConfig`,
  `VcpuConfig`, `VolumeConfig`, `NetConfig`, `PortForwardConfig`). The declarative
  config the VMM boots from.
- `state`: `VmState`, `VmStatus`. The lifecycle state and the health snapshot
  returned by the `Status` op.
- `pty`: `PtyStreamFrame`, the `TYPE_*` frame constants, and the read/write
  framing helpers for the interactive PTY stream.

## Framing

- Control plane: each `ApiRequest` and `ApiResponse` is one message framed as
  `[4-byte big-endian length][JSON body]`.
- PTY stream: after an `attach_pty` request, the connection switches to
  `[1-byte type][4-byte big-endian length][payload]` frames, where type is one of
  `TYPE_DATA`, `TYPE_RESIZE` (JSON cols/rows), `TYPE_EXIT` (JSON exit code),
  `TYPE_ERROR`, `TYPE_START`.

## Using it

See `vmm/docs/INTEGRATION.md` for a full bring-your-own-orchestrator guide with a
minimal client example. In brief: spawn `vmm serve --socket <path>`, connect to
the socket, send `ApiRequest::Create(VmSpec { config })` length-prefixed, read
the `ApiResponse`, then drive `exec`, `snapshot`, `stop`, and the rest.

## License

AGPL-3.0-or-later. See the root `LICENSE` file. `tarit-proto` is licensed the
same as the rest of Tarit, so an orchestrator that links this crate and runs a
modified Tarit as a network service inherits the AGPL network-copyleft
obligation.
