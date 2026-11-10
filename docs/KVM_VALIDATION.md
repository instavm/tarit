# Manual privileged KVM validation

This public repository must not register a root-capable Actions runner. A
main-only `if` in one workflow and custom labels do not stop another workflow
from selecting that runner. `--ephemeral` removes a registration after one job;
it does not prevent the wrong first job or erase a compromised host.
`scripts/setup-kvm-runner.sh` therefore refuses registration without changing
the host. No repository/organization settings are changed by this procedure.

## Approval and isolation

An operator independently reviews a full immutable 40-character commit SHA,
including scripts, workflows, build scripts, dependencies and guest assets.
Only commits reachable from upstream `instavm/tarit` main are eligible. Main
ancestry is not security approval: merged malicious code still runs as root.
No PR head or mutable branch name is an approved execution target.

The optional `Prepare manual KVM validation` workflow runs on GitHub-hosted
Ubuntu after manual dispatch from main. It validates the SHA and prints this
procedure; it never checks out or executes the selected commit's code. Its
success is preparation only, not evidence that any KVM test passed. Normal PR
CI remains on GitHub-hosted runners, including Linux/KVM type checking.

Create a fresh disposable Linux x86_64 VM with nested KVM, `/dev/kvm`, Rust,
compiler dependencies, iproute2, nftables, conntrack-tools and required guest
assets. Do not use production/shared hosts or reuse a previous test VM/disk.
Use no cloud instance identity/metadata credentials, GitHub/PAT/SSH credentials,
runner registration tokens, mounted host sockets, production network routes or
secrets. Restrict network access to needed public build inputs; kernel-level
tests and arbitrary root build code are inside this VM's trust boundary.
Root is limited to the lifetime of this manually approved disposable VM; no
persistent passwordless sudo rule or Actions service is installed.

## Run the exact commit

From an operator-controlled terminal inside that VM, fetch the public upstream
and detach at the reviewed SHA. Replace the placeholder before executing:

```sh
reviewed_sha=FULL_REVIEWED_40_CHARACTER_SHA
git clone https://github.com/instavm/tarit.git
cd tarit
git checkout --detach "$reviewed_sha"
git rev-parse HEAD
# Inspect the checked-out instructions/script against the reviewed version.
# Stage approved assets at guest-assets/vmlinux and guest-assets/rootfs.ext4,
# or supply absolute KERNEL and ROOTFS paths. Verify their recorded hashes.
sudo env "PATH=$PATH" TARIT_DISPOSABLE_KVM_ACK=destroy-after-this-run \
  bash scripts/run-trusted-kvm.sh "$reviewed_sha"
```

The script requires a clean checkout, exact HEAD match, Linux/KVM, explicit
acknowledgement and upstream-main ancestry before building or testing. It uses
the existing readiness harness for E2E (including suspend, snapshot disk and
checkpoint), KVM tests, cold/warm create latency and strict performance gates.
Optional longer soak runs remain available in `scripts/run-readiness-gates.sh`.
Build caches and guest disks must be fresh or independently trusted; never
restore artifacts/caches supplied by PR workflows.

Copy `target/readiness/<timestamp>/` logs, `summary.tsv` and `host.txt` out as
untrusted result data. Record the reviewed SHA, reviewer, toolchain and guest
asset SHA256 hashes alongside the results. A passing preparation workflow is
not a substitute for these hardware results. Destroy the VM and every attached
test disk after this single run, on success, failure, cancellation or timeout;
use an external expiry/reaper so loss of the operator session does not leave it
running. The acknowledgement cannot enforce destruction or prove isolation.

Before use, have an administrator check repository/organization runner
registrations and the candidate host for an Actions runner service,
`/var/lib/tarit-ci/runner/.runner` and `/etc/sudoers.d/tarit-ci`. An earlier
review observed zero registered runners; that observation can become stale.
If a persistent runner was previously installed, stop and deregister it through
an administrator-controlled procedure and discard/reimage its host before
following this procedure. This PR does not inspect, remove or repair existing
runner registrations, services, sudoers rules or cloud infrastructure.

Automated hardware CI would require a separately enforced runner-group policy
limited to an immutable trusted workflow, trusted provisioning/destruction and
fresh one-job machines with no shared credentials or disks. That infrastructure
is not implemented here. See [GitHub's self-hosted runner security guidance](https://docs.github.com/en/actions/reference/security/secure-use).
