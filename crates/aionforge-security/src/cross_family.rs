//! The cross-family consolidation guard: pure family comparison and the
//! refuse-or-warn decision (07 §3, M6.T01).
//!
//! Subliminal traits transmit through inference-backed consolidation when the
//! consolidating model shares a base-model family with the writers whose content it
//! relates or condenses (07 T3). The shipped consolidation path is deterministic and
//! runs no inference model, so the guard is inert by default; it stays as standing
//! substrate policy over any injected inference-backed link evolver. The guard is the
//! substrate's verification that the families differ — stateless, no store, no I/O:
//! the engine facade constructs it from policy and the declared consolidator identity,
//! the L2 driver calls [`CrossFamilyGuard::evaluate`] with the writer families the
//! store resolved, and this module only ever compares strings. That keeps it
//! unit-testable in isolation and reusable by the M6.T05 trait-transfer probe's
//! same-family control.
//!
//! "Family" arrives as free text on both sides — the writer's is host-asserted at
//! capture, the consolidator's is the injected evolver's configured model id — so the
//! comparison normalizes at comparison time (trim, ASCII-lowercase, hyphen-token
//! boundaries, a closed vendor-root table) and **fails closed on ambiguity**: a
//! boundary-prefix or shared-root relation always resolves to [`FamilyVerdict::Same`]
//! (the riskier path), and anything empty or unresolvable is
//! [`FamilyVerdict::Unverifiable`], never "differs" — 07 §3 rejects auto-routing
//! precisely because an unverifiable family breaks the guard.

use aionforge_store::WriterFamilySet;

/// The closed vendor-root table: a model id whose leading hyphen-token matches a
/// prefix resolves to that vendor's root, so two ids of one vendor with no prefix
/// relation between them (`gpt-5` vs `o3`) still compare as the same family.
///
/// Closed by design and amended under the same discipline as a closed enum (the
/// M6.T01 design synthesis, Q1): an unmapped vendor falls back to leading-token
/// equality, which is softer but still catches a shared lineage prefix.
///
/// The table must enumerate every *published alias token* of a multi-token vendor,
/// because an unknown alias of a known vendor fails OPEN to `Differ` (the M6.T01
/// review's confirmed bypass: `chatgpt-4o-latest` vs `gpt-4o` compared as
/// different families until `chatgpt` joined the table). Amendments are factual —
/// tokens a vendor actually ships — never speculative.
const VENDOR_ROOTS: &[(&str, &str)] = &[
    ("claude", "anthropic"),
    ("gpt", "openai"),
    ("chatgpt", "openai"),
    ("codex", "openai"),
    ("o1", "openai"),
    ("o3", "openai"),
    ("o4", "openai"),
    ("gemini", "google"),
    ("gemma", "google"),
    ("llama", "meta"),
    ("mistral", "mistral"),
    ("codestral", "mistral"),
    ("ministral", "mistral"),
    ("magistral", "mistral"),
    ("devstral", "mistral"),
    ("pixtral", "mistral"),
    ("qwen", "alibaba"),
    ("qwq", "alibaba"),
];

/// How two family strings compare (07 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FamilyVerdict {
    /// The families resolve to the same base-model lineage — the guarded case.
    Same,
    /// The families verifiably differ.
    Differ,
    /// One side is empty or unresolvable: nothing can vouch, so nothing may pass.
    Unverifiable,
}

/// Whether a fired guard refuses the consolidation or lets it proceed with a
/// warning (plan M6.T01: "refused (or warned, per config)").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GuardMode {
    /// Skip the offending item and audit the refusal. The default: a security
    /// guard defaults to its strong mode, and it costs nothing until a host
    /// consciously enables an inference-backed consolidation rule.
    #[default]
    Refuse,
    /// Proceed, but audit the same finding as a warning.
    Warn,
}

/// Why the guard fired, carried into the audit payload so the probe (and the
/// M6.T05 control) can assert the guard fired rather than the model declining.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardReason {
    /// A writer family resolved to the consolidator's own family.
    SameFamily {
        /// The raw writer family that matched.
        writer_family: String,
    },
    /// A source's writer family could not be resolved (or was empty).
    UnverifiableWriter,
    /// The consolidating identity declared an empty family for an inference call.
    UnverifiableConsolidator,
}

/// The guard's decision for one consolidation item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// The consolidating identity declares no model family: a deterministic rule
    /// implementation, not an inference call — outside the guard's scope (04 §41).
    NotInference,
    /// Every writer family verifiably differs from the consolidator's.
    Pass,
    /// The guard fired under [`GuardMode::Refuse`]: skip the item, write nothing.
    Refused(GuardReason),
    /// The guard fired under [`GuardMode::Warn`]: proceed, but audit the finding.
    Warned(GuardReason),
}

/// The configured guard: the policy mode plus the declared consolidating family,
/// constructed once per call at the engine facade and applied per item in the L2
/// driver (the 01-architecture split: L3 owns the policy, the driver is where the
/// writer families are in scope).
#[derive(Debug, Clone)]
pub struct CrossFamilyGuard {
    mode: GuardMode,
    consolidator_family: Option<String>,
}

