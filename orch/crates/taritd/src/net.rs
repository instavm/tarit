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
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
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
const NET_STATE_VERSION: u32 = 1;
const STALE_TAP_MIN_AGE: Duration = Duration::from_secs(30);

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
    state_path: PathBuf,
    uplink: String,
}

impl NetProvisioner {
    /// Detect the default-route interface, recover persisted slot ownership,
    /// ensure the shared nft table exists, and sweep stale taritd-owned artifacts.
    pub fn new(
        state_path: PathBuf,
        live_vm_ids: impl IntoIterator<Item = Uuid>,
    ) -> Result<Self, OrchError> {
        let live_vm_ids = live_vm_ids.into_iter().collect::<HashSet<_>>();
        let uplink = default_uplink()?;
        ensure_host_networking()?;

        let entries = load_state(&state_path)?;
        let (allocator, dropped) = SlotAllocator::from_entries(entries, &live_vm_ids);
        if dropped > 0 {
            tracing::warn!(
                dropped,
                "net: pruned stale allocation records during recovery"
            );
        }
        persist_allocator(&state_path, &allocator)?;

        let provisioner = Self {
            inner: Mutex::new(allocator),
            state_path,
            uplink,
        };
        let report = provisioner.sweep_orphans()?;
        if report.has_work() {
            tracing::info!(
                taps_removed = report.taps_removed,
                nft_rules_removed = report.nft_rules_removed,
                ingress_tables_removed = report.ingress_tables_removed,
                "net: startup stale sweep completed"
            );
        }
        Ok(provisioner)
    }

    pub fn uplink(&self) -> &str {
        &self.uplink
    }

