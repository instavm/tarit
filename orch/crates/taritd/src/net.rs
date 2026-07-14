//! Per-VM host networking provisioning (tap + /30 + NAT masquerade).
//!
//! Production host networking is the orchestrator's job (the VMM only attaches a
//! virtio-net device to a pre-created tap). Each VM gets a private /30 out of
//! 172.16.0.0/16: `.1` is the host (tap) side, `.2` the guest. We enable IPv4
//! forwarding and per-allocation masquerade rules so guest egress is NAT'd out
//! the host uplink. Requires CAP_NET_ADMIN (run taritd as root); gated behind
//! `TARIT_ENABLE_NET` so the default (no-net) path is unaffected.

use ipnet::{IpNet, Ipv4Net};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};
use std::time::Duration;
use tarit_types::OrchError;
use uuid::Uuid;

const NFT_TABLE: &str = "taritd_nat";
const NFT_CHAIN: &str = "post";
/// Filter/forward chain that enforces per-VM egress allowlists (R-005). The
/// orchestrator owns host networking, so egress is enforced here on the host
/// rather than only validated inside the VMM. Named to avoid the reserved nft
/// keyword `fwd`.
const NFT_FWD_CHAIN: &str = "vm_egress";
/// Filter/input chain that rejects guest-initiated traffic to the host while
/// preserving only stateful replies to host-initiated flows.
const NFT_INPUT_CHAIN: &str = "vm_input";
/// Each TAP gets its own netdev table so teardown can atomically remove its
/// ingress base chain without touching another VM's filter.
const NFT_INGRESS_TABLE_PREFIX: &str = "taritd_ingress_";
const NFT_INGRESS_CHAIN: &str = "ingress";
const TAP_PREFIX: &str = "insta";
const NET_POOL_SLOTS: u32 = 1 << 14;
const NET_STATE_VERSION: u32 = 2;
const STALE_TAP_MIN_AGE: Duration = Duration::from_secs(30);
static STATE_WRITE_SEQUENCE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static FAIL_NEXT_STATE_DIRECTORY_SYNC: AtomicBool = AtomicBool::new(false);

/// A provisioned per-VM network: the tap name and the /30 addressing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetAlloc {
    pub idx: u32,
    pub vm_id: Uuid,
    pub tap: String,
    pub host_ip: String,
    pub guest_ip: String,
    pub prefix: u8,
}

impl NetAlloc {
    /// Derive the /30 for slot `idx`: base = 172.16.0.0 + idx*4, host = base+1,
    /// guest = base+2. The /16 contains 16,384 non-overlapping /30 slots.
    fn for_slot(vm_id: Uuid, idx: u32) -> Result<Self, OrchError> {
        if idx >= NET_POOL_SLOTS {
            return Err(OrchError::Internal(format!(
                "network slot {idx} exceeds /30 pool size {NET_POOL_SLOTS}"
            )));
        }
        let base = idx * 4;
        let (b2, b3) = ((base >> 8) as u8, (base & 0xff) as u8);
        Ok(NetAlloc {
            idx,
            vm_id,
            tap: tap_name(idx),
            host_ip: format!("172.16.{b2}.{}", b3 + 1),
            guest_ip: format!("172.16.{b2}.{}", b3 + 2),
            prefix: 30,
        })
    }

    #[cfg(test)]
    fn for_idx(idx: u32) -> Self {
        Self::for_slot(Uuid::nil(), idx).unwrap()
    }

    /// The `ip=` kernel cmdline fragment that auto-configures the guest eth0
    /// (client:server:gw:netmask:host:dev:autoconf). No DNS here — the guest
    /// gets the gateway; DNS is a higher-layer concern.
    pub fn ip_cmdline(&self) -> String {
        format!(
            "ip={}::{}:255.255.255.252::eth0:off",
            self.guest_ip, self.host_ip
        )
    }
}

/// Allocates and provisions per-VM taps. `uplink` is the host egress iface.
pub struct NetProvisioner {
    inner: Mutex<SlotAllocator>,
    network_transactions: NetworkTransactionLock,
    state_path: PathBuf,
    uplink: String,
    /// A post-rename state-sync error leaves the on-disk ownership ambiguous.
    /// Keep all current reservations and refuse further provisioning in this
    /// process rather than risk reusing a slot after a failed free.
    fail_closed: AtomicBool,
}

#[derive(Default)]
struct NetworkTransactionLock {
    // Acquire this before `NetProvisioner::inner`; never acquire it while
    // holding the allocator lock. This keeps update, teardown, and slot reuse
    // in one order.
    inner: Mutex<()>,
}

impl NetworkTransactionLock {
    fn run<T>(&self, work: impl FnOnce() -> T) -> Result<T, OrchError> {
        let _guard = self
            .inner
            .lock()
            .map_err(|_| OrchError::Internal("net egress update lock poisoned".into()))?;
        Ok(work())
    }
}

impl NetProvisioner {
    /// Detect the default-route interface, recover persisted slot ownership,
    /// ensure the shared nft table exists, and sweep stale taritd-owned artifacts.
    pub fn new(
        state_path: PathBuf,
        live_vm_ids: impl IntoIterator<Item = Uuid>,
    ) -> Result<Self, OrchError> {
        let live_vm_ids = live_vm_ids.into_iter().collect::<HashSet<_>>();
        // This is deliberately the first fallible startup action. A corrupt
        // state file or failed uplink lookup must not leave an old Tarit TAP
        // forwarding while recovery cannot establish its owner or policy.
        let _ = startup_preflight()?;
        let uplink = default_uplink()?;
        let entries = load_state(&state_path, &live_vm_ids)?;
        let allocator = SlotAllocator::from_entries(entries)?;
        let provisioner = Self {
            inner: Mutex::new(allocator),
            network_transactions: NetworkTransactionLock::default(),
            state_path,
            uplink,
            fail_closed: AtomicBool::new(false),
        };
        let all_allocations = provisioner.all_allocations()?;
        let recovered_allocations = provisioner.allocations_for_vms(&live_vm_ids)?;
        if let Err(error) = ensure_host_networking() {
            return Err(provisioner.recovery_failure_after_emergency_isolation(
                &all_allocations,
                "initialize nft base policy",
                error,
            ));
        }
        for alloc in all_allocations
            .iter()
            .filter(|alloc| !live_vm_ids.contains(&alloc.vm_id))
        {
            if let Err(error) = provisioner.teardown_locked(alloc) {
                return Err(provisioner.recovery_failure_after_emergency_isolation(
                    &all_allocations,
                    "remove stale persisted network allocation before slot reuse",
                    error,
                ));
            }
        }
        provisioner.reconcile_recovered_allocations(&recovered_allocations)?;
        if let Err(error) = (|| {
            let allocator = provisioner
                .inner
                .lock()
                .map_err(|_| OrchError::Internal("net allocator lock poisoned".into()))?;
            persist_allocator(&provisioner.state_path, &allocator)
                .map_err(PersistenceError::into_orch)
        })() {
            return Err(provisioner.recovery_failure_after_cleanup(
                &recovered_allocations,
                "persist recovered network state before quarantine release",
                error,
            ));
        }
        if let Err(error) = provisioner.validate_complete_effective_policies(None) {
            return Err(provisioner.recovery_failure_after_cleanup(
                &recovered_allocations,
                "revalidate closed Tarit security-chain ownership before quarantine release",
                error,
            ));
        }
        if let Err(error) =
            provisioner.delete_recovery_quarantine_atomically(&recovered_allocations)
        {
            return Err(provisioner.recovery_failure_after_cleanup(
                &recovered_allocations,
                "release recovered allocation quarantine",
                error,
            ));
        }
        Ok(provisioner)
    }

    pub fn uplink(&self) -> &str {
        &self.uplink
    }

    /// Create a tap for a new VM: allocate a reusable slot, persist ownership,
    /// create `ip tuntap`, configure the host /30, and add an nft NAT rule.
    pub fn provision(&self, vm_id: Uuid) -> Result<NetAlloc, OrchError> {
        self.network_transactions
            .run(|| self.provision_locked(vm_id))?
    }

