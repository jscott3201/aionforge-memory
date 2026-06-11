//! Acceptance tests for the cross-family guard's pure logic (07 §3, M6.T01):
//! the comparison-time normalization (boundary prefix, vendor roots, leading
//! token), the fail-closed unverifiable handling, and the any-match decision.
//! The M6.T05 trait-transfer probe reuses exactly this surface for its
//! same-family control.

use aionforge_security::{
    CrossFamilyGuard, FamilyVerdict, GuardDecision, GuardMode, GuardReason, family_verdict,
};
use aionforge_store::WriterFamilySet;

fn writers(families: &[&str]) -> WriterFamilySet {
    WriterFamilySet {
        families: families.iter().map(|f| f.to_string()).collect(),
        unverifiable: false,
    }
}

#[test]
fn a_boundary_prefix_is_the_same_family_in_either_direction() {
    // The status-quo bypass: a host asserting the bare family while the
    // completer declares the full model id must compare Same.
    assert_eq!(
        family_verdict("claude", "claude-sonnet-4-6"),
        FamilyVerdict::Same
    );
    assert_eq!(
        family_verdict("claude-sonnet-4-6", "claude"),
        FamilyVerdict::Same
    );
    assert_eq!(family_verdict("gpt-4o", "gpt-4o-mini"), FamilyVerdict::Same);
    // Anchored at the hyphen boundary: a shared character prefix is not a match
    // by itself ("claudex" still resolves Differ only if the roots/tokens miss —
    // here "claude" maps to anthropic and "claudex" leads with a different token).
    assert_eq!(family_verdict("gpt-4", "gpt-40-mini"), FamilyVerdict::Same,);
}

#[test]
fn the_vendor_root_table_catches_same_vendor_ids_with_no_prefix_relation() {
    assert_eq!(family_verdict("gpt-5", "o3-mini"), FamilyVerdict::Same);
    assert_eq!(family_verdict("o1", "gpt-4o"), FamilyVerdict::Same);
    // The review's confirmed bypass: a published vendor ALIAS with a different
    // leading token must still resolve to the vendor root, or a real
    // same-base-model pair compares as cross-family (fails open).
    assert_eq!(
        family_verdict("gpt-4o", "chatgpt-4o-latest"),
        FamilyVerdict::Same
    );
    assert_eq!(
        family_verdict("codex-mini-latest", "gpt-5"),
        FamilyVerdict::Same
    );
    assert_eq!(
        family_verdict("ministral-8b", "mistral-large"),
        FamilyVerdict::Same
    );
    assert_eq!(family_verdict("qwq-32b", "qwen-3"), FamilyVerdict::Same);
    assert_eq!(
        family_verdict("codestral-embed", "mistral-large"),
        FamilyVerdict::Same
    );
    assert_eq!(
        family_verdict("gemma-3-27b", "gemini-3-pro"),
        FamilyVerdict::Same
    );
    // Mapped to different roots: verifiably different.
    assert_eq!(
        family_verdict("claude-opus-4-8", "gpt-5"),
        FamilyVerdict::Differ
    );
}

#[test]
fn unmapped_vendors_compare_by_leading_token() {
    assert_eq!(
        family_verdict("deepseek-r1", "deepseek-v3"),
        FamilyVerdict::Same
    );
    assert_eq!(
        family_verdict("deepseek-r1", "qwen-3"),
        FamilyVerdict::Differ
    );
    // One mapped, one not: the leading tokens differ, so they differ.
    assert_eq!(
        family_verdict("claude-sonnet-4-6", "qwen-3"),
        FamilyVerdict::Differ
    );
    // A shared character prefix without the hyphen boundary is no relation.
    assert_eq!(family_verdict("claude", "claudex"), FamilyVerdict::Differ);
}

#[test]
fn normalization_is_trim_and_case_only() {
    assert_eq!(
        family_verdict("  Claude-Sonnet-4-6 ", "CLAUDE"),
        FamilyVerdict::Same
    );
    assert_eq!(family_verdict("", "claude"), FamilyVerdict::Unverifiable);
    assert_eq!(family_verdict("claude", "   "), FamilyVerdict::Unverifiable);
}

#[test]
fn a_rule_consolidator_is_not_inference() {
    let guard = CrossFamilyGuard::new(GuardMode::Refuse, None);
    assert_eq!(
        guard.evaluate(&writers(&["claude-sonnet-4-6"])),
        GuardDecision::NotInference,
        "no declared family means no model call: outside the guard's scope"
    );
}

#[test]
fn an_empty_consolidator_family_is_unverifiable_not_a_pass() {
    let guard = CrossFamilyGuard::new(GuardMode::Refuse, Some("  ".to_string()));
    assert_eq!(
        guard.evaluate(&writers(&["claude-sonnet-4-6"])),
        GuardDecision::Refused(GuardReason::UnverifiableConsolidator),
        "an inference call with no declared family must not slip past the guard"
    );
}

#[test]
fn any_same_family_writer_poisons_the_item() {
    let guard = CrossFamilyGuard::new(GuardMode::Refuse, Some("claude-sonnet-4-6".to_string()));
    // All writers verifiably differ: pass.
    assert_eq!(
        guard.evaluate(&writers(&["gpt-5", "mistral-large"])),
        GuardDecision::Pass
    );
    // One same-family writer among many: fire, naming the match.
    assert_eq!(
        guard.evaluate(&writers(&["gpt-5", "claude", "mistral-large"])),
        GuardDecision::Refused(GuardReason::SameFamily {
            writer_family: "claude".to_string()
        })
    );
}

#[test]
fn an_unverifiable_writer_set_fires_fail_closed() {
    let guard = CrossFamilyGuard::new(GuardMode::Refuse, Some("claude-sonnet-4-6".to_string()));
    let set = WriterFamilySet {
        families: vec!["gpt-5".to_string()],
        unverifiable: true,
    };
    assert_eq!(
        guard.evaluate(&set),
        GuardDecision::Refused(GuardReason::UnverifiableWriter),
        "one source nobody can vouch for poisons the item, however many resolved"
    );
}

#[test]
fn warn_mode_fires_the_same_finding_without_refusing() {
    let guard = CrossFamilyGuard::new(GuardMode::Warn, Some("claude-sonnet-4-6".to_string()));
    assert_eq!(
        guard.evaluate(&writers(&["claude"])),
        GuardDecision::Warned(GuardReason::SameFamily {
            writer_family: "claude".to_string()
        })
    );
    assert_eq!(
        guard.evaluate(&writers(&["gpt-5"])),
        GuardDecision::Pass,
        "warn mode still passes a clean item silently"
    );
}

#[test]
fn the_default_mode_is_refuse() {
    assert_eq!(GuardMode::default(), GuardMode::Refuse);
}