    /// Create a tap for a new VM: allocate a reusable slot, persist ownership,
    /// create `ip tuntap`, configure the host /30, and add an nft NAT rule.
    pub fn provision(&self, vm_id: Uuid) -> Result<NetAlloc, OrchError> {
        let alloc = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| OrchError::Internal("net allocator lock poisoned".into()))?;
            let alloc = inner.allocate(vm_id)?;
            persist_allocator(&self.state_path, &inner)?;
            alloc
        };

        if let Err(e) = self.provision_host(&alloc) {
            self.best_effort_delete(&alloc);
            self.free_allocation(&alloc);
            return Err(e);
        }

        Ok(alloc)
    }

    /// Remove a VM's tap and nft rule(s), then free and persist the slot.
    /// Idempotent and best-effort: every step is attempted and failures are logged.
    pub fn teardown(&self, alloc: &NetAlloc) {
        self.best_effort_delete(alloc);
        self.free_allocation(alloc);
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
        // Build every rule before touching nft, so a bad rule cannot leave a
        // half-applied policy (default-open) on the host.
        let rules = egress_policy_argv(alloc, allowlist, allow_existing)?;
        self.delete_egress_rules_for_alloc(alloc)?;
        for argv in rules {
            run_argv(&argv)?;
        }
        Ok(allowlist.len())
    }

    fn delete_egress_rules_for_alloc(&self, alloc: &NetAlloc) -> Result<usize, OrchError> {
        let tag = egress_comment(alloc);
        delete_nft_rules_in_chain(NFT_FWD_CHAIN, |line| line.contains(&tag))
    }

    /// Teardown by VM id from recovered persistent state. This covers restart
    /// cases where the supervisor no longer has a RunningVm/NetAlloc in memory.
    pub fn teardown_vm_id(&self, vm_id: Uuid) {
        let alloc = match self.inner.lock() {
            Ok(inner) => inner
                .by_vm
                .get(&vm_id)
                .copied()
                .and_then(|slot| NetAlloc::for_slot(vm_id, slot).ok()),
            Err(_) => {
                tracing::warn!(%vm_id, "net allocator lock poisoned while looking up VM teardown");
                None
            }
        };
        if let Some(alloc) = alloc {
            self.teardown(&alloc);
        }
    }

    fn provision_host(&self, alloc: &NetAlloc) -> Result<(), OrchError> {
        let tap = tap_name(alloc.idx);
        let _ = run("ip", &["link", "del", &tap]);
        self.delete_nft_rules_for_slot(alloc.idx)?;
        for argv in tap_provision_argv(alloc) {
            run_argv(&argv)?;
        }
        self.add_nft_rule(alloc)
    }

    fn add_nft_rule(&self, alloc: &NetAlloc) -> Result<(), OrchError> {
        run_argv(&masquerade_nft_argv(alloc, &self.uplink))
    }

    fn best_effort_delete(&self, alloc: &NetAlloc) {
        let tap = tap_name(alloc.idx);
        if let Err(e) = run("ip", &["link", "del", &tap]) {
            tracing::warn!(tap = %alloc.tap, vm_id = %alloc.vm_id, slot = alloc.idx, "net: tap delete skipped/failed: {e}");
        }
        match self.delete_nft_rules_for_alloc(alloc) {
            Ok(deleted) if deleted > 0 => tracing::debug!(
                tap = %alloc.tap,
                vm_id = %alloc.vm_id,
                slot = alloc.idx,
                deleted,
                "net: deleted per-VM nft rule(s)"
            ),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(tap = %alloc.tap, vm_id = %alloc.vm_id, slot = alloc.idx, "net: nft cleanup failed: {e}")
            }
        }
    }

    fn free_allocation(&self, alloc: &NetAlloc) {
        match self.inner.lock() {
            Ok(mut inner) => {
                inner.free(alloc);
                if let Err(e) = persist_allocator(&self.state_path, &inner) {
                    tracing::warn!(vm_id = %alloc.vm_id, slot = alloc.idx, "net: failed to persist freed slot: {e}");
                }
            }
            Err(_) => {
                tracing::warn!(vm_id = %alloc.vm_id, slot = alloc.idx, "net allocator lock poisoned while freeing slot")
            }
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
                match self.delete_nft_rules_for_slot(slot) {
                    Ok(n) => report.nft_rules_removed += n,
                    Err(e) => {
                        tracing::warn!(tap = %tap.name, slot, "net: failed to delete stale tap nft rule(s): {e}")
                    }
                }
            }
            match run("ip", &["link", "del", &tap.name]) {
                Ok(()) => report.taps_removed += 1,
                Err(e) => tracing::warn!(tap = %tap.name, "net: failed to delete stale tap: {e}"),
            }
        }

        report.nft_rules_removed += self.delete_orphan_nft_rules(&active)?;
        report.ingress_tables_removed += self.delete_orphan_ingress_tables(&active)?;
        Ok(report)
    }

    fn delete_nft_rules_for_alloc(&self, alloc: &NetAlloc) -> Result<usize, OrchError> {
        let mut removed = 0;
        for chain in [NFT_CHAIN, NFT_FWD_CHAIN, NFT_INPUT_CHAIN] {
            removed +=
                delete_nft_rules_in_chain(chain, |line| is_taritd_nft_rule_for_alloc(line, alloc))?;
        }
        removed += delete_ingress_table_for_slot(alloc.idx)?;
        Ok(removed)
    }

    fn delete_nft_rules_for_slot(&self, slot: u32) -> Result<usize, OrchError> {
        let mut removed = 0;
        for chain in [NFT_CHAIN, NFT_FWD_CHAIN, NFT_INPUT_CHAIN] {
            removed +=
                delete_nft_rules_in_chain(chain, |line| is_taritd_nft_rule_for_slot(line, slot))?;
        }
        removed += delete_ingress_table_for_slot(slot)?;
        Ok(removed)
    }

    fn delete_orphan_nft_rules(&self, active: &BTreeMap<u32, Uuid>) -> Result<usize, OrchError> {
        let mut removed = 0;
        for chain in [NFT_CHAIN, NFT_FWD_CHAIN, NFT_INPUT_CHAIN] {
            removed +=
                delete_nft_rules_in_chain(chain, |line| is_orphan_taritd_nft_rule(line, active))?;
        }
        Ok(removed)
    }

    fn delete_orphan_ingress_tables(
        &self,
        active: &BTreeMap<u32, Uuid>,
    ) -> Result<usize, OrchError> {
        let active_slots = active.keys().copied().collect::<HashSet<_>>();
        let tables = ingress_table_names()?;
        let stale = stale_ingress_tables_to_sweep(&tables, &active_slots);
        for table in &stale {
            let slot = ingress_slot_from_table_name(table)
                .expect("stale ingress table names are parsed from the fixed prefix");
            run_argv(&delete_ingress_table_argv(slot))?;
        }
        Ok(stale.len())
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

#[derive(Debug)]
struct SlotAllocator {
    free: BTreeSet<u32>,
    by_slot: BTreeMap<u32, Uuid>,
    by_vm: HashMap<Uuid, u32>,
}

impl SlotAllocator {
    fn empty() -> Self {
        Self {
            free: (0..NET_POOL_SLOTS).collect(),
            by_slot: BTreeMap::new(),
            by_vm: HashMap::new(),
        }
    }

    fn from_entries(entries: Vec<NetStateEntry>, live_vm_ids: &HashSet<Uuid>) -> (Self, usize) {
        let mut allocator = Self::empty();
        let mut dropped = 0;
        for entry in entries {
            if !live_vm_ids.contains(&entry.vm_id)
                || entry.slot >= NET_POOL_SLOTS
                || entry.tap != tap_name(entry.slot)
                || allocator.by_slot.contains_key(&entry.slot)
                || allocator.by_vm.contains_key(&entry.vm_id)
            {
                dropped += 1;
                continue;
            }
            allocator.free.remove(&entry.slot);
            allocator.by_slot.insert(entry.slot, entry.vm_id);
            allocator.by_vm.insert(entry.vm_id, entry.slot);
        }
        (allocator, dropped)
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
        NetAlloc::for_slot(vm_id, slot)
    }

    fn free(&mut self, alloc: &NetAlloc) {
        match self.by_vm.remove(&alloc.vm_id) {
            Some(slot) => {
                self.by_slot.remove(&slot);
                self.free.insert(slot);
            }
            None => match self.by_slot.get(&alloc.idx).copied() {
                Some(owner) if owner == alloc.vm_id => {
                    self.by_slot.remove(&alloc.idx);
                    self.free.insert(alloc.idx);
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
}

fn load_state(path: &Path) -> Result<Vec<NetStateEntry>, OrchError> {
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
    let state = serde_json::from_str::<NetStateFile>(&text)
        .map_err(|e| OrchError::Internal(format!("parse net state {}: {e}", path.display())))?;
    if state.version != NET_STATE_VERSION {
        return Err(OrchError::Internal(format!(
            "unsupported net state version {} in {}",
            state.version,
            path.display()
        )));
    }
    Ok(state.allocations)
}

fn persist_allocator(path: &Path, allocator: &SlotAllocator) -> Result<(), OrchError> {
    persist_entries(path, allocator.entries())
}

fn persist_entries(path: &Path, allocations: Vec<NetStateEntry>) -> Result<(), OrchError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            OrchError::Internal(format!("create net state dir {}: {e}", parent.display()))
        })?;
    }
    let state = NetStateFile {
        version: NET_STATE_VERSION,
        allocations,
    };
    let text = serde_json::to_string_pretty(&state)
        .map_err(|e| OrchError::Internal(format!("encode net state: {e}")))?;
    let tmp = state_write_path(path);
    std::fs::write(&tmp, text)
        .map_err(|e| OrchError::Internal(format!("write net state {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        OrchError::Internal(format!(
            "replace net state {} with {}: {e}",
            path.display(),
            tmp.display()
        ))
    })?;
    Ok(())
}

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
    match delete_nft_rules_matching(is_legacy_masquerade_rule) {
        Ok(deleted) if deleted > 0 => {
            tracing::info!(deleted, "net: removed legacy broad masquerade rule(s)")
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("net: failed to remove legacy broad masquerade rule(s): {e}"),
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
            "{ type nat hook postrouting priority 100 ; }",
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

fn tap_provision_argv(alloc: &NetAlloc) -> Vec<Vec<String>> {
    let tap = tap_name(alloc.idx);
    let ingress_table = ingress_table_name(alloc.idx);
    let ingress_comment = nft_quote(&ingress_comment(alloc));
    let guard_comment = nft_quote(&guard_comment(alloc));
    let input_comment = nft_quote(&input_comment(alloc));
    let interface = nft_quote(&tap);
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
            "ip".into(),
            "drop".into(),
            "comment".into(),
            input_comment,
        ],
        command_argv(&["ip", "link", "set", &tap, "up"]),
    ]);
    argv
}