    fn provision_locked(&self, vm_id: Uuid) -> Result<NetAlloc, OrchError> {
        self.ensure_provisioning_available()?;
        let alloc = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| OrchError::Internal("net allocator lock poisoned".into()))?;
            let alloc = inner.allocate(vm_id)?;
            if let Err(error) = persist_allocator(&self.state_path, &inner) {
                return Err(self.persistence_failure("persist newly allocated network slot", error));
            }
            alloc
        };

        let policy = self.egress_policy_for(&alloc)?;
        self.prepare_slot_for_provision(&alloc)?;
        if let Err(e) = self.provision_host(&alloc, &policy) {
            if let Err(cleanup_error) = self.contain_failed_provision(&alloc) {
                tracing::warn!(
                    tap = %alloc.tap,
                    vm_id = %alloc.vm_id,
                    slot = alloc.idx,
                    "net: retained failed provisioning allocation after containment failure: {cleanup_error}"
                );
            }
            return Err(e);
        }

        Ok(alloc)
    }

    /// Remove a VM's tap and exact nft rule(s), then free and persist the slot.
    /// Idempotent and fail-closed: a slot remains owned until interface deletion
    /// and exact policy cleanup are both confirmed.
    pub fn teardown(&self, alloc: &NetAlloc) -> Result<(), OrchError> {
        self.network_transactions
            .run(|| self.teardown_locked(alloc))?
    }

    fn teardown_locked(&self, alloc: &NetAlloc) -> Result<(), OrchError> {
        self.delete_tap_for_teardown(alloc)?;
        self.delete_nft_rules_for_alloc(alloc)?;
        self.require_no_tagged_policy_residual(alloc)?;
        self.free_allocation_locked(alloc)
    }

    /// Enforce a VM's egress allowlist on the host (R-005). The orchestrator
    /// owns the tap + NAT, so it also owns egress filtering: this reprograms the
    /// forward chain for the guest IP so the allowlist is actually applied, not
    /// merely validated. An empty allowlist is deny-all (only established and
    /// related return traffic survives when `allow_existing` is set). Returns
    /// the number of allow rules programmed.
    pub fn apply_egress(
        &self,
        alloc: &NetAlloc,
        allowlist: &[String],
        allow_existing: bool,
    ) -> Result<usize, OrchError> {
        self.network_transactions
            .run(|| self.apply_egress_locked(alloc, allowlist, allow_existing))?
    }

    fn apply_egress_locked(
        &self,
        alloc: &NetAlloc,
        allowlist: &[String],
        allow_existing: bool,
    ) -> Result<usize, OrchError> {
        self.require_active_allocation(alloc)?;
        // Build every rule before touching nft, so a bad rule cannot leave a
        // half-applied policy (default-open) on the host.
        let policy = EgressPolicy {
            allowlist: allowlist.to_vec(),
            allow_existing,
        };
        egress_policy_argv(alloc, allowlist, allow_existing)?;
        if let Err(error) = self.install_egress_update_quarantine(alloc) {
            return Err(self.fail_egress_update(alloc, error));
        }
        if let Err(error) = (|| {
            self.validate_security_chain_ownership()?;
            let listing = command_stdout(
                "nft",
                &["-a", "list", "chain", "ip", NFT_TABLE, NFT_FWD_CHAIN],
            )?;
            run_nft_script(&egress_replacement_script(alloc, &policy, &listing)?)?;
            self.verify_egress_policy(alloc, &policy)?;
            self.persist_egress_policy(alloc, policy)?;
            self.validate_complete_effective_policies(None)?;
            self.delete_egress_update_quarantine_atomically(alloc)
        })() {
            return Err(self.fail_egress_update(alloc, error));
        }
        Ok(allowlist.len())
    }

    /// Teardown by VM id from recovered persistent state. This covers restart
    /// cases where the supervisor no longer has a RunningVm/NetAlloc in memory.
    pub fn teardown_vm_id(&self, vm_id: Uuid) -> Result<(), OrchError> {
        self.network_transactions
            .run(|| self.teardown_vm_id_locked(vm_id))?
    }

    fn teardown_vm_id_locked(&self, vm_id: Uuid) -> Result<(), OrchError> {
        let alloc = self
            .inner
            .lock()
            .map_err(|_| {
                OrchError::Internal(
                    "net allocator lock poisoned while looking up VM teardown".into(),
                )
            })?
            .by_vm
            .get(&vm_id)
            .copied()
            .map(|slot| NetAlloc::for_slot(vm_id, slot))
            .transpose()?;
        if let Some(alloc) = alloc {
            self.teardown_locked(&alloc)?;
        }
        Ok(())
    }

    fn prepare_slot_for_provision(&self, alloc: &NetAlloc) -> Result<(), OrchError> {
        self.delete_tap_for_teardown(alloc).map_err(|error| {
            OrchError::Internal(format!(
                "net: cannot remove pre-existing strict TAP {} before provisioning; retaining slot and existing policy: {error}",
                alloc.tap
            ))
        })?;
        delete_ingress_table_for_slot(alloc.idx)?;
        self.delete_nft_rules_for_slot(alloc.idx)?;
        Ok(())
    }

    fn provision_host(&self, alloc: &NetAlloc, policy: &EgressPolicy) -> Result<(), OrchError> {
        for argv in tap_provision_argv(alloc, &self.uplink) {
            run_argv(&argv)?;
        }
        self.add_nft_rule(alloc)?;
        self.install_egress_policy(alloc, policy)?;
        self.validate_complete_effective_policies(None)?;
        run("ip", &["link", "set", &alloc.tap, "up"])
    }

    fn add_nft_rule(&self, alloc: &NetAlloc) -> Result<(), OrchError> {
        run_argv(&masquerade_nft_argv(alloc, &self.uplink))
    }

    fn egress_policy_for(&self, alloc: &NetAlloc) -> Result<EgressPolicy, OrchError> {
        self.inner
            .lock()
            .map_err(|_| OrchError::Internal("net allocator lock poisoned".into()))?
            .egress_policy_for(alloc)
    }

    fn persist_egress_policy(
        &self,
        alloc: &NetAlloc,
        policy: EgressPolicy,
    ) -> Result<(), OrchError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| OrchError::Internal("net allocator lock poisoned".into()))?;
        let previous = inner.replace_egress_policy(alloc, policy)?;
        if let Err(error) = persist_allocator(&self.state_path, &inner) {
            inner.egress_by_vm.insert(alloc.vm_id, previous);
            return Err(self.persistence_failure("persist egress policy update", error));
        }
        Ok(())
    }

    fn install_egress_policy(
        &self,
        alloc: &NetAlloc,
        policy: &EgressPolicy,
    ) -> Result<(), OrchError> {
        for argv in egress_policy_argv(alloc, &policy.allowlist, policy.allow_existing)? {
            run_argv(&argv)?;
        }
        Ok(())
    }

    fn fail_egress_update(&self, alloc: &NetAlloc, error: OrchError) -> OrchError {
        match run("ip", &["link", "set", &alloc.tap, "down"]) {
            Ok(()) => error,
            Err(link_down_error) => {
                let mut failures = vec![format!("link-down failed: {link_down_error}")];
                if let Err(link_delete_error) = run("ip", &["link", "del", &alloc.tap]) {
                    failures.push(format!("link-delete failed: {link_delete_error}"));
                }
                for argv in emergency_forwarding_disable_argv() {
                    if let Err(forwarding_error) = run_argv(&argv) {
                        failures.push(format!(
                            "host forwarding disable failed ({}): {forwarding_error}",
                            argv.join(" ")
                        ));
                    }
                }
                OrchError::Internal(format!(
                    "net: update egress policy for {}: {error}; emergency containment attempted: {}",
                    alloc.tap,
                    failures.join("; ")
                ))
            }
        }
    }

    fn install_egress_update_quarantine(&self, alloc: &NetAlloc) -> Result<(), OrchError> {
        let interface = nft_quote(&alloc.tap);
        let comment = nft_quote(&egress_update_quarantine_comment(alloc));
        run_nft_script(&format!(
            "insert rule ip {NFT_TABLE} {NFT_FWD_CHAIN} iifname {interface} drop comment {comment}\n"
        ))
    }

    fn delete_egress_update_quarantine_atomically(
        &self,
        alloc: &NetAlloc,
    ) -> Result<(), OrchError> {
        let listing = command_stdout(
            "nft",
            &["-a", "list", "chain", "ip", NFT_TABLE, NFT_FWD_CHAIN],
        )?;
        let handles = listing
            .lines()
            .filter(|line| {
                is_recognized_taritd_rule(NFT_FWD_CHAIN, line)
                    && is_egress_update_quarantine_rule_for_alloc(line, alloc)
            })
            .map(|line| {
                nft_handle(line).ok_or_else(|| {
                    OrchError::Internal(format!(
                        "net: egress update quarantine for {} has no nft handle",
                        alloc.tap
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if handles.is_empty() {
            return Err(OrchError::Internal(format!(
                "net: missing egress update quarantine for {}",
                alloc.tap
            )));
        }
        let script = handles
            .into_iter()
            .map(|handle| format!("delete rule ip {NFT_TABLE} {NFT_FWD_CHAIN} handle {handle}"))
            .collect::<Vec<_>>()
            .join("\n");
        run_nft_script(&(script + "\n"))
    }

    fn verify_egress_policy(
        &self,
        alloc: &NetAlloc,
        policy: &EgressPolicy,
    ) -> Result<(), OrchError> {
        self.validate_complete_effective_policies(Some((alloc, policy)))?;
        let listing = command_stdout(
            "nft",
            &["-a", "list", "chain", "ip", NFT_TABLE, NFT_FWD_CHAIN],
        )?;
        for rule in egress_policy_argv(alloc, &policy.allowlist, policy.allow_existing)? {
            let expected = rule[6..].join(" ");
            if !listing.contains(&expected) {
                return Err(OrchError::Internal(format!(
                    "net: egress policy for {} is missing rule {expected:?}",
                    alloc.tap
                )));
            }
        }
        Ok(())
    }

    fn all_allocations(&self) -> Result<Vec<NetAlloc>, OrchError> {
        self.inner
            .lock()
            .map_err(|_| OrchError::Internal("net allocator lock poisoned".into()))?
            .active_allocations()
            .into_iter()
            .map(|(slot, vm_id)| NetAlloc::for_slot(vm_id, slot))
            .collect()
    }

    fn allocations_for_vms(&self, vm_ids: &HashSet<Uuid>) -> Result<Vec<NetAlloc>, OrchError> {
        Ok(self
            .all_allocations()?
            .into_iter()
            .filter(|alloc| vm_ids.contains(&alloc.vm_id))
            .collect())
    }

    /// Return the recovered network allocation for a single VM, if one is
    /// persisted. Used when re-adopting VMs after a taritd restart so the
    /// supervisor can attach the recovered TAP/IP to the running VM handle.
    pub fn allocation_for_vm(&self, vm_id: Uuid) -> Result<Option<NetAlloc>, OrchError> {
        let slot = self
            .inner
            .lock()
            .map_err(|_| OrchError::Internal("net allocator lock poisoned".into()))?
            .by_vm
            .get(&vm_id)
            .copied();
        match slot {
            Some(slot) => Ok(Some(NetAlloc::for_slot(vm_id, slot)?)),
            None => Ok(None),
        }
    }

    fn ensure_provisioning_available(&self) -> Result<(), OrchError> {
        if self.fail_closed.load(Ordering::SeqCst) {
            Err(OrchError::Internal(
                "net: provisioning is fail-closed after an ambiguous durable-state write; restart only after inspecting and reconciling TARIT_NET_STATE"
                    .into(),
            ))
        } else {
            Ok(())
        }
    }

    fn persistence_failure(&self, operation: &str, error: PersistenceError) -> OrchError {
        if error.is_ambiguous() {
            self.fail_closed.store(true, Ordering::SeqCst);
            OrchError::Internal(format!(
                "net: {operation}: {error}; current process is fail-closed and retains in-memory ownership because the renamed state may be durable"
            ))
        } else {
            OrchError::Internal(format!("net: {operation}: {error}"))
        }
    }

    fn contain_failed_provision(&self, alloc: &NetAlloc) -> Result<(), OrchError> {
        self.delete_tap_for_teardown(alloc)?;
        delete_ingress_table_for_slot(alloc.idx)?;
        self.delete_nft_rules_for_slot(alloc.idx)?;
        Ok(())
    }

    fn reconcile_recovered_allocations(&self, allocations: &[NetAlloc]) -> Result<(), OrchError> {
        if let Err(error) = self.install_recovery_quarantine(allocations) {
            return Err(self.recovery_failure_after_emergency_isolation(
                allocations,
                "install recovery quarantine",
                error,
            ));
        }
        let isolation = self.emergency_isolate_recovered_taps(allocations);
        if !isolation.failures.is_empty() {
            return Err(self.recovery_failure_after_emergency_isolation(
                allocations,
                "isolate recovered allocations before reconciliation",
                OrchError::Internal(isolation.failures.join("; ")),
            ));
        }
        for alloc in allocations {
            if let Err(error) = self.egress_policy_for(alloc) {
                return Err(self.recovery_failure_after_emergency_isolation(
                    allocations,
                    "load persisted recovered egress policy",
                    error,
                ));
            }
        }

        for alloc in allocations {
            if let Err(error) = self.delete_stale_recovery_rules_for_alloc(alloc) {
                return Err(self.recovery_failure_after_emergency_isolation(
                    allocations,
                    "purge stale recovered policy",
                    error,
                ));
            }
        }

        for alloc in allocations {
            if let Err(error) = self.program_recovered_allocation(alloc) {
                return Err(self.recovery_failure_after_cleanup(
                    allocations,
                    "program recovered policy",
                    error,
                ));
            }
            if let Err(error) = self.verify_recovered_allocation_policy(alloc) {
                return Err(self.recovery_failure_after_cleanup(
                    allocations,
                    "verify recovered policy",
                    error,
                ));
            }
        }

        let report = match self.sweep_orphans() {
            Ok(report) => report,
            Err(error) => {
                return Err(self.recovery_failure_after_cleanup(
                    allocations,
                    "sweep stale networking artifacts",
                    error,
                ));
            }
        };
        if report.has_work() {
            tracing::info!(
                taps_removed = report.taps_removed,
                nft_rules_removed = report.nft_rules_removed,
                ingress_tables_removed = report.ingress_tables_removed,
                "net: startup stale sweep completed"
            );
        }
        if let Err(error) = self.validate_security_chain_ownership() {
            return Err(self.recovery_failure_after_cleanup(
                allocations,
                "validate closed Tarit security-chain ownership",
                error,
            ));
        }
        self.raise_recovered_allocations(allocations)
            .map_err(|error| {
                self.recovery_failure_after_cleanup(
                    allocations,
                    "raise recovered allocations under quarantine",
                    error,
                )
            })
    }

    fn install_recovery_quarantine(&self, allocations: &[NetAlloc]) -> Result<(), OrchError> {
        if allocations.is_empty() {
            return Ok(());
        }
        run_nft_script(&recovery_quarantine_script(allocations))
    }

    fn raise_recovered_allocations(&self, allocations: &[NetAlloc]) -> Result<(), OrchError> {
        for alloc in allocations {
            run("ip", &["link", "set", &alloc.tap, "up"])?;
        }
        Ok(())
    }

    fn delete_recovery_quarantine_atomically(
        &self,
        allocations: &[NetAlloc],
    ) -> Result<(), OrchError> {
        if allocations.is_empty() {
            return Ok(());
        }
        let script = recovery_quarantine_delete_script(allocations)?;
        run_nft_script(&script)
    }

    fn recovery_failure_after_emergency_isolation(
        &self,
        allocations: &[NetAlloc],
        context: &str,
        original_error: OrchError,
    ) -> OrchError {
        let isolation = self.emergency_isolate_all_tarit_taps(allocations);
        let mut failures = isolation.failures;
        for argv in emergency_forwarding_disable_argv() {
            if let Err(error) = run_argv(&argv) {
                failures.push(format!(
                    "emergency host forwarding disable failed ({}): {error}",
                    argv.join(" ")
                ));
            }
        }
        let details = if failures.is_empty() {
            "all emergency containment commands completed".to_string()
        } else {
            failures.join("; ")
        };
        OrchError::Internal(format!(
            "net: {context}: {original_error}; catastrophic containment attempted: {details}"
        ))
    }

    fn emergency_isolate_recovered_taps(&self, allocations: &[NetAlloc]) -> IsolationReport {
        let taps = allocations
            .iter()
            .map(|alloc| alloc.tap.clone())
            .collect::<Vec<_>>();
        emergency_isolate_tap_names(&taps)
    }

    fn emergency_isolate_all_tarit_taps(&self, allocations: &[NetAlloc]) -> IsolationReport {
        let mut taps = allocations
            .iter()
            .map(|alloc| alloc.tap.clone())
            .collect::<BTreeSet<_>>();
        match discover_strict_tap_names() {
            Ok(discovered) => taps.extend(discovered),
            Err(error) => {
                let mut isolation =
                    emergency_isolate_tap_names(&taps.into_iter().collect::<Vec<_>>());
                isolation
                    .failures
                    .insert(0, format!("discover strict Tarit TAPs: {error}"));
                return isolation;
            }
        }
        emergency_isolate_tap_names(&taps.into_iter().collect::<Vec<_>>())
    }

    fn validate_security_chain_ownership(&self) -> Result<(), OrchError> {
        for chain in [NFT_CHAIN, NFT_FWD_CHAIN, NFT_INPUT_CHAIN] {
            let listing = command_stdout("nft", &["-a", "list", "chain", "ip", NFT_TABLE, chain])?;
            if chain == NFT_CHAIN {
                validate_taritd_nat_chain(&listing)?;
            } else {
                validate_taritd_security_chain(chain, &listing)?;
            }
        }
        Ok(())
    }

    fn validate_complete_effective_policies(
        &self,
        replacement: Option<(&NetAlloc, &EgressPolicy)>,
    ) -> Result<(), OrchError> {
        let mut policies = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| OrchError::Internal("net allocator lock poisoned".into()))?;
            inner
                .active_allocations()
                .into_iter()
                .map(|(slot, vm_id)| {
                    let alloc = NetAlloc::for_slot(vm_id, slot)?;
                    let policy = inner.egress_policy_for(&alloc)?;
                    Ok((alloc, policy))
                })
                .collect::<Result<Vec<_>, OrchError>>()?
        };
        if let Some((alloc, policy)) = replacement {
            let entry = policies
                .iter_mut()
                .find(|(candidate, _)| candidate == alloc)
                .ok_or_else(|| {
                    OrchError::NotFound(format!(
                        "network allocation for VM {} is no longer active",
                        alloc.vm_id
                    ))
                })?;
            entry.1 = policy.clone();
        }
        let forward = command_stdout(
            "nft",
            &["-a", "list", "chain", "ip", NFT_TABLE, NFT_FWD_CHAIN],
        )?;
        let input = command_stdout(
            "nft",
            &["-a", "list", "chain", "ip", NFT_TABLE, NFT_INPUT_CHAIN],
        )?;
        let nat = command_stdout("nft", &["-a", "list", "chain", "ip", NFT_TABLE, NFT_CHAIN])?;
        validate_complete_effective_security_policies(
            &policies,
            &self.uplink,
            &nat,
            &forward,
            &input,
        )
    }

    fn recovery_failure_after_cleanup(
        &self,
        allocations: &[NetAlloc],
        context: &str,
        original_error: OrchError,
    ) -> OrchError {
        let isolation = self.emergency_isolate_all_tarit_taps(allocations);
        let mut failures = isolation.failures;
        for argv in emergency_forwarding_disable_argv() {
            if let Err(error) = run_argv(&argv) {
                failures.push(format!(
                    "emergency host forwarding disable failed ({}): {error}",
                    argv.join(" ")
                ));
            }
        }
        failures.extend(self.best_effort_recovery_cleanup(allocations, &isolation.contained));
        if failures.is_empty() {
            OrchError::Internal(format!("net: {context}: {original_error}"))
        } else {
            OrchError::Internal(format!(
                "net: {context}: {original_error}; {}",
                failures.join("; ")
            ))
        }
    }

    fn program_recovered_allocation(&self, alloc: &NetAlloc) -> Result<(), OrchError> {
        for argv in recovered_tap_reconcile_argv(alloc, &self.uplink) {
            run_argv(&argv)?;
        }
        self.add_nft_rule(alloc)?;
        self.install_egress_policy(alloc, &self.egress_policy_for(alloc)?)
    }

    fn verify_recovered_allocation_policy(&self, alloc: &NetAlloc) -> Result<(), OrchError> {
        self.validate_security_chain_ownership()?;
        for (setting, expected) in tap_sysctl_settings(&alloc.tap) {
            let actual = command_stdout("sysctl", &["-qn", &setting])?;
            if actual.trim() != expected {
                return Err(OrchError::Internal(format!(
                    "net: recovered tap {} has {setting}={}, expected {expected}",
                    alloc.tap,
                    actual.trim()
                )));
            }
        }
        let ingress_table = ingress_table_name(alloc.idx);
        let ingress = command_stdout("nft", &["-a", "list", "table", "netdev", &ingress_table])?;
        if !ingress_table_belongs_to_alloc(&ingress, alloc) {
            return Err(OrchError::Internal(format!(
                "net: recovered tap {} ingress policy is incomplete",
                alloc.tap
            )));
        }
        for (chain, kind, minimum_rules) in [
            (NFT_CHAIN, TaritdNftRuleKind::Nat, 1),
            (NFT_FWD_CHAIN, TaritdNftRuleKind::Guard, 3),
            (NFT_INPUT_CHAIN, TaritdNftRuleKind::Input, 3),
        ] {
            let listing = command_stdout("nft", &["-a", "list", "chain", "ip", NFT_TABLE, chain])?;
            let found = listing
                .lines()
                .filter_map(parse_taritd_nft_rule_tag)
                .filter(|tag| {
                    tag.kind == kind
                        && tag.slot == alloc.idx
                        && tag.vm_id == alloc.vm_id
                        && tag.tap == alloc.tap
                })
                .count();
            if found < minimum_rules {
                return Err(OrchError::Internal(format!(
                    "net: recovered tap {} has only {found}/{minimum_rules} {kind:?} policy rules",
                    alloc.tap
                )));
            }
        }
        let policy = self.egress_policy_for(alloc)?;
        let expected_egress = egress_policy_argv(alloc, &policy.allowlist, policy.allow_existing)?;
        let forward = command_stdout(
            "nft",
            &["-a", "list", "chain", "ip", NFT_TABLE, NFT_FWD_CHAIN],
        )?;
        for rule in expected_egress {
            let expected = rule[6..].join(" ");
            if !forward.contains(&expected) {
                return Err(OrchError::Internal(format!(
                    "net: recovered tap {} is missing persisted egress policy rule {expected:?}",
                    alloc.tap
                )));
            }
        }
        Ok(())
    }

    fn best_effort_recovery_cleanup(
        &self,
        allocations: &[NetAlloc],
        contained_taps: &BTreeSet<String>,
    ) -> Vec<String> {
        let mut failures = Vec::new();
        for alloc in allocations {
            if !contained_taps.contains(&alloc.tap) {
                failures.push(format!(
                    "retained ingress and policy for {} because link containment failed",
                    alloc.tap
                ));
                continue;
            }
            if let Err(error) = self.delete_recovery_rules_for_alloc(alloc) {
                failures.push(format!(
                    "partial recovered policy cleanup failed for {}: {error}",
                    alloc.tap
                ));
            }
            if let Err(error) = delete_ingress_table_for_alloc(alloc) {
                failures.push(format!(
                    "partial recovered ingress cleanup failed for {}: {error}",
                    alloc.tap
                ));
            }
        }
        failures
    }

    fn delete_tap_for_teardown(&self, alloc: &NetAlloc) -> Result<(), OrchError> {
        if strict_tap_is_absent(&alloc.tap)? {
            return Ok(());
        }
        run("ip", &["link", "set", &alloc.tap, "down"]).map_err(|error| {
            OrchError::Internal(format!(
                "net: cannot contain TAP {} before teardown; retaining policy and slot: {error}",
                alloc.tap
            ))
        })?;
        run("ip", &["link", "del", &alloc.tap]).map_err(|error| {
            OrchError::Internal(format!(
                "net: cannot delete contained TAP {}; retaining policy and slot: {error}",
                alloc.tap
            ))
        })?;
        if strict_tap_is_absent(&alloc.tap)? {
            Ok(())
        } else {
            Err(OrchError::Internal(format!(
                "net: TAP {} remained present after deletion; retaining policy and slot",
                alloc.tap
            )))
        }
    }

    fn free_allocation_locked(&self, alloc: &NetAlloc) -> Result<(), OrchError> {
        let mut inner = self.inner.lock().map_err(|_| {
            OrchError::Internal("net allocator lock poisoned while freeing slot".into())
        })?;
        let original = inner.clone();
        inner.free(alloc);
        if let Err(error) = persist_allocator(&self.state_path, &inner) {
            *inner = original;
            return Err(self.persistence_failure(
                &format!(
                    "persist freed slot {}; retaining allocation ownership",
                    alloc.idx
                ),
                error,
            ));
        }
        Ok(())
    }

    fn require_active_allocation(&self, alloc: &NetAlloc) -> Result<(), OrchError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| OrchError::Internal("net allocator lock poisoned".into()))?;
        if inner.by_vm.get(&alloc.vm_id) == Some(&alloc.idx)
            && inner.by_slot.get(&alloc.idx) == Some(&alloc.vm_id)
        {
            Ok(())
        } else {
            Err(OrchError::NotFound(format!(
                "network allocation for VM {} is no longer active",
                alloc.vm_id
            )))
        }
    }

    fn sweep_orphans(&self) -> Result<SweepReport, OrchError> {
        let active = self
            .inner
            .lock()
            .map_err(|_| OrchError::Internal("net allocator lock poisoned".into()))?
            .active_allocations();
        let active_slots = active.keys().copied().collect::<HashSet<_>>();
        let taps = discover_taps()?;
        let stale_taps = stale_taps_to_sweep(&taps, &active_slots, STALE_TAP_MIN_AGE);

        let mut report = SweepReport::default();
        for tap in stale_taps {
            if let Some(slot) = slot_from_tap(&tap.name) {
                let policy_rules = self.count_nft_rules_for_slot(slot)?;
                if policy_rules == 0 {
                    tracing::debug!(
                        tap = %tap.name,
                        slot,
                        "net: preserving unowned stale-named tap without Tarit policy"
                    );
                    continue;
                }
                let orphan = NetAlloc::for_slot(Uuid::nil(), slot)?;
                self.delete_tap_for_teardown(&orphan)?;
                let removed = self.delete_nft_rules_for_slot(slot)?;
                report.nft_rules_removed += removed;
                if removed == 0 {
                    return Err(OrchError::Internal(format!(
                        "net: exact orphan policy for {} disappeared after TAP deletion; refusing slot reuse",
                        tap.name
                    )));
                }
                report.taps_removed += 1;
            }
        }

        let strict_taps = discover_strict_tap_names()?;
        report.nft_rules_removed += self.delete_orphan_nft_rules(&active, &strict_taps)?;
        report.ingress_tables_removed += self.delete_orphan_ingress_tables(&active)?;
        Ok(report)
    }

    fn delete_nft_rules_for_alloc(&self, alloc: &NetAlloc) -> Result<usize, OrchError> {
        let mut removed = 0;
        for chain in [NFT_CHAIN, NFT_FWD_CHAIN, NFT_INPUT_CHAIN] {
            removed += delete_nft_rules_in_chain(chain, |line| {
                is_recognized_taritd_rule(chain, line) && is_taritd_nft_rule_for_alloc(line, alloc)
            })?;
        }
        removed += delete_ingress_table_for_alloc(alloc)?;
        Ok(removed)
    }

    fn require_no_tagged_policy_residual(&self, alloc: &NetAlloc) -> Result<(), OrchError> {
        let listing = command_stdout("nft", &["-a", "list", "table", "ip", NFT_TABLE])?;
        if has_exact_allocation_tag(&listing, alloc) {
            return Err(OrchError::Internal(format!(
                "net: tagged policy remains for {} after exact cleanup; retaining allocation ownership",
                alloc.tap
            )));
        }
        for table in netdev_table_names()? {
            let listing = command_stdout("nft", &["-a", "list", "table", "netdev", &table])?;
            if has_exact_allocation_tag(&listing, alloc) {
                return Err(OrchError::Internal(format!(
                    "net: tagged netdev policy remains for {} after exact cleanup; retaining allocation ownership",
                    alloc.tap
                )));
            }
        }
        Ok(())
    }

    fn delete_recovery_rules_for_alloc(&self, alloc: &NetAlloc) -> Result<usize, OrchError> {
        let mut removed = 0;
        for chain in [NFT_CHAIN, NFT_FWD_CHAIN, NFT_INPUT_CHAIN] {
            removed += delete_nft_rules_in_chain(chain, |line| {
                is_recognized_taritd_rule(chain, line)
                    && is_recovery_nft_rule_for_alloc(line, alloc)
            })?;
        }
        Ok(removed)
    }

    fn delete_stale_recovery_rules_for_alloc(&self, alloc: &NetAlloc) -> Result<usize, OrchError> {
        let mut removed = 0;
        for chain in [NFT_CHAIN, NFT_FWD_CHAIN, NFT_INPUT_CHAIN] {
            removed += delete_nft_rules_in_chain(chain, |line| {
                is_recognized_taritd_rule(chain, line)
                    && is_stale_recovery_rule_for_alloc(line, alloc)
            })?;
        }
        removed += delete_ingress_table_for_alloc(alloc)?;
        Ok(removed)
    }

    fn delete_nft_rules_for_slot(&self, slot: u32) -> Result<usize, OrchError> {
        let mut removed = 0;
        for chain in [NFT_CHAIN, NFT_FWD_CHAIN, NFT_INPUT_CHAIN] {
            removed += delete_nft_rules_in_chain(chain, |line| {
                is_recognized_taritd_rule(chain, line) && is_taritd_nft_rule_for_slot(line, slot)
            })?;
        }
        Ok(removed)
    }

    fn count_nft_rules_for_slot(&self, slot: u32) -> Result<usize, OrchError> {
        let mut count = 0;
        for chain in [NFT_CHAIN, NFT_FWD_CHAIN, NFT_INPUT_CHAIN] {
            count += command_stdout("nft", &["-a", "list", "chain", "ip", NFT_TABLE, chain])?
                .lines()
                .filter(|line| {
                    is_recognized_taritd_rule(chain, line)
                        && is_taritd_nft_rule_for_slot(line, slot)
                })
                .count();
        }
        Ok(count)
    }

    fn delete_orphan_nft_rules(
        &self,
        active: &BTreeMap<u32, Uuid>,
        strict_taps: &[String],
    ) -> Result<usize, OrchError> {
        let mut removed = 0;
        for chain in [NFT_CHAIN, NFT_FWD_CHAIN, NFT_INPUT_CHAIN] {
            removed += delete_nft_rules_in_chain(chain, |line| {
                is_recognized_taritd_rule(chain, line)
                    && is_orphan_taritd_nft_rule(line, active)
                    && parse_taritd_nft_rule_tag(line)
                        .is_some_and(|tag| !strict_taps.iter().any(|tap| tap == &tag.tap))
            })?;
        }
        Ok(removed)
    }

    fn delete_orphan_ingress_tables(
        &self,
        active: &BTreeMap<u32, Uuid>,
    ) -> Result<usize, OrchError> {
        let _ = active;
        // A table name only encodes a slot. Without a recovered or explicitly
        // torn-down allocation we cannot prove its VM identity, so preserve it.
        Ok(0)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SweepReport {
    taps_removed: usize,
    nft_rules_removed: usize,
    ingress_tables_removed: usize,
}

impl SweepReport {
    fn has_work(self) -> bool {
        self.taps_removed > 0 || self.nft_rules_removed > 0 || self.ingress_tables_removed > 0
    }
}

#[derive(Debug, Clone)]
struct SlotAllocator {
    free: BTreeSet<u32>,
    by_slot: BTreeMap<u32, Uuid>,
    by_vm: HashMap<Uuid, u32>,
    egress_by_vm: HashMap<Uuid, Option<EgressPolicy>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct EgressPolicy {
    allowlist: Vec<String>,
    allow_existing: bool,
}

impl SlotAllocator {
    fn empty() -> Self {
        Self {
            free: (0..NET_POOL_SLOTS).collect(),
            by_slot: BTreeMap::new(),
            by_vm: HashMap::new(),
            egress_by_vm: HashMap::new(),
        }
    }

    fn from_entries(entries: Vec<NetStateEntry>) -> Result<Self, OrchError> {
        validate_state_entries(&entries)?;
        let mut allocator = Self::empty();
        for NetStateEntry {
            slot,
            vm_id,
            tap: _,
            egress,
        } in entries
        {
            allocator.free.remove(&slot);
            allocator.by_slot.insert(slot, vm_id);
            allocator.by_vm.insert(vm_id, slot);
            allocator.egress_by_vm.insert(vm_id, egress);
        }
        Ok(allocator)
    }

    fn allocate(&mut self, vm_id: Uuid) -> Result<NetAlloc, OrchError> {
        if let Some(slot) = self.by_vm.get(&vm_id).copied() {
            return NetAlloc::for_slot(vm_id, slot);
        }
        let Some(slot) = self.free.pop_first() else {
            return Err(OrchError::Overloaded {
                message: format!(
                    "per-VM network address pool exhausted ({NET_POOL_SLOTS} /30 slots in 172.16.0.0/16)"
                ),
                retry_after_secs: 1,
            });
        };
        self.by_slot.insert(slot, vm_id);
        self.by_vm.insert(vm_id, slot);
        self.egress_by_vm
            .insert(vm_id, Some(EgressPolicy::default()));
        NetAlloc::for_slot(vm_id, slot)
    }

    fn free(&mut self, alloc: &NetAlloc) {
        match self.by_vm.remove(&alloc.vm_id) {
            Some(slot) => {
                self.by_slot.remove(&slot);
                self.free.insert(slot);
                self.egress_by_vm.remove(&alloc.vm_id);
            }
            None => match self.by_slot.get(&alloc.idx).copied() {
                Some(owner) if owner == alloc.vm_id => {
                    self.by_slot.remove(&alloc.idx);
                    self.free.insert(alloc.idx);
                    self.egress_by_vm.remove(&alloc.vm_id);
                }
                Some(owner) => tracing::warn!(
                    slot = alloc.idx,
                    expected_vm = %alloc.vm_id,
                    owner_vm = %owner,
                    "net: refused to free slot owned by a different VM"
                ),
                None if alloc.idx < NET_POOL_SLOTS => {
                    self.free.insert(alloc.idx);
                }
                None => {}
            },
        }
    }

    fn egress_policy_for(&self, alloc: &NetAlloc) -> Result<EgressPolicy, OrchError> {
        match (
            self.by_vm.get(&alloc.vm_id).copied(),
            self.egress_by_vm.get(&alloc.vm_id),
        ) {
            (Some(slot), Some(Some(policy))) if slot == alloc.idx => Ok(policy.clone()),
            _ => Err(OrchError::Internal(format!(
                "net: missing persisted egress policy for recovered allocation {} (slot {}); refusing recovery",
                alloc.vm_id, alloc.idx
            ))),
        }
    }

    fn replace_egress_policy(
        &mut self,
        alloc: &NetAlloc,
        policy: EgressPolicy,
    ) -> Result<Option<EgressPolicy>, OrchError> {
        if self.by_vm.get(&alloc.vm_id).copied() != Some(alloc.idx) {
            return Err(OrchError::Internal(format!(
                "net: cannot persist egress policy for unallocated VM {}",
                alloc.vm_id
            )));
        }
        Ok(self
            .egress_by_vm
            .insert(alloc.vm_id, Some(policy))
            .flatten())
    }

    fn active_allocations(&self) -> BTreeMap<u32, Uuid> {
        self.by_slot.clone()
    }

    fn entries(&self) -> Vec<NetStateEntry> {
        self.by_slot
            .iter()
            .map(|(slot, vm_id)| NetStateEntry {
                slot: *slot,
                vm_id: *vm_id,
                tap: tap_name(*slot),
                egress: self.egress_by_vm.get(vm_id).cloned().flatten(),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NetStateFile {
    version: u32,
    allocations: Vec<NetStateEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NetStateEntry {
    slot: u32,
    vm_id: Uuid,
    tap: String,
    #[serde(default)]
    egress: Option<EgressPolicy>,
}

fn load_state(path: &Path, live_vm_ids: &HashSet<Uuid>) -> Result<Vec<NetStateEntry>, OrchError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(OrchError::Internal(format!(
                "read net state {}: {e}",
                path.display()
            )))
        }
    };
    decode_net_state(&text, path, live_vm_ids)
}

fn decode_net_state(
    text: &str,
    path: &Path,
    live_vm_ids: &HashSet<Uuid>,
) -> Result<Vec<NetStateEntry>, OrchError> {
    let state = serde_json::from_str::<NetStateFile>(text)
        .map_err(|e| OrchError::Internal(format!("parse net state {}: {e}", path.display())))?;
    validate_state_entries(&state.allocations)?;
    match state.version {
        NET_STATE_VERSION => Ok(state.allocations),
        1 => {
            if state
                .allocations
                .iter()
                .any(|allocation| live_vm_ids.contains(&allocation.vm_id))
            {
                Err(OrchError::Internal(format!(
                    "net state version 1 in {} has live allocations without required egress semantics; refusing recovery",
                    path.display()
                )))
            } else {
                Ok(state.allocations)
            }
        }
        version => Err(OrchError::Internal(format!(
            "unsupported net state version {version} in {}",
            path.display()
        ))),
    }
}

fn validate_state_entries(entries: &[NetStateEntry]) -> Result<(), OrchError> {
    let mut slots = HashSet::new();
    let mut vm_ids = HashSet::new();
    for entry in entries {
        if entry.slot >= NET_POOL_SLOTS {
            return Err(OrchError::Internal(format!(
                "net state has invalid slot {} outside the /30 pool",
                entry.slot
            )));
        }
        if entry.vm_id.is_nil() {
            return Err(OrchError::Internal(
                "net state has ambiguous nil VM ownership".into(),
            ));
        }
        if entry.tap != tap_name(entry.slot) {
            return Err(OrchError::Internal(format!(
                "net state has contradictory identity: slot {} is not TAP {}",
                entry.slot,
                tap_name(entry.slot)
            )));
        }
        if !slots.insert(entry.slot) {
            return Err(OrchError::Internal(format!(
                "net state has duplicate ownership of slot {}",
                entry.slot
            )));
        }
        if !vm_ids.insert(entry.vm_id) {
            return Err(OrchError::Internal(format!(
                "net state has duplicate ownership for VM {}",
                entry.vm_id
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
fn legacy_v1_reader_accepts_version(version: u32) -> Result<(), OrchError> {
    if version == 1 {
        Ok(())
    } else {
        Err(OrchError::Internal(format!(
            "unsupported net state version {version}"
        )))
    }
}

#[derive(Debug)]
enum PersistenceError {
    NotCommitted(OrchError),
    CommitAmbiguous(OrchError),
}

impl PersistenceError {
    fn is_ambiguous(&self) -> bool {
        matches!(self, Self::CommitAmbiguous(_))
    }

    fn into_orch(self) -> OrchError {
        match self {
            Self::NotCommitted(error) => error,
            Self::CommitAmbiguous(error) => {
                OrchError::Internal(format!("{error}; state commit is ambiguous after rename"))
            }
        }
    }
}

impl std::fmt::Display for PersistenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCommitted(error) => write!(formatter, "{error}"),
            Self::CommitAmbiguous(error) => {
                write!(formatter, "{error}; state commit is ambiguous after rename")
            }
        }
    }
}

fn persist_allocator(path: &Path, allocator: &SlotAllocator) -> Result<(), PersistenceError> {
    persist_entries(path, allocator.entries())
}

fn persist_entries(path: &Path, allocations: Vec<NetStateEntry>) -> Result<(), PersistenceError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            PersistenceError::NotCommitted(OrchError::Internal(format!(
                "create net state dir {}: {e}",
                parent.display()
            )))
        })?;
    }
    let state = NetStateFile {
        version: NET_STATE_VERSION,
        allocations,
    };
    let text = serde_json::to_string_pretty(&state).map_err(|e| {
        PersistenceError::NotCommitted(OrchError::Internal(format!("encode net state: {e}")))
    })?;
    let (tmp, mut file) = {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| "net-state.json".into());
        let mut created = None;
        for _ in 0..128 {
            let sequence = STATE_WRITE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let candidate = parent.join(format!(
                ".{file_name}.tmp-{}-{sequence}",
                std::process::id()
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&candidate) {
                Ok(file) => {
                    created = Some((candidate, file));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(PersistenceError::NotCommitted(OrchError::Internal(
                        format!("create net state temp {}: {error}", candidate.display()),
                    )));
                }
            }
        }
        created.ok_or_else(|| {
            PersistenceError::NotCommitted(OrchError::Internal(format!(
                "create unique net state temp beside {}: exhausted retries",
                path.display()
            )))
        })?
    };
    if let Err(error) = file
        .write_all(text.as_bytes())
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(PersistenceError::NotCommitted(OrchError::Internal(
            format!("write durable net state {}: {error}", tmp.display()),
        )));
    }
    drop(file);
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(PersistenceError::CommitAmbiguous(OrchError::Internal(
            format!(
                "replace net state {} with {}: {error}",
                path.display(),
                tmp.display()
            ),
        )));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    sync_state_directory(parent).map_err(|e| {
        PersistenceError::CommitAmbiguous(OrchError::Internal(format!(
            "sync net state directory {}: {e}",
            parent.display()
        )))
    })?;
    Ok(())
}

fn sync_state_directory(parent: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_NEXT_STATE_DIRECTORY_SYNC.swap(false, Ordering::SeqCst) {
        return Err(std::io::Error::other("injected directory sync failure"));
    }
    std::fs::File::open(parent).and_then(|directory| directory.sync_all())
}

#[cfg(test)]
fn state_write_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy())
        .unwrap_or_else(|| "net-state.json".into());
    path.with_file_name(format!("{file_name}.new"))
}

fn ensure_host_networking() -> Result<(), OrchError> {
    for argv in host_nft_base_argv() {
        if argv.first().is_some_and(|command| command == "nft") {
            run_nft_allowing_existing(&argv)?;
        } else {
            run_argv(&argv)?;
        }
    }
    for chain in [NFT_CHAIN, NFT_FWD_CHAIN, NFT_INPUT_CHAIN] {
        validate_existing_nft_base_chain_topology(chain)?;
    }
    Ok(())
}

fn host_nft_base_argv() -> Vec<Vec<String>> {
    vec![
        command_argv(&["sysctl", "-qw", "net.ipv4.ip_forward=1"]),
        command_argv(&["nft", "add", "table", "ip", NFT_TABLE]),
        command_argv(&[
            "nft",
            "add",
            "chain",
            "ip",
            NFT_TABLE,
            NFT_CHAIN,
            "{ type nat hook postrouting priority 100 ; policy accept ; }",
        ]),
        command_argv(&[
            "nft",
            "add",
            "chain",
            "ip",
            NFT_TABLE,
            NFT_FWD_CHAIN,
            "{ type filter hook forward priority 0 ; policy accept ; }",
        ]),
        command_argv(&[
            "nft",
            "add",
            "chain",
            "ip",
            NFT_TABLE,
            NFT_INPUT_CHAIN,
            "{ type filter hook input priority 0 ; policy accept ; }",
        ]),
    ]
}

fn validate_existing_nft_base_chain_topology(chain: &str) -> Result<(), OrchError> {
    let listing = command_stdout("nft", &["-j", "list", "chain", "ip", NFT_TABLE, chain])?;
    validate_nft_base_chain_topology_json(chain, &listing)
}

fn validate_nft_base_chain_topology_json(chain: &str, listing: &str) -> Result<(), OrchError> {
    let (expected_type, expected_hook, expected_priority) = match chain {
        NFT_CHAIN => ("nat", "postrouting", 100),
        NFT_FWD_CHAIN => ("filter", "forward", 0),
        NFT_INPUT_CHAIN => ("filter", "input", 0),
        _ => {
            return Err(OrchError::Internal(format!(
                "net: no expected topology for nft base chain {chain}"
            )))
        }
    };
    let value: serde_json::Value = serde_json::from_str(listing).map_err(|error| {
        OrchError::Internal(format!(
            "net: parse nft JSON for {NFT_TABLE}/{chain}: {error}"
        ))
    })?;
    let Some(nftables) = value.get("nftables").and_then(serde_json::Value::as_array) else {
        return Err(OrchError::Internal(format!(
            "net: nft JSON for {NFT_TABLE}/{chain} has no nftables array"
        )));
    };
    let Some(base_chain) = nftables
        .iter()
        .filter_map(|entry| entry.get("chain"))
        .find(|entry| {
            entry.get("family").and_then(serde_json::Value::as_str) == Some("ip")
                && entry.get("table").and_then(serde_json::Value::as_str) == Some(NFT_TABLE)
                && entry.get("name").and_then(serde_json::Value::as_str) == Some(chain)
        })
    else {
        return Err(OrchError::Internal(format!(
            "net: nft base chain ip {NFT_TABLE} {chain} is missing"
        )));
    };
    let valid = base_chain.get("type").and_then(serde_json::Value::as_str) == Some(expected_type)
        && base_chain.get("hook").and_then(serde_json::Value::as_str) == Some(expected_hook)
        && base_chain.get("prio").and_then(serde_json::Value::as_i64) == Some(expected_priority)
        && base_chain.get("policy").and_then(serde_json::Value::as_str) == Some("accept");
    if valid {
        Ok(())
    } else {
        Err(OrchError::Internal(format!(
            "net: unexpected nft base-chain topology for ip {NFT_TABLE} {chain}"
        )))
    }
}

fn tap_provision_argv(alloc: &NetAlloc, uplink: &str) -> Vec<Vec<String>> {
    let tap = tap_name(alloc.idx);
    let ingress_table = ingress_table_name(alloc.idx);
    let ingress_comment = nft_quote(&ingress_comment(alloc));
    let guard_comment = nft_quote(&guard_comment(alloc));
    let input_comment = nft_quote(&input_comment(alloc));
    let interface = nft_quote(&tap);
    let uplink = nft_quote(uplink);
    let mut argv = vec![vec![
        "ip".into(),
        "tuntap".into(),
        "add".into(),
        "dev".into(),
        tap.clone(),
        "mode".into(),
        "tap".into(),
    ]];
    argv.extend(tap_sysctl_argv(&tap));
    argv.extend([
        vec![
            "nft".into(),
            "add".into(),
            "table".into(),
            "netdev".into(),
            ingress_table.clone(),
        ],
        vec![
            "nft".into(),
            "add".into(),
            "chain".into(),
            "netdev".into(),
            ingress_table.clone(),
            NFT_INGRESS_CHAIN.into(),
            format!("{{ type filter hook ingress device {tap} priority filter ; policy drop ; }}"),
        ],
        vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "netdev".into(),
            ingress_table.clone(),
            NFT_INGRESS_CHAIN.into(),
            "ether".into(),
            "type".into(),
            "arp".into(),
            "accept".into(),
            "comment".into(),
            ingress_comment.clone(),
        ],
        vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "netdev".into(),
            ingress_table,
            NFT_INGRESS_CHAIN.into(),
            "ether".into(),
            "type".into(),
            "ip".into(),
            "accept".into(),
            "comment".into(),
            ingress_comment,
        ],
        vec![
            "ip".into(),
            "addr".into(),
            "add".into(),
            format!("{}/{}", alloc.host_ip, alloc.prefix),
            "dev".into(),
            tap.clone(),
        ],
        vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "ip".into(),
            NFT_TABLE.into(),
            NFT_FWD_CHAIN.into(),
            "iifname".into(),
            interface.clone(),
            "ip".into(),
            "saddr".into(),
            "!=".into(),
            alloc.guest_ip.clone(),
            "counter".into(),
            "drop".into(),
            "comment".into(),
            guard_comment.clone(),
        ],
        vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "ip".into(),
            NFT_TABLE.into(),
            NFT_FWD_CHAIN.into(),
            "iifname".into(),
            interface.clone(),
            "ip".into(),
            "saddr".into(),
            alloc.guest_ip.clone(),
            "ip".into(),
            "daddr".into(),
            "172.16.0.0/16".into(),
            "drop".into(),
            "comment".into(),
            guard_comment.clone(),
        ],
        vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "ip".into(),
            NFT_TABLE.into(),
            NFT_FWD_CHAIN.into(),
            "iifname".into(),
            interface.clone(),
            "ip".into(),
            "saddr".into(),
            alloc.guest_ip.clone(),
            "oifname".into(),
            "!=".into(),
            uplink,
            "drop".into(),
            "comment".into(),
            guard_comment,
        ],
        vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "ip".into(),
            NFT_TABLE.into(),
            NFT_INPUT_CHAIN.into(),
            "iifname".into(),
            interface.clone(),
            "ip".into(),
            "saddr".into(),
            "!=".into(),
            alloc.guest_ip.clone(),
            "counter".into(),
            "drop".into(),
            "comment".into(),
            input_comment.clone(),
        ],
        vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "ip".into(),
            NFT_TABLE.into(),
            NFT_INPUT_CHAIN.into(),
            "iifname".into(),
            interface.clone(),
            "ct".into(),
            "state".into(),
            "established,related".into(),
            "accept".into(),
            "comment".into(),
            input_comment.clone(),
        ],
        vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "ip".into(),
            NFT_TABLE.into(),
            NFT_INPUT_CHAIN.into(),
            "iifname".into(),
            interface,
            "drop".into(),
            "comment".into(),
            input_comment,
        ],
    ]);
    argv
}

