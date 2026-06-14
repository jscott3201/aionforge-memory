//! The capture-side privacy and prompt-injection filter (04 §1, 07 §2).
//!
//! [`CaptureFilter`] runs on the capture hot path before an episode is committed: it
//! redacts configured sensitive spans and detects/strips known prompt-injection
//! markers, recording what it did in the [`FilterOutcome`] that the capture path
//! folds into `Episode.origin` (02 §6.1). It is local and synchronous, so it adds no
//! network round-trip to capture.
//!
//! The default marker set is **precision-first** and hardened against a published
//! corpus (M6.T03): each pattern is an anchored multi-token override / exfiltration /
//! role-swap phrase, measured to fire on no benign trigger-word row (zero false positives
//! on `leolee99/NotInject`) while clearing a block-rate floor on
//! `deepset/prompt-injections` (`tests/injection_corpus.rs`). It deliberately matches only
//! the imperative-override family a string filter can recognize; semantic / role-play
//! injection that carries no marker phrase is out of scope here and is the job of the
//! recall-side untrusted-data tagging and system-role exclusion (M6.T02) and the red-team
//! probes (M6.T04). It "raises the bar" (07 §2) — not a complete injection defense.
//! Callers can supply their own pattern sets via [`CaptureFilter::new`].
//!
//! Redaction spans are reported as byte offsets into the *original* content (the
//! `Redaction.span` contract), and the matched text is replaced with a typed
//! `[redacted:<kind>]` placeholder; injection markers are stripped from the cleaned
//! content and their ids collected into `injection_flags`. Matches are applied as a
//! single deterministic, non-overlapping edit pass: the earliest start wins and the
//! longer match breaks a tie. A later match fully covered by an applied one is
//! dropped; one that only partially overlaps still has its uncovered tail replaced,
//! so the pass is fail-closed — no matched (sensitive) byte is ever copied out.

use aionforge_domain::nodes::episodic::Redaction;
use aionforge_domain::{FilterOutcome, PrivacyFilter};
use regex::Regex;

use crate::error::SecurityError;

/// An extra check a regex match must pass before it is treated as a real hit — a cheap way to
/// cut false positives a regex alone cannot (e.g. distinguishing a card number from an ISBN or
/// product code). M6.T03 may add more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchValidator {
    /// The match's digits must satisfy the Luhn checksum (payment-card numbers do; ISBNs,
    /// tracking numbers, and arbitrary digit runs almost never do).
    Luhn,
    /// The match must be an email address, not the `git@host` user/host prefix of an SSH remote.
    EmailAddress,
}

impl MatchValidator {
    /// Whether this regex hit passes its structural check.
    fn accepts(self, content: &str, start: usize, end: usize) -> bool {
        let matched = &content[start..end];
        match self {
            MatchValidator::Luhn => luhn_valid(matched),
            MatchValidator::EmailAddress => email_address_valid(content, matched, end),
        }
    }
}

/// A configured redaction rule: a regex whose matches are recorded and replaced, optionally
/// gated by a [`MatchValidator`] that rejects regex matches failing a structural check.
#[derive(Debug, Clone)]
pub struct RedactionPattern {
    id: String,
    kind: String,
    regex: Regex,
    validator: Option<MatchValidator>,
}

impl RedactionPattern {
    /// Compile a redaction rule. `id` names the rule (recorded as `pattern_id`),
    /// `kind` labels the sensitive-data class, and `pattern` is its regex.
    ///
    /// # Errors
    /// Returns [`SecurityError::InvalidPattern`] if `pattern` is not a valid regex.
    pub fn new(
        id: impl Into<String>,
        kind: impl Into<String>,
        pattern: &str,
    ) -> Result<Self, SecurityError> {
        let id = id.into();
        let regex = Regex::new(pattern).map_err(|source| SecurityError::InvalidPattern {
            id: id.clone(),
            source,
        })?;
        Ok(Self {
            id,
            kind: kind.into(),
            regex,
            validator: None,
        })
    }

    /// Gate this rule's matches on `validator`; a match that fails the check is not redacted.
    #[must_use]
    pub fn with_validator(mut self, validator: MatchValidator) -> Self {
        self.validator = Some(validator);
        self
    }
}

