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

use aionforge_domain::blocks::Identity;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_security::{FamilyVerdict, family_verdict};
use aionforge_store::Store;

use crate::EngineError;

pub use aionforge_security::GuardMode;

/// Cross-family guard posture (07 §3, M6.T01): what a fired guard does, and the
/// declared consolidating family the startup single-family check compares against.
///
/// There is deliberately **no off-switch**: the guard is substrate policy over
/// every inference-calling consolidation rule, and it is inert until a host
/// injects an inference-backed link evolver (off by default; the shipped path is
/// deterministic), so the all-defaults posture pays nothing.
#[derive(Debug, Clone, Default)]
pub struct ConsolidationGuardPolicy {
    /// What a fired guard does: [`GuardMode::Refuse`] (default) skips the item and
    /// audits; [`GuardMode::Warn`] proceeds and audits the same finding.
    pub mode: GuardMode,
    /// The model family the deployment consolidates with, populated by the host
    /// when an inference-backed link evolver is in use (its declared family). Feeds
    /// the startup single-family warning; the per-call guard reads the injected
    /// evolver's identity and works whether or not this is set.
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

/// A condition the constructor surfaces for the host to log — the engine has no
/// logging dependency, so emission is the host's job via
/// [`Memory::startup_warnings`](crate::Memory::startup_warnings). Each warning is
/// also written as an audit row at construction (the constructor receives `now`,
/// so no ambient clock is involved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupWarning {
    /// Every enrolled agent declares the consolidating model's family (07 §3): the
    /// deployment writes and condenses with one base model, the exact posture the
    /// subliminal-trait guard exists to flag. The per-call guard will refuse (or
    /// warn through, per mode) every note an inference evolver of that family would relate.
    SingleFamilyDeployment {
        /// The declared consolidating family the agents all match.
        family: String,
    },
}

/// The single-family startup check (07 §3, M6.T01 design Q7): when a consolidating
/// family is declared and **every** enrolled agent's family compares `Same` against
/// it, the deployment is single-family — surface the typed warning and write the
/// audit row. Best-effort by design: no declared family (or no enrolled agents yet)
/// skips the check, and the per-call guard remains the enforcement either way.
pub(crate) fn single_family_check(
    store: &Store,
    declared: Option<&str>,
    now: &Timestamp,
) -> Result<Vec<StartupWarning>, EngineError> {
    let Some(declared) = declared else {
        return Ok(Vec::new());
    };
    let families = store.distinct_agent_families()?;
    let single_family = !families.is_empty()
        && families
            .iter()
            .all(|family| family_verdict(family, declared) == FamilyVerdict::Same);
    if !single_family {
        return Ok(Vec::new());
    }
    store.commit_audit(&startup_guard_audit(declared, &families, now))?;
    Ok(vec![StartupWarning::SingleFamilyDeployment {
        family: declared.to_string(),
    }])
}

/// The deterministic actor (and subject) the startup check writes under: the guard
/// itself, not any one agent — a deployment-level finding has no per-node subject.
fn guard_actor() -> Id {
    Id::from_content_hash(b"aionforge/cross-family-guard-v1")
}

/// The `subliminal_guard_warning` audit row for a single-family deployment. Its id
/// is content-addressed over the declared family and the agent-family set — never
/// the instant — so every restart of the same deployment dedups to one row, and a
/// changed fleet records a new finding.
fn startup_guard_audit(declared: &str, families: &[String], now: &Timestamp) -> AuditEvent {
    let key = format!(
        "audit|subliminal_guard|startup|{declared}|{}",
        families.join(",")
    );
    let actor = guard_actor();
    AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(key.as_bytes()),
            ingested_at: now.clone(),
            namespace: Namespace::Global,
            expired_at: None,
        },
        kind: AuditKind::SubliminalGuardWarning,
        subject_id: actor,
        actor_id: actor,
        payload: serde_json::json!({
            "action": "startup",
            "rule": "startup",
            "reason": "single_family_deployment",
            "consolidator_family": declared,
            "writer_families": families,
        }),
        signature: String::new(),
        occurred_at: now.clone(),
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