fn recovered_tap_reconcile_argv(alloc: &NetAlloc, uplink: &str) -> Vec<Vec<String>> {
    tap_provision_argv(alloc, uplink)
        .into_iter()
        .filter_map(|mut argv| {
            if argv.starts_with(&["ip".into(), "tuntap".into(), "add".into()]) {
                return None;
            }
            if argv.starts_with(&["ip".into(), "link".into(), "set".into()]) {
                return None;
            }
            if argv.starts_with(&["ip".into(), "addr".into(), "add".into()]) {
                argv[2] = "replace".into();
            }
            Some(argv)
        })
        .collect()
}

fn tap_sysctl_argv(tap: &str) -> Vec<Vec<String>> {
    tap_sysctl_settings(tap)
        .into_iter()
        .map(|(setting, value)| vec!["sysctl".into(), "-qw".into(), format!("{setting}={value}")])
        .collect()
}

fn tap_sysctl_settings(tap: &str) -> [(String, &'static str); 6] {
    [
        (format!("net.ipv6.conf.{tap}.disable_ipv6"), "1"),
        (format!("net.ipv6.conf.{tap}.forwarding"), "0"),
        (format!("net.ipv6.conf.{tap}.accept_ra"), "0"),
        (format!("net.ipv6.conf.{tap}.autoconf"), "0"),
        (format!("net.ipv6.conf.{tap}.accept_redirects"), "0"),
        (format!("net.ipv4.conf.{tap}.rp_filter"), "1"),
    ]
}

fn masquerade_nft_argv(alloc: &NetAlloc, uplink: &str) -> Vec<String> {
    vec![
        "nft".into(),
        "add".into(),
        "rule".into(),
        "ip".into(),
        NFT_TABLE.into(),
        NFT_CHAIN.into(),
        "iifname".into(),
        nft_quote(&tap_name(alloc.idx)),
        "ip".into(),
        "saddr".into(),
        alloc.guest_ip.clone(),
        "oifname".into(),
        nft_quote(uplink),
        "masquerade".into(),
        "comment".into(),
        nft_quote(&nft_comment(alloc)),
    ]
}

fn command_argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

fn nft_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn run_argv(argv: &[String]) -> Result<(), OrchError> {
    let (command, args) = argv
        .split_first()
        .ok_or_else(|| OrchError::Internal("empty network command".into()))?;
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    run(command, &args)
}

fn run_nft_script(script: &str) -> Result<(), OrchError> {
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| OrchError::Internal(format!("nft -f -: {e}")))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| OrchError::Internal("nft -f - stdin unavailable".into()))?
        .write_all(script.as_bytes())
        .map_err(|e| OrchError::Internal(format!("write nft recovery quarantine: {e}")))?;
    let output = child
        .wait_with_output()
        .map_err(|e| OrchError::Internal(format!("nft -f -: {e}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(OrchError::Internal(format!(
            "nft -f - failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn run_nft_allowing_existing(argv: &[String]) -> Result<(), OrchError> {
    match run_argv(argv) {
        Ok(()) => Ok(()),
        Err(error) if error.to_string().contains("File exists") => Ok(()),
        Err(error) => Err(error),
    }
}

fn default_uplink() -> Result<String, OrchError> {
    let out = Command::new("ip")
        .args(["route", "get", "8.8.8.8"])
        .output()
        .map_err(|e| OrchError::Internal(format!("ip route get: {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout);
    // "8.8.8.8 via 172.31.32.1 dev enp39s0 src ..." → take the token after "dev".
    let mut it = text.split_whitespace();
    while let Some(tok) = it.next() {
        if tok == "dev" {
            if let Some(dev) = it.next() {
                return Ok(dev.to_string());
            }
        }
    }
    Err(OrchError::Internal(
        "could not detect default uplink".into(),
    ))
}

fn run(cmd: &str, args: &[&str]) -> Result<(), OrchError> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| OrchError::Internal(format!("{cmd}: {e}")))?;
    if !out.status.success() {
        return Err(OrchError::Internal(format!(
            "{cmd} {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

fn command_stdout(cmd: &str, args: &[&str]) -> Result<String, OrchError> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| OrchError::Internal(format!("{cmd}: {e}")))?;
    if !out.status.success() {
        return Err(OrchError::Internal(format!(
            "{cmd} {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn strict_tap_names_from_link_json(listing: &str) -> Result<Vec<String>, OrchError> {
    let links: Vec<serde_json::Value> = serde_json::from_str(listing).map_err(|error| {
        OrchError::Internal(format!("parse structured ip link output: {error}"))
    })?;
    let mut taps = links
        .into_iter()
        .filter_map(|link| {
            link.get("ifname")
                .and_then(serde_json::Value::as_str)
                .filter(|name| slot_from_tap(name).is_some())
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    taps.sort();
    taps.dedup();
    Ok(taps)
}

fn discover_strict_tap_names() -> Result<Vec<String>, OrchError> {
    strict_tap_names_from_link_json(&command_stdout("ip", &["-j", "link", "show"])?)
}

fn strict_tap_is_absent(tap: &str) -> Result<bool, OrchError> {
    Ok(!discover_strict_tap_names()?.iter().any(|name| name == tap))
}

/// Quarantine pre-existing strict Tarit TAPs before any configuration, database,
/// image, or VM discovery can fail. Repeating this is safe: lowering an already
/// down TAP is idempotent.
pub fn startup_preflight() -> Result<Vec<String>, OrchError> {
    let taps = match discover_strict_tap_names() {
        Ok(taps) => taps,
        Err(error) => {
            return Err(catastrophic_startup_containment_error(
                "discover strict Tarit TAPs before recovery",
                vec![error.to_string()],
            ))
        }
    };
    let isolation = emergency_isolate_tap_names(&taps);
    if isolation.failures.is_empty() {
        Ok(taps)
    } else {
        Err(catastrophic_startup_containment_error(
            "contain pre-existing Tarit TAPs before recovery",
            isolation.failures,
        ))
    }
}

fn catastrophic_startup_containment_error(context: &str, mut failures: Vec<String>) -> OrchError {
    for argv in emergency_forwarding_disable_argv() {
        if let Err(error) = run_argv(&argv) {
            failures.push(format!(
                "emergency host forwarding disable failed ({}): {error}",
                argv.join(" ")
            ));
        }
    }
    OrchError::Internal(format!(
        "net: {context}; catastrophic containment attempted: {}",
        failures.join("; ")
    ))
}

#[derive(Default)]
struct IsolationReport {
    contained: BTreeSet<String>,
    failures: Vec<String>,
}

fn emergency_isolate_tap_names(taps: &[String]) -> IsolationReport {
    let mut report = IsolationReport::default();
    for tap in taps {
        match run("ip", &["link", "set", tap, "down"]) {
            Ok(()) => {
                report.contained.insert(tap.clone());
            }
            Err(link_down_error) => {
                report.failures.push(format!(
                    "emergency link-down failed for {tap}: {link_down_error}"
                ));
                match run("ip", &["link", "del", tap]) {
                    Ok(()) => {
                        report.contained.insert(tap.clone());
                        tracing::warn!(
                            tap,
                            "net: deleted recovered tap after link-down isolation failed"
                        );
                    }
                    Err(link_delete_error) => report.failures.push(format!(
                        "emergency link-delete failed for {tap}: {link_delete_error}"
                    )),
                }
            }
        }
    }
    report
}

fn emergency_forwarding_disable_argv() -> Vec<Vec<String>> {
    vec![
        command_argv(&["sysctl", "-qw", "net.ipv4.ip_forward=0"]),
        command_argv(&["sysctl", "-qw", "net.ipv6.conf.all.forwarding=0"]),
    ]
}

fn discover_taps() -> Result<Vec<TapCandidate>, OrchError> {
    let text = command_stdout("ip", &["-o", "link", "show"])?;
    Ok(text
        .lines()
        .filter_map(parse_ip_link_name)
        .filter(|name| slot_from_tap(name).is_some())
        .map(|name| TapCandidate {
            age: tap_age(&name),
            name,
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TapCandidate {
    name: String,
    age: Option<Duration>,
}

fn stale_taps_to_sweep(
    taps: &[TapCandidate],
    active_slots: &HashSet<u32>,
    min_age: Duration,
) -> Vec<TapCandidate> {
    taps.iter()
        .filter(|tap| {
            let Some(slot) = slot_from_tap(&tap.name) else {
                return false;
            };
            !active_slots.contains(&slot) && tap.age.is_some_and(|age| age >= min_age)
        })
        .cloned()
        .collect()
}

fn tap_age(name: &str) -> Option<Duration> {
    std::fs::metadata(format!("/sys/class/net/{name}"))
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()
}

fn parse_ip_link_name(line: &str) -> Option<String> {
    let mut parts = line.splitn(3, ':');
    parts.next()?;
    let name = parts.next()?.trim().split('@').next()?.to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn tap_name(slot: u32) -> String {
    format!("{TAP_PREFIX}{slot}")
}

fn slot_from_tap(name: &str) -> Option<u32> {
    let raw = name.strip_prefix(TAP_PREFIX)?;
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let slot = raw.parse::<u32>().ok()?;
    (slot < NET_POOL_SLOTS).then_some(slot)
}

fn ingress_table_name(slot: u32) -> String {
    format!("{NFT_INGRESS_TABLE_PREFIX}{slot}")
}

fn ingress_slot_from_table_name(table: &str) -> Option<u32> {
    let raw = table.strip_prefix(NFT_INGRESS_TABLE_PREFIX)?;
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let slot = raw.parse::<u32>().ok()?;
    (slot < NET_POOL_SLOTS).then_some(slot)
}

fn ingress_table_names() -> Result<Vec<String>, OrchError> {
    Ok(netdev_table_names()?
        .into_iter()
        .filter(|table| ingress_slot_from_table_name(table).is_some())
        .collect())
}

fn netdev_table_names() -> Result<Vec<String>, OrchError> {
    let listing = command_stdout("nft", &["list", "tables", "netdev"])?;
    Ok(listing
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            match (words.next(), words.next(), words.next(), words.next()) {
                (Some("table"), Some("netdev"), Some(table), None) => Some(table.to_string()),
                _ => None,
            }
        })
        .collect())
}

#[cfg(test)]
fn stale_ingress_tables_to_sweep(tables: &[String], active_slots: &HashSet<u32>) -> Vec<String> {
    tables
        .iter()
        .filter(|table| {
            ingress_slot_from_table_name(table).is_some_and(|slot| !active_slots.contains(&slot))
        })
        .cloned()
        .collect()
}

fn delete_ingress_table_argv(slot: u32) -> Vec<String> {
    vec![
        "nft".into(),
        "delete".into(),
        "table".into(),
        "netdev".into(),
        ingress_table_name(slot),
    ]
}

fn nft_listing_tokens(listing: &str) -> Option<Vec<String>> {
    let listing = listing
        .lines()
        .map(|line| {
            line.rsplit_once("# handle ")
                .filter(|(_, handle)| {
                    let handle = handle.trim();
                    !handle.is_empty() && handle.bytes().all(|byte| byte.is_ascii_digit())
                })
                .map(|(rule, _)| rule.trim_end())
                .unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for character in listing.chars() {
        if quoted {
            token.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
                tokens.push(std::mem::take(&mut token));
            }
            continue;
        }
        match character {
            '"' => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
                token.push(character);
                quoted = true;
            }
            '{' | '}' | ';' => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
                tokens.push(character.to_string());
            }
            character if character.is_whitespace() => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
            }
            _ => token.push(character),
        }
    }
    if quoted {
        None
    } else {
        if !token.is_empty() {
            tokens.push(token);
        }
        Some(tokens)
    }
}

fn consume_nft_tokens(tokens: &[String], index: &mut usize, expected: &[String]) -> bool {
    if tokens.get(*index..*index + expected.len()) != Some(expected) {
        return false;
    }
    *index += expected.len();
    true
}

fn ingress_table_belongs_to_alloc(listing: &str, alloc: &NetAlloc) -> bool {
    let Some(tokens) = nft_listing_tokens(listing) else {
        return false;
    };
    let mut index = 0;
    let comment = nft_quote(&ingress_comment(alloc));
    let table = ingress_table_name(alloc.idx);
    let device = nft_quote(&alloc.tap);
    let required = [
        vec!["table".into(), "netdev".into(), table, "{".into()],
        vec!["chain".into(), NFT_INGRESS_CHAIN.into(), "{".into()],
        vec![
            "type".into(),
            "filter".into(),
            "hook".into(),
            "ingress".into(),
            "device".into(),
            device,
            "priority".into(),
            "filter".into(),
            ";".into(),
            "policy".into(),
            "drop".into(),
            ";".into(),
        ],
        vec![
            "ether".into(),
            "type".into(),
            "arp".into(),
            "accept".into(),
            "comment".into(),
            comment.clone(),
        ],
        vec![
            "ether".into(),
            "type".into(),
            "ip".into(),
            "accept".into(),
            "comment".into(),
            comment,
        ],
    ];
    for (rule_index, expected) in required.iter().enumerate() {
        if !consume_nft_tokens(&tokens, &mut index, expected) {
            return false;
        }
        if matches!(rule_index, 3 | 4) && tokens.get(index).is_some_and(|token| token == ";") {
            index += 1;
        }
    }
    tokens.get(index) == Some(&"}".to_string())
        && tokens.get(index + 1) == Some(&"}".to_string())
        && index + 2 == tokens.len()
}

fn delete_ingress_table_for_alloc(alloc: &NetAlloc) -> Result<usize, OrchError> {
    let table = ingress_table_name(alloc.idx);
    if !ingress_table_names()?.iter().any(|name| name == &table) {
        return Ok(0);
    }
    let listing = command_stdout("nft", &["-a", "list", "table", "netdev", &table])?;
    if !ingress_table_belongs_to_alloc(&listing, alloc) {
        return Err(OrchError::Internal(format!(
            "net: refusing to delete ingress table {table}: it is not the exact managed policy for VM {} on {}",
            alloc.vm_id, alloc.tap
        )));
    }
    run_argv(&delete_ingress_table_argv(alloc.idx))?;
    Ok(1)
}

fn ingress_table_owner(listing: &str, slot: u32) -> Option<NetAlloc> {
    let prefix = format!("taritd-ingress slot={slot} vm=");
    let mut owners = listing
        .lines()
        .filter_map(|line| line.rsplit_once(" comment \""))
        .filter_map(|(_, comment)| comment.split_once('"').map(|(comment, _)| comment))
        .filter_map(|comment| {
            let rest = comment.strip_prefix(&prefix)?;
            let (vm_id, tap) = rest.split_once(" tap=")?;
            (tap == tap_name(slot) && !vm_id.contains(char::is_whitespace))
                .then(|| Uuid::parse_str(vm_id).ok())
                .flatten()
        })
        .collect::<BTreeSet<_>>();
    (owners.len() == 1)
        .then(|| owners.pop_first())
        .flatten()
        .and_then(|vm_id| NetAlloc::for_slot(vm_id, slot).ok())
}

fn delete_ingress_table_for_slot(slot: u32) -> Result<usize, OrchError> {
    let table = ingress_table_name(slot);
    if !ingress_table_names()?.iter().any(|name| name == &table) {
        return Ok(0);
    }
    let listing = command_stdout("nft", &["-a", "list", "table", "netdev", &table])?;
    let alloc = ingress_table_owner(&listing, slot).ok_or_else(|| {
        OrchError::Internal(format!(
            "net: refusing to delete ingress table {table}: exact managed owner is unknown"
        ))
    })?;
    delete_ingress_table_for_alloc(&alloc)
}

fn nft_comment(alloc: &NetAlloc) -> String {
    format!(
        "taritd slot={} vm={} tap={}",
        alloc.idx,
        alloc.vm_id,
        tap_name(alloc.idx)
    )
}

fn ingress_comment(alloc: &NetAlloc) -> String {
    format!(
        "taritd-ingress slot={} vm={} tap={}",
        alloc.idx,
        alloc.vm_id,
        tap_name(alloc.idx)
    )
}

fn has_exact_allocation_tag(listing: &str, alloc: &NetAlloc) -> bool {
    let comments = [
        nft_comment(alloc),
        egress_comment(alloc),
        guard_comment(alloc),
        input_comment(alloc),
        recovery_quarantine_comment(alloc),
        egress_update_quarantine_comment(alloc),
        ingress_comment(alloc),
    ];
    listing.lines().any(|line| {
        comments
            .iter()
            .any(|comment| line.contains(&format!("comment \"{comment}\"")))
    })
}

fn guard_comment(alloc: &NetAlloc) -> String {
    format!(
        "taritd-guard slot={} vm={} tap={}",
        alloc.idx,
        alloc.vm_id,
        tap_name(alloc.idx)
    )
}

fn input_comment(alloc: &NetAlloc) -> String {
    format!(
        "taritd-input slot={} vm={} tap={}",
        alloc.idx,
        alloc.vm_id,
        tap_name(alloc.idx)
    )
}

fn recovery_quarantine_comment(alloc: &NetAlloc) -> String {
    format!(
        "taritd-recovery-quarantine slot={} vm={} tap={}",
        alloc.idx,
        alloc.vm_id,
        tap_name(alloc.idx)
    )
}

fn egress_update_quarantine_comment(alloc: &NetAlloc) -> String {
    format!(
        "taritd-egress-update-quarantine slot={} vm={} tap={}",
        alloc.idx,
        alloc.vm_id,
        tap_name(alloc.idx)
    )
}

fn recovery_quarantine_script(allocations: &[NetAlloc]) -> String {
    allocations
        .iter()
        .flat_map(|alloc| {
            let interface = nft_quote(&tap_name(alloc.idx));
            let comment = nft_quote(&recovery_quarantine_comment(alloc));
            [
                format!(
                    "insert rule ip {NFT_TABLE} {NFT_FWD_CHAIN} iifname {interface} drop comment {comment}"
                ),
                format!(
                    "insert rule ip {NFT_TABLE} {NFT_INPUT_CHAIN} iifname {interface} drop comment {comment}"
                ),
            ]
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn recovery_quarantine_delete_script(allocations: &[NetAlloc]) -> Result<String, OrchError> {
    let mut commands = Vec::new();
    for chain in [NFT_FWD_CHAIN, NFT_INPUT_CHAIN] {
        let listing = command_stdout("nft", &["-a", "list", "chain", "ip", NFT_TABLE, chain])?;
        for alloc in allocations {
            let handles = listing
                .lines()
                .filter(|line| {
                    is_recognized_taritd_rule(chain, line)
                        && is_recovery_quarantine_rule_for_alloc(line, alloc)
                })
                .filter_map(nft_handle)
                .collect::<Vec<_>>();
            if handles.is_empty() {
                return Err(OrchError::Internal(format!(
                    "net: missing recovery quarantine for {} in {chain}",
                    alloc.tap
                )));
            }
            commands.extend(
                handles
                    .into_iter()
                    .map(|handle| format!("delete rule ip {NFT_TABLE} {chain} handle {handle}")),
            );
        }
    }
    Ok(commands.join("\n") + "\n")
}

fn delete_nft_rules_in_chain(
    chain: &str,
    mut predicate: impl FnMut(&str) -> bool,
) -> Result<usize, OrchError> {
    let listing = command_stdout("nft", &["-a", "list", "chain", "ip", NFT_TABLE, chain])?;
    let handles = listing
        .lines()
        .filter(|line| predicate(line))
        .filter_map(nft_handle)
        .collect::<Vec<_>>();

    for handle in &handles {
        run(
            "nft",
            &["delete", "rule", "ip", NFT_TABLE, chain, "handle", handle],
        )?;
    }
    Ok(handles.len())
}

/// Comment tag stamped on every egress rule for a VM, so the rules can be found
/// and removed on update or teardown.
fn egress_comment(alloc: &NetAlloc) -> String {
    format!(
        "taritd-egress slot={} vm={} tap={}",
        alloc.idx,
        alloc.vm_id,
        tap_name(alloc.idx)
    )
}

/// Parse one `cidr[:port[/proto]]` allowlist entry, mirroring the VMM grammar.
/// Returns `(cidr, port, proto)` where `proto` is `None` for "any port/proto".
/// An explicitly specified port zero is rejected.
fn parse_egress_entry(entry: &str) -> Result<(String, u16, Option<&'static str>), OrchError> {
    if entry.is_empty() {
        return Err(OrchError::BadRequest("empty egress rule".into()));
    }
    if entry.contains(['[', ']']) || entry.matches(':').count() > 1 {
        return Err(OrchError::BadRequest(format!(
            "bad egress rule {entry:?}: IPv6 CIDRs are unsupported"
        )));
    }
    let (cidr, port_proto) = match entry.split_once(':') {
        Some(("", _)) => {
            return Err(OrchError::BadRequest(format!(
                "bad egress rule {entry:?}: missing CIDR"
            )))
        }
        Some((cidr, rest)) => (cidr, Some(rest)),
        None => (entry, None),
    };
    let cidr = parse_ipv4_egress_cidr(cidr, entry)?.to_string();
    let Some(port_proto) = port_proto else {
        return Ok((cidr, 0, None));
    };
    let (port_str, proto) = match port_proto.split_once('/') {
        Some((port, "tcp")) => (port, "tcp"),
        Some((port, "udp")) => (port, "udp"),
        Some((_, other)) => {
            return Err(OrchError::BadRequest(format!(
                "bad egress rule {entry:?}: unknown proto {other:?}"
            )))
        }
        None => (port_proto, "tcp"),
    };
    let port: u16 = port_str.parse().map_err(|_| {
        OrchError::BadRequest(format!(
            "bad egress rule {entry:?}: invalid port {port_str:?}"
        ))
    })?;
    if port == 0 {
        return Err(OrchError::BadRequest(format!(
            "bad egress rule {entry:?}: invalid port {port_str:?}"
        )));
    }
    Ok((cidr, port, Some(proto)))
}

fn parse_ipv4_egress_cidr(cidr: &str, entry: &str) -> Result<Ipv4Net, OrchError> {
    match cidr.parse::<IpAddr>() {
        Ok(IpAddr::V4(addr)) => Ok(Ipv4Net::from(addr)),
        Ok(IpAddr::V6(_)) => Err(OrchError::BadRequest(format!(
            "bad egress rule {entry:?}: IPv6 CIDRs are unsupported"
        ))),
        Err(_) => match cidr.parse::<IpNet>() {
            Ok(IpNet::V4(cidr)) => Ok(cidr.trunc()),
            Ok(IpNet::V6(_)) => Err(OrchError::BadRequest(format!(
                "bad egress rule {entry:?}: IPv6 CIDRs are unsupported"
            ))),
            Err(_) => Err(OrchError::BadRequest(format!(
                "bad egress rule {entry:?}: invalid IPv4 CIDR {cidr:?}"
            ))),
        },
    }
}

/// Build the nft `add rule` argv for one allowlist entry in the forward chain.
fn compile_host_egress_rule(
    alloc: &NetAlloc,
    entry: &str,
    comment: &str,
) -> Result<Vec<String>, OrchError> {
    let (cidr, port, proto) = parse_egress_entry(entry)?;
    let tap = nft_quote(&tap_name(alloc.idx));
    let mut args: Vec<String> = vec![
        "add".into(),
        "rule".into(),
        "ip".into(),
        NFT_TABLE.into(),
        NFT_FWD_CHAIN.into(),
        "iifname".into(),
        tap,
        "ip".into(),
        "saddr".into(),
        alloc.guest_ip.clone(),
        "ip".into(),
        "daddr".into(),
        cidr,
    ];
    if let Some(proto) = proto {
        args.push(proto.into());
        args.push("dport".into());
        args.push(port.to_string());
    }
    args.push("accept".into());
    args.push("comment".into());
    args.push(comment.into());
    Ok(args)
}

fn egress_policy_argv(
    alloc: &NetAlloc,
    allowlist: &[String],
    allow_existing: bool,
) -> Result<Vec<Vec<String>>, OrchError> {
    let comment = nft_quote(&egress_comment(alloc));
    let tap = nft_quote(&tap_name(alloc.idx));
    let mut rules = Vec::with_capacity(allowlist.len() + usize::from(allow_existing) + 1);
    if allow_existing {
        rules.push(vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "ip".into(),
            NFT_TABLE.into(),
            NFT_FWD_CHAIN.into(),
            "iifname".into(),
            tap.clone(),
            "ip".into(),
            "saddr".into(),
            alloc.guest_ip.clone(),
            "ct".into(),
            "state".into(),
            "established,related".into(),
            "accept".into(),
            "comment".into(),
            comment.clone(),
        ]);
    }
    for entry in allowlist {
        let mut rule = compile_host_egress_rule(alloc, entry, &comment)?;
        rule.insert(0, "nft".into());
        rules.push(rule);
    }
    rules.push(vec![
        "nft".into(),
        "add".into(),
        "rule".into(),
        "ip".into(),
        NFT_TABLE.into(),
        NFT_FWD_CHAIN.into(),
        "iifname".into(),
        tap,
        "ip".into(),
        "saddr".into(),
        alloc.guest_ip.clone(),
        "drop".into(),
        "comment".into(),
        comment,
    ]);
    Ok(rules)
}

fn egress_replacement_script(
    alloc: &NetAlloc,
    policy: &EgressPolicy,
    listing: &str,
) -> Result<String, OrchError> {
    let mut commands = listing
        .lines()
        .filter(|line| {
            is_recognized_taritd_rule(NFT_FWD_CHAIN, line)
                && is_egress_nft_rule_for_alloc(line, alloc)
        })
        .map(|line| {
            nft_handle(line).ok_or_else(|| {
                OrchError::Internal(format!(
                    "net: egress rule for {} has no nft handle",
                    alloc.tap
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|handle| format!("delete rule ip {NFT_TABLE} {NFT_FWD_CHAIN} handle {handle}"))
        .collect::<Vec<_>>();
    commands.extend(
        egress_policy_argv(alloc, &policy.allowlist, policy.allow_existing)?
            .into_iter()
            .map(|argv| {
                debug_assert_eq!(argv.first().map(String::as_str), Some("nft"));
                argv[1..].join(" ")
            }),
    );
    Ok(commands.join("\n") + "\n")
}

fn nft_handle(line: &str) -> Option<String> {
    let (_, handle) = line.rsplit_once("# handle ")?;
    let handle = handle.trim();
    (!handle.is_empty() && handle.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| handle.to_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaritdNftRuleKind {
    Nat,
    Egress,
    Guard,
    Input,
    RecoveryQuarantine,
    EgressUpdateQuarantine,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaritdNftRuleTag {
    kind: TaritdNftRuleKind,
    slot: u32,
    vm_id: Uuid,
    tap: String,
}

fn parse_taritd_nft_rule_tag(line: &str) -> Option<TaritdNftRuleTag> {
    let (_, comment) = line.rsplit_once(" comment \"")?;
    let (comment, suffix) = comment.split_once('"')?;
    if !suffix.trim().is_empty() && nft_handle(line).is_none() {
        return None;
    }
    let (kind, fields) = [
        (
            "taritd-recovery-quarantine ",
            TaritdNftRuleKind::RecoveryQuarantine,
        ),
        (
            "taritd-egress-update-quarantine ",
            TaritdNftRuleKind::EgressUpdateQuarantine,
        ),
        ("taritd-egress ", TaritdNftRuleKind::Egress),
        ("taritd-guard ", TaritdNftRuleKind::Guard),
        ("taritd-input ", TaritdNftRuleKind::Input),
        ("taritd ", TaritdNftRuleKind::Nat),
    ]
    .into_iter()
    .find_map(|(prefix, kind)| comment.strip_prefix(prefix).map(|fields| (kind, fields)))?;
    let mut fields = fields.split_whitespace();
    let slot = fields.next()?.strip_prefix("slot=")?.parse::<u32>().ok()?;
    let vm_id = Uuid::parse_str(fields.next()?.strip_prefix("vm=")?).ok()?;
    let tap = fields.next()?.strip_prefix("tap=")?;
    if fields.next().is_some() || tap != tap_name(slot) {
        return None;
    }
    Some(TaritdNftRuleTag {
        kind,
        slot,
        vm_id,
        tap: tap.to_string(),
    })
}

fn is_taritd_nft_rule_for_alloc(line: &str, alloc: &NetAlloc) -> bool {
    parse_taritd_nft_rule_tag(line).is_some_and(|tag| {
        tag.slot == alloc.idx && tag.vm_id == alloc.vm_id && tag.tap == alloc.tap
    })
}

fn is_egress_nft_rule_for_alloc(line: &str, alloc: &NetAlloc) -> bool {
    parse_taritd_nft_rule_tag(line).is_some_and(|tag| {
        tag.kind == TaritdNftRuleKind::Egress
            && tag.slot == alloc.idx
            && tag.vm_id == alloc.vm_id
            && tag.tap == alloc.tap
    })
}

fn is_recovery_nft_rule_for_alloc(line: &str, alloc: &NetAlloc) -> bool {
    parse_taritd_nft_rule_tag(line).is_some_and(|tag| {
        tag.slot == alloc.idx
            && tag.vm_id == alloc.vm_id
            && tag.tap == alloc.tap
            && matches!(
                tag.kind,
                TaritdNftRuleKind::Nat
                    | TaritdNftRuleKind::Egress
                    | TaritdNftRuleKind::Guard
                    | TaritdNftRuleKind::Input
            )
    })
}

fn is_recovery_quarantine_rule_for_alloc(line: &str, alloc: &NetAlloc) -> bool {
    parse_taritd_nft_rule_tag(line).is_some_and(|tag| {
        tag.kind == TaritdNftRuleKind::RecoveryQuarantine
            && tag.slot == alloc.idx
            && tag.vm_id == alloc.vm_id
            && tag.tap == alloc.tap
    })
}

fn is_egress_update_quarantine_rule_for_alloc(line: &str, alloc: &NetAlloc) -> bool {
    parse_taritd_nft_rule_tag(line).is_some_and(|tag| {
        tag.kind == TaritdNftRuleKind::EgressUpdateQuarantine
            && tag.slot == alloc.idx
            && tag.vm_id == alloc.vm_id
            && tag.tap == alloc.tap
    })
}

fn is_stale_recovery_rule_for_alloc(line: &str, alloc: &NetAlloc) -> bool {
    parse_taritd_nft_rule_tag(line).is_some_and(|tag| {
        tag.slot == alloc.idx
            && tag.tap == alloc.tap
            && !(tag.kind == TaritdNftRuleKind::RecoveryQuarantine && tag.vm_id == alloc.vm_id)
    })
}

fn is_taritd_nft_rule_for_slot(line: &str, slot: u32) -> bool {
    parse_taritd_nft_rule_tag(line).is_some_and(|tag| tag.slot == slot)
}

fn is_orphan_taritd_nft_rule(line: &str, active: &BTreeMap<u32, Uuid>) -> bool {
    parse_taritd_nft_rule_tag(line)
        .is_some_and(|tag| active.get(&tag.slot).copied() != Some(tag.vm_id))
}

fn security_chain_rule_text(line: &str) -> Option<String> {
    let (rule, _) = line.rsplit_once(" comment \"")?;
    let words = rule.split_whitespace().collect::<Vec<_>>();
    let mut normalized = Vec::with_capacity(words.len());
    let mut index = 0;
    while index < words.len() {
        if words[index] == "counter"
            && matches!(
                words.get(index + 1..index + 5),
                Some(["packets", packets, "bytes", bytes])
                    if packets.bytes().all(|byte| byte.is_ascii_digit())
                        && bytes.bytes().all(|byte| byte.is_ascii_digit())
            )
        {
            normalized.push("counter");
            index += 5;
        } else {
            normalized.push(words[index]);
            index += 1;
        }
    }
    Some(normalized.join(" "))
}

fn is_recognized_taritd_rule(chain: &str, line: &str) -> bool {
    let Some(tag) = parse_taritd_nft_rule_tag(line) else {
        return false;
    };
    let Some(rule) = security_chain_rule_text(line) else {
        return false;
    };
    match chain {
        NFT_CHAIN => {
            let tap = nft_quote(&tag.tap);
            let Some(alloc) = NetAlloc::for_slot(tag.vm_id, tag.slot).ok() else {
                return false;
            };
            tag.kind == TaritdNftRuleKind::Nat
                && valid_masquerade_rule(&rule, &tap, &alloc.guest_ip)
        }
        NFT_FWD_CHAIN | NFT_INPUT_CHAIN => valid_taritd_security_rule(chain, &rule, &tag).is_some(),
        _ => false,
    }
}

fn validate_taritd_nat_chain(listing: &str) -> Result<(), OrchError> {
    for line in listing
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if !line.contains("comment \"taritd") {
            continue;
        }
        if !is_recognized_taritd_rule(NFT_CHAIN, line) {
            return Err(OrchError::Internal(format!(
                "net: refusing activation: invalid tagged rule shape in {NFT_CHAIN}: {line}"
            )));
        }
    }
    Ok(())
}

fn validate_taritd_security_chain(chain: &str, listing: &str) -> Result<(), OrchError> {
    if !matches!(chain, NFT_FWD_CHAIN | NFT_INPUT_CHAIN) {
        return Err(OrchError::Internal(format!(
            "net: {chain} is not a Tarit security chain"
        )));
    }
    let mut order = HashMap::<String, Vec<(usize, SecurityRuleRole)>>::new();
    for (index, line) in listing.lines().enumerate() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with("table ")
            || line.starts_with("chain ")
            || line.starts_with("type ")
            || line == "}"
        {
            continue;
        }
        let tag = parse_taritd_nft_rule_tag(line).ok_or_else(|| {
            OrchError::Internal(format!(
                "net: refusing activation: unrecognized rule in closed Tarit {chain} chain: {line}"
            ))
        })?;
        let Some(rule) = security_chain_rule_text(line) else {
            return Err(OrchError::Internal(format!(
                "net: refusing activation: malformed managed rule in {chain}: {line}"
            )));
        };
        let role = valid_taritd_security_rule(chain, &rule, &tag).ok_or_else(|| {
            OrchError::Internal(format!(
                "net: refusing activation: invalid managed rule shape in {chain}: {line}"
            ))
        })?;
        order
            .entry(tag.tap.clone())
            .or_default()
            .push((index, role));
    }
    for (tap, rules) in order {
        validate_security_rule_order(chain, &tap, &rules)?;
    }
    Ok(())
}

fn validate_security_rule_order(
    chain: &str,
    tap: &str,
    rules: &[(usize, SecurityRuleRole)],
) -> Result<(), OrchError> {
    let first = |roles: &[SecurityRuleRole]| {
        rules
            .iter()
            .filter(|(_, role)| roles.contains(role))
            .map(|(index, _)| *index)
            .min()
    };
    let last = |roles: &[SecurityRuleRole]| {
        rules
            .iter()
            .filter(|(_, role)| roles.contains(role))
            .map(|(index, _)| *index)
            .max()
    };
    match chain {
        NFT_FWD_CHAIN => {
            let guards = [
                SecurityRuleRole::ForwardSourceGuard,
                SecurityRuleRole::ForwardLateralGuard,
                SecurityRuleRole::ForwardUplinkGuard,
            ];
            let first_accept = first(&[
                SecurityRuleRole::EgressStateful,
                SecurityRuleRole::EgressAllow,
            ]);
            if let (Some(last_guard), Some(first_accept)) = (last(&guards), first_accept) {
                if last_guard > first_accept {
                    return Err(OrchError::Internal(format!(
                        "net: refusing activation: {tap} guard follows an egress accept"
                    )));
                }
            }
            if let (Some(stateful), Some(allow)) = (
                first(&[SecurityRuleRole::EgressStateful]),
                first(&[SecurityRuleRole::EgressAllow]),
            ) {
                if stateful > allow {
                    return Err(OrchError::Internal(format!(
                        "net: refusing activation: {tap} stateful return follows an egress allow"
                    )));
                }
            }
            if let (Some(allow), Some(deny)) = (
                last(&[SecurityRuleRole::EgressAllow]),
                first(&[SecurityRuleRole::EgressDeny]),
            ) {
                if allow > deny {
                    return Err(OrchError::Internal(format!(
                        "net: refusing activation: {tap} default deny precedes an egress allow"
                    )));
                }
            }
        }
        NFT_INPUT_CHAIN => {
            if let (Some(source), Some(accept)) = (
                last(&[SecurityRuleRole::InputSourceGuard]),
                first(&[SecurityRuleRole::InputStateful]),
            ) {
                if source > accept {
                    return Err(OrchError::Internal(format!(
                        "net: refusing activation: {tap} input source guard follows a return accept"
                    )));
                }
            }
            if let (Some(accept), Some(deny)) = (
                last(&[SecurityRuleRole::InputStateful]),
                first(&[SecurityRuleRole::InputDeny]),
            ) {
                if accept > deny {
                    return Err(OrchError::Internal(format!(
                        "net: refusing activation: {tap} input default deny precedes a return accept"
                    )));
                }
            }
        }
        _ => unreachable!("security chain validated above"),
    }
    Ok(())
}

fn validate_complete_effective_security_policies(
    allocations: &[(NetAlloc, EgressPolicy)],
    uplink: &str,
    nat: &str,
    forward: &str,
    input: &str,
) -> Result<(), OrchError> {
    validate_taritd_nat_chain(nat)?;
    validate_taritd_security_chain(NFT_FWD_CHAIN, forward)?;
    validate_taritd_security_chain(NFT_INPUT_CHAIN, input)?;
    for (alloc, policy) in allocations {
        validate_complete_masquerade_policy(alloc, uplink, nat)?;
        validate_complete_forward_policy(alloc, policy, uplink, forward)?;
        validate_complete_input_policy(alloc, input)?;
    }
    for (chain, listing) in [
        (NFT_CHAIN, nat),
        (NFT_FWD_CHAIN, forward),
        (NFT_INPUT_CHAIN, input),
    ] {
        for line in listing
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            if line.starts_with("table ")
                || line.starts_with("chain ")
                || line.starts_with("type ")
                || line == "}"
            {
                continue;
            }
            let tag = parse_taritd_nft_rule_tag(line).ok_or_else(|| {
                OrchError::Internal(format!(
                    "net: refusing activation: unrecognized rule in closed Tarit {chain} chain: {line}"
                ))
            })?;
            if !allocations.iter().any(|(alloc, _)| {
                alloc.idx == tag.slot && alloc.vm_id == tag.vm_id && alloc.tap == tag.tap
            }) {
                return Err(OrchError::Internal(format!(
                    "net: refusing activation: {chain} contains rule for inactive allocation {}",
                    tag.tap
                )));
            }
        }
    }
    Ok(())
}

fn allocation_rules(
    chain: &str,
    listing: &str,
    alloc: &NetAlloc,
) -> Result<Vec<(usize, SecurityRuleRole, String)>, OrchError> {
    listing
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.trim();
            let tag = parse_taritd_nft_rule_tag(line)?;
            (tag.slot == alloc.idx && tag.vm_id == alloc.vm_id && tag.tap == alloc.tap)
                .then_some((index, line, tag))
        })
        .map(|(index, line, tag)| {
            let rule = security_chain_rule_text(line).ok_or_else(|| {
                OrchError::Internal(format!(
                    "net: refusing activation: malformed managed rule for {}",
                    alloc.tap
                ))
            })?;
            let role = valid_taritd_security_rule(chain, &rule, &tag).ok_or_else(|| {
                OrchError::Internal(format!(
                    "net: refusing activation: invalid managed rule for {}",
                    alloc.tap
                ))
            })?;
            Ok((index, role, rule))
        })
        .collect()
}

fn exactly_one_rule(
    alloc: &NetAlloc,
    rules: &[(usize, SecurityRuleRole, String)],
    role: SecurityRuleRole,
    expected: &str,
) -> Result<usize, OrchError> {
    let matches = rules
        .iter()
        .filter(|(_, actual_role, rule)| *actual_role == role && rule == expected)
        .map(|(index, _, _)| *index)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(OrchError::Internal(format!(
            "net: refusing activation: {} requires exactly one {role:?} rule {expected:?}, found {}",
            alloc.tap,
            matches.len()
        )));
    }
    Ok(matches[0])
}

fn validate_complete_forward_policy(
    alloc: &NetAlloc,
    policy: &EgressPolicy,
    uplink: &str,
    listing: &str,
) -> Result<(), OrchError> {
    let rules = allocation_rules(NFT_FWD_CHAIN, listing, alloc)?;
    let tap = nft_quote(&alloc.tap);
    let guards = [
        (
            SecurityRuleRole::ForwardSourceGuard,
            format!("iifname {tap} ip saddr != {} counter drop", alloc.guest_ip),
        ),
        (
            SecurityRuleRole::ForwardLateralGuard,
            format!(
                "iifname {tap} ip saddr {} ip daddr 172.16.0.0/16 drop",
                alloc.guest_ip
            ),
        ),
        (
            SecurityRuleRole::ForwardUplinkGuard,
            format!("iifname {tap} ip saddr {} oifname != ", alloc.guest_ip),
        ),
    ];
    let mut guard_positions = Vec::with_capacity(guards.len());
    for (role, expected) in guards {
        let position = match role {
            SecurityRuleRole::ForwardUplinkGuard => {
                let matches = rules
                    .iter()
                    .filter(|(_, actual_role, rule)| {
                        *actual_role == role
                            && valid_uplink_guard_rule(rule, &tap, &alloc.guest_ip, uplink)
                    })
                    .map(|(index, _, _)| *index)
                    .collect::<Vec<_>>();
                if matches.len() != 1 {
                    return Err(OrchError::Internal(format!(
                        "net: refusing activation: {} requires exactly one uplink guard, found {}",
                        alloc.tap,
                        matches.len()
                    )));
                }
                matches[0]
            }
            _ => exactly_one_rule(alloc, &rules, role, &expected)?,
        };
        guard_positions.push(position);
    }

    let mut expected_allows = Vec::with_capacity(policy.allowlist.len());
    let mut seen_allows = BTreeSet::new();
    for entry in &policy.allowlist {
        let argv = compile_host_egress_rule(alloc, entry, &nft_quote(&egress_comment(alloc)))?;
        let expected = argv[5..argv.len() - 2].join(" ");
        if !seen_allows.insert(expected.clone()) {
            return Err(OrchError::Internal(format!(
                "net: refusing activation: persisted policy for {} duplicates allow {entry:?}",
                alloc.tap
            )));
        }
        expected_allows.push(expected);
    }
    let stateful = if policy.allow_existing {
        Some(exactly_one_rule(
            alloc,
            &rules,
            SecurityRuleRole::EgressStateful,
            &format!(
                "iifname {tap} ip saddr {} ct state established,related accept",
                alloc.guest_ip
            ),
        )?)
    } else {
        None
    };
    let mut allow_positions = Vec::with_capacity(expected_allows.len());
    for expected in &expected_allows {
        allow_positions.push(exactly_one_rule(
            alloc,
            &rules,
            SecurityRuleRole::EgressAllow,
            expected,
        )?);
    }
    let deny = exactly_one_rule(
        alloc,
        &rules,
        SecurityRuleRole::EgressDeny,
        &format!("iifname {tap} ip saddr {} drop", alloc.guest_ip),
    )?;
    if rules
        .iter()
        .filter(|(_, role, _)| {
            matches!(
                role,
                SecurityRuleRole::ForwardSourceGuard
                    | SecurityRuleRole::ForwardLateralGuard
                    | SecurityRuleRole::ForwardUplinkGuard
                    | SecurityRuleRole::EgressStateful
                    | SecurityRuleRole::EgressAllow
                    | SecurityRuleRole::EgressDeny
            )
        })
        .count()
        != guard_positions.len() + usize::from(stateful.is_some()) + allow_positions.len() + 1
    {
        return Err(OrchError::Internal(format!(
            "net: refusing activation: {} has duplicate or unpersisted egress policy rules",
            alloc.tap
        )));
    }
    let first_accept = stateful
        .into_iter()
        .chain(allow_positions.iter().copied())
        .min();
    if first_accept.is_some_and(|accept| guard_positions.iter().any(|guard| *guard > accept))
        || guard_positions.iter().any(|guard| *guard > deny)
    {
        return Err(OrchError::Internal(format!(
            "net: refusing activation: {} has an accept or default deny before all egress guards",
            alloc.tap
        )));
    }
    if let Some(stateful) = stateful {
        if allow_positions.iter().any(|allow| stateful > *allow) {
            return Err(OrchError::Internal(format!(
                "net: refusing activation: {} stateful return follows a persisted allow",
                alloc.tap
            )));
        }
    }
    if allow_positions
        .windows(2)
        .any(|positions| positions[0] > positions[1])
        || allow_positions.iter().any(|allow| *allow > deny)
    {
        return Err(OrchError::Internal(format!(
            "net: refusing activation: {} has misordered persisted allows or default deny",
            alloc.tap
        )));
    }
    Ok(())
}

fn validate_complete_masquerade_policy(
    alloc: &NetAlloc,
    uplink: &str,
    listing: &str,
) -> Result<(), OrchError> {
    let rules = listing
        .lines()
        .filter(|line| is_taritd_nft_rule_for_alloc(line, alloc))
        .map(|line| {
            security_chain_rule_text(line).ok_or_else(|| {
                OrchError::Internal(format!(
                    "net: refusing activation: malformed managed NAT rule for {}",
                    alloc.tap
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let expected_tap = nft_quote(&alloc.tap);
    if rules.len() != 1
        || !valid_masquerade_rule_for_uplink(&rules[0], &expected_tap, &alloc.guest_ip, uplink)
    {
        return Err(OrchError::Internal(format!(
            "net: refusing activation: {} requires exactly one masquerade rule for uplink {uplink:?}",
            alloc.tap
        )));
    }
    Ok(())
}

fn validate_complete_input_policy(alloc: &NetAlloc, listing: &str) -> Result<(), OrchError> {
    let rules = allocation_rules(NFT_INPUT_CHAIN, listing, alloc)?;
    let tap = nft_quote(&alloc.tap);
    let source = exactly_one_rule(
        alloc,
        &rules,
        SecurityRuleRole::InputSourceGuard,
        &format!("iifname {tap} ip saddr != {} counter drop", alloc.guest_ip),
    )?;
    let stateful = exactly_one_rule(
        alloc,
        &rules,
        SecurityRuleRole::InputStateful,
        &format!("iifname {tap} ct state established,related accept"),
    )?;
    let deny = exactly_one_rule(
        alloc,
        &rules,
        SecurityRuleRole::InputDeny,
        &format!("iifname {tap} drop"),
    )?;
    if rules
        .iter()
        .filter(|(_, role, _)| {
            matches!(
                role,
                SecurityRuleRole::InputSourceGuard
                    | SecurityRuleRole::InputStateful
                    | SecurityRuleRole::InputDeny
            )
        })
        .count()
        != 3
    {
        return Err(OrchError::Internal(format!(
            "net: refusing activation: {} has duplicate input policy rules",
            alloc.tap
        )));
    }
    if !(source < stateful && stateful < deny) {
        return Err(OrchError::Internal(format!(
            "net: refusing activation: {} has misordered input source guard, return accept, or default deny",
            alloc.tap
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecurityRuleRole {
    ForwardSourceGuard,
    ForwardLateralGuard,
    ForwardUplinkGuard,
    InputSourceGuard,
    InputStateful,
    InputDeny,
    EgressStateful,
    EgressAllow,
    EgressDeny,
    Quarantine,
}

fn valid_taritd_security_rule(
    chain: &str,
    rule: &str,
    tag: &TaritdNftRuleTag,
) -> Option<SecurityRuleRole> {
    let tap = nft_quote(&tag.tap);
    let alloc = NetAlloc::for_slot(tag.vm_id, tag.slot).ok()?;
    match (chain, tag.kind) {
        (_, TaritdNftRuleKind::RecoveryQuarantine) if rule == format!("iifname {tap} drop") => {
            Some(SecurityRuleRole::Quarantine)
        }
        (NFT_FWD_CHAIN, TaritdNftRuleKind::EgressUpdateQuarantine)
            if rule == format!("iifname {tap} drop") =>
        {
            Some(SecurityRuleRole::Quarantine)
        }
        (NFT_FWD_CHAIN, TaritdNftRuleKind::Guard)
            if rule == format!("iifname {tap} ip saddr != {} counter drop", alloc.guest_ip) =>
        {
            Some(SecurityRuleRole::ForwardSourceGuard)
        }
        (NFT_FWD_CHAIN, TaritdNftRuleKind::Guard)
            if rule
                == format!(
                    "iifname {tap} ip saddr {} ip daddr 172.16.0.0/16 drop",
                    alloc.guest_ip
                ) =>
        {
            Some(SecurityRuleRole::ForwardLateralGuard)
        }
        (NFT_FWD_CHAIN, TaritdNftRuleKind::Guard)
            if valid_any_uplink_guard_rule(rule, &tap, &alloc.guest_ip) =>
        {
            Some(SecurityRuleRole::ForwardUplinkGuard)
        }
        (NFT_INPUT_CHAIN, TaritdNftRuleKind::Input)
            if rule == format!("iifname {tap} ip saddr != {} counter drop", alloc.guest_ip) =>
        {
            Some(SecurityRuleRole::InputSourceGuard)
        }
        (NFT_INPUT_CHAIN, TaritdNftRuleKind::Input)
            if rule == format!("iifname {tap} ct state established,related accept") =>
        {
            Some(SecurityRuleRole::InputStateful)
        }
        (NFT_INPUT_CHAIN, TaritdNftRuleKind::Input) if rule == format!("iifname {tap} drop") => {
            Some(SecurityRuleRole::InputDeny)
        }
        (NFT_FWD_CHAIN, TaritdNftRuleKind::Egress)
            if rule
                == format!(
                    "iifname {tap} ip saddr {} ct state established,related accept",
                    alloc.guest_ip
                ) =>
        {
            Some(SecurityRuleRole::EgressStateful)
        }
        (NFT_FWD_CHAIN, TaritdNftRuleKind::Egress)
            if rule == format!("iifname {tap} ip saddr {} drop", alloc.guest_ip) =>
        {
            Some(SecurityRuleRole::EgressDeny)
        }
        (NFT_FWD_CHAIN, TaritdNftRuleKind::Egress)
            if valid_egress_allow_rule(rule, &tap, &alloc.guest_ip) =>
        {
            Some(SecurityRuleRole::EgressAllow)
        }
        _ => None,
    }
}

fn valid_any_uplink_guard_rule(rule: &str, tap: &str, guest_ip: &str) -> bool {
    let words = rule.split_whitespace().collect::<Vec<_>>();
    matches!(
        words.as_slice(),
        ["iifname", interface, "ip", "saddr", source, "oifname", "!=", uplink, "drop"]
            if *interface == tap
                && *source == guest_ip
                && uplink.starts_with('"')
                && uplink.ends_with('"')
                && uplink.len() > 2
    )
}

fn valid_uplink_guard_rule(rule: &str, tap: &str, guest_ip: &str, uplink: &str) -> bool {
    valid_any_uplink_guard_rule(rule, tap, guest_ip)
        && rule
            .split_whitespace()
            .nth(7)
            .is_some_and(|actual| actual == nft_quote(uplink))
}

fn valid_masquerade_rule(rule: &str, tap: &str, guest_ip: &str) -> bool {
    let words = rule.split_whitespace().collect::<Vec<_>>();
    matches!(
        words.as_slice(),
        ["iifname", interface, "ip", "saddr", source, "oifname", uplink, "masquerade"]
            if *interface == tap
                && *source == guest_ip
                && uplink.starts_with('"')
                && uplink.ends_with('"')
                && uplink.len() > 2
    )
}

fn valid_masquerade_rule_for_uplink(rule: &str, tap: &str, guest_ip: &str, uplink: &str) -> bool {
    valid_masquerade_rule(rule, tap, guest_ip)
        && rule
            .split_whitespace()
            .nth(6)
            .is_some_and(|actual| actual == nft_quote(uplink))
}

fn valid_egress_allow_rule(rule: &str, tap: &str, guest_ip: &str) -> bool {
    let prefix = format!("iifname {tap} ip saddr {guest_ip} ip daddr ");
    let Some(rest) = rule.strip_prefix(&prefix) else {
        return false;
    };
    let words = rest.split_whitespace().collect::<Vec<_>>();
    let (cidr, tail) = match words.as_slice() {
        [cidr, "accept"] => (*cidr, &[][..]),
        [cidr, proto, "dport", port, "accept"] if matches!(*proto, "tcp" | "udp") => {
            (*cidr, &[*port][..])
        }
        _ => return false,
    };
    parse_ipv4_egress_cidr(cidr, cidr).is_ok()
        && (tail.is_empty() || tail[0].parse::<u16>().is_ok_and(|port| port != 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    static RECOVERY_TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    static RECOVERY_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn alloc_addressing_is_a_slash30() {
        let a = NetAlloc::for_idx(0);
        assert_eq!(a.host_ip, "172.16.0.1");
        assert_eq!(a.guest_ip, "172.16.0.2");
        assert_eq!(a.tap, "insta0");
        let b = NetAlloc::for_idx(1);
        assert_eq!(b.host_ip, "172.16.0.5");
        assert_eq!(b.guest_ip, "172.16.0.6");
        // 64 slots per third-octet block: idx 64 rolls to 172.16.1.x.
        let c = NetAlloc::for_idx(64);
        assert_eq!(c.host_ip, "172.16.1.1");
        let last = NetAlloc::for_idx(NET_POOL_SLOTS - 1);
        assert_eq!(last.host_ip, "172.16.255.253");
        assert_eq!(last.guest_ip, "172.16.255.254");
    }

    #[test]
    fn ip_cmdline_has_guest_gateway_and_mask() {
        let a = NetAlloc::for_idx(2);
        let c = a.ip_cmdline();
        assert!(c.starts_with("ip=172.16.0.10::172.16.0.9:255.255.255.252"));
        assert!(c.ends_with("eth0:off"));
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    fn complete_policy_listings(
        alloc: &NetAlloc,
        policy: &EgressPolicy,
    ) -> (String, String, String) {
        let tap = nft_quote(&alloc.tap);
        let nat = format!(
            "iifname {tap} ip saddr {} oifname \"eth0\" masquerade comment {} # handle 0",
            alloc.guest_ip,
            nft_quote(&nft_comment(alloc))
        );
        let forward = [
            format!(
                "iifname {tap} ip saddr != {} counter drop comment {} # handle 1",
                alloc.guest_ip,
                nft_quote(&guard_comment(alloc))
            ),
            format!(
                "iifname {tap} ip saddr {} ip daddr 172.16.0.0/16 drop comment {} # handle 2",
                alloc.guest_ip,
                nft_quote(&guard_comment(alloc))
            ),
            format!(
                "iifname {tap} ip saddr {} oifname != \"eth0\" drop comment {} # handle 3",
                alloc.guest_ip,
                nft_quote(&guard_comment(alloc))
            ),
        ]
        .into_iter()
        .chain(
            egress_policy_argv(alloc, &policy.allowlist, policy.allow_existing)
                .unwrap()
                .into_iter()
                .enumerate()
                .map(|(index, rule)| format!("{} # handle {}", rule[6..].join(" "), index + 4)),
        )
        .collect::<Vec<_>>()
        .join("\n");
        let input = [
            format!(
                "iifname {tap} ip saddr != {} counter drop comment {} # handle 11",
                alloc.guest_ip,
                nft_quote(&input_comment(alloc))
            ),
            format!(
                "iifname {tap} ct state established,related accept comment {} # handle 12",
                nft_quote(&input_comment(alloc))
            ),
            format!(
                "iifname {tap} drop comment {} # handle 13",
                nft_quote(&input_comment(alloc))
            ),
        ]
        .join("\n");
        (nat, forward, input)
    }

    #[test]
    fn nft_string_quotes_are_escaped() {
        assert_eq!(
            nft_quote("uplink\"; flush ruleset"),
            "\"uplink\\\"; flush ruleset\""
        );
        assert_eq!(nft_quote(r"uplink\name"), r#""uplink\\name""#);
    }

    #[test]
    fn host_base_policy_creates_forward_and_input_chains() {
        assert_eq!(
            host_nft_base_argv(),
            vec![
                argv(&["sysctl", "-qw", "net.ipv4.ip_forward=1"]),
                argv(&["nft", "add", "table", "ip", "taritd_nat"]),
                argv(&[
                    "nft",
                    "add",
                    "chain",
                    "ip",
                    "taritd_nat",
                    "post",
                    "{ type nat hook postrouting priority 100 ; policy accept ; }",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "chain",
                    "ip",
                    "taritd_nat",
                    "vm_egress",
                    "{ type filter hook forward priority 0 ; policy accept ; }",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "chain",
                    "ip",
                    "taritd_nat",
                    "vm_input",
                    "{ type filter hook input priority 0 ; policy accept ; }",
                ]),
            ]
        );
    }

    #[test]
    fn existing_base_chains_must_match_the_expected_nft_topology() {
        let valid = r#"{
          "nftables": [{
            "chain": {
              "family": "ip",
              "table": "taritd_nat",
              "name": "vm_egress",
              "type": "filter",
              "hook": "forward",
              "prio": 0,
              "policy": "accept"
            }
          }]
        }"#;
        assert!(validate_nft_base_chain_topology_json(NFT_FWD_CHAIN, valid).is_ok());

        let wrong_hook = valid.replace("\"forward\"", "\"input\"");
        assert!(validate_nft_base_chain_topology_json(NFT_FWD_CHAIN, &wrong_hook).is_err());
        let unhooked = valid.replace("\"hook\": \"forward\",", "");
        assert!(validate_nft_base_chain_topology_json(NFT_FWD_CHAIN, &unhooked).is_err());
    }

    #[test]
    fn base_setup_preserves_untagged_operator_masquerade_rules() {
        let _environment_guard = RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-operator-masquerade-test-{}-{sequence}",
                std::process::id()
            ));
        let bin = root.join("bin");
        let log = root.join("commands.log");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();
        let command = r#"#!/bin/sh
printf '%s %s\n' "${0##*/}" "$*" >> "$TARIT_TEST_COMMAND_LOG"
case "${0##*/}:$*" in
  "ip:-j link show") echo '[]' ;;
  "nft:-j list chain ip taritd_nat post") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"post","type":"nat","hook":"postrouting","prio":100,"policy":"accept"}}]}' ;;
  "nft:-j list chain ip taritd_nat vm_egress") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"vm_egress","type":"filter","hook":"forward","prio":0,"policy":"accept"}}]}' ;;
  "nft:-j list chain ip taritd_nat vm_input") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"vm_input","type":"filter","hook":"input","prio":0,"policy":"accept"}}]}' ;;
  "nft:-a list chain ip taritd_nat post") echo 'ip saddr 172.16.0.0/16 oifname "eth0" masquerade comment "operator NAT" # handle 99' ;;
esac
"#;
        for name in ["nft", "sysctl"] {
            let path = bin.join(name);
            std::fs::write(&path, command).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = std::fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&path, permissions).unwrap();
            }
        }
        let old_path = std::env::var_os("PATH");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                old_path.as_deref().unwrap_or_default().to_string_lossy()
            ),
        );
        std::env::set_var("TARIT_TEST_COMMAND_LOG", &log);
        let result = ensure_host_networking();
        if let Some(ref path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        std::env::remove_var("TARIT_TEST_COMMAND_LOG");

        result.unwrap();
        let commands = std::fs::read_to_string(&log).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(
            !commands.contains("nft delete rule ip taritd_nat post handle 99"),
            "{commands}"
        );
    }

    #[test]
    fn input_default_deny_uses_bare_drop_and_rejects_legacy_ip_drop() {
        let alloc = NetAlloc::for_idx(0);
        let comment = nft_quote(&input_comment(&alloc));
        let expected = argv(&[
            "nft",
            "add",
            "rule",
            "ip",
            "taritd_nat",
            "vm_input",
            "iifname",
            "\"insta0\"",
            "drop",
            "comment",
            &comment,
        ]);
        for (kind, plan) in [
            ("provision", tap_provision_argv(&alloc, "eth0")),
            ("reconcile", recovered_tap_reconcile_argv(&alloc, "eth0")),
        ] {
            let input_rules = plan
                .into_iter()
                .filter(|rule| rule.get(5).is_some_and(|chain| chain == NFT_INPUT_CHAIN))
                .collect::<Vec<_>>();
            assert!(
                input_rules.contains(&expected),
                "{kind} input default deny must compile as: {expected:?}\nactual rules: {input_rules:?}"
            );
        }

        let listing = format!(
            concat!(
                "iifname \"insta0\" ip saddr != 172.16.0.2 counter drop comment {0} # handle 1\n",
                "iifname \"insta0\" ct state established,related accept comment {0} # handle 2\n",
                "iifname \"insta0\" drop comment {0} # handle 3"
            ),
            comment
        );
        assert!(validate_taritd_security_chain(NFT_INPUT_CHAIN, &listing).is_ok());
        let legacy = listing.replace("iifname \"insta0\" drop", "iifname \"insta0\" ip drop");
        assert!(validate_taritd_security_chain(NFT_INPUT_CHAIN, &legacy).is_err());
    }

    #[test]
    fn tap_provision_plan_hardens_before_link_is_up() {
        let alloc = NetAlloc::for_idx(0);
        let plan = tap_provision_argv(&alloc, "eth0");
        assert_eq!(
            plan,
            vec![
                argv(&["ip", "tuntap", "add", "dev", "insta0", "mode", "tap"]),
                argv(&["sysctl", "-qw", "net.ipv6.conf.insta0.disable_ipv6=1",]),
                argv(&["sysctl", "-qw", "net.ipv6.conf.insta0.forwarding=0"]),
                argv(&["sysctl", "-qw", "net.ipv6.conf.insta0.accept_ra=0"]),
                argv(&["sysctl", "-qw", "net.ipv6.conf.insta0.autoconf=0"]),
                argv(&["sysctl", "-qw", "net.ipv6.conf.insta0.accept_redirects=0",]),
                argv(&["sysctl", "-qw", "net.ipv4.conf.insta0.rp_filter=1"]),
                argv(&["nft", "add", "table", "netdev", "taritd_ingress_0"]),
                argv(&[
                    "nft",
                    "add",
                    "chain",
                    "netdev",
                    "taritd_ingress_0",
                    "ingress",
                    "{ type filter hook ingress device insta0 priority filter ; policy drop ; }",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "netdev",
                    "taritd_ingress_0",
                    "ingress",
                    "ether",
                    "type",
                    "arp",
                    "accept",
                    "comment",
                    "\"taritd-ingress slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "netdev",
                    "taritd_ingress_0",
                    "ingress",
                    "ether",
                    "type",
                    "ip",
                    "accept",
                    "comment",
                    "\"taritd-ingress slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
                argv(&["ip", "addr", "add", "172.16.0.1/30", "dev", "insta0",]),
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "ip",
                    "taritd_nat",
                    "vm_egress",
                    "iifname",
                    "\"insta0\"",
                    "ip",
                    "saddr",
                    "!=",
                    "172.16.0.2",
                    "counter",
                    "drop",
                    "comment",
                    "\"taritd-guard slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "ip",
                    "taritd_nat",
                    "vm_egress",
                    "iifname",
                    "\"insta0\"",
                    "ip",
                    "saddr",
                    "172.16.0.2",
                    "ip",
                    "daddr",
                    "172.16.0.0/16",
                    "drop",
                    "comment",
                    "\"taritd-guard slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "ip",
                    "taritd_nat",
                    "vm_egress",
                    "iifname",
                    "\"insta0\"",
                    "ip",
                    "saddr",
                    "172.16.0.2",
                    "oifname",
                    "!=",
                    "\"eth0\"",
                    "drop",
                    "comment",
                    "\"taritd-guard slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "ip",
                    "taritd_nat",
                    "vm_input",
                    "iifname",
                    "\"insta0\"",
                    "ip",
                    "saddr",
                    "!=",
                    "172.16.0.2",
                    "counter",
                    "drop",
                    "comment",
                    "\"taritd-input slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "ip",
                    "taritd_nat",
                    "vm_input",
                    "iifname",
                    "\"insta0\"",
                    "ct",
                    "state",
                    "established,related",
                    "accept",
                    "comment",
                    "\"taritd-input slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "ip",
                    "taritd_nat",
                    "vm_input",
                    "iifname",
                    "\"insta0\"",
                    "drop",
                    "comment",
                    "\"taritd-input slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
            ]
        );
    }

    #[test]
    fn recovery_quarantine_script_covers_all_recovered_taps_atomically() {
        let first = NetAlloc::for_idx(7);
        let second = NetAlloc::for_idx(8);
        assert_eq!(
            recovery_quarantine_script(&[first.clone(), second.clone()]),
            format!(
                concat!(
                    "insert rule ip taritd_nat vm_egress iifname \"insta7\" drop comment ",
                    "\"taritd-recovery-quarantine slot=7 vm={} tap=insta7\"\n",
                    "insert rule ip taritd_nat vm_input iifname \"insta7\" drop comment ",
                    "\"taritd-recovery-quarantine slot=7 vm={} tap=insta7\"\n",
                    "insert rule ip taritd_nat vm_egress iifname \"insta8\" drop comment ",
                    "\"taritd-recovery-quarantine slot=8 vm={} tap=insta8\"\n",
                    "insert rule ip taritd_nat vm_input iifname \"insta8\" drop comment ",
                    "\"taritd-recovery-quarantine slot=8 vm={} tap=insta8\"\n"
                ),
                first.vm_id, first.vm_id, second.vm_id, second.vm_id,
            )
        );
    }

    #[test]
    fn stale_recovery_rule_selection_removes_prior_owner_but_keeps_quarantine() {
        let recovered = NetAlloc::for_idx(7);
        let prior_owner = Uuid::new_v4();
        for line in [
            format!(
                "ip saddr 172.16.0.30 masquerade comment \"taritd slot=7 vm={prior_owner} tap=insta7\""
            ),
            format!(
                "iifname \"insta7\" ip saddr 172.16.0.30 drop comment \"taritd-egress slot=7 vm={prior_owner} tap=insta7\""
            ),
            format!(
                "iifname \"insta7\" drop comment \"taritd-input slot=7 vm={prior_owner} tap=insta7\""
            ),
        ] {
            assert!(is_stale_recovery_rule_for_alloc(&line, &recovered), "{line}");
        }
        assert!(!is_stale_recovery_rule_for_alloc(
            &format!(
                "iifname \"insta7\" drop comment \"{}\"",
                recovery_quarantine_comment(&recovered)
            ),
            &recovered,
        ));
        assert!(!is_stale_recovery_rule_for_alloc(
            "iifname \"insta7\" drop comment \"operator rule\"",
            &recovered,
        ));
        assert!(!is_stale_recovery_rule_for_alloc(
            &format!(
                "iifname \"insta7\" drop comment \"operator note: taritd slot=7 vm={prior_owner} tap=insta7\""
            ),
            &recovered,
        ));
        assert!(!is_stale_recovery_rule_for_alloc(
            &format!(
                "iifname \"insta7\" drop comment \"taritd-guard slot=7 vm={prior_owner} tap=insta8\""
            ),
            &recovered,
        ));
        let unknown_current_owner_shape = format!(
            "iifname \"insta7\" ip saddr {} ip daddr 198.18.0.1 drop comment \"{}\"",
            recovered.guest_ip,
            egress_comment(&recovered)
        );
        assert!(is_stale_recovery_rule_for_alloc(
            &unknown_current_owner_shape,
            &recovered,
        ));
        assert!(
            !is_recognized_taritd_rule(NFT_FWD_CHAIN, &unknown_current_owner_shape),
            "unknown current-owner shapes must be retained so closed-chain validation blocks recovery"
        );
    }

    #[test]
    fn egress_cleanup_requires_an_exact_managed_comment() {
        let alloc = NetAlloc::for_idx(7);
        assert!(is_egress_nft_rule_for_alloc(
            &format!(
                "iifname \"insta7\" ip drop comment \"{}\"",
                egress_comment(&alloc)
            ),
            &alloc,
        ));
        assert!(!is_egress_nft_rule_for_alloc(
            &format!(
                "iifname \"insta7\" ip drop comment \"operator note: {}\"",
                egress_comment(&alloc)
            ),
            &alloc,
        ));
    }

    #[test]
    fn forward_egress_guards_precede_broad_guest_allowlists() {
        let alloc = NetAlloc::for_idx(0);
        let mut forward_rules = tap_provision_argv(&alloc, "eth0")
            .into_iter()
            .filter(|rule| rule.get(5).is_some_and(|chain| chain == NFT_FWD_CHAIN))
            .collect::<Vec<_>>();
        forward_rules.extend(
            egress_policy_argv(
                &alloc,
                &["0.0.0.0/0".to_string(), "172.16.0.0/16".to_string()],
                true,
            )
            .unwrap(),
        );

        assert_eq!(
            forward_rules,
            vec![
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "ip",
                    "taritd_nat",
                    "vm_egress",
                    "iifname",
                    "\"insta0\"",
                    "ip",
                    "saddr",
                    "!=",
                    "172.16.0.2",
                    "counter",
                    "drop",
                    "comment",
                    "\"taritd-guard slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "ip",
                    "taritd_nat",
                    "vm_egress",
                    "iifname",
                    "\"insta0\"",
                    "ip",
                    "saddr",
                    "172.16.0.2",
                    "ip",
                    "daddr",
                    "172.16.0.0/16",
                    "drop",
                    "comment",
                    "\"taritd-guard slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "ip",
                    "taritd_nat",
                    "vm_egress",
                    "iifname",
                    "\"insta0\"",
                    "ip",
                    "saddr",
                    "172.16.0.2",
                    "oifname",
                    "!=",
                    "\"eth0\"",
                    "drop",
                    "comment",
                    "\"taritd-guard slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "ip",
                    "taritd_nat",
                    "vm_egress",
                    "iifname",
                    "\"insta0\"",
                    "ip",
                    "saddr",
                    "172.16.0.2",
                    "ct",
                    "state",
                    "established,related",
                    "accept",
                    "comment",
                    "\"taritd-egress slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "ip",
                    "taritd_nat",
                    "vm_egress",
                    "iifname",
                    "\"insta0\"",
                    "ip",
                    "saddr",
                    "172.16.0.2",
                    "ip",
                    "daddr",
                    "0.0.0.0/0",
                    "accept",
                    "comment",
                    "\"taritd-egress slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "ip",
                    "taritd_nat",
                    "vm_egress",
                    "iifname",
                    "\"insta0\"",
                    "ip",
                    "saddr",
                    "172.16.0.2",
                    "ip",
                    "daddr",
                    "172.16.0.0/16",
                    "accept",
                    "comment",
                    "\"taritd-egress slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "ip",
                    "taritd_nat",
                    "vm_egress",
                    "iifname",
                    "\"insta0\"",
                    "ip",
                    "saddr",
                    "172.16.0.2",
                    "drop",
                    "comment",
                    "\"taritd-egress slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
            ]
        );
    }

    #[test]
    fn forged_source_guard_counts_before_dropping() {
        let alloc = NetAlloc::for_idx(0);
        let source_guard = tap_provision_argv(&alloc, "eth0")
            .into_iter()
            .find(|argv| {
                argv.windows(3)
                    .any(|window| window == ["ip", "saddr", "!="])
            })
            .unwrap();
        assert!(
            source_guard
                .windows(2)
                .any(|window| window == ["counter", "drop"]),
            "{source_guard:?}"
        );
    }

    #[test]
    fn broad_guest_policy_remains_bound_to_its_own_tap_and_source() {
        let guest_a = NetAlloc::for_idx(0);
        let guest_b = NetAlloc::for_idx(1);
        let policy_a = egress_policy_argv(&guest_a, &["0.0.0.0/0".to_string()], false).unwrap();
        let policy_b = egress_policy_argv(&guest_b, &["0.0.0.0/0".to_string()], false).unwrap();

        assert_eq!(
            policy_a[0],
            argv(&[
                "nft",
                "add",
                "rule",
                "ip",
                "taritd_nat",
                "vm_egress",
                "iifname",
                "\"insta0\"",
                "ip",
                "saddr",
                "172.16.0.2",
                "ip",
                "daddr",
                "0.0.0.0/0",
                "accept",
                "comment",
                "\"taritd-egress slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
            ])
        );
        assert_eq!(
            policy_b[0],
            argv(&[
                "nft",
                "add",
                "rule",
                "ip",
                "taritd_nat",
                "vm_egress",
                "iifname",
                "\"insta1\"",
                "ip",
                "saddr",
                "172.16.0.6",
                "ip",
                "daddr",
                "0.0.0.0/0",
                "accept",
                "comment",
                "\"taritd-egress slot=1 vm=00000000-0000-0000-0000-000000000000 tap=insta1\"",
            ])
        );
    }

    #[test]
    fn ingress_recovery_and_teardown_only_target_owned_slot_tables() {
        let active_slots = HashSet::from([1]);
        let tables = vec![
            "taritd_ingress_0".to_string(),
            "taritd_ingress_1".to_string(),
            "taritd_ingress_16384".to_string(),
            "foreign_ingress_0".to_string(),
        ];
        assert_eq!(
            stale_ingress_tables_to_sweep(&tables, &active_slots),
            vec!["taritd_ingress_0".to_string()]
        );
        assert_eq!(
            delete_ingress_table_argv(0),
            argv(&["nft", "delete", "table", "netdev", "taritd_ingress_0"])
        );
    }

    #[test]
    fn ingress_table_cleanup_rejects_a_slot_collision_owned_by_another_vm() {
        let recovered = NetAlloc::for_idx(7);
        let colliding_vm = Uuid::new_v4();
        let owned = format!(
            r#"table netdev taritd_ingress_7 {{
 chain ingress {{
  type filter hook ingress device "insta7" priority filter; policy drop;
  ether type arp accept comment "{}"
  ether type ip accept comment "{}"
 }}
}}"#,
            ingress_comment(&recovered),
            ingress_comment(&recovered),
        );
        let collision = owned.replace(&recovered.vm_id.to_string(), &colliding_vm.to_string());
        let operator_rule = owned.replace(
            &format!(
                "  ether type ip accept comment \"{}\"",
                ingress_comment(&recovered)
            ),
            "  counter accept",
        );
        let deceptive_type_filter_rule = owned.replace(
            &format!(
                "  ether type ip accept comment \"{}\"",
                ingress_comment(&recovered)
            ),
            &format!(
                "  meta l4proto type filter drop\n  ether type ip accept comment \"{}\"",
                ingress_comment(&recovered)
            ),
        );

        assert!(ingress_table_belongs_to_alloc(&owned, &recovered));
        assert!(!ingress_table_belongs_to_alloc(&collision, &recovered));
        assert!(!ingress_table_belongs_to_alloc(&operator_rule, &recovered));
        assert!(!ingress_table_belongs_to_alloc(
            &deceptive_type_filter_rule,
            &recovered
        ));
    }

    #[test]
    fn ingress_table_cleanup_preserves_a_slot_collision() {
        let _environment_guard = RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let recovered_vm = Uuid::new_v4();
        let colliding_vm = Uuid::new_v4();
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-ingress-collision-test-{}-{sequence}",
                std::process::id()
            ));
        let bin = root.join("bin");
        let log = root.join("commands.log");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();
        let command = r#"#!/bin/sh
printf '%s %s\n' "${0##*/}" "$*" >> "$TARIT_TEST_COMMAND_LOG"
case "${0##*/}:$*" in
  "nft:list tables netdev") echo "table netdev taritd_ingress_7" ;;
  "nft:-a list table netdev taritd_ingress_7") cat <<EOF
table netdev taritd_ingress_7 {
 chain ingress {
  type filter hook ingress device "insta7" priority filter; policy drop;
  ether type arp accept comment "taritd-ingress slot=7 vm=$TARIT_TEST_COLLIDING_VM tap=insta7"
  ether type ip accept comment "taritd-ingress slot=7 vm=$TARIT_TEST_COLLIDING_VM tap=insta7"
 }
}
EOF
    ;;
esac
"#;
        let path = bin.join("nft");
        std::fs::write(&path, command).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).unwrap();
        }
        let old_path = std::env::var_os("PATH");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                old_path.as_deref().unwrap_or_default().to_string_lossy()
            ),
        );
        std::env::set_var("TARIT_TEST_COMMAND_LOG", &log);
        std::env::set_var("TARIT_TEST_COLLIDING_VM", colliding_vm.to_string());
        let result = delete_ingress_table_for_alloc(&NetAlloc::for_slot(recovered_vm, 7).unwrap());
        if let Some(ref path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        std::env::remove_var("TARIT_TEST_COMMAND_LOG");
        std::env::remove_var("TARIT_TEST_COLLIDING_VM");

        assert!(result.is_err());
        let commands = std::fs::read_to_string(&log).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(
            !commands.contains("nft delete table netdev taritd_ingress_7"),
            "{commands}"
        );
    }

    #[test]
    fn masquerade_rule_is_bound_to_its_tap_and_guest_source() {
        let alloc = NetAlloc::for_idx(0);
        assert_eq!(
            masquerade_nft_argv(&alloc, "eth0"),
            argv(&[
                "nft",
                "add",
                "rule",
                "ip",
                "taritd_nat",
                "post",
                "iifname",
                "\"insta0\"",
                "ip",
                "saddr",
                "172.16.0.2",
                "oifname",
                "\"eth0\"",
                "masquerade",
                "comment",
                "\"taritd slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
            ])
        );
    }

    #[test]
    fn egress_policy_binds_state_allow_and_drop_rules_to_the_tap() {
        let alloc = NetAlloc::for_idx(0);
        assert_eq!(
            egress_policy_argv(&alloc, &["198.51.100.10:443".to_string()], true).unwrap(),
            vec![
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "ip",
                    "taritd_nat",
                    "vm_egress",
                    "iifname",
                    "\"insta0\"",
                    "ip",
                    "saddr",
                    "172.16.0.2",
                    "ct",
                    "state",
                    "established,related",
                    "accept",
                    "comment",
                    "\"taritd-egress slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "ip",
                    "taritd_nat",
                    "vm_egress",
                    "iifname",
                    "\"insta0\"",
                    "ip",
                    "saddr",
                    "172.16.0.2",
                    "ip",
                    "daddr",
                    "198.51.100.10/32",
                    "tcp",
                    "dport",
                    "443",
                    "accept",
                    "comment",
                    "\"taritd-egress slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
                argv(&[
                    "nft",
                    "add",
                    "rule",
                    "ip",
                    "taritd_nat",
                    "vm_egress",
                    "iifname",
                    "\"insta0\"",
                    "ip",
                    "saddr",
                    "172.16.0.2",
                    "drop",
                    "comment",
                    "\"taritd-egress slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
            ]
        );
    }

    #[test]
    fn egress_entry_parses_cidr_port_and_proto() {
        assert_eq!(
            parse_egress_entry("10.0.0.0/8").unwrap(),
            ("10.0.0.0/8".to_string(), 0, None)
        );
        assert_eq!(
            parse_egress_entry("1.2.3.4:443").unwrap(),
            ("1.2.3.4/32".to_string(), 443, Some("tcp"))
        );
        assert_eq!(
            parse_egress_entry("8.8.8.8:53/udp").unwrap(),
            ("8.8.8.8/32".to_string(), 53, Some("udp"))
        );
        assert!(parse_egress_entry("").is_err());
        assert!(parse_egress_entry(":443").is_err());
        assert!(parse_egress_entry("1.2.3.4:443/sctp").is_err());
        assert!(parse_egress_entry("1.2.3.4:notaport").is_err());
    }

    #[test]
    fn egress_entry_rejects_injected_and_malformed_ipv4_cidrs() {
        for entry in [
            "192.0.2.0/24 accept; list chain ip taritd_nat vm_egress; #",
            "192.0.2.0/24 accept; list chain ip taritd_nat vm_egress; #:443",
            "192.0.2.0/33",
            "192.0.2/24",
            "192.0.2.0/not-a-prefix",
            "egress.example",
            "192.0.2.0/24 ",
            "192.0.2.0/24:0",
        ] {
            assert!(
                parse_egress_entry(entry).is_err(),
                "invalid egress entry was accepted: {entry:?}"
            );
        }
        for entry in ["2001:db8::/32", "2001:db8::/32:443", "[2001:db8::1]:443"] {
            assert!(matches!(
                parse_egress_entry(entry).unwrap_err(),
                OrchError::BadRequest(message) if message.contains("IPv6")
            ));
        }
    }

    #[test]
    fn compile_host_egress_rule_builds_forward_accept() {
        let alloc = NetAlloc::for_idx(0);
        let any = compile_host_egress_rule(&alloc, "10.0.0.0/8", "\"c\"").unwrap();
        assert_eq!(
            any,
            vec![
                "add",
                "rule",
                "ip",
                NFT_TABLE,
                NFT_FWD_CHAIN,
                "iifname",
                "\"insta0\"",
                "ip",
                "saddr",
                "172.16.0.2",
                "ip",
                "daddr",
                "10.0.0.0/8",
                "accept",
                "comment",
                "\"c\"",
            ]
        );
        let tcp = compile_host_egress_rule(&alloc, "1.2.3.4:443", "\"c\"").unwrap();
        assert!(tcp.windows(3).any(|w| w == ["tcp", "dport", "443"]));
        assert_eq!(tcp.last().unwrap(), "\"c\"");

        let canonical = compile_host_egress_rule(&alloc, "192.0.2.42/24:443", "\"c\"").unwrap();
        assert!(canonical
            .windows(3)
            .any(|w| w == ["ip", "daddr", "192.0.2.0/24"]));

        let host = compile_host_egress_rule(&alloc, "192.0.2.42", "\"c\"").unwrap();
        assert!(host
            .windows(3)
            .any(|w| w == ["ip", "daddr", "192.0.2.42/32"]));
    }

    #[test]
    fn allocator_reuses_freed_slots() {
        let mut allocator = SlotAllocator::empty();
        let vm1 = Uuid::new_v4();
        let vm2 = Uuid::new_v4();
        let vm3 = Uuid::new_v4();

        let a = allocator.allocate(vm1).unwrap();
        let b = allocator.allocate(vm2).unwrap();
        assert_eq!(a.idx, 0);
        assert_eq!(b.idx, 1);

        allocator.free(&a);
        let c = allocator.allocate(vm3).unwrap();
        assert_eq!(c.idx, 0);
        assert_eq!(c.host_ip, a.host_ip);
        assert_eq!(c.guest_ip, a.guest_ip);
    }

    #[test]
    fn allocator_exhaustion_returns_overloaded() {
        let mut allocator = SlotAllocator::empty();
        for _ in 0..NET_POOL_SLOTS {
            allocator.allocate(Uuid::new_v4()).unwrap();
        }
        let err = allocator.allocate(Uuid::new_v4()).unwrap_err();
        match err {
            OrchError::Overloaded { message, .. } => {
                assert!(message.contains("network address pool exhausted"));
                assert!(message.contains(&NET_POOL_SLOTS.to_string()));
            }
            other => panic!("expected overloaded, got {other:?}"),
        }
    }

    #[test]
    fn allocator_recovers_valid_entries() {
        let live_vm = Uuid::new_v4();
        let stale_vm = Uuid::new_v4();
        let entries = vec![
            NetStateEntry {
                slot: 7,
                vm_id: live_vm,
                tap: "insta7".into(),
                egress: Some(EgressPolicy::default()),
            },
            NetStateEntry {
                slot: 8,
                vm_id: stale_vm,
                tap: "insta8".into(),
                egress: Some(EgressPolicy::default()),
            },
        ];
        let mut allocator = SlotAllocator::from_entries(entries).unwrap();

        assert_eq!(allocator.by_vm.get(&live_vm), Some(&7));
        assert_eq!(allocator.by_vm.get(&stale_vm), Some(&8));
        let alloc = allocator.allocate(Uuid::new_v4()).unwrap();
        assert_eq!(alloc.idx, 0);
    }

    #[test]
    fn persisted_state_rejects_ambiguous_ownership_instead_of_dropping_entries() {
        let first_vm = Uuid::new_v4();
        let second_vm = Uuid::new_v4();
        let cases = [
            (
                "malformed slot",
                format!(
                    r#"{{"version":2,"allocations":[{{"slot":{NET_POOL_SLOTS},"vm_id":"{first_vm}","tap":"insta{NET_POOL_SLOTS}","egress":{{}}}}]}}"#
                ),
            ),
            (
                "contradictory tap identity",
                format!(
                    r#"{{"version":2,"allocations":[{{"slot":7,"vm_id":"{first_vm}","tap":"insta8","egress":{{}}}}]}}"#
                ),
            ),
            (
                "duplicate VM",
                format!(
                    r#"{{"version":2,"allocations":[{{"slot":7,"vm_id":"{first_vm}","tap":"insta7","egress":{{}}}},{{"slot":8,"vm_id":"{first_vm}","tap":"insta8","egress":{{}}}}]}}"#
                ),
            ),
            (
                "duplicate slot",
                format!(
                    r#"{{"version":2,"allocations":[{{"slot":7,"vm_id":"{first_vm}","tap":"insta7","egress":{{}}}},{{"slot":7,"vm_id":"{second_vm}","tap":"insta7","egress":{{}}}}]}}"#
                ),
            ),
        ];

        for (name, state) in cases {
            assert!(
                decode_net_state(&state, Path::new("state.json"), &HashSet::new()).is_err(),
                "{name} state was accepted"
            );
        }

        let ambiguous_entries = vec![
            NetStateEntry {
                slot: 7,
                vm_id: first_vm,
                tap: "insta7".into(),
                egress: Some(EgressPolicy::default()),
            },
            NetStateEntry {
                slot: 8,
                vm_id: first_vm,
                tap: "insta8".into(),
                egress: Some(EgressPolicy::default()),
            },
        ];
        assert!(
            SlotAllocator::from_entries(ambiguous_entries).is_err(),
            "allocator silently dropped an ambiguous owner"
        );
    }

    #[test]
    fn recovered_legacy_allocation_without_egress_state_fails_closed() {
        let vm_id = Uuid::new_v4();
        let allocator = SlotAllocator::from_entries(vec![NetStateEntry {
            slot: 7,
            vm_id,
            tap: "insta7".into(),
            egress: None,
        }])
        .unwrap();
        let alloc = NetAlloc::for_slot(vm_id, 7).unwrap();

        assert!(matches!(
            allocator.egress_policy_for(&alloc),
            Err(OrchError::Internal(message)) if message.contains("missing persisted egress policy")
        ));
    }

    #[test]
    fn legacy_recovery_without_egress_state_keeps_tap_quarantined_and_down() {
        let _environment_guard = RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let vm_id = Uuid::new_v4();
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-legacy-egress-recovery-test-{}-{sequence}",
                std::process::id()
            ));
        let bin = root.join("bin");
        let log = root.join("commands.log");
        let quarantine = root.join("quarantine.nft");
        let state_path = root.join("state.json");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            &state_path,
            format!(
                r#"{{"version":2,"allocations":[{{"slot":7,"vm_id":"{vm_id}","tap":"insta7"}}]}}"#
            ),
        )
        .unwrap();
        let command = r#"#!/bin/sh
printf '%s %s\n' "${0##*/}" "$*" >> "$TARIT_TEST_COMMAND_LOG"
case "${0##*/}:$*" in
  "ip:route get 8.8.8.8") echo "8.8.8.8 via 192.0.2.1 dev eth0 src 192.0.2.2" ;;
  "ip:-j link show") echo '[]' ;;
  "nft:-j list chain ip taritd_nat post") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"post","type":"nat","hook":"postrouting","prio":100,"policy":"accept"}}]}' ;;
  "nft:-j list chain ip taritd_nat vm_egress") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"vm_egress","type":"filter","hook":"forward","prio":0,"policy":"accept"}}]}' ;;
  "nft:-j list chain ip taritd_nat vm_input") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"vm_input","type":"filter","hook":"input","prio":0,"policy":"accept"}}]}' ;;
  "nft:-f -") cat > "$TARIT_TEST_QUARANTINE" ;;