/// The Luhn (mod-10) checksum over the digits in `candidate`, requiring a payment-card-length
/// run (13–19 digits). Separators are ignored; non-card digit runs (ISBN-13, UPC, order ids)
/// almost never satisfy it, so it sharply cuts the card pattern's false positives.
fn luhn_valid(candidate: &str) -> bool {
    let digits: Vec<u32> = candidate.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            if i % 2 == 1 {
                let doubled = d * 2;
                if doubled > 9 { doubled - 9 } else { doubled }
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

fn email_address_valid(content: &str, matched: &str, end: usize) -> bool {
    let local = matched.split('@').next().unwrap_or_default();
    let next = content[end..].chars().next();
    if local == "git" && matches!(next, Some(':') | Some('/')) {
        return false;
    }
    true
}

/// A known prompt-injection marker: a regex whose matches are flagged and stripped.
#[derive(Debug, Clone)]
pub struct InjectionMarker {
    id: String,
    regex: Regex,
}

impl InjectionMarker {
    /// Compile an injection marker. `id` names the marker (recorded in
    /// `injection_flags`); `pattern` is its regex (use the `(?i)` flag for
    /// case-insensitivity).
    ///
    /// # Errors
    /// Returns [`SecurityError::InvalidPattern`] if `pattern` is not a valid regex.
    pub fn new(id: impl Into<String>, pattern: &str) -> Result<Self, SecurityError> {
        let id = id.into();
        let regex = Regex::new(pattern).map_err(|source| SecurityError::InvalidPattern {
            id: id.clone(),
            source,
        })?;
        Ok(Self { id, regex })
    }
}

/// The capture-side privacy/injection filter (07 §2).
#[derive(Debug, Clone)]
pub struct CaptureFilter {
    redactions: Vec<RedactionPattern>,
    markers: Vec<InjectionMarker>,
}

impl CaptureFilter {
    /// Build a filter from explicit redaction and injection-marker rule sets.
    #[must_use]
    pub fn new(redactions: Vec<RedactionPattern>, markers: Vec<InjectionMarker>) -> Self {
        Self {
            redactions,
            markers,
        }
    }

    /// Build a filter with the conservative v1.0 default pattern set.
    ///
    /// # Errors
    /// Returns [`SecurityError::InvalidPattern`] only if a built-in pattern fails to
    /// compile, which the unit tests guard against.
    pub fn with_defaults() -> Result<Self, SecurityError> {
        let redactions = DEFAULT_REDACTIONS
            .iter()
            .map(|&(id, kind, pattern, validator)| {
                let rule = RedactionPattern::new(id, kind, pattern)?;
                Ok(match validator {
                    Some(v) => rule.with_validator(v),
                    None => rule,
                })
            })
            .collect::<Result<Vec<_>, SecurityError>>()?;
        let markers = DEFAULT_MARKERS
            .iter()
            .map(|&(id, pattern)| InjectionMarker::new(id, pattern))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::new(redactions, markers))
    }
}

/// One planned change to the content, before overlap resolution.
struct Edit {
    start: usize,
    end: usize,
    replacement: String,
    redaction: Option<Redaction>,
    flag: Option<String>,
    /// Index into `self.markers` for a marker edit, so an applied hit can be tallied
    /// in marker-declaration order (`None` for a redaction edit).
    marker_idx: Option<usize>,
}

impl PrivacyFilter for CaptureFilter {
    type Error = SecurityError;

    fn filter(&self, content: &str) -> Result<FilterOutcome, Self::Error> {
        let mut edits: Vec<Edit> = Vec::new();

        for pattern in &self.redactions {
            for m in pattern.regex.find_iter(content) {
                // A validated rule (e.g. Luhn for cards) drops matches that fail the check, so a
                // regex that necessarily over-matches stays low-false-positive.
                if let Some(validator) = pattern.validator
                    && !validator.accepts(content, m.start(), m.end())
                {
                    continue;
                }
                edits.push(Edit {
                    start: m.start(),
                    end: m.end(),
                    replacement: format!("[redacted:{}]", pattern.kind),
                    redaction: Some(Redaction {
                        pattern_id: pattern.id.clone(),
                        span: (m.start(), m.end()),
                        kind: pattern.kind.clone(),
                    }),
                    flag: None,
                    marker_idx: None,
                });
            }
        }
        for (idx, marker) in self.markers.iter().enumerate() {
            for m in marker.regex.find_iter(content) {
                edits.push(Edit {
                    start: m.start(),
                    end: m.end(),
                    replacement: String::new(),
                    redaction: None,
                    flag: Some(marker.id.clone()),
                    marker_idx: Some(idx),
                });
            }
        }

        // Deterministic, non-overlapping edit order: earliest start first, longer
        // match first on a tie. The walk below is fail-closed: a later edit fully
        // covered by an applied one is dropped, but one that only partially overlaps
        // still has its uncovered tail replaced — a redaction silently dropped here
        // would leak the raw sensitive bytes its match started inside.
        edits.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));

        let mut cleaned = String::with_capacity(content.len());
        let mut redactions = Vec::new();
        let mut injection_flags: Vec<String> = Vec::new();
        // Applied-hit tally per marker, indexed by position in `self.markers` so the
        // counts stay in declaration order without a clock, RNG, or hash map.
        let mut marker_hit_counts = vec![0u32; self.markers.len()];
        let mut cursor = 0usize;

        for edit in edits {
            if edit.end <= cursor {
                continue; // fully covered by an already-applied edit
            }
            if edit.start >= cursor {
                // Copy the run of unmatched (non-sensitive) text before this edit.
                cleaned.push_str(&content[cursor..edit.start]);
            }
            // Emit the placeholder, never the raw tail, and advance past the whole
            // match so a partially overlapped sensitive span cannot survive.
            cleaned.push_str(&edit.replacement);
            cursor = edit.end;
            if let Some(redaction) = edit.redaction {
                redactions.push(redaction);
            }
            // Tally the applied marker before the de-duplicating `injection_flags` push,
            // so a marker that fires more than once is counted each time it fires.
            if let Some(idx) = edit.marker_idx {
                marker_hit_counts[idx] += 1;
            }
            if let Some(id) = edit.flag
                && !injection_flags.contains(&id)
            {
                injection_flags.push(id);
            }
        }
        cleaned.push_str(&content[cursor..]);

        // Emit per-marker counts in declaration order, one entry per marker that fired.
        let marker_hits = self
            .markers
            .iter()
            .zip(&marker_hit_counts)
            .filter(|&(_, &count)| count > 0)
            .map(|(marker, &count)| (marker.id.clone(), count))
            .collect();

        Ok(FilterOutcome {
            cleaned,
            redactions,
            injection_flags,
            marker_hits,
        })
    }
}

