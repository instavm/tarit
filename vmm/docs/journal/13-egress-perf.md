# 13 — Phase 6: DNS-aware egress + warm pools (M14)

*Goal: production egress + the full performance budget (PRD Phase 6, §8).*

## What I did

### `vmm-net/src/dns.rs` — DNS-aware egress expansion (PRD §8)

`DnsAwarePolicy { cidr_rules, domain_rules }` + `expand(policy, resolver) -> EgressPolicy`:
- Resolves each `DomainRule`'s domain to IPs via a `DnsResolver` trait,
  emitting one `EgressRule` per resolved IP (`/32`).
- CIDR rules pass through unchanged.
- Unresolvable domains are silently dropped (the rule effectively doesn't
  apply — the destination is unreachable).
- `MapResolver` — a HashMap-backed mock resolver so the expansion is unit-
  testable without real DNS.

Five tests: CIDR-only pass-through, domain resolves to IP, unresolvable
dropped, multiple-IP domain emits multiple rules, mixed CIDR + domain
combines.

The PRD §8 "Pair with a controlled resolver so the guest can't smuggle
traffic via arbitrary DNS" is the runtime contract — the guest's DNS
*must* go through this resolver (enforced at the netns level), so the IPs
it programs into nftables are the only ones the policy knows about.

## What worked

- **The `DnsResolver` trait** makes DNS an injection point — tests use
  `MapResolver`, production uses the real `getaddrinfo`. The expansion
  logic is identical in both; only the resolver differs.
- **Domain → `/32` rule** is the right granularity. A domain with multiple
  IPs (a CDN) expands to multiple rules, each allowing its `/32` + port.
  No over-permissive `/24` just because one IP was resolved.

## What went wrong

No failures — pure data-shape work on top of the M8 egress policy model.

## Next

`14-migration.md` — Phase 7: live migration state machine.