esac
"#;
        for name in ["ip", "nft", "sysctl"] {
            let path = bin.join(name);
            std::fs::write(&path, command).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = std::fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&path, permissions).unwrap();
            }
        }
        let old_path = std::env::var_os("PATH");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                old_path.as_deref().unwrap_or_default().to_string_lossy()
            ),
        );
        std::env::set_var("TARIT_TEST_COMMAND_LOG", &log);
        std::env::set_var("TARIT_TEST_QUARANTINE", &quarantine);
        let result = NetProvisioner::new(state_path, [vm_id]);
        if let Some(path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        std::env::remove_var("TARIT_TEST_COMMAND_LOG");
        std::env::remove_var("TARIT_TEST_QUARANTINE");

        let error = match result {
            Ok(_) => panic!("legacy state without egress policy unexpectedly recovered"),
            Err(error) => error,
        };
        let commands = std::fs::read_to_string(&log).unwrap();
        let quarantine_script = std::fs::read_to_string(&quarantine).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(
            error
                .to_string()
                .contains("missing persisted egress policy"),
            "{error}"
        );
        assert!(quarantine_script.contains("taritd-recovery-quarantine slot=7"));
        assert!(commands.contains("ip link set insta7 down"), "{commands}");
        assert!(!commands.contains("ip link set insta7 up"), "{commands}");
    }

    #[test]
    fn recovered_live_allocation_restores_narrow_egress_policy_before_available() {
        let _environment_guard = RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let vm_id = Uuid::new_v4();
        let old_vm_id = Uuid::new_v4();
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-recovery-test-{}-{sequence}",
                std::process::id()
            ));
        let bin = root.join("bin");
        let log = root.join("commands.log");
        let fake_state = root.join("fake-state");
        let state_path = root.join("state.json");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            &state_path,
            format!(
                r#"{{"version":2,"allocations":[{{"slot":7,"vm_id":"{vm_id}","tap":"insta7","egress":{{"allowlist":["198.51.100.10:443"],"allow_existing":true}}}}]}}"#
            ),
        )
        .unwrap();

        let command = r#"#!/bin/sh