impl CrossFamilyGuard {
    /// Build a guard for one consolidating identity. `consolidator_family` is the
    /// identity's declared `model_family` — `None` for a pure rule implementation,
    /// which the guard treats as not-inference.
    #[must_use]
    pub fn new(mode: GuardMode, consolidator_family: Option<String>) -> Self {
        Self {
            mode,
            consolidator_family,
        }
    }

    /// The configured policy mode.
    #[must_use]
    pub fn mode(&self) -> GuardMode {
        self.mode
    }

    /// The declared consolidating family, as configured.
    #[must_use]
    pub fn consolidator_family(&self) -> Option<&str> {
        self.consolidator_family.as_deref()
    }

    /// Decide one item: compare every resolved writer family against the
    /// consolidator's, any-match-fire (one same-or-unverifiable source poisons the
    /// item — an average would launder exactly the content the guard exists to
    /// keep away from a same-family condenser).
    #[must_use]
    pub fn evaluate(&self, writers: &WriterFamilySet) -> GuardDecision {
        let Some(consolidator) = self.consolidator_family.as_deref() else {
            return GuardDecision::NotInference;
        };
        if consolidator.trim().is_empty() {
            // An inference call with no declared family: unverifiable on the
            // consolidator side breaks both guard and lineage (07 §3).
            return self.fire(GuardReason::UnverifiableConsolidator);
        }
        if writers.unverifiable {
            return self.fire(GuardReason::UnverifiableWriter);
        }
        for writer in &writers.families {
            match family_verdict(writer, consolidator) {
                FamilyVerdict::Same => {
                    return self.fire(GuardReason::SameFamily {
                        writer_family: writer.clone(),
                    });
                }
                FamilyVerdict::Unverifiable => {
                    return self.fire(GuardReason::UnverifiableWriter);
                }
                FamilyVerdict::Differ => {}
            }
        }
        GuardDecision::Pass
    }

    fn fire(&self, reason: GuardReason) -> GuardDecision {
        match self.mode {
            GuardMode::Refuse => GuardDecision::Refused(reason),
            GuardMode::Warn => GuardDecision::Warned(reason),
        }
    }
}

/// Compare two family strings (07 §3; design Q1). Normalization happens here, at
/// comparison time only — stored provenance is never rewritten.
///
/// In order: trim + ASCII-lowercase (either side empty ⇒
/// [`FamilyVerdict::Unverifiable`]); hyphen-token boundary-prefix in either
/// direction (`claude` ⊑ `claude-sonnet-4-6` ⇒ `Same`, but `claude` vs `claudex`
/// is no boundary match); the closed vendor-root table (`gpt-5` and `o3` share
/// the openai root ⇒ `Same`); finally leading-token equality for unmapped
/// vendors. Raw-id equality alone would be the status-quo bypass — the writer
/// asserting `claude` while the consolidator declares `claude-sonnet-4-6` must
/// compare `Same`.
#[must_use]
pub fn family_verdict(writer: &str, consolidator: &str) -> FamilyVerdict {
    let writer = writer.trim().to_ascii_lowercase();
    let consolidator = consolidator.trim().to_ascii_lowercase();
    if writer.is_empty() || consolidator.is_empty() {
        return FamilyVerdict::Unverifiable;
    }
    if boundary_prefix(&writer, &consolidator) || boundary_prefix(&consolidator, &writer) {
        return FamilyVerdict::Same;
    }
    match (vendor_root(&writer), vendor_root(&consolidator)) {
        (Some(a), Some(b)) if a == b => FamilyVerdict::Same,
        (Some(a), Some(b)) if a != b => FamilyVerdict::Differ,
        // At most one side maps: fall back to the leading hyphen-token.
        _ if leading_token(&writer) == leading_token(&consolidator) => FamilyVerdict::Same,
        _ => FamilyVerdict::Differ,
    }
}

/// Whether `short` is a hyphen-token boundary-prefix of `long`: equal, or `long`
/// continues with `-` exactly where `short` ends. Anchored at the boundary so
/// `gpt-4` does not spuriously match `gpt-40-mini`.
fn boundary_prefix(short: &str, long: &str) -> bool {
    if short == long {
        return true;
    }
    long.strip_prefix(short)
        .is_some_and(|rest| rest.starts_with('-'))
}

/// The vendor root for a normalized id, when its leading token (or a boundary
/// prefix) matches the closed table.
fn vendor_root(normalized: &str) -> Option<&'static str> {
    VENDOR_ROOTS.iter().find_map(|(prefix, root)| {
        (leading_token(normalized) == *prefix || boundary_prefix(prefix, normalized))
            .then_some(*root)
    })
}

/// The id up to its first hyphen.
fn leading_token(normalized: &str) -> &str {
    normalized.split('-').next().unwrap_or(normalized)
}
