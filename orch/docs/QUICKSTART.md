# Quickstart

This is the shortest path from an empty Linux/KVM host to a running microVM you
can exec into and SSH into. For the full option list see
[CONFIGURATION.md](CONFIGURATION.md); for running and troubleshooting a fleet see
[OPERATIONS.md](OPERATIONS.md).

## Prerequisites

- A Linux host with KVM (`/dev/kvm` present). macOS can build and cross-check but
  cannot run microVMs.
- Rust stable toolchain.
- A guest kernel (`vmlinux`/`bzImage`) and an ext4 rootfs readable by `taritd`.
- For cluster mode only: PostgreSQL reachable from every node.
- For host networking only: root or `CAP_NET_ADMIN`, plus `ip` and `nft`.

## 1. Build both binaries

`taritd` (this repo) and the `vmm` microVM backend (sibling repo):

```sh
cd /path/to/tarit/orch
cargo build --release -p taritd

cd /path/to/tarit/vmm
cargo build --release --features "vmm-core/kvm vmm-core/boot vmm-memory-backend/kvm"
```

## 2. Run one node

```sh
cd /path/to/tarit/orch

export TARIT_API_KEY="$(openssl rand -hex 24)"
export TARIT_LISTEN='127.0.0.1:8080'
export TARIT_VMM_BIN='/path/to/tarit/vmm/target/release/vmm'
export TARIT_KERNEL='/var/lib/taritd/vmlinux.microvm'
export TARIT_ROOTFS='/var/lib/taritd/rootfs.ext4'
export TARIT_ROOTFS_READONLY=1        # required when many VMs share one base image
export TARIT_SOCKET_DIR="$HOME/.taritd/sockets"
export TARIT_DB="$HOME/.taritd/fleet.db"

./target/release/taritd
```

Check it is up:

```sh
curl -sf http://127.0.0.1:8080/health
curl -sf -H "X-API-Key: $TARIT_API_KEY" http://127.0.0.1:8080/v1/cluster
```

The same binary is also a CLI. Point it at the node and reuse the API key:

```sh
export TARIT_BASE_URL='http://127.0.0.1:8080'
./target/release/taritd vm ls
```

## 3. Create a VM and run a command

```sh
# HTTP
vm=$(curl -sf -H "X-API-Key: $TARIT_API_KEY" -H 'content-type: application/json' \
  -d '{"vcpus":1,"memory_mib":256}' http://127.0.0.1:8080/v1/vms \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')

# or CLI
# vm=$(./target/release/taritd --json vm create --vcpus 1 --memory-mib 256 | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')

./target/release/taritd exec "$vm" 'uname -a'
```

## 4. Interactive PTY and SSH

Both reach the guest through the vsock agent. There is no in-guest sshd.

Register your public key, then open an interactive PTY over the CLI:

```sh
./target/release/taritd ssh-key add "$(cat ~/.ssh/id_ed25519.pub)"
./target/release/taritd pty "$vm"          # interactive shell; resize and exit work
```

Enable the SSH gateway to use a normal `ssh` client. The SSH username is the VM
id; the gateway authenticates by registered key and bridges to the guest PTY:

```sh
export TARIT_SSH_GATEWAY=1
export TARIT_SSH_GATEWAY_ADDR='0.0.0.0:2222'
# restart taritd with these set, then:
ssh -p 2222 "$vm"@<taritd-host>
```

The same PTY stream is available over WebSocket at
`WS /v1/vms/{id}/pty/{pty_id}/connect?api_key=...` (binary frames are raw bytes,
text frames are JSON `resize`/`exit`).

## 5. Cluster mode (optional)

Cluster mode is selected by setting `TARIT_DATABASE_URL` (shared PostgreSQL) and
a non-default `TARIT_PEER_SECRET` on every node. Any node then accepts API
traffic and forwards to the owning host.

```sh
export TARIT_DATABASE_URL='postgres://user:pass@db.example:5432/taritd?sslmode=require'
export TARIT_PEER_SECRET="$(openssl rand -hex 32)"
export TARIT_HOST_ID="$(hostname)"
export TARIT_RPC_ADDR="http://$(hostname -i | awk '{print $1}'):8080"
```

Each node needs its own `TARIT_HOST_ID`, `TARIT_RPC_ADDR`, `TARIT_SOCKET_DIR`,
and `TARIT_DB`. See [OPERATIONS.md](OPERATIONS.md) for the full three-node table,
load balancer guidance, and RDS setup, and [RESILIENCE.md](RESILIENCE.md) for the
failover and scale behavior each of these settings drives.

## Next steps

- [Configuration reference](CONFIGURATION.md) - every environment variable and
  the TOML config file.
- [Resilience and scale scenarios](RESILIENCE.md) - failover, durability, and
  capacity behavior, with the tests that validate each.
- [Operations](OPERATIONS.md) - clustering, warm pool, networking, security,
  benchmarks, troubleshooting.
- [API](API.md) - full HTTP surface.