/// Default redaction rules: `(id, kind, regex)`. Conservative to keep the benign
/// false-positive rate low (07 §2 acceptance); M6.T03 expands and tunes these.
const DEFAULT_REDACTIONS: &[(&str, &str, &str, Option<MatchValidator>)] = &[
    (
        "email",
        "email",
        r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}",
        Some(MatchValidator::EmailAddress),
    ),
    (
        "us_phone",
        "phone",
        r"\b(?:\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}\b",
        None,
    ),
    // 13–19 digits in single space/dash-separated groups, then a Luhn check — so a real card
    // is caught whatever its formatting, while an ISBN-13 or product code (which fail Luhn) is
    // left alone. The raw `{13,16}`-digit regex matched any such run; the validator is the fix.
    (
        "credit_card",
        "card",
        r"\b\d(?:[ -]?\d){12,18}\b",
        Some(MatchValidator::Luhn),
    ),
    ("secret_key", "secret", r"\bsk-[A-Za-z0-9_-]{20,}\b", None),
];

/// Default injection markers: `(id, regex)`. All case-insensitive, all anchored on
/// multi-token override/exfiltration/role-swap *phrases* rather than bare trigger words.
///
/// This set is hardened against a published corpus (M6.T03): every pattern was measured
/// against the benign trigger-word corpus (`leolee99/NotInject`) to keep its benign
/// false-positive contribution at zero, and the set as a whole is gated by the corpus
/// harness (`tests/injection_corpus.rs`). The discipline is precision-first: a capture-time
/// string filter only sees the imperative-override family, so markers that would also fire
/// on benign task/role dialogue ("act as a translator", "your next task") are deliberately
/// excluded — that semantic-injection surface is the recall-side layer's job (M6.T02) and
/// the red-team probes' (M6.T04), not this filter's. See `docs/capture.md` for the
/// honest-scope framing.
const DEFAULT_MARKERS: &[(&str, &str)] = &[
    // Override the prior context: "ignore the above", "forget all previous tasks",
    // "disregard prior instructions", "ignore your previous instructions". Requires an
    // override verb + a previous/above scope; the determiner slot accepts the article,
    // possessives, and demonstratives ("the/your/these/those") so the very common possessive
    // phrasing fires, but the scope adjective stays the override-flavored set (previous/prior/
    // earlier/above/...) — deliberately NOT "last", whose optional-noun shape ("ignore the
    // last warning") is a benign-traffic false-positive surface the corpus does not contain.
    // The bare verb ("you can ignore the error") never fires.
    (
        "ignore_or_forget_context",
        r"(?i)\b(?:ignore|disregard|forget)\s+(?:all\s+|everything\s+|any\s+|about\s+)?(?:the\s+|your\s+|these\s+|those\s+)?(?:previous|prior|earlier|above|preceding|foregoing)(?:\s+(?:instructions?|prompts?|tasks?|orders?|rules?|messages?|commands?|conversations?|context|directions?))?\b",
    ),
    // "forget everything we discussed / I told you / above / before" — the same override
    // intent phrased without a previous/above adjective.
    (
        "forget_everything",
        r"(?i)\b(?:forget|ignore)\s+everything\s+(?:we|i|you|that|above|before|said)\b",
    ),
    // "override your instructions / system / prompt".
    (
        "override_instructions",
        r"(?i)\boverride\s+(?:your\s+)?(?:previous\s+)?(?:instructions|system|prompt)",
    ),
    // A spoofed instruction header: "new instructions:", "updated instructions:".
    (
        "new_instructions",
        r"(?i)(?:new|updated)\s+instructions\s*:",
    ),
    // A spoofed system-prompt header: "system prompt:".
    ("system_prompt", r"(?i)system\s+prompt\s*:"),
    // Exfiltration: "reveal/show/print your system prompt", "repeat the initial instructions".
    // Requires an exfil verb head AND a prompt/instructions object, so benign "the system
    // prompt is long" (no verb) and "show your work" (wrong object) do not fire.
    (
        "reveal_system_prompt",
        r"(?i)\b(?:reveal|show|print|repeat|output|display|tell|give)\s+(?:me\s+)?(?:all\s+)?(?:your|the)\s+(?:(?:system|initial|original|hidden|previous)\s+)?(?:prompts?|instructions?)\b",
    ),
    // Persona override toward a jailbreak identity: "you are now DAN", "act as an
    // unrestricted ...", "developer mode". Gated on a jailbreak-persona object token so
    // benign "you are now logged in" / "act as a translator" never fire.
    (
        "role_override_persona",
        r"(?i)\b(?:you\s+are\s+now|from\s+now\s+on\s+you\s+(?:are|will\s+be|act)|act\s+as|behave\s+as|pretend\s+(?:to\s+be|you\s+are)|roleplay\s+as|stop\s+being)\s+(?:a\s+|an\s+|the\s+)?(?:dan|jailbroken|unrestricted|uncensored|developer\s+mode|admin|root|evil)\b",
    ),
    // The "do anything now" / developer-mode jailbreak signatures (DAN family).
    (
        "do_anything_now",
        r"(?i)\bdo\s+anything\s+now\b|\bdeveloper\s+mode\s+(?:enabled|on|activated)\b|\b(?:enable|enter|activate)\s+developer\s+mode\b",
    ),
    // A spoofed prompt boundary used to smuggle "new" instructions: dash fences, fake
    // </system> tags, or [INST]/[SYS] markers. Anchored on the structural delimiter, never
    // on bare words like "system" or "end".
    (
        "prompt_boundary_spoof",
        r"(?i)(?:-{2,}\s*end\s+of\s+(?:system\s+)?prompt\s*-{2,}|\bend\s+of\s+(?:instructions?|prompt)\b\s*[:.]|<\s*/?\s*(?:system|instructions?)\s*>|\[/?(?:INST|SYS)\])",
    ),
];
