//! x86_64 CPU templates — precomputed CPUID + MSR masks.
//!
//! Precomputed templates avoid per-boot feature negotiation. Masking to a
//! common fleet baseline keeps migration safe: incompatible targets are
//! rejected at negotiation.
//!
//! A CPU template is a named set of CPUID leaves + MSR bits that the VMM
//! presents to the guest, masking out features the host has but the fleet
//! baseline doesn't guarantee. This makes migration safe (the guest never
//! sees features that vary across hosts) and boot fast (no negotiation).

use serde::{Deserialize, Serialize};

/// A CPUID leaf (eax in, eax/ebx/ecx/edx out).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CpuidEntry {
    pub leaf: u32,
    pub subleaf: u32,
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

/// A named CPU template (e.g. "T2", "T2S", "T2CL").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuTemplate {
    pub name: String,
    /// CPUID register masks (leaf, subleaf, eax/ebx/ecx/edx are ANDed).
    pub cpuid: Vec<CpuidEntry>,
    /// MSR bits to clear (features to hide from the guest).
    pub msr_clear: Vec<(u32, u64)>,
}

impl CpuTemplate {
    /// The "bare" template — no masking, expose whatever the host has. The
    /// minimal default; real fleets use a masked template for migration
    /// safety.
    pub fn bare() -> Self {
        Self {
            name: "bare".into(),
            cpuid: vec![],
            msr_clear: vec![],
        }
    }

    /// A simple "masked" template that hides a few commonly-varying features
    /// (production CPU templates mask dozens; this is the scaffold).
    pub fn masked_basic() -> Self {
        Self {
            name: "masked_basic".into(),
            cpuid: vec![CpuidEntry {
                leaf: 7,
                subleaf: 0,
                eax: 0,
                ebx: 0,
                ecx: 0,
                edx: 0,
            }],
            msr_clear: vec![],
        }
    }
}

/// The list of fleet-baseline templates the VMM supports.
pub fn templates() -> Vec<CpuTemplate> {
    vec![CpuTemplate::bare(), CpuTemplate::masked_basic()]
}

/// Look up a template by name (used by the migration negotiation, §9d).
pub fn by_name(name: &str) -> Option<CpuTemplate> {
    templates().into_iter().find(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_has_no_masking() {
        let b = CpuTemplate::bare();
        assert!(b.cpuid.is_empty());
        assert!(b.msr_clear.is_empty());
        assert_eq!(b.name, "bare");
    }

    #[test]
    fn masked_basic_clears_leaf7_extended_features() {
        let m = CpuTemplate::masked_basic();
        let e = m
            .cpuid
            .iter()
            .find(|e| e.leaf == 7 && e.subleaf == 0)
            .expect("leaf 7 entry");
        assert_eq!(e.ecx, 0);
        assert_eq!(e.edx, 0);
    }

    #[test]
    fn by_name_finds_known_templates() {
        assert!(by_name("bare").is_some());
        assert!(by_name("masked_basic").is_some());
        assert!(by_name("nonexistent").is_none());
    }

    #[test]
    fn template_serializes_round_trip() {
        // A template must be serde-serializable so it can be sent over the
        // migration negotiation channel.
        let t = CpuTemplate::masked_basic();
        let s = serde_json::to_string(&t).unwrap();
        let back: CpuTemplate = serde_json::from_str(&s).unwrap();
        assert_eq!(t.name, back.name);
        assert_eq!(t.cpuid.len(), back.cpuid.len());
    }
}
