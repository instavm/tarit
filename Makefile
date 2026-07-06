# Tarit build. See README.md for the quickstart.
#
#   make              build both binaries (vmm + taritd) and the guest agent
#   sudo make install build, then install both binaries to $(BINDIR)
#   make vmm          build only the VMM (and the guest agent)
#   sudo make install-vmm  install only the VMM
#
# Install location (override e.g. `make install PREFIX=$HOME/.local`):
PREFIX  ?= /usr/local
BINDIR  ?= $(PREFIX)/bin
DESTDIR ?=

CARGO ?= cargo
# The VMM needs the `boot` feature for the KVM boot path (Linux + KVM only).
# On macOS, build the host-only paths with `make VMM_FEATURES=`.
VMM_FEATURES ?= boot

.PHONY: all build vmm taritd agent install install-vmm guest clean

# Build both binaries and the guest agent (default).
all: build

build: vmm taritd agent

vmm: agent
	$(CARGO) build --release --manifest-path vmm/Cargo.toml -p vmm $(if $(VMM_FEATURES),--features $(VMM_FEATURES),)

taritd:
	$(CARGO) build --release --manifest-path orch/Cargo.toml -p taritd

# Static guest exec agent (used as PID 1 / exec server inside the microVM).
agent:
	$(MAKE) -C vmm/guest/agent

# One-time quickstart assets: build a vsock-capable guest kernel and pre-pull an
# Ubuntu rootfs (into guest-assets/), so starting a microVM later is instant.
guest:
	./scripts/setup-guest.sh

# Install both binaries to $(BINDIR) (needs write access; use sudo for /usr/local).
install: build
	install -d "$(DESTDIR)$(BINDIR)"
	install -m755 vmm/target/release/vmm     "$(DESTDIR)$(BINDIR)/vmm"
	install -m755 orch/target/release/taritd "$(DESTDIR)$(BINDIR)/taritd"
	@echo "installed vmm + taritd to $(DESTDIR)$(BINDIR)"

# Install only the VMM.
install-vmm: vmm
	install -d "$(DESTDIR)$(BINDIR)"
	install -m755 vmm/target/release/vmm "$(DESTDIR)$(BINDIR)/vmm"
	@echo "installed vmm to $(DESTDIR)$(BINDIR)"

clean:
	$(CARGO) clean --manifest-path vmm/Cargo.toml
	$(CARGO) clean --manifest-path orch/Cargo.toml
	$(MAKE) -C vmm/guest/agent clean