printf '%s %s\n' "${0##*/}" "$*" >> "$TARIT_TEST_COMMAND_LOG"
case "${0##*/}:$*" in
  "ip:route get 8.8.8.8") echo "8.8.8.8 via 192.0.2.1 dev eth0 src 192.0.2.2" ;;
  "ip:-o link show") echo "7: insta7: <BROADCAST,UP> mtu 1500" ;;
  "ip:-j link show") echo '[]' ;;
  "nft:-j list chain ip taritd_nat post") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"post","type":"nat","hook":"postrouting","prio":100,"policy":"accept"}}]}' ;;
  "nft:-j list chain ip taritd_nat vm_egress") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"vm_egress","type":"filter","hook":"forward","prio":0,"policy":"accept"}}]}' ;;
  "nft:-j list chain ip taritd_nat vm_input") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"vm_input","type":"filter","hook":"input","prio":0,"policy":"accept"}}]}' ;;
  "sysctl:-qn net.ipv6.conf.insta7.forwarding"|"sysctl:-qn net.ipv6.conf.insta7.accept_ra"|"sysctl:-qn net.ipv6.conf.insta7.autoconf"|"sysctl:-qn net.ipv6.conf.insta7.accept_redirects") echo 0 ;;
  "sysctl:-qn net.ipv6.conf.insta7.disable_ipv6"|"sysctl:-qn net.ipv4.conf.insta7.rp_filter") echo 1 ;;
  "nft:-f -")
    script=$(cat)
    case "$script" in
      *"insert rule ip taritd_nat vm_egress iifname \"insta7\" drop comment \"taritd-recovery-quarantine slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\""*)
        printf '%s\n' "$script" > "$TARIT_TEST_FAKE_STATE.quarantine-install"
        printf '%s\n' "$script" | grep -F "insert rule ip taritd_nat vm_input iifname \"insta7\" drop comment \"taritd-recovery-quarantine slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\"" >/dev/null ||
          exit 1
        touch "$TARIT_TEST_FAKE_STATE.quarantine"
        ;;
      *"delete rule ip taritd_nat vm_egress handle "*)
        printf '%s\n' "$script" > "$TARIT_TEST_FAKE_STATE.quarantine-release"
        printf '%s\n' "$script" | grep -F "delete rule ip taritd_nat vm_input handle " >/dev/null ||
          exit 1
        rm -f "$TARIT_TEST_FAKE_STATE.quarantine"
        ;;
      *) echo "unexpected nft recovery transaction: $script" >&2; exit 1 ;;
    esac
    ;;
  "nft:list tables netdev")
    if [ -e "$TARIT_TEST_FAKE_STATE.policy" ] || [ ! -e "$TARIT_TEST_FAKE_STATE.initial-ingress-removed" ]; then
      echo "table netdev taritd_ingress_7"
    fi
    ;;
  "nft:-a list table netdev taritd_ingress_7")
    echo "table netdev taritd_ingress_7 { chain ingress { type filter hook ingress device \"insta7\" priority filter; policy drop; ether type arp accept comment \"taritd-ingress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\"; ether type ip accept comment \"taritd-ingress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\"; } }"
    ;;
  "nft:add table netdev taritd_ingress_7") touch "$TARIT_TEST_FAKE_STATE.policy" ;;
  "nft:-a list chain ip taritd_nat post")
    if [ ! -e "$TARIT_TEST_FAKE_STATE.initial-post-removed" ]; then
      echo "iifname \"insta7\" ip saddr 172.16.0.30 oifname \"eth0\" masquerade comment \"taritd slot=7 vm=$TARIT_TEST_OLD_VM_ID tap=insta7\" # handle 1"
    elif [ -e "$TARIT_TEST_FAKE_STATE.policy" ]; then
      echo "iifname \"insta7\" ip saddr 172.16.0.30 oifname \"eth0\" masquerade comment \"taritd slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 11"
    fi
    ;;
  "nft:-a list chain ip taritd_nat vm_egress")
    [ ! -e "$TARIT_TEST_FAKE_STATE.quarantine" ] || echo "iifname \"insta7\" drop comment \"taritd-recovery-quarantine slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 20"
    if [ ! -e "$TARIT_TEST_FAKE_STATE.initial-forward-removed" ]; then
      echo "iifname \"insta7\" ip saddr != 172.16.0.30 counter drop comment \"taritd-guard slot=7 vm=$TARIT_TEST_OLD_VM_ID tap=insta7\" # handle 2"
      echo "iifname \"insta7\" ip saddr 172.16.0.30 drop comment \"taritd-egress slot=7 vm=$TARIT_TEST_OLD_VM_ID tap=insta7\" # handle 3"
    elif [ -e "$TARIT_TEST_FAKE_STATE.policy" ]; then
      echo "iifname \"insta7\" ip saddr != 172.16.0.30 counter drop comment \"taritd-guard slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 12"
      echo "iifname \"insta7\" ip saddr 172.16.0.30 ip daddr 172.16.0.0/16 drop comment \"taritd-guard slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 13"
      echo "iifname \"insta7\" ip saddr 172.16.0.30 oifname != \"eth0\" drop comment \"taritd-guard slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 14"
      echo "iifname \"insta7\" ip saddr 172.16.0.30 ct state established,related accept comment \"taritd-egress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 15"
      echo "iifname \"insta7\" ip saddr 172.16.0.30 ip daddr 198.51.100.10/32 tcp dport 443 accept comment \"taritd-egress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 16"
      echo "iifname \"insta7\" ip saddr 172.16.0.30 drop comment \"taritd-egress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 17"
    fi
    ;;
  "nft:-a list chain ip taritd_nat vm_input")
    [ ! -e "$TARIT_TEST_FAKE_STATE.quarantine" ] || echo "iifname \"insta7\" drop comment \"taritd-recovery-quarantine slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 21"
    if [ ! -e "$TARIT_TEST_FAKE_STATE.initial-input-removed" ]; then
      echo "iifname \"insta7\" drop comment \"taritd-input slot=7 vm=$TARIT_TEST_OLD_VM_ID tap=insta7\" # handle 4"
    elif [ -e "$TARIT_TEST_FAKE_STATE.policy" ]; then
      echo "iifname \"insta7\" ip saddr != 172.16.0.30 counter drop comment \"taritd-input slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 15"
      echo "iifname \"insta7\" ct state established,related accept comment \"taritd-input slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 16"
      echo "iifname \"insta7\" drop comment \"taritd-input slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 17"
    fi
    ;;
  "nft:delete table netdev taritd_ingress_7") touch "$TARIT_TEST_FAKE_STATE.initial-ingress-removed" ;;
  "nft:delete rule ip taritd_nat post handle 1") touch "$TARIT_TEST_FAKE_STATE.initial-post-removed" ;;
  "nft:delete rule ip taritd_nat vm_egress handle 2"|"nft:delete rule ip taritd_nat vm_egress handle 3") touch "$TARIT_TEST_FAKE_STATE.initial-forward-removed" ;;
  "nft:delete rule ip taritd_nat vm_input handle 4") touch "$TARIT_TEST_FAKE_STATE.initial-input-removed" ;;
  "nft:delete rule ip taritd_nat vm_input handle 17") rm -f "$TARIT_TEST_FAKE_STATE.policy" ;;