fn tap_sysctl_argv(tap: &str) -> Vec<Vec<String>> {
    [
        format!("net.ipv6.conf.{tap}.disable_ipv6=1"),
        format!("net.ipv6.conf.{tap}.forwarding=0"),
        format!("net.ipv6.conf.{tap}.accept_ra=0"),
        format!("net.ipv6.conf.{tap}.autoconf=0"),
        format!("net.ipv6.conf.{tap}.accept_redirects=0"),
        format!("net.ipv4.conf.{tap}.rp_filter=1"),
    ]
    .into_iter()
    .map(|setting| vec!["sysctl".into(), "-qw".into(), setting])
    .collect()
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
        .filter(|table| ingress_slot_from_table_name(table).is_some())
        .collect())
}

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

fn delete_ingress_table_for_slot(slot: u32) -> Result<usize, OrchError> {
    let table = ingress_table_name(slot);
    if !ingress_table_names()?.iter().any(|name| name == &table) {
        return Ok(0);
    }
    run_argv(&delete_ingress_table_argv(slot))?;
    Ok(1)
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

fn delete_nft_rules_matching(predicate: impl FnMut(&str) -> bool) -> Result<usize, OrchError> {
    delete_nft_rules_in_chain(NFT_CHAIN, predicate)
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
/// Port zero is rejected.
fn parse_egress_entry(entry: &str) -> Result<(String, u16, Option<&'static str>), OrchError> {
    if entry.is_empty() {
        return Err(OrchError::BadRequest("empty egress rule".into()));
    }
    if matches!(entry.parse::<IpAddr>(), Ok(IpAddr::V6(_)))
        || matches!(entry.parse::<IpNet>(), Ok(IpNet::V6(_)))
    {
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
        Ok(IpAddr::V4(addr)) => Ipv4Net::new(addr, 32)
            .map(|cidr| cidr.trunc())
            .map_err(|_| {
                OrchError::BadRequest(format!(
                    "bad egress rule {entry:?}: invalid IPv4 CIDR {cidr:?}"
                ))
            }),
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

fn nft_handle(line: &str) -> Option<String> {
    line.split("# handle ")
        .nth(1)?
        .split_whitespace()
        .next()
        .map(ToOwned::to_owned)
}

fn is_legacy_masquerade_rule(line: &str) -> bool {
    line.contains("ip saddr 172.16.0.0/16")
        && line.contains("masquerade")
        && !line.contains("taritd slot=")
}

fn is_taritd_nft_rule_for_alloc(line: &str, alloc: &NetAlloc) -> bool {
    is_taritd_nft_rule(line)
        && (is_taritd_nft_rule_for_slot(line, alloc.idx)
            || line.contains(&format!("vm={}", alloc.vm_id))
            || line.contains(&format!("tap={}", tap_name(alloc.idx))))
}

fn is_taritd_nft_rule_for_slot(line: &str, slot: u32) -> bool {
    is_taritd_nft_rule(line)
        && parse_nft_comment_value(line, "slot=").and_then(|s| s.parse::<u32>().ok()) == Some(slot)
}

fn is_orphan_taritd_nft_rule(line: &str, active: &BTreeMap<u32, Uuid>) -> bool {
    if !is_taritd_nft_rule(line) {
        return false;
    }
    let Some(slot) = parse_nft_comment_value(line, "slot=").and_then(|s| s.parse::<u32>().ok())
    else {
        return true;
    };
    let Some(vm_id) = parse_nft_comment_value(line, "vm=").and_then(|s| Uuid::parse_str(&s).ok())
    else {
        return !active.contains_key(&slot);
    };
    active.get(&slot).copied() != Some(vm_id)
}

fn is_taritd_nft_rule(line: &str) -> bool {
    [
        "taritd slot=",
        "taritd-egress slot=",
        "taritd-guard slot=",
        "taritd-input slot=",
    ]
    .iter()
    .any(|prefix| line.contains(prefix))
}

fn parse_nft_comment_value(line: &str, key: &str) -> Option<String> {
    let rest = line.split(key).nth(1)?;
    let value = rest
        .split(|c: char| c.is_whitespace() || c == '"')
        .next()?
        .trim_end_matches(',');
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    "{ type nat hook postrouting priority 100 ; }",
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
    fn tap_provision_plan_hardens_before_link_is_up() {
        let alloc = NetAlloc::for_idx(0);
        let plan = tap_provision_argv(&alloc);
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
                    "ip",
                    "drop",
                    "comment",
                    "\"taritd-input slot=0 vm=00000000-0000-0000-0000-000000000000 tap=insta0\"",
                ]),
                argv(&["ip", "link", "set", "insta0", "up"]),
            ]
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
        assert!(matches!(
            parse_egress_entry("2001:db8::/32").unwrap_err(),
            OrchError::BadRequest(message) if message.contains("IPv6")
        ));
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
    fn allocator_recovers_only_live_valid_entries() {
        let live_vm = Uuid::new_v4();
        let stale_vm = Uuid::new_v4();
        let entries = vec![
            NetStateEntry {
                slot: 7,
                vm_id: live_vm,
                tap: "insta7".into(),
            },
            NetStateEntry {
                slot: 8,
                vm_id: stale_vm,
                tap: "insta8".into(),
            },
            NetStateEntry {
                slot: NET_POOL_SLOTS,
                vm_id: Uuid::new_v4(),
                tap: format!("insta{NET_POOL_SLOTS}"),
            },
        ];
        let (mut allocator, dropped) =
            SlotAllocator::from_entries(entries, &HashSet::from([live_vm]));

        assert_eq!(dropped, 2);
        assert_eq!(allocator.by_vm.get(&live_vm), Some(&7));
        let alloc = allocator.allocate(Uuid::new_v4()).unwrap();
        assert_eq!(alloc.idx, 0);
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
            "iifname \"insta1\" ip drop comment \"taritd-input slot=1 vm={old_vm} tap=insta1\" # handle 7"
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
}
