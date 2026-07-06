//! vmm-net: per-VM networking with host-enforced egress (PRD §8).
//!
//! Topology: each microVM gets a virtio-net device backed by a host tap
//! interface inside a dedicated network namespace. Egress is enforced on the
//! host side of the tap — code the guest can never touch.
//!
//!   guest eth0 (virtio-net) ── tap0 ── [ per-VM network namespace ]
//!                                         │  veth pair / TC redirect
//!                                         ▼
//!                                host routing + NAT (SNAT)
//!                                         │
//!                          ┌──────────────▼──────────────┐
//!                          │ EGRESS ENFORCEMENT (host)    │
//!                          │  nftables allowlist           │
//!                          │  + eBPF/XDP fast path         │
//!                          │  + DNS-aware policy           │
//!                          │  + token-bucket rate limit   │
//!                          └───────────────────────────────┘

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod dns;
pub mod egress;
pub mod live_egress;
pub mod netns;
pub mod nft_compiler;
pub mod port_forward;
pub mod rate_limit;
pub mod tap;

pub use dns::{expand, DnsAwarePolicy, DnsResolver, DomainRule, MapResolver};
pub use egress::{EgressPolicy, EgressRule};
pub use live_egress::{
    compile_egress_update, diff_policies, EgressUpdate, EgressUpdateResult, PolicyDiff,
};
pub use netns::{NetNs, NetNsError};
pub use nft_compiler::{
    compile_table, compile_to_nft, try_compile_table, try_compile_to_nft, NftCompileError,
};
pub use port_forward::{
    compile_port_forward, compile_port_forward_table, try_compile_port_forward,
    try_compile_port_forward_table, PortForward, PortForwardError,
};
pub use rate_limit::TokenBucket;
pub use tap::{Tap, TapError};