esac
"#;
        for name in ["ip", "nft", "sysctl"] {
            let path = bin.join(name);
            std::fs::write(&path, command).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = std::fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&path, permissions).unwrap();
            }
        }

        let old_path = std::env::var_os("PATH");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                old_path.as_deref().unwrap_or_default().to_string_lossy()
            ),
        );
        std::env::set_var("TARIT_TEST_COMMAND_LOG", &log);
        std::env::set_var("TARIT_TEST_FAKE_STATE", &fake_state);
        std::env::set_var("TARIT_TEST_OLD_VM_ID", old_vm_id.to_string());
        std::env::set_var("TARIT_TEST_VM_ID", vm_id.to_string());
        let result = NetProvisioner::new(state_path.clone(), [vm_id])
            .and_then(|_| NetProvisioner::new(state_path, [vm_id]));
        if let Some(path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        std::env::remove_var("TARIT_TEST_COMMAND_LOG");
        std::env::remove_var("TARIT_TEST_FAKE_STATE");
        std::env::remove_var("TARIT_TEST_OLD_VM_ID");
        std::env::remove_var("TARIT_TEST_VM_ID");

        result.unwrap();
        let commands = std::fs::read_to_string(&log).unwrap();
        let quarantine_install =
            std::fs::read_to_string(fake_state.with_extension("quarantine-install")).unwrap();
        let quarantine_release =
            std::fs::read_to_string(fake_state.with_extension("quarantine-release")).unwrap();
        std::fs::remove_dir_all(&root).unwrap();

        assert!(quarantine_install.contains("insert rule ip taritd_nat vm_egress"));
        assert!(quarantine_install.contains("insert rule ip taritd_nat vm_input"));
        assert!(quarantine_install.contains(&recovery_quarantine_comment(
            &NetAlloc::for_slot(vm_id, 7).unwrap()
        )));
        assert!(quarantine_release.contains("delete rule ip taritd_nat vm_egress handle"));
        assert!(quarantine_release.contains("delete rule ip taritd_nat vm_input handle"));
        assert!(!commands.contains("ip tuntap add dev insta7 mode tap"));
        assert!(
            commands.find("nft -f -").unwrap() < commands.find("ip link set insta7 down").unwrap()
        );
        assert!(
            commands
                .find("nft delete rule ip taritd_nat post handle 1")
                .unwrap()
                < commands.find("ip link set insta7 up").unwrap()
        );
        let link_ups = commands
            .match_indices("ip link set insta7 up")
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(link_ups.len(), 2, "{commands}");
        assert_eq!(
            commands
                .matches("nft delete table netdev taritd_ingress_7")
                .count(),
            1
        );
        assert!(
            commands
                .find("nft delete table netdev taritd_ingress_7")
                .unwrap()
                < link_ups[0],
            "{commands}"
        );
        for expected in [
            "nft delete rule ip taritd_nat post handle 1",
            "nft delete rule ip taritd_nat vm_egress handle 2",
            "nft delete rule ip taritd_nat vm_egress handle 3",
            "nft delete rule ip taritd_nat vm_input handle 4",
        ] {
            assert_eq!(
                commands.lines().filter(|line| *line == expected).count(),
                1,
                "{commands}"
            );
            assert!(commands.find(expected).unwrap() < link_ups[0], "{commands}");
        }
        for expected in [
            "nft delete rule ip taritd_nat post handle 11",
            "nft delete rule ip taritd_nat vm_egress handle 12",
            "nft delete rule ip taritd_nat vm_egress handle 13",
            "nft delete rule ip taritd_nat vm_egress handle 14",
            "nft delete rule ip taritd_nat vm_input handle 15",
            "nft delete rule ip taritd_nat vm_input handle 16",
            "nft delete rule ip taritd_nat vm_input handle 17",
        ] {
            assert_eq!(
                commands.lines().filter(|line| *line == expected).count(),
                1,
                "{commands}"
            );
            assert!(commands.find(expected).unwrap() < link_ups[1], "{commands}");
        }
        for expected in [
            "sysctl -qw net.ipv6.conf.insta7.disable_ipv6=1",
            "nft add table netdev taritd_ingress_7",
            "nft add chain netdev taritd_ingress_7 ingress",
            "nft add rule netdev taritd_ingress_7 ingress ether type arp accept",
            "nft add rule netdev taritd_ingress_7 ingress ether type ip accept",
            "nft add rule ip taritd_nat vm_egress iifname \"insta7\" ip saddr != 172.16.0.30 counter drop",
            "nft add rule ip taritd_nat vm_egress iifname \"insta7\" ip saddr 172.16.0.30 ip daddr 172.16.0.0/16 drop",
            "nft add rule ip taritd_nat vm_egress iifname \"insta7\" ip saddr 172.16.0.30 oifname != \"eth0\" drop",
            "nft add rule ip taritd_nat vm_egress iifname \"insta7\" ip saddr 172.16.0.30 ct state established,related accept",
            "nft add rule ip taritd_nat vm_egress iifname \"insta7\" ip saddr 172.16.0.30 ip daddr 198.51.100.10/32 tcp dport 443 accept",
            "nft add rule ip taritd_nat vm_egress iifname \"insta7\" ip saddr 172.16.0.30 drop",
            "nft add rule ip taritd_nat vm_input iifname \"insta7\" ip saddr != 172.16.0.30 counter drop",
            "nft add rule ip taritd_nat vm_input iifname \"insta7\" ct state established,related accept",
            "nft add rule ip taritd_nat vm_input iifname \"insta7\" drop",
            "nft add rule ip taritd_nat post iifname \"insta7\" ip saddr 172.16.0.30 oifname \"eth0\" masquerade",
        ] {
            let additions = commands
                .match_indices(expected)
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            assert_eq!(additions.len(), 2, "recovery omitted {expected:?}:\n{commands}");
            assert!(
                additions.iter().zip(&link_ups).all(|(add, up)| add < up),
                "recovery delayed required guard {expected:?}:\n{commands}"
            );
        }
    }

    #[test]
    fn recovery_sweep_failure_keeps_recovered_tap_contained_before_activation() {
        let _environment_guard = RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let vm_id = Uuid::new_v4();
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-recovery-sweep-failure-test-{}-{sequence}",
                std::process::id()
            ));
        let bin = root.join("bin");
        let log = root.join("commands.log");
        let fake_state = root.join("fake-state");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();

        let command = r#"#!/bin/sh
printf '%s %s\n' "${0##*/}" "$*" >> "$TARIT_TEST_COMMAND_LOG"
case "${0##*/}:$*" in
  "ip:-o link show") echo "7: insta7: <BROADCAST,UP> mtu 1500" ;;
  "nft:list tables netdev")
    calls=$(cat "$TARIT_TEST_FAKE_STATE.table-calls" 2>/dev/null || echo 0)
    calls=$((calls + 1))
    echo "$calls" > "$TARIT_TEST_FAKE_STATE.table-calls"
    [ "$calls" -eq 1 ] || { echo "simulated orphan sweep failure" >&2; exit 1; }
    echo "table netdev taritd_ingress_7"
    ;;
  "nft:-a list table netdev taritd_ingress_7") cat <<EOF
table netdev taritd_ingress_7 {
 chain ingress {
  type filter hook ingress device "insta7" priority filter; policy drop;
  ether type arp accept comment "taritd-ingress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7"
  ether type ip accept comment "taritd-ingress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7"
 }
}
EOF
    ;;
  "sysctl:-qn net.ipv6.conf.insta7.forwarding"|"sysctl:-qn net.ipv6.conf.insta7.accept_ra"|"sysctl:-qn net.ipv6.conf.insta7.autoconf"|"sysctl:-qn net.ipv6.conf.insta7.accept_redirects") echo 0 ;;
  "sysctl:-qn net.ipv6.conf.insta7.disable_ipv6"|"sysctl:-qn net.ipv4.conf.insta7.rp_filter") echo 1 ;;
  "nft:-a list chain ip taritd_nat "*)
    calls=$(cat "$TARIT_TEST_FAKE_STATE.chain-calls" 2>/dev/null || echo 0)
    calls=$((calls + 1))
    echo "$calls" > "$TARIT_TEST_FAKE_STATE.chain-calls"
    [ "$calls" -lt 8 ] || { echo "simulated orphan sweep failure" >&2; exit 1; }
    case "$*" in
      "-a list chain ip taritd_nat post") echo "iifname \"insta7\" ip saddr 172.16.0.30 oifname \"eth0\" masquerade comment \"taritd slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 1" ;;
      "-a list chain ip taritd_nat vm_egress") cat <<EOF
iifname "insta7" drop comment "taritd-recovery-quarantine slot=7 vm=$TARIT_TEST_VM_ID tap=insta7" # handle 20
iifname "insta7" ip saddr != 172.16.0.30 counter drop comment "taritd-guard slot=7 vm=$TARIT_TEST_VM_ID tap=insta7" # handle 2
iifname "insta7" ip saddr 172.16.0.30 ip daddr 172.16.0.0/16 drop comment "taritd-guard slot=7 vm=$TARIT_TEST_VM_ID tap=insta7" # handle 3
iifname "insta7" ip saddr 172.16.0.30 oifname != "eth0" drop comment "taritd-guard slot=7 vm=$TARIT_TEST_VM_ID tap=insta7" # handle 4
iifname "insta7" ip saddr 172.16.0.30 drop comment "taritd-egress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7" # handle 5
EOF
        ;;
      "-a list chain ip taritd_nat vm_input") cat <<EOF
iifname "insta7" drop comment "taritd-recovery-quarantine slot=7 vm=$TARIT_TEST_VM_ID tap=insta7" # handle 21
iifname "insta7" ip saddr != 172.16.0.30 counter drop comment "taritd-input slot=7 vm=$TARIT_TEST_VM_ID tap=insta7" # handle 6
iifname "insta7" ct state established,related accept comment "taritd-input slot=7 vm=$TARIT_TEST_VM_ID tap=insta7" # handle 7
iifname "insta7" drop comment "taritd-input slot=7 vm=$TARIT_TEST_VM_ID tap=insta7" # handle 8
EOF
        ;;
    esac
    ;;
  "nft:-f -") cat >/dev/null ;;
esac
"#;
        for name in ["ip", "nft", "sysctl"] {
            let path = bin.join(name);
            std::fs::write(&path, command).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = std::fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&path, permissions).unwrap();
            }
        }

        let mut allocator = SlotAllocator::empty();
        let _alloc = allocator.allocate(vm_id).unwrap();
        allocator.by_slot.remove(&0);
        allocator.by_slot.insert(7, vm_id);
        allocator.by_vm.insert(vm_id, 7);
        let recovered = NetAlloc::for_slot(vm_id, 7).unwrap();
        let provisioner = NetProvisioner {
            inner: Mutex::new(allocator),
            network_transactions: NetworkTransactionLock::default(),
            state_path: root.join("state.json"),
            uplink: "eth0".into(),
            fail_closed: AtomicBool::new(false),
        };
        let old_path = std::env::var_os("PATH");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                old_path.as_deref().unwrap_or_default().to_string_lossy()
            ),
        );
        std::env::set_var("TARIT_TEST_COMMAND_LOG", &log);
        std::env::set_var("TARIT_TEST_FAKE_STATE", &fake_state);
        std::env::set_var("TARIT_TEST_VM_ID", vm_id.to_string());
        let result = provisioner.reconcile_recovered_allocations(&[recovered]);
        if let Some(path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        for variable in [
            "TARIT_TEST_COMMAND_LOG",
            "TARIT_TEST_FAKE_STATE",
            "TARIT_TEST_VM_ID",
        ] {
            std::env::remove_var(variable);
        }

        let error = result.expect_err("an orphan sweep failure must fail recovery");
        let commands = std::fs::read_to_string(&log).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(
            error.to_string().contains("simulated orphan sweep failure"),
            "{error}"
        );
        assert!(!commands.contains("ip link set insta7 up"), "{commands}");
    }

    #[test]
    fn recovered_allocation_keeps_quarantine_when_link_down_fails() {
        let _environment_guard = RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let vm_id = Uuid::new_v4();
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-recovery-link-down-failure-test-{}-{sequence}",
                std::process::id()
            ));
        let bin = root.join("bin");
        let log = root.join("commands.log");
        let state_path = root.join("state.json");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            &state_path,
            format!(
                r#"{{"version":2,"allocations":[{{"slot":7,"vm_id":"{vm_id}","tap":"insta7","egress":{{"allowlist":[],"allow_existing":false}}}}]}}"#
            ),
        )
        .unwrap();

        let command = r#"#!/bin/sh
printf '%s %s\n' "${0##*/}" "$*" >> "$TARIT_TEST_COMMAND_LOG"
case "${0##*/}:$*" in
  "ip:route get 8.8.8.8") echo "8.8.8.8 via 192.0.2.1 dev eth0 src 192.0.2.2" ;;
  "ip:link set insta7 down") echo "simulated link-down failure" >&2; exit 1 ;;
  "ip:-j link show") echo '[]' ;;
  "nft:-j list chain ip taritd_nat post") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"post","type":"nat","hook":"postrouting","prio":100,"policy":"accept"}}]}' ;;
  "nft:-j list chain ip taritd_nat vm_egress") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"vm_egress","type":"filter","hook":"forward","prio":0,"policy":"accept"}}]}' ;;
  "nft:-j list chain ip taritd_nat vm_input") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"vm_input","type":"filter","hook":"input","prio":0,"policy":"accept"}}]}' ;;
  "nft:-f -") cat >/dev/null ;;
esac
"#;
        for name in ["ip", "nft", "sysctl"] {
            let path = bin.join(name);
            std::fs::write(&path, command).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = std::fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&path, permissions).unwrap();
            }
        }

        let old_path = std::env::var_os("PATH");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                old_path.as_deref().unwrap_or_default().to_string_lossy()
            ),
        );
        std::env::set_var("TARIT_TEST_COMMAND_LOG", &log);
        let result = NetProvisioner::new(state_path, [vm_id]);
        if let Some(path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        std::env::remove_var("TARIT_TEST_COMMAND_LOG");

        assert!(result.is_err());
        let commands = std::fs::read_to_string(&log).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(
            commands.find("nft -f -").unwrap() < commands.find("ip link set insta7 down").unwrap(),
            "{commands}"
        );
        assert!(!commands.contains("ip link set insta7 up"), "{commands}");
        assert!(
            !commands.contains("nft delete rule ip taritd_nat vm_egress"),
            "{commands}"
        );
    }

    #[test]
    fn failed_quarantine_installation_emergency_isolates_every_recovered_tap() {
        let _environment_guard = RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let first_vm_id = Uuid::new_v4();
        let second_vm_id = Uuid::new_v4();
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-recovery-quarantine-failure-test-{}-{sequence}",
                std::process::id()
            ));
        let bin = root.join("bin");
        let log = root.join("commands.log");
        let state_path = root.join("state.json");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            &state_path,
            format!(
                r#"{{"version":2,"allocations":[{{"slot":7,"vm_id":"{first_vm_id}","tap":"insta7","egress":{{"allowlist":[],"allow_existing":false}}}},{{"slot":8,"vm_id":"{second_vm_id}","tap":"insta8","egress":{{"allowlist":[],"allow_existing":false}}}}]}}"#,
            ),
        )
        .unwrap();

        let command = r#"#!/bin/sh
printf '%s %s\n' "${0##*/}" "$*" >> "$TARIT_TEST_COMMAND_LOG"
case "${0##*/}:$*" in
  "ip:route get 8.8.8.8") echo "8.8.8.8 via 192.0.2.1 dev eth0 src 192.0.2.2" ;;
  "ip:link set insta7 down") echo "simulated link-down failure" >&2; exit 1 ;;
  "ip:link del insta7") echo "simulated link-delete failure" >&2; exit 1 ;;
  "ip:-j link show") echo '[]' ;;
  "nft:-j list chain ip taritd_nat post") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"post","type":"nat","hook":"postrouting","prio":100,"policy":"accept"}}]}' ;;
  "nft:-j list chain ip taritd_nat vm_egress") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"vm_egress","type":"filter","hook":"forward","prio":0,"policy":"accept"}}]}' ;;
  "nft:-j list chain ip taritd_nat vm_input") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"vm_input","type":"filter","hook":"input","prio":0,"policy":"accept"}}]}' ;;
  "nft:-f -") cat >/dev/null; echo "simulated quarantine failure" >&2; exit 1 ;;
esac
"#;
        for name in ["ip", "nft", "sysctl"] {
            let path = bin.join(name);
            std::fs::write(&path, command).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = std::fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&path, permissions).unwrap();
            }
        }

        let old_path = std::env::var_os("PATH");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                old_path.as_deref().unwrap_or_default().to_string_lossy()
            ),
        );
        std::env::set_var("TARIT_TEST_COMMAND_LOG", &log);
        let result = NetProvisioner::new(state_path, [first_vm_id, second_vm_id]);
        if let Some(path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        std::env::remove_var("TARIT_TEST_COMMAND_LOG");

        let error = match result {
            Ok(_) => panic!("quarantine failure unexpectedly recovered allocations"),
            Err(error) => error.to_string(),
        };
        let commands = std::fs::read_to_string(&log).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(error.contains("nft -f - failed"), "{error}");
        assert!(
            error.contains("emergency link-down failed for insta7"),
            "{error}"
        );
        let quarantine = commands.find("nft -f -").unwrap();
        for expected in [
            "ip link set insta7 down",
            "ip link del insta7",
            "ip link set insta8 down",
            "sysctl -qw net.ipv4.ip_forward=0",
            "sysctl -qw net.ipv6.conf.all.forwarding=0",
        ] {
            assert!(
                commands
                    .find(expected)
                    .is_some_and(|index| quarantine < index),
                "missing emergency containment {expected:?}:\n{commands}"
            );
        }
        assert!(!commands.contains("ip link set insta7 up"), "{commands}");
        assert!(!commands.contains("ip link set insta8 up"), "{commands}");
    }

    #[test]
    fn later_recovery_failure_keeps_every_recovered_tap_quarantined_and_down() {
        let _environment_guard = RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let first_vm_id = Uuid::new_v4();
        let second_vm_id = Uuid::new_v4();
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-recovery-transaction-failure-test-{}-{sequence}",
                std::process::id()
            ));
        let bin = root.join("bin");
        let log = root.join("commands.log");
        let fake_state = root.join("fake-state");
        let quarantine = root.join("quarantine.nft");
        let state_path = root.join("state.json");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            &state_path,
            format!(
                r#"{{"version":2,"allocations":[{{"slot":7,"vm_id":"{first_vm_id}","tap":"insta7","egress":{{"allowlist":[],"allow_existing":false}}}},{{"slot":8,"vm_id":"{second_vm_id}","tap":"insta8","egress":{{"allowlist":[],"allow_existing":false}}}}]}}"#,
            ),
        )
        .unwrap();

        let command = r#"#!/bin/sh
