//! The cross-family consolidation-guard policy (07 §3, M6.T01).
//!
//! Split out of `lib.rs` (which sits against the file-size cap). The engine owns
//! this policy because 01-architecture assigns the subliminal guard to the L3
//! facade as cross-cutting policy: the facade constructs the configured
//! [`CrossFamilyGuard`](aionforge_security::CrossFamilyGuard) from this struct plus
//! the per-call summarizer/evolver identity and injects it into the L2 drivers —
//! the mode is set once at construction, never per call, so user code cannot drop
//! it (07 §3: "enforced at the substrate, not left to user code").
//!
//! The host maps `aionforge-config`'s `ConsolidationGuardConfig` into this
//! field-for-field — the engine takes no config dependency, the same indirection
//! as every policy sibling — and the engine re-validates its own copy.

pub use aionforge_security::GuardMode;

/// Cross-family guard posture (07 §3, M6.T01): what a fired guard does, and the
/// declared consolidating family the startup single-family check compares against.
///
/// There is deliberately **no off-switch**: the guard is substrate policy over
/// every inference-calling consolidation rule, and it is inert until a host
/// injects an LLM-backed summarizer or link evolver (both off by default), so the
/// all-defaults posture pays nothing.
#[derive(Debug, Clone, Default)]
pub struct ConsolidationGuardPolicy {
    /// What a fired guard does: [`GuardMode::Refuse`] (default) skips the item and
    /// audits; [`GuardMode::Warn`] proceeds and audits the same finding.
    pub mode: GuardMode,
    /// The model family the deployment consolidates with, populated by the host
    /// from its completer configuration when distillation or LLM link evolution is
    /// in use. Feeds the startup single-family warning; the per-call guard reads
    /// the injected identity and works whether or not this is set.
    pub declared_consolidator_family: Option<String>,
}

impl ConsolidationGuardPolicy {
    /// Validate the policy: a declared family, when set, must be non-empty — an
    /// unverifiable declaration is worse than none (07 §3). Mirrors the config
    /// crate's check; the engine re-validates fail-closed however the host
    /// populated it.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if let Some(family) = &self.declared_consolidator_family
            && family.trim().is_empty()
        {
            return Err(
                "consolidation guard: declared_consolidator_family, when set, must be \
                 non-empty; omit it to skip the startup check"
                    .to_string(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_posture_is_refuse_and_validates() {
        let policy = ConsolidationGuardPolicy::default();
        assert_eq!(policy.mode, GuardMode::Refuse);
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn an_empty_declared_family_is_rejected() {
        let policy = ConsolidationGuardPolicy {
            declared_consolidator_family: Some("  ".to_string()),
            ..ConsolidationGuardPolicy::default()
        };
        assert!(policy.validate().is_err());

        let policy = ConsolidationGuardPolicy {
            declared_consolidator_family: Some("claude-sonnet-4-6".to_string()),
            ..ConsolidationGuardPolicy::default()
        };
        assert!(policy.validate().is_ok());
    }
}