printf '%s %s\n' "${0##*/}" "$*" >> "$TARIT_TEST_COMMAND_LOG"
case "${0##*/}:$*" in
  "ip:route get 8.8.8.8") echo "8.8.8.8 via 192.0.2.1 dev eth0 src 192.0.2.2" ;;
  "ip:link set insta7 down") touch "$TARIT_TEST_FAKE_STATE.insta7.down"; rm -f "$TARIT_TEST_FAKE_STATE.insta7.up" ;;
  "ip:link set insta7 up") touch "$TARIT_TEST_FAKE_STATE.insta7.up"; rm -f "$TARIT_TEST_FAKE_STATE.insta7.down" ;;
  "ip:link set insta8 down") touch "$TARIT_TEST_FAKE_STATE.insta8.down"; rm -f "$TARIT_TEST_FAKE_STATE.insta8.up" ;;
  "ip:link set insta8 up") touch "$TARIT_TEST_FAKE_STATE.insta8.up"; rm -f "$TARIT_TEST_FAKE_STATE.insta8.down" ;;
  "ip:-j link show") echo '[]' ;;
  "nft:-j list chain ip taritd_nat post") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"post","type":"nat","hook":"postrouting","prio":100,"policy":"accept"}}]}' ;;
  "nft:-j list chain ip taritd_nat vm_egress") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"vm_egress","type":"filter","hook":"forward","prio":0,"policy":"accept"}}]}' ;;
  "nft:-j list chain ip taritd_nat vm_input") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"vm_input","type":"filter","hook":"input","prio":0,"policy":"accept"}}]}' ;;
  "sysctl:-qn net.ipv6.conf.insta7.forwarding"|"sysctl:-qn net.ipv6.conf.insta7.accept_ra"|"sysctl:-qn net.ipv6.conf.insta7.autoconf"|"sysctl:-qn net.ipv6.conf.insta7.accept_redirects") echo 0 ;;
  "sysctl:-qn net.ipv6.conf.insta7.disable_ipv6"|"sysctl:-qn net.ipv4.conf.insta7.rp_filter") echo 1 ;;
  "nft:-f -") cat > "$TARIT_TEST_QUARANTINE" ;;
  "nft:list tables netdev")
    [ ! -e "$TARIT_TEST_FAKE_STATE.policy7" ] || echo "table netdev taritd_ingress_7"
    [ ! -e "$TARIT_TEST_FAKE_STATE.policy8" ] || echo "table netdev taritd_ingress_8"
    ;;
  "nft:add table netdev taritd_ingress_7") touch "$TARIT_TEST_FAKE_STATE.policy7" ;;
  "nft:add table netdev taritd_ingress_8") touch "$TARIT_TEST_FAKE_STATE.policy8" ;;
  "nft:add rule ip taritd_nat vm_egress iifname \"insta8\" ip saddr !="*) touch "$TARIT_TEST_FAKE_STATE.partial8"; exit 1 ;;
  "nft:-a list table netdev taritd_ingress_7") [ ! -e "$TARIT_TEST_FAKE_STATE.policy7" ] || {
    echo "table netdev taritd_ingress_7 { chain ingress { type filter hook ingress device \"insta7\" priority filter; policy drop; ether type arp accept comment \"taritd-ingress slot=7 vm=$TARIT_TEST_VM_7 tap=insta7\"; ether type ip accept comment \"taritd-ingress slot=7 vm=$TARIT_TEST_VM_7 tap=insta7\"; } }"
  } ;;
  "nft:-a list table netdev taritd_ingress_8") [ ! -e "$TARIT_TEST_FAKE_STATE.policy8" ] || {
    echo "table netdev taritd_ingress_8 { chain ingress { type filter hook ingress device \"insta8\" priority filter; policy drop; ether type arp accept comment \"taritd-ingress slot=8 vm=$TARIT_TEST_VM_8 tap=insta8\"; ether type ip accept comment \"taritd-ingress slot=8 vm=$TARIT_TEST_VM_8 tap=insta8\"; } }"
  } ;;
  "nft:-a list chain ip taritd_nat post")
    [ ! -e "$TARIT_TEST_FAKE_STATE.policy7" ] || echo "iifname \"insta7\" ip saddr 172.16.0.30 oifname \"eth0\" masquerade comment \"taritd slot=7 vm=$TARIT_TEST_VM_7 tap=insta7\" # handle 11"
    ;;
  "nft:-a list chain ip taritd_nat vm_egress")
    echo "iifname \"insta7\" drop comment \"taritd-recovery-quarantine slot=7 vm=$TARIT_TEST_VM_7 tap=insta7\" # handle 20"
    echo "iifname \"insta8\" drop comment \"taritd-recovery-quarantine slot=8 vm=$TARIT_TEST_VM_8 tap=insta8\" # handle 21"
    [ ! -e "$TARIT_TEST_FAKE_STATE.policy7" ] || {
      echo "iifname \"insta7\" ip saddr != 172.16.0.30 counter drop comment \"taritd-guard slot=7 vm=$TARIT_TEST_VM_7 tap=insta7\" # handle 12"
      echo "iifname \"insta7\" ip saddr 172.16.0.30 ip daddr 172.16.0.0/16 drop comment \"taritd-guard slot=7 vm=$TARIT_TEST_VM_7 tap=insta7\" # handle 15"
      echo "iifname \"insta7\" ip saddr 172.16.0.30 oifname != \"eth0\" drop comment \"taritd-guard slot=7 vm=$TARIT_TEST_VM_7 tap=insta7\" # handle 16"
      echo "iifname \"insta7\" ip saddr 172.16.0.30 drop comment \"taritd-egress slot=7 vm=$TARIT_TEST_VM_7 tap=insta7\" # handle 19"
    }
    [ ! -e "$TARIT_TEST_FAKE_STATE.partial8" ] || echo "iifname \"insta8\" ip saddr != 172.16.0.34 counter drop comment \"taritd-guard slot=8 vm=$TARIT_TEST_VM_8 tap=insta8\" # handle 13"
    ;;
  "nft:-a list chain ip taritd_nat vm_input")
    echo "iifname \"insta7\" drop comment \"taritd-recovery-quarantine slot=7 vm=$TARIT_TEST_VM_7 tap=insta7\" # handle 22"
    echo "iifname \"insta8\" drop comment \"taritd-recovery-quarantine slot=8 vm=$TARIT_TEST_VM_8 tap=insta8\" # handle 23"
    [ ! -e "$TARIT_TEST_FAKE_STATE.policy7" ] || {
      echo "iifname \"insta7\" ip saddr != 172.16.0.30 counter drop comment \"taritd-input slot=7 vm=$TARIT_TEST_VM_7 tap=insta7\" # handle 14"
      echo "iifname \"insta7\" ct state established,related accept comment \"taritd-input slot=7 vm=$TARIT_TEST_VM_7 tap=insta7\" # handle 17"
      echo "iifname \"insta7\" drop comment \"taritd-input slot=7 vm=$TARIT_TEST_VM_7 tap=insta7\" # handle 18"
    }
    ;;
  "nft:delete rule ip taritd_nat post handle 11") touch "$TARIT_TEST_FAKE_STATE.policy7-nat-cleaned" ;;
  "nft:delete rule ip taritd_nat vm_egress handle 12") touch "$TARIT_TEST_FAKE_STATE.policy7-guard-cleaned" ;;
  "nft:delete rule ip taritd_nat vm_egress handle 15"|"nft:delete rule ip taritd_nat vm_egress handle 16") touch "$TARIT_TEST_FAKE_STATE.policy7-guard-cleaned" ;;
  "nft:delete rule ip taritd_nat vm_egress handle 19") touch "$TARIT_TEST_FAKE_STATE.policy7-egress-cleaned" ;;
  "nft:delete rule ip taritd_nat vm_egress handle 13") touch "$TARIT_TEST_FAKE_STATE.policy8-guard-cleaned" ;;
  "nft:delete rule ip taritd_nat vm_input handle 14") touch "$TARIT_TEST_FAKE_STATE.policy7-input-cleaned" ;;
  "nft:delete rule ip taritd_nat vm_input handle 17"|"nft:delete rule ip taritd_nat vm_input handle 18") touch "$TARIT_TEST_FAKE_STATE.policy7-input-cleaned" ;;
  "nft:delete rule ip taritd_nat vm_egress handle 20"|"nft:delete rule ip taritd_nat vm_egress handle 21"|"nft:delete rule ip taritd_nat vm_input handle 22"|"nft:delete rule ip taritd_nat vm_input handle 23") touch "$TARIT_TEST_FAKE_STATE.quarantine-removed" ;;
  "nft:delete table netdev taritd_ingress_7") touch "$TARIT_TEST_FAKE_STATE.policy7-ingress-cleaned" ;;
  "nft:delete table netdev taritd_ingress_8") touch "$TARIT_TEST_FAKE_STATE.policy8-ingress-cleaned" ;;
esac
"#;
        for name in ["ip", "nft", "sysctl"] {
            let path = bin.join(name);
            std::fs::write(&path, command).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = std::fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&path, permissions).unwrap();
            }
        }

        let old_path = std::env::var_os("PATH");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                old_path.as_deref().unwrap_or_default().to_string_lossy()
            ),
        );
        std::env::set_var("TARIT_TEST_COMMAND_LOG", &log);
        std::env::set_var("TARIT_TEST_FAKE_STATE", &fake_state);
        std::env::set_var("TARIT_TEST_QUARANTINE", &quarantine);
        std::env::set_var("TARIT_TEST_VM_7", first_vm_id.to_string());
        std::env::set_var("TARIT_TEST_VM_8", second_vm_id.to_string());
        let result = NetProvisioner::new(state_path, [first_vm_id, second_vm_id]);
        if let Some(path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        for variable in [
            "TARIT_TEST_COMMAND_LOG",
            "TARIT_TEST_FAKE_STATE",
            "TARIT_TEST_QUARANTINE",
            "TARIT_TEST_VM_7",
            "TARIT_TEST_VM_8",
        ] {
            std::env::remove_var(variable);
        }

        assert!(result.is_err());
        let commands = std::fs::read_to_string(&log).unwrap();
        let quarantine_script = std::fs::read_to_string(&quarantine).unwrap();
        let first_cleanup = commands
            .find("nft delete rule ip taritd_nat post handle 11")
            .unwrap();
        for tap in ["insta7", "insta8"] {
            assert!(
                commands
                    .rfind(&format!("ip link set {tap} down"))
                    .is_some_and(|index| index < first_cleanup),
                "cleanup began before the late containment of {tap}:\n{commands}"
            );
        }
        for expected in [
            "taritd-recovery-quarantine slot=7",
            "taritd-recovery-quarantine slot=8",
        ] {
            assert!(quarantine_script.contains(expected), "{quarantine_script}");
        }
        for path in [
            fake_state.with_extension("insta7.down"),
            fake_state.with_extension("insta8.down"),
            fake_state.with_extension("policy7-nat-cleaned"),
            fake_state.with_extension("policy7-guard-cleaned"),
            fake_state.with_extension("policy7-egress-cleaned"),
            fake_state.with_extension("policy7-input-cleaned"),
            fake_state.with_extension("policy7-ingress-cleaned"),
            fake_state.with_extension("policy8-guard-cleaned"),
            fake_state.with_extension("policy8-ingress-cleaned"),
        ] {
            assert!(
                path.exists(),
                "missing stateful cleanup marker {}",
                path.display()
            );
        }
        assert!(
            !fake_state.with_extension("quarantine-removed").exists(),
            "failure removed the retained quarantine:\n{commands}"
        );
        std::fs::remove_dir_all(&root).unwrap();
        assert!(!commands.contains("ip link set insta7 up"), "{commands}");
        assert!(!commands.contains("ip link set insta8 up"), "{commands}");
    }

    #[test]
    fn recovered_allocation_cleans_partial_policy_and_stays_down() {
        let _environment_guard = RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let vm_id = Uuid::new_v4();
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-recovery-partial-failure-test-{}-{sequence}",
                std::process::id()
            ));
        let bin = root.join("bin");
        let log = root.join("commands.log");
        let fake_state = root.join("fake-state");
        let state_path = root.join("state.json");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            &state_path,
            format!(
                r#"{{"version":2,"allocations":[{{"slot":7,"vm_id":"{vm_id}","tap":"insta7","egress":{{"allowlist":[],"allow_existing":false}}}}]}}"#
            ),
        )
        .unwrap();

        let command = r#"#!/bin/sh
printf '%s %s\n' "${0##*/}" "$*" >> "$TARIT_TEST_COMMAND_LOG"
case "${0##*/}:$*" in
  "ip:route get 8.8.8.8") echo "8.8.8.8 via 192.0.2.1 dev eth0 src 192.0.2.2" ;;
  "ip:-j link show") echo '[]' ;;
  "nft:-j list chain ip taritd_nat post") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"post","type":"nat","hook":"postrouting","prio":100,"policy":"accept"}}]}' ;;
  "nft:-j list chain ip taritd_nat vm_egress") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"vm_egress","type":"filter","hook":"forward","prio":0,"policy":"accept"}}]}' ;;
  "nft:-j list chain ip taritd_nat vm_input") echo '{"nftables":[{"chain":{"family":"ip","table":"taritd_nat","name":"vm_input","type":"filter","hook":"input","prio":0,"policy":"accept"}}]}' ;;
  "nft:-f -") cat >/dev/null ;;
  "nft:add table netdev taritd_ingress_7") touch "$TARIT_TEST_FAKE_STATE.ingress" ;;
  "nft:add rule ip taritd_nat vm_egress iifname \"insta7\" ip saddr != 172.16.0.30 counter drop comment \"taritd-guard slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\"") touch "$TARIT_TEST_FAKE_STATE.partial"; exit 1 ;;
  "nft:-a list chain ip taritd_nat vm_egress") [ ! -e "$TARIT_TEST_FAKE_STATE.partial" ] || echo "iifname \"insta7\" ip saddr != 172.16.0.30 counter drop comment \"taritd-guard slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 10" ;;
  "nft:list tables netdev") [ ! -e "$TARIT_TEST_FAKE_STATE.ingress" ] || echo "table netdev taritd_ingress_7" ;;
  "nft:-a list table netdev taritd_ingress_7") [ ! -e "$TARIT_TEST_FAKE_STATE.ingress" ] || echo "table netdev taritd_ingress_7 { chain ingress { type filter hook ingress device \"insta7\" priority filter; policy drop; ether type arp accept comment \"taritd-ingress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\"; ether type ip accept comment \"taritd-ingress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\"; } }" ;;
  "nft:delete rule ip taritd_nat vm_egress handle 10") touch "$TARIT_TEST_FAKE_STATE.guard-cleaned" ;;
  "nft:delete table netdev taritd_ingress_7") touch "$TARIT_TEST_FAKE_STATE.ingress-cleaned" ;;
esac
"#;
        for name in ["ip", "nft", "sysctl"] {
            let path = bin.join(name);
            std::fs::write(&path, command).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = std::fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&path, permissions).unwrap();
            }
        }

        let old_path = std::env::var_os("PATH");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                old_path.as_deref().unwrap_or_default().to_string_lossy()
            ),
        );
        std::env::set_var("TARIT_TEST_COMMAND_LOG", &log);
        std::env::set_var("TARIT_TEST_FAKE_STATE", &fake_state);
        std::env::set_var("TARIT_TEST_VM_ID", vm_id.to_string());
        let result = NetProvisioner::new(state_path, [vm_id]);
        if let Some(path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        std::env::remove_var("TARIT_TEST_COMMAND_LOG");
        std::env::remove_var("TARIT_TEST_FAKE_STATE");
        std::env::remove_var("TARIT_TEST_VM_ID");

        assert!(result.is_err());
        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(fake_state.with_extension("guard-cleaned").exists());
        assert!(fake_state.with_extension("ingress-cleaned").exists());
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            commands.matches("ip link set insta7 down").count(),
            2,
            "{commands}"
        );
        assert!(!commands.contains("ip link set insta7 up"), "{commands}");
    }

    #[test]
    fn stale_sweep_selects_only_old_orphan_taritd_taps() {
        let taps = vec![
            TapCandidate {
                name: "insta0".into(),
                age: Some(Duration::from_secs(120)),
            },
            TapCandidate {
                name: "insta1".into(),
                age: Some(Duration::from_secs(120)),
            },
            TapCandidate {
                name: "insta2".into(),
                age: Some(Duration::from_secs(1)),
            },
            TapCandidate {
                name: "tap0".into(),
                age: Some(Duration::from_secs(120)),
            },
            TapCandidate {
                name: format!("insta{NET_POOL_SLOTS}"),
                age: Some(Duration::from_secs(120)),
            },
            TapCandidate {
                name: "insta3".into(),
                age: None,
            },
        ];
        let selected = stale_taps_to_sweep(&taps, &HashSet::from([1]), Duration::from_secs(30));
        assert_eq!(
            selected.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["insta0"]
        );
    }

    #[test]
    fn orphan_nft_rule_selection_requires_taritd_comment() {
        let active_vm = Uuid::new_v4();
        let old_vm = Uuid::new_v4();
        let active = BTreeMap::from([(0, active_vm)]);
        let active_line = format!(
            "ip saddr 172.16.0.2 oif eth0 masquerade comment \"taritd slot=0 vm={active_vm} tap=insta0\" # handle 4"
        );
        let stale_line = format!(
            "ip saddr 172.16.0.6 oif eth0 masquerade comment \"taritd slot=1 vm={old_vm} tap=insta1\" # handle 5"
        );
        let stale_egress_line = format!(
            "iifname \"insta1\" ip saddr 172.16.0.6 drop comment \"taritd-egress slot=1 vm={old_vm} tap=insta1\" # handle 6"
        );
        let stale_input_line = format!(
            "iifname \"insta1\" drop comment \"taritd-input slot=1 vm={old_vm} tap=insta1\" # handle 7"
        );
        let foreign_line = "ip saddr 10.0.0.0/8 masquerade # handle 6";

        assert!(!is_orphan_taritd_nft_rule(&active_line, &active));
        assert!(is_orphan_taritd_nft_rule(&stale_line, &active));
        assert!(is_orphan_taritd_nft_rule(&stale_egress_line, &active));
        assert!(is_orphan_taritd_nft_rule(&stale_input_line, &active));
        assert!(!is_orphan_taritd_nft_rule(foreign_line, &active));
        assert_eq!(nft_handle(&stale_line).as_deref(), Some("5"));
    }

    #[test]
    fn parses_ip_link_names_and_tap_slots() {
        assert_eq!(
            parse_ip_link_name("12: insta9: <BROADCAST> mtu 1500"),
            Some("insta9".into())
        );
        assert_eq!(
            parse_ip_link_name("13: insta10@if2: <BROADCAST> mtu 1500"),
            Some("insta10".into())
        );
        assert_eq!(slot_from_tap("insta10"), Some(10));
        assert_eq!(slot_from_tap("insta"), None);
        assert_eq!(slot_from_tap("instaabc"), None);
        assert_eq!(slot_from_tap(&format!("insta{NET_POOL_SLOTS}")), None);
    }

    #[test]
    fn security_chains_reject_an_earlier_unmanaged_accept_and_accept_real_nft_handles() {
        let alloc = NetAlloc::for_idx(7);
        let managed = format!(
            concat!(
                "iifname \"insta7\" ip saddr != 172.16.0.30 counter drop comment \"{}\" # handle 41\n",
                "iifname \"insta7\" ip saddr 172.16.0.30 ip daddr 172.16.0.0/16 drop comment \"{}\" # handle 42\n",
                "iifname \"insta7\" ip saddr 172.16.0.30 oifname != \"eth0\" drop comment \"{}\" # handle 43\n",
                "iifname \"insta7\" ip saddr 172.16.0.30 ct state established,related accept comment \"{}\" # handle 44\n",
                "iifname \"insta7\" ip saddr 172.16.0.30 ip daddr 198.51.100.10/32 tcp dport 443 accept comment \"{}\" # handle 45\n",
                "iifname \"insta7\" ip saddr 172.16.0.30 drop comment \"{}\" # handle 46\n",
            ),
            guard_comment(&alloc),
            guard_comment(&alloc),
            guard_comment(&alloc),
            egress_comment(&alloc),
            egress_comment(&alloc),
            egress_comment(&alloc),
        );

        assert!(validate_taritd_security_chain(NFT_FWD_CHAIN, &managed).is_ok());
        let unsafe_listing = format!(
            "iifname \"insta7\" accept comment \"operator exception\" # handle 40\n{managed}"
        );
        assert!(validate_taritd_security_chain(NFT_FWD_CHAIN, &unsafe_listing).is_err());
        let mut misordered = managed.lines().collect::<Vec<_>>();
        misordered.swap(3, 4);
        let misordered = misordered.join("\n");
        assert!(validate_taritd_security_chain(NFT_FWD_CHAIN, &misordered).is_err());
    }

    #[test]
    fn managed_ingress_table_accepts_real_nft_handle_suffixes() {
        let alloc = NetAlloc::for_idx(7);
        let listing = format!(
            r#"table netdev taritd_ingress_7 {{
 chain ingress {{
  type filter hook ingress device "insta7" priority filter; policy drop;
  ether type arp accept comment "{}" # handle 10
  ether type ip accept comment "{}" # handle 11
 }}
}}"#,
            ingress_comment(&alloc),
            ingress_comment(&alloc),
        );

        assert!(ingress_table_belongs_to_alloc(&listing, &alloc));
        assert_eq!(
            nft_handle(&format!(
                "ether type ip accept comment \"{}\" # handle 11",
                ingress_comment(&alloc)
            ))
            .as_deref(),
            Some("11")
        );
    }

    #[test]
    fn egress_replacement_is_one_transaction_with_final_default_deny() {
        let alloc = NetAlloc::for_idx(7);
        let listing = format!(
            "iifname \"insta7\" ip saddr 172.16.0.30 drop comment \"{}\" # handle 55",
            egress_comment(&alloc)
        );
        let script = egress_replacement_script(
            &alloc,
            &EgressPolicy {
                allowlist: vec!["198.51.100.10:443".into()],
                allow_existing: true,
            },
            &listing,
        )
        .unwrap();

        let delete = script
            .find("delete rule ip taritd_nat vm_egress handle 55")
            .unwrap();
        let stateful = script.find("ct state established,related accept").unwrap();
        let allow = script
            .find("ip daddr 198.51.100.10/32 tcp dport 443 accept")
            .unwrap();
        let deny = script
            .rfind("iifname \"insta7\" ip saddr 172.16.0.30 drop")
            .unwrap();
        assert!(
            delete < stateful && stateful < allow && allow < deny,
            "{script}"
        );
    }

    #[test]
    fn startup_tap_discovery_uses_strict_structured_link_names() {
        let links = r#"[
          {"ifindex":7,"ifname":"insta7","flags":["UP"]},
          {"ifindex":8,"ifname":"insta8@if7","flags":["UP"]},
          {"ifindex":9,"ifname":"insta-not-a-slot","flags":["UP"]},
          {"ifindex":10,"ifname":"operator0","flags":["UP"]}
        ]"#;

        assert_eq!(
            strict_tap_names_from_link_json(links).unwrap(),
            vec!["insta7".to_string()]
        );
    }

    #[test]
    fn network_state_v2_only_migrates_v1_without_live_allocations() {
        assert_eq!(NET_STATE_VERSION, 2);
        let live_vm = Uuid::new_v4();
        let live_v1 = format!(
            r#"{{"version":1,"allocations":[{{"slot":7,"vm_id":"{live_vm}","tap":"insta7"}}]}}"#
        );
        assert!(
            decode_net_state(&live_v1, Path::new("state.json"), &HashSet::from([live_vm])).is_err()
        );

        let empty_v1 = r#"{"version":1,"allocations":[]}"#;
        assert!(decode_net_state(empty_v1, Path::new("state.json"), &HashSet::new()).is_ok());
        let v2 = r#"{"version":2,"allocations":[]}"#;
        assert!(decode_net_state(v2, Path::new("state.json"), &HashSet::new()).is_ok());
        assert!(legacy_v1_reader_accepts_version(2).is_err());
    }

    #[test]
    fn corrupt_state_is_not_loaded_before_existing_strict_taps_are_contained() {
        let _environment_guard = RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-corrupt-state-containment-test-{}-{sequence}",
                std::process::id()
            ));
        let bin = root.join("bin");
        let log = root.join("commands.log");
        let state_path = root.join("state.json");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();
        let original_state = "not JSON";
        std::fs::write(&state_path, original_state).unwrap();
        let command = r#"#!/bin/sh
printf '%s %s\n' "${0##*/}" "$*" >> "$TARIT_TEST_COMMAND_LOG"
case "${0##*/}:$*" in
  "ip:-j link show") echo '[{"ifname":"insta7"},{"ifname":"operator0"}]' ;;
  "ip:link set insta7 down") ;;
  "ip:route get 8.8.8.8") echo "8.8.8.8 via 192.0.2.1 dev eth0 src 192.0.2.2" ;;
esac
"#;
        let path = bin.join("ip");
        std::fs::write(&path, command).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).unwrap();
        }
        let old_path = std::env::var_os("PATH");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                old_path.as_deref().unwrap_or_default().to_string_lossy()
            ),
        );
        std::env::set_var("TARIT_TEST_COMMAND_LOG", &log);
        let result = NetProvisioner::new(state_path.clone(), std::iter::empty());
        if let Some(path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        std::env::remove_var("TARIT_TEST_COMMAND_LOG");

        let commands = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            std::fs::read_to_string(&state_path).unwrap(),
            original_state
        );
        std::fs::remove_dir_all(&root).unwrap();
        assert!(result.is_err());
        assert!(
            commands.find("ip link set insta7 down").unwrap()
                < commands.find("ip route get 8.8.8.8").unwrap(),
            "{commands}"
        );
        assert!(!commands.contains("operator0"), "{commands}");
    }

    #[test]
    fn duplicate_state_is_contained_and_not_rewritten_before_recovery() {
        let _environment_guard = RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let vm_id = Uuid::new_v4();
        let second_vm_id = Uuid::new_v4();
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-duplicate-state-containment-test-{}-{sequence}",
                std::process::id()
            ));
        let bin = root.join("bin");
        let log = root.join("commands.log");
        let state_path = root.join("state.json");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();
        let original_state = format!(
            r#"{{"version":2,"allocations":[{{"slot":7,"vm_id":"{vm_id}","tap":"insta7","egress":{{}}}},{{"slot":7,"vm_id":"{second_vm_id}","tap":"insta7","egress":{{}}}}]}}"#
        );
        std::fs::write(&state_path, &original_state).unwrap();
        let command = r#"#!/bin/sh
printf '%s %s\n' "${0##*/}" "$*" >> "$TARIT_TEST_COMMAND_LOG"
case "${0##*/}:$*" in
  "ip:-j link show") echo '[{"ifname":"insta7"}]' ;;
  "ip:link set insta7 down") ;;
  "ip:route get 8.8.8.8") echo "8.8.8.8 via 192.0.2.1 dev eth0 src 192.0.2.2" ;;
esac
"#;
        for name in ["ip", "nft", "sysctl"] {
            let path = bin.join(name);
            std::fs::write(&path, command).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = std::fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&path, permissions).unwrap();
            }
        }

        let old_path = std::env::var_os("PATH");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                old_path.as_deref().unwrap_or_default().to_string_lossy()
            ),
        );
        std::env::set_var("TARIT_TEST_COMMAND_LOG", &log);
        let result = NetProvisioner::new(state_path.clone(), [vm_id]);
        if let Some(path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        std::env::remove_var("TARIT_TEST_COMMAND_LOG");

        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(&state_path).unwrap(),
            original_state
        );
        assert!(
            commands.find("ip link set insta7 down").unwrap()
                < commands.find("ip route get 8.8.8.8").unwrap(),
            "{commands}"
        );
        assert!(
            !commands.contains("nft "),
            "ambiguous state reached host policy recovery:\n{commands}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn live_egress_update_replaces_rules_atomically_while_quarantined() {
        let _environment_guard = RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let vm_id = Uuid::new_v4();
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-egress-transaction-test-{}-{sequence}",
                std::process::id()
            ));
        let bin = root.join("bin");
        let log = root.join("commands.log");
        let scripts = root.join("scripts.log");
        let state = root.join("fake-state");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();
        let command = r#"#!/bin/sh
printf '%s %s\n' "${0##*/}" "$*" >> "$TARIT_TEST_COMMAND_LOG"
forward_rules() {
  [ ! -e "$TARIT_TEST_STATE.quarantine" ] ||
    echo "iifname \"insta7\" drop comment \"taritd-egress-update-quarantine slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 99"
  echo "iifname \"insta7\" ip saddr != 172.16.0.30 counter drop comment \"taritd-guard slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 11"
  echo "iifname \"insta7\" ip saddr 172.16.0.30 ip daddr 172.16.0.0/16 drop comment \"taritd-guard slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 12"
  echo "iifname \"insta7\" ip saddr 172.16.0.30 oifname != \"eth0\" drop comment \"taritd-guard slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 13"
  if [ -e "$TARIT_TEST_STATE.replaced" ]; then
    echo "iifname \"insta7\" ip saddr 172.16.0.30 ct state established,related accept comment \"taritd-egress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 21"
    echo "iifname \"insta7\" ip saddr 172.16.0.30 ip daddr 198.51.100.10/32 tcp dport 443 accept comment \"taritd-egress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 22"
    echo "iifname \"insta7\" ip saddr 172.16.0.30 drop comment \"taritd-egress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 23"
  else
    echo "iifname \"insta7\" ip saddr 172.16.0.30 drop comment \"taritd-egress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 55"
  fi
}
case "${0##*/}:$*" in
  "nft:-f -")
    script=$(cat)
    printf '%s\n---\n' "$script" >> "$TARIT_TEST_SCRIPTS"
    case "$script" in
      *"taritd-egress-update-quarantine"*) touch "$TARIT_TEST_STATE.quarantine" ;;
      *"delete rule ip taritd_nat vm_egress handle 55"*) touch "$TARIT_TEST_STATE.replaced" ;;
      *"delete rule ip taritd_nat vm_egress handle 21"*)
        [ "${TARIT_TEST_FAIL_REPLACEMENT:-0}" = 1 ] && exit 1
        ;;
      *"delete rule ip taritd_nat vm_egress handle 99"*) rm -f "$TARIT_TEST_STATE.quarantine" ;;
      *) echo "unexpected nft transaction: $script" >&2; exit 1 ;;
    esac
    ;;
  "nft:-a list chain ip taritd_nat post")
    echo "iifname \"insta7\" ip saddr 172.16.0.30 oifname \"eth0\" masquerade comment \"taritd slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 1"
    ;;
  "nft:-a list chain ip taritd_nat vm_egress") forward_rules ;;
  "nft:-a list chain ip taritd_nat vm_input")
    echo "iifname \"insta7\" ip saddr != 172.16.0.30 counter drop comment \"taritd-input slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 31"
    echo "iifname \"insta7\" ct state established,related accept comment \"taritd-input slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 32"
    echo "iifname \"insta7\" drop comment \"taritd-input slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 33"
    ;;
esac
"#;
        for name in ["ip", "nft"] {
            let path = bin.join(name);
            std::fs::write(&path, command).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = std::fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&path, permissions).unwrap();
            }
        }
        let mut allocator = SlotAllocator::empty();
        let _ = allocator.allocate(vm_id).unwrap();
        allocator.by_slot.remove(&0);
        allocator.by_slot.insert(7, vm_id);
        allocator.by_vm.insert(vm_id, 7);
        let provisioner = NetProvisioner {
            inner: Mutex::new(allocator),
            network_transactions: NetworkTransactionLock::default(),
            state_path: root.join("state.json"),
            uplink: "eth0".into(),
            fail_closed: AtomicBool::new(false),
        };
        let old_path = std::env::var_os("PATH");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                old_path.as_deref().unwrap_or_default().to_string_lossy()
            ),
        );
        std::env::set_var("TARIT_TEST_COMMAND_LOG", &log);
        std::env::set_var("TARIT_TEST_SCRIPTS", &scripts);
        std::env::set_var("TARIT_TEST_STATE", &state);
        std::env::set_var("TARIT_TEST_VM_ID", vm_id.to_string());
        let result = provisioner.apply_egress(
            &NetAlloc::for_slot(vm_id, 7).unwrap(),
            &["198.51.100.10:443".into()],
            true,
        );
        if let Some(ref path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        for variable in [
            "TARIT_TEST_COMMAND_LOG",
            "TARIT_TEST_SCRIPTS",
            "TARIT_TEST_STATE",
            "TARIT_TEST_VM_ID",
        ] {
            std::env::remove_var(variable);
        }

        result.unwrap();
        let commands = std::fs::read_to_string(&log).unwrap();
        let scripts_text = std::fs::read_to_string(&scripts).unwrap();
        assert!(!state.with_extension("quarantine").exists());
        assert_eq!(commands.matches("nft -f -").count(), 3, "{commands}");
        assert!(!commands.contains("nft delete rule"), "{commands}");
        assert!(scripts_text.contains("insert rule ip taritd_nat vm_egress"));
        assert!(scripts_text.contains("delete rule ip taritd_nat vm_egress handle 55"));
        let replacement = scripts_text
            .split("---")
            .find(|script| script.contains("handle 55"))
            .unwrap();
        assert!(
            replacement.find("handle 55").unwrap()
                < replacement
                    .find("ct state established,related accept")
                    .unwrap()
                && replacement
                    .find("ct state established,related accept")
                    .unwrap()
                    < replacement
                        .find("ip daddr 198.51.100.10/32 tcp dport 443 accept")
                        .unwrap()
                && replacement
                    .find("ip daddr 198.51.100.10/32 tcp dport 443 accept")
                    .unwrap()
                    < replacement
                        .rfind("iifname \"insta7\" ip saddr 172.16.0.30 drop")
                        .unwrap(),
            "{replacement}"
        );

        std::env::set_var("TARIT_TEST_COMMAND_LOG", &log);
        std::env::set_var("TARIT_TEST_SCRIPTS", &scripts);
        std::env::set_var("TARIT_TEST_STATE", &state);
        std::env::set_var("TARIT_TEST_VM_ID", vm_id.to_string());
        std::env::set_var("TARIT_TEST_FAIL_REPLACEMENT", "1");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                old_path.as_deref().unwrap_or_default().to_string_lossy()
            ),
        );
        let failure = provisioner.apply_egress(
            &NetAlloc::for_slot(vm_id, 7).unwrap(),
            &["203.0.113.10:443".into()],
            true,
        );
        if let Some(ref path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        for variable in [
            "TARIT_TEST_COMMAND_LOG",
            "TARIT_TEST_SCRIPTS",
            "TARIT_TEST_STATE",
            "TARIT_TEST_VM_ID",
            "TARIT_TEST_FAIL_REPLACEMENT",
        ] {
            std::env::remove_var(variable);
        }
        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(failure.is_err());
        assert!(state.with_extension("quarantine").exists());
        assert!(commands.contains("ip link set insta7 down"), "{commands}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn catastrophic_fallback_disables_both_forwarding_families() {
        assert_eq!(
            emergency_forwarding_disable_argv(),
            vec![
                argv(&["sysctl", "-qw", "net.ipv4.ip_forward=0"]),
                argv(&["sysctl", "-qw", "net.ipv6.conf.all.forwarding=0"]),
            ]
        );
    }

    #[test]
    fn security_chain_accepts_real_nft_counter_output() {
        let alloc = NetAlloc::for_idx(7);
        let listing = format!(
            "iifname \"insta7\" ip saddr != 172.16.0.30 counter packets 17 bytes 4096 drop comment \"{}\" # handle 41",
            guard_comment(&alloc)
        );

        assert!(validate_taritd_security_chain(NFT_FWD_CHAIN, &listing).is_ok());
    }

    #[test]
    fn unknown_tagged_egress_shape_is_not_selected_for_deletion() {
        let alloc = NetAlloc::for_idx(7);
        let listing = format!(
            "iifname \"insta7\" ip saddr 172.16.0.30 tcp flags syn accept comment \"{}\" # handle 55",
            egress_comment(&alloc)
        );

        let replacement = egress_replacement_script(
            &alloc,
            &EgressPolicy {
                allowlist: Vec::new(),
                allow_existing: false,
            },
            &listing,
        )
        .unwrap();

        assert!(!replacement.contains("handle 55"), "{replacement}");
        assert!(validate_taritd_security_chain(NFT_FWD_CHAIN, &listing).is_err());

        let malformed_nat = format!(
            "iifname \"insta7\" ip saddr 172.16.0.30 accept comment \"{}\" # handle 56",
            nft_comment(&alloc)
        );
        assert!(!is_recognized_taritd_rule(NFT_CHAIN, &malformed_nat));
        assert!(validate_taritd_nat_chain(&malformed_nat).is_err());
    }

    #[test]
    fn persisted_network_state_is_private_and_has_no_leftover_temp_file() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-state-persistence-test-{}-{sequence}",
                std::process::id()
            ));
        let state_path = root.join("state.json");
        let _ = std::fs::remove_dir_all(&root);

        persist_entries(&state_path, Vec::new()).unwrap();

        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(std::fs::read_to_string(&state_path)
            .unwrap()
            .contains("\"version\": 2"));
        assert!(!state_write_path(&state_path).exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn egress_update_lock_serializes_concurrent_transactions() {
        let lock = std::sync::Arc::new(NetworkTransactionLock::default());
        let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let maximum = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut workers = Vec::new();

        for _ in 0..2 {
            let lock = std::sync::Arc::clone(&lock);
            let in_flight = std::sync::Arc::clone(&in_flight);
            let maximum = std::sync::Arc::clone(&maximum);
            let barrier = std::sync::Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                lock.run(|| {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                })
                .unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }

        assert_eq!(maximum.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn complete_policy_validation_rejects_each_missing_reordered_or_duplicated_rule() {
        let alloc = NetAlloc::for_idx(7);
        let policy = EgressPolicy {
            allowlist: vec!["198.51.100.10:443".into(), "203.0.113.20:53/udp".into()],
            allow_existing: true,
        };
        let (nat, forward, input) = complete_policy_listings(&alloc, &policy);
        assert!(validate_complete_effective_security_policies(
            &[(alloc.clone(), policy.clone())],
            "eth0",
            &nat,
            &forward,
            &input,
        )
        .is_ok());

        for (chain, marker) in [
            ("forward", "ip saddr != 172.16.0.30 counter drop"),
            ("forward", "ip daddr 172.16.0.0/16 drop"),
            ("forward", "oifname != \"eth0\" drop"),
            ("forward", "ct state established,related accept"),
            ("forward", "ip daddr 198.51.100.10/32 tcp dport 443 accept"),
            ("forward", "ip daddr 203.0.113.20/32 udp dport 53 accept"),
            ("forward", "ip saddr 172.16.0.30 drop"),
            ("input", "ip saddr != 172.16.0.30 counter drop"),
            ("input", "ct state established,related accept"),
            ("input", "iifname \"insta7\" drop"),
        ] {
            let mut candidate_forward = forward.clone();
            let mut candidate_input = input.clone();
            let lines = if chain == "forward" {
                &mut candidate_forward
            } else {
                &mut candidate_input
            };
            let position = lines
                .lines()
                .position(|line| line.contains(marker))
                .expect("baseline policy contains rule");
            *lines = lines
                .lines()
                .enumerate()
                .filter(|(index, _)| *index != position)
                .map(|(_, line)| line)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                validate_complete_effective_security_policies(
                    &[(alloc.clone(), policy.clone())],
                    "eth0",
                    &nat,
                    &candidate_forward,
                    &candidate_input,
                )
                .is_err(),
                "missing {chain} rule {marker:?} was accepted"
            );
        }

        for (chain, marker) in [
            ("forward", "ip saddr != 172.16.0.30 counter drop"),
            ("forward", "ip daddr 172.16.0.0/16 drop"),
            ("forward", "oifname != \"eth0\" drop"),
            ("forward", "ct state established,related accept"),
            ("forward", "ip daddr 198.51.100.10/32 tcp dport 443 accept"),
            ("forward", "ip daddr 203.0.113.20/32 udp dport 53 accept"),
            ("forward", "ip saddr 172.16.0.30 drop"),
            ("input", "ip saddr != 172.16.0.30 counter drop"),
            ("input", "ct state established,related accept"),
            ("input", "iifname \"insta7\" drop"),
        ] {
            let mut candidate_forward = forward.clone();
            let mut candidate_input = input.clone();
            let lines = if chain == "forward" {
                &mut candidate_forward
            } else {
                &mut candidate_input
            };
            let duplicate = lines
                .lines()
                .find(|line| line.contains(marker))
                .expect("baseline policy contains rule")
                .to_owned();
            lines.push('\n');
            lines.push_str(&duplicate);
            assert!(
                validate_complete_effective_security_policies(
                    &[(alloc.clone(), policy.clone())],
                    "eth0",
                    &nat,
                    &candidate_forward,
                    &candidate_input,
                )
                .is_err(),
                "duplicate {chain} rule {marker:?} was accepted"
            );
        }

        for (chain, marker, move_to_end) in [
            ("forward", "ip saddr != 172.16.0.30 counter drop", true),
            ("forward", "ip daddr 172.16.0.0/16 drop", true),
            ("forward", "oifname != \"eth0\" drop", true),
            ("forward", "ct state established,related accept", false),
            (
                "forward",
                "ip daddr 198.51.100.10/32 tcp dport 443 accept",
                false,
            ),
            (
                "forward",
                "ip daddr 203.0.113.20/32 udp dport 53 accept",
                false,
            ),
            ("forward", "ip saddr 172.16.0.30 drop", false),
            ("input", "ip saddr != 172.16.0.30 counter drop", true),
            ("input", "ct state established,related accept", false),
            ("input", "iifname \"insta7\" drop", false),
        ] {
            let mut candidate_forward = forward.clone();
            let mut candidate_input = input.clone();
            let lines = if chain == "forward" {
                &mut candidate_forward
            } else {
                &mut candidate_input
            };
            let mut reordered = lines.lines().map(str::to_owned).collect::<Vec<_>>();
            let position = reordered
                .iter()
                .position(|line| line.contains(marker))
                .expect("baseline policy contains rule");
            let moved = reordered.remove(position);
            if move_to_end {
                reordered.push(moved);
            } else {
                reordered.insert(0, moved);
            }
            *lines = reordered.join("\n");
            assert!(
                validate_complete_effective_security_policies(
                    &[(alloc.clone(), policy.clone())],
                    "eth0",
                    &nat,
                    &candidate_forward,
                    &candidate_input,
                )
                .is_err(),
                "reordered {chain} rule {marker:?} was accepted"
            );
        }
    }

    #[test]
    fn complete_policy_validation_rejects_wrong_provisioner_uplink() {
        let alloc = NetAlloc::for_idx(7);
        let policy = EgressPolicy {
            allowlist: vec!["198.51.100.10:443".into()],
            allow_existing: true,
        };
        let (nat, forward, input) = complete_policy_listings(&alloc, &policy);
        let wrong_guard_uplink = forward.replace("oifname != \"eth0\"", "oifname != \"wlan0\"");

        assert!(
            validate_complete_effective_security_policies(
                &[(alloc.clone(), policy.clone())],
                "eth0",
                &nat,
                &wrong_guard_uplink,
                &input,
            )
            .is_err(),
            "a complete forward policy accepted a guard for the wrong uplink"
        );
        let wrong_nat_uplink = nat.replace("oifname \"eth0\"", "oifname \"wlan0\"");
        assert!(
            validate_complete_effective_security_policies(
                &[(alloc, policy)],
                "eth0",
                &wrong_nat_uplink,
                &forward,
                &input,
            )
            .is_err(),
            "a complete policy accepted masquerade for the wrong uplink"
        );
    }

    #[test]
    fn masquerade_validation_rejects_wrong_provisioner_uplink() {
        let alloc = NetAlloc::for_idx(7);
        let wrong_uplink = format!(
            "iifname \"{}\" ip saddr {} oifname \"wlan0\" masquerade",
            alloc.tap, alloc.guest_ip
        );

        assert!(
            !valid_masquerade_rule_for_uplink(
                &wrong_uplink,
                &nft_quote(&alloc.tap),
                &alloc.guest_ip,
                "eth0",
            ),
            "a masquerade rule for the wrong uplink was accepted"
        );
    }

    #[test]
    fn teardown_link_delete_failure_retains_policy_and_slot() {
        let _environment_guard = RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let vm_id = Uuid::new_v4();
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-teardown-link-delete-failure-test-{}-{sequence}",
                std::process::id()
            ));
        let bin = root.join("bin");
        let log = root.join("commands.log");
        let state_path = root.join("state.json");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();

        let command = r#"#!/bin/sh
printf '%s %s\n' "${0##*/}" "$*" >> "$TARIT_TEST_COMMAND_LOG"
case "${0##*/}:$*" in
  "ip:-j link show") echo '[{"ifname":"insta7"}]' ;;
  "ip:link set insta7 down") ;;
  "ip:link del insta7") echo "simulated link-delete failure" >&2; exit 1 ;;
  "nft:-a list chain ip taritd_nat post") echo "iifname \"insta7\" ip saddr 172.16.0.30 oifname \"eth0\" masquerade comment \"taritd slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 1" ;;
  "nft:-a list chain ip taritd_nat vm_egress") echo "iifname \"insta7\" ip saddr 172.16.0.30 drop comment \"taritd-egress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 2" ;;
  "nft:-a list chain ip taritd_nat vm_input") echo "iifname \"insta7\" drop comment \"taritd-input slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 3" ;;
  "nft:list tables netdev") echo "table netdev taritd_ingress_7" ;;
  "nft:-a list table netdev taritd_ingress_7") cat <<EOF
table netdev taritd_ingress_7 {
 chain ingress {
  type filter hook ingress device "insta7" priority filter; policy drop;
  ether type arp accept comment "taritd-ingress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7"
  ether type ip accept comment "taritd-ingress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7"
 }
}
EOF
  ;;
esac
"#;
        for name in ["ip", "nft"] {
            let path = bin.join(name);
            std::fs::write(&path, command).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = std::fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&path, permissions).unwrap();
            }
        }

        let mut allocator = SlotAllocator::empty();
        let alloc = NetAlloc::for_slot(vm_id, 7).unwrap();
        allocator.free.remove(&7);
        allocator.by_slot.insert(7, vm_id);
        allocator.by_vm.insert(vm_id, 7);
        allocator
            .egress_by_vm
            .insert(vm_id, Some(EgressPolicy::default()));
        persist_allocator(&state_path, &allocator).unwrap();
        let provisioner = NetProvisioner {
            inner: Mutex::new(allocator),
            network_transactions: NetworkTransactionLock::default(),
            state_path: state_path.clone(),
            uplink: "eth0".into(),
            fail_closed: AtomicBool::new(false),
        };

        let old_path = std::env::var_os("PATH");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                old_path.as_deref().unwrap_or_default().to_string_lossy()
            ),
        );
        std::env::set_var("TARIT_TEST_COMMAND_LOG", &log);
        std::env::set_var("TARIT_TEST_VM_ID", vm_id.to_string());
        let error = provisioner
            .teardown(&alloc)
            .expect_err("a failed TAP deletion must fail teardown for callers");
        if let Some(path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        std::env::remove_var("TARIT_TEST_COMMAND_LOG");
        std::env::remove_var("TARIT_TEST_VM_ID");

        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(
            error.to_string().contains("cannot delete contained TAP"),
            "{error}"
        );
        assert!(provisioner.require_active_allocation(&alloc).is_ok());
        assert!(std::fs::read_to_string(&state_path)
            .unwrap()
            .contains(&vm_id.to_string()));
        std::fs::remove_dir_all(&root).unwrap();
        assert!(commands.contains("ip link set insta7 down"), "{commands}");
        assert!(commands.contains("ip link del insta7"), "{commands}");
        assert!(
            !commands.contains("nft delete rule") && !commands.contains("nft delete table"),
            "teardown removed policy after link deletion failed:\n{commands}"
        );
    }

    #[test]
    fn teardown_keeps_ownership_when_an_unknown_exact_tagged_rule_remains() {
        let _environment_guard = RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let vm_id = Uuid::new_v4();
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-teardown-unknown-residual-test-{}-{sequence}",
                std::process::id()
            ));
        let bin = root.join("bin");
        let log = root.join("commands.log");
        let state_path = root.join("state.json");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();

        let command = r#"#!/bin/sh
printf '%s %s\n' "${0##*/}" "$*" >> "$TARIT_TEST_COMMAND_LOG"
case "${0##*/}:$*" in
  "ip:-j link show")
    [ -e "$TARIT_TEST_STATE.tap-deleted" ] && echo '[]' || echo '[{"ifname":"insta7"}]'
    ;;
  "ip:link set insta7 down") ;;
  "ip:link del insta7") touch "$TARIT_TEST_STATE.tap-deleted" ;;
  "nft:-a list chain ip taritd_nat post")
    echo "iifname \"insta7\" ip saddr 172.16.0.30 oifname \"eth0\" masquerade comment \"taritd slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 1"
    ;;
  "nft:-a list chain ip taritd_nat vm_egress")
    echo "iifname \"insta7\" ip saddr 172.16.0.30 drop comment \"taritd-egress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 2"
    echo "iifname \"insta7\" ip saddr 172.16.0.30 tcp flags syn accept comment \"taritd-egress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 55"
    ;;
  "nft:-a list chain ip taritd_nat vm_input")
    echo "iifname \"insta7\" drop comment \"taritd-input slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 3"
    ;;
  "nft:list tables netdev") ;;
  "nft:-a list table ip taritd_nat")
    echo "table ip taritd_nat {"
    echo " chain vm_egress {"
    echo "  iifname \"insta7\" ip saddr 172.16.0.30 tcp flags syn accept comment \"taritd-egress slot=7 vm=$TARIT_TEST_VM_ID tap=insta7\" # handle 55"
    echo " }"
    echo "}"
    ;;
esac
"#;
        for name in ["ip", "nft"] {
            let path = bin.join(name);
            std::fs::write(&path, command).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = std::fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&path, permissions).unwrap();
            }
        }

        let mut allocator = SlotAllocator::empty();
        let alloc = NetAlloc::for_slot(vm_id, 7).unwrap();
        allocator.free.remove(&7);
        allocator.by_slot.insert(7, vm_id);
        allocator.by_vm.insert(vm_id, 7);
        allocator
            .egress_by_vm
            .insert(vm_id, Some(EgressPolicy::default()));
        persist_allocator(&state_path, &allocator).unwrap();
        let provisioner = NetProvisioner {
            inner: Mutex::new(allocator),
            network_transactions: NetworkTransactionLock::default(),
            state_path: state_path.clone(),
            uplink: "eth0".into(),
            fail_closed: AtomicBool::new(false),
        };

        let old_path = std::env::var_os("PATH");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                old_path.as_deref().unwrap_or_default().to_string_lossy()
            ),
        );
        std::env::set_var("TARIT_TEST_COMMAND_LOG", &log);
        std::env::set_var("TARIT_TEST_STATE", root.join("fake-state"));
        std::env::set_var("TARIT_TEST_VM_ID", vm_id.to_string());
        let error = provisioner
            .teardown(&alloc)
            .expect_err("an unknown tagged residual must retain ownership");
        if let Some(path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        for variable in [
            "TARIT_TEST_COMMAND_LOG",
            "TARIT_TEST_STATE",
            "TARIT_TEST_VM_ID",
        ] {
            std::env::remove_var(variable);
        }

        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(
            error.to_string().contains("tagged policy remains"),
            "{error}"
        );
        assert!(provisioner.require_active_allocation(&alloc).is_ok());
        assert!(std::fs::read_to_string(&state_path)
            .unwrap()
            .contains(&vm_id.to_string()));
        assert!(
            commands.contains("nft delete rule ip taritd_nat post handle 1")
                && commands.contains("nft delete rule ip taritd_nat vm_egress handle 2")
                && commands.contains("nft delete rule ip taritd_nat vm_input handle 3"),
            "exact tagged policy was not cleaned first:\n{commands}"
        );
        assert!(
            !commands.contains("handle 55"),
            "unknown tagged policy was deleted automatically:\n{commands}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn provisioning_retains_old_policy_when_existing_tap_cannot_be_deleted() {
        let _environment_guard = RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-provision-existing-tap-failure-test-{}-{sequence}",
                std::process::id()
            ));
        let bin = root.join("bin");
        let log = root.join("commands.log");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();
        let command = r#"#!/bin/sh
printf '%s %s\n' "${0##*/}" "$*" >> "$TARIT_TEST_COMMAND_LOG"
case "${0##*/}:$*" in
  "ip:-j link show") echo '[{"ifname":"insta7"}]' ;;
  "ip:link set insta7 down") ;;
  "ip:link del insta7") echo "simulated link-delete failure" >&2; exit 1 ;;
esac
"#;
        for name in ["ip", "nft"] {
            let path = bin.join(name);
            std::fs::write(&path, command).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = std::fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&path, permissions).unwrap();
            }
        }
        let provisioner = NetProvisioner {
            inner: Mutex::new(SlotAllocator::empty()),
            network_transactions: NetworkTransactionLock::default(),
            state_path: root.join("state.json"),
            uplink: "eth0".into(),
            fail_closed: AtomicBool::new(false),
        };
        let alloc = NetAlloc::for_slot(Uuid::new_v4(), 7).unwrap();
        let old_path = std::env::var_os("PATH");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                old_path.as_deref().unwrap_or_default().to_string_lossy()
            ),
        );
        std::env::set_var("TARIT_TEST_COMMAND_LOG", &log);
        let error = provisioner
            .prepare_slot_for_provision(&alloc)
            .expect_err("a surviving strict TAP must abort provisioning");
        if let Some(path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        std::env::remove_var("TARIT_TEST_COMMAND_LOG");

        let commands = std::fs::read_to_string(&log).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(
            error.to_string().contains("pre-existing strict TAP"),
            "{error}"
        );
        assert!(
            !commands.contains("nft "),
            "old slot policy was touched before TAP deletion was confirmed:\n{commands}"
        );
    }

    #[test]
    fn network_transaction_serializes_update_against_release_and_slot_reuse() {
        use std::sync::mpsc;

        let lock = std::sync::Arc::new(NetworkTransactionLock::default());
        let allocator = std::sync::Arc::new(Mutex::new(SlotAllocator::empty()));
        let vm_id = Uuid::new_v4();
        let released = allocator.lock().unwrap().allocate(vm_id).unwrap();
        let update_entered = std::sync::Arc::new(std::sync::Barrier::new(2));
        let release_attempted = std::sync::Arc::new(std::sync::Barrier::new(2));
        let (update_done_tx, update_done_rx) = mpsc::channel();
        let (reuse_tx, reuse_rx) = mpsc::channel();

        let update_lock = std::sync::Arc::clone(&lock);
        let update_entered_worker = std::sync::Arc::clone(&update_entered);
        let update = std::thread::spawn(move || {
            update_lock
                .run(|| {
                    update_entered_worker.wait();
                    update_done_rx.recv().unwrap();
                })
                .unwrap();
        });

        let release_lock = std::sync::Arc::clone(&lock);
        let release_allocator = std::sync::Arc::clone(&allocator);
        let release_attempted_worker = std::sync::Arc::clone(&release_attempted);
        let released_for_worker = released.clone();
        let release = std::thread::spawn(move || {
            release_attempted_worker.wait();
            release_lock
                .run(|| {
                    let mut allocator = release_allocator.lock().unwrap();
                    allocator.free(&released_for_worker);
                    reuse_tx
                        .send(allocator.allocate(Uuid::new_v4()).unwrap().idx)
                        .unwrap();
                })
                .unwrap();
        });

        update_entered.wait();
        release_attempted.wait();
        assert!(
            reuse_rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "release/reuse ran while the update transaction held the network lock"
        );
        update_done_tx.send(()).unwrap();
        assert_eq!(reuse_rx.recv().unwrap(), released.idx);
        update.join().unwrap();
        release.join().unwrap();
    }

    #[test]
    fn ambiguous_directory_sync_retains_slot_and_fails_closed_provisioning() {
        let _environment_guard = RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-state-directory-sync-failure-test-{}-{sequence}",
                std::process::id()
            ));
        let state_path = root.join("state.json");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let vm_id = Uuid::new_v4();
        let mut allocator = SlotAllocator::empty();
        let alloc = allocator.allocate(vm_id).unwrap();
        persist_allocator(&state_path, &allocator).unwrap();
        let provisioner = NetProvisioner {
            inner: Mutex::new(allocator),
            network_transactions: NetworkTransactionLock::default(),
            state_path: state_path.clone(),
            uplink: "eth0".into(),
            fail_closed: AtomicBool::new(false),
        };

        FAIL_NEXT_STATE_DIRECTORY_SYNC.store(true, Ordering::SeqCst);
        let error = provisioner
            .free_allocation_locked(&alloc)
            .expect_err("directory-sync ambiguity must retain the allocation");

        assert!(
            error.to_string().contains("fail-closed"),
            "unexpected persistence error: {error}"
        );
        assert!(provisioner.require_active_allocation(&alloc).is_ok());
        assert!(
            !std::fs::read_to_string(&state_path)
                .unwrap()
                .contains(&vm_id.to_string()),
            "the rename completed before the injected directory-sync failure"
        );
        assert!(matches!(
            provisioner.provision(Uuid::new_v4()),
            Err(OrchError::Internal(message)) if message.contains("fail-closed")
        ));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stale_fixed_new_state_file_does_not_block_unique_persistence() {
        let sequence = RECOVERY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target")
            .join(format!(
                "net-state-stale-temp-test-{}-{sequence}",
                std::process::id()
            ));
        let state_path = root.join("state.json");
        let stale = state_write_path(&state_path);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&stale, "unrelated stale writer").unwrap();

        persist_entries(&state_path, Vec::new()).unwrap();

        assert!(
            stale.exists(),
            "persistence must not select a fixed .new path"
        );
        assert!(std::fs::read_to_string(&state_path)
            .unwrap()
            .contains("\"version\": 2"));
        assert!(
            std::fs::read_dir(&root).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")),
            "failed persistence cleanup left a generated temporary file"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
