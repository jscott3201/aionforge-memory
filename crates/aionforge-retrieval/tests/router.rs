//! Integration tests for the query-class router (03 §3).
//!
//! Pure string heuristics — no store or embedder. The tests pin each class, the
//! precedence between overlapping cues, and the profile each class maps to.

use aionforge_retrieval::{QueryClass, Signal, classify, profile_for, route};

#[test]
fn a_quoted_phrase_routes_to_quote() {
    assert_eq!(classify(r#""to be or not to be""#), QueryClass::Quote);
    assert_eq!(
        classify(r#"find the line "all that glitters""#),
        QueryClass::Quote
    );
}

#[test]
fn temporal_markers_route_to_temporal() {
    assert_eq!(
        classify("what happened before the merger"),
        QueryClass::Temporal
    );
    assert_eq!(classify("events in 2021"), QueryClass::Temporal);
    assert_eq!(classify("the policy last year"), QueryClass::Temporal);
    assert_eq!(classify("trends in the last decade"), QueryClass::Temporal);
    assert_eq!(
        classify("growth over the past century"),
        QueryClass::Temporal
    );
    assert_eq!(
        classify("what did we decide yesterday"),
        QueryClass::Temporal
    );
}

#[test]
fn a_bare_proper_noun_routes_to_entity() {
    assert_eq!(classify("Ada Lovelace"), QueryClass::Entity);
    assert_eq!(classify("France"), QueryClass::Entity);
    assert_eq!(classify("Project Apollo"), QueryClass::Entity);
}

#[test]
fn associative_cues_route_to_multi_hop() {
    assert_eq!(
        classify("how are neurons and memory related"),
        QueryClass::MultiHop
    );
    assert_eq!(classify("why did the system fail"), QueryClass::MultiHop);
    assert_eq!(
        classify("the connection between sleep and recall"),
        QueryClass::MultiHop
    );
    // Causal phrases (added after review) route multi-hop.
    assert_eq!(
        classify("what leads to increased mortality"),
        QueryClass::MultiHop
    );
    assert_eq!(
        classify("errors that result in data loss"),
        QueryClass::MultiHop
    );
}

#[test]
fn title_cased_topic_phrases_are_not_entities() {
    // A long title-cased phrase reads as a topic, not a proper noun, so it stays on
    // the safe single-hop default rather than triggering graph expansion (03 §3).
    assert_eq!(
        classify("Quantum Entanglement Breakthrough"),
        QueryClass::SingleHopFactual
    );
    assert_eq!(
        classify("Digital Transformation Initiative"),
        QueryClass::SingleHopFactual
    );
    // One- and two-token proper nouns still route to entity.
    assert_eq!(classify("France"), QueryClass::Entity);
    assert_eq!(classify("Ada Lovelace"), QueryClass::Entity);
}

#[test]
fn a_plain_question_defaults_to_single_hop_factual() {
    assert_eq!(
        classify("what is the capital of france"),
        QueryClass::SingleHopFactual
    );
    assert_eq!(
        classify("the dosage of aspirin"),
        QueryClass::SingleHopFactual
    );
    assert_eq!(classify(""), QueryClass::SingleHopFactual);
}

#[test]
fn source_and_file_anchors_route_to_quote() {
    assert_eq!(
        classify("docs/2026-plan.md"),
        QueryClass::Quote,
        "a dated path is an exact source lookup, not a temporal query"
    );
    assert_eq!(
        classify("embedding-guide graph-algorithms contributing procedures"),
        QueryClass::Quote,
        "multiple hyphenated source anchors should favor lexical lookup"
    );
}

#[test]
fn precedence_runs_specific_to_general() {
    // Quote beats a temporal marker.
    assert_eq!(
        classify(r#""machine learning" recently"#),
        QueryClass::Quote
    );
    // Temporal beats a multi-hop cue (so the bi-temporal filter applies).
    assert_eq!(
        classify("why did prices rise in 2008"),
        QueryClass::Temporal
    );
    // An interrogative blocks the bare-entity reading.
    assert_eq!(
        classify("who is Ada Lovelace"),
        QueryClass::SingleHopFactual
    );
}

#[test]
fn route_pairs_classification_with_its_profile() {
    let profile = route("what is the capital of france");
    assert_eq!(profile.class, QueryClass::SingleHopFactual);
    assert_eq!(profile, profile_for(QueryClass::SingleHopFactual));
}

#[test]
fn single_hop_factual_suppresses_graph_and_exact_reranks() {
    let p = profile_for(QueryClass::SingleHopFactual);
    assert!(
        !p.graph_expansion,
        "single-hop suppresses graph expansion (03 §3)"
    );
    assert!(
        p.exact_rerank,
        "factual uses the high-precision rerank (03 §4)"
    );
    assert!(p.restrict_to_fact_kinds);
    assert!(p.weights.lexical > 0.0 && p.weights.lexical_anchor > 0.0 && p.weights.dense > 0.0);
}

#[test]
fn multi_hop_enables_graph_expansion() {
    let p = profile_for(QueryClass::MultiHop);
    assert!(
        p.graph_expansion,
        "multi-hop enables graph expansion (03 §3)"
    );
    assert!(p.weights.graph > 0.0 && p.weights.dense > 0.0);
}

#[test]
fn temporal_applies_the_bitemporal_filter() {
    let p = profile_for(QueryClass::Temporal);
    assert!(
        p.bitemporal_filter,
        "temporal applies the bi-temporal filter (03 §5)"
    );
    assert!(!p.graph_expansion);
    assert!(p.weights.recency > 0.0);
}

#[test]
fn quote_is_lexical_only() {
    let p = profile_for(QueryClass::Quote);
    assert!(p.quote_phrase);
    assert!(p.weights.lexical > 0.0);
    assert_eq!(p.weights.dense, 0.0, "quote suppresses dense");
    assert!(
        p.weights.lexical_anchor > 0.0,
        "quote/source lookup anchors the highest lexical matches"
    );
    assert_eq!(p.weights.graph, 0.0, "quote suppresses graph");
    assert_eq!(p.weights.recency, 0.0);
    assert_eq!(p.weights.trust, 0.0);
}

#[test]
fn entity_seeds_graph_and_drops_recency() {
    let p = profile_for(QueryClass::Entity);
    assert!(p.graph_expansion);
    assert_eq!(
        p.weights.recency, 0.0,
        "entity lookups are not recency-driven"
    );
    assert!(p.weights.graph > 0.0 && p.weights.dense > 0.0);
}

#[test]
fn the_calibrated_classes_floor_off_topic_and_quote_stays_off() {
    // The four dense-bearing classes all floor at 0.60 — off-topic-rejection wins measured
    // independently on the eval harness: SingleHopFactual (#282, floor_sweep), Temporal
    // (beam_temporal_floor), MultiHop + Entity (beam_multihop_floor). All three runs converge
    // on 0.60 because it is the gemini embedder's natural on-topic/off-topic boundary on
    // BEAM-shaped queries. Pinning the exact values guards against an accidental change (they
    // are gemini-cosine-calibrated; re-measure if the embedder changes).
    for class in [
        QueryClass::SingleHopFactual,
        QueryClass::Temporal,
        QueryClass::MultiHop,
        QueryClass::Entity,
    ] {
        assert!(
            (profile_for(class).min_relevance - 0.60).abs() < 1e-12,
            "{class:?} floors off-topic hits at 0.60",
        );
    }
    // Quote stays OFF on its own merits — its dense weight is 0, so a dense floor is meaningless.
    assert_eq!(
        profile_for(QueryClass::Quote).min_relevance,
        0.0,
        "Quote keeps its dense-relevance floor OFF (dense weight is 0)",
    );
}

#[test]
fn only_the_associative_classes_exempt_graph_recovered_gold_from_the_floor() {
    // MultiHop + Entity carry the "dense-OR-signal" hybrid exemption: graph/support-recovered
    // gold (legitimately FAR in vector space) survives the 0.60 floor via Support/Graph. The
    // dense-only classes carry an EMPTY exemption set, so their floor behaviour is byte-identical
    // to the pre-hybrid dense-only gate.
    for class in [QueryClass::MultiHop, QueryClass::Entity] {
        assert_eq!(
            profile_for(class).floor_exempt_signals,
            &[Signal::Support, Signal::Graph],
            "{class:?} exempts graph-recovered gold from the dense floor",
        );
    }
    for class in [
        QueryClass::SingleHopFactual,
        QueryClass::Temporal,
        QueryClass::Quote,
    ] {
        assert!(
            profile_for(class).floor_exempt_signals.is_empty(),
            "{class:?} is dense-only — no floor exemption",
        );
    }
}

#[test]
fn signal_weights_accessor_maps_each_signal() {
    let p = profile_for(QueryClass::MultiHop);
    assert_eq!(p.weights.weight(Signal::Lexical), p.weights.lexical);
    assert_eq!(
        p.weights.weight(Signal::LexicalAnchor),
        p.weights.lexical_anchor
    );
    assert_eq!(p.weights.weight(Signal::Dense), p.weights.dense);
    assert_eq!(p.weights.weight(Signal::Support), p.weights.support);
    assert_eq!(p.weights.weight(Signal::Graph), p.weights.graph);
    assert_eq!(p.weights.weight(Signal::Authority), p.weights.authority);
    assert_eq!(p.weights.weight(Signal::Recency), p.weights.recency);
    assert_eq!(p.weights.weight(Signal::Importance), p.weights.importance);
    assert_eq!(p.weights.weight(Signal::Trust), p.weights.trust);
}

#[test]
fn the_global_authority_prior_is_staged_off_in_every_profile() {
    // R1 ships the global-authority fusion signal as inert plumbing: the mechanism (a seedless
    // PageRank Store method + the Signal + the gated retriever block) is wired, but every class
    // weights it at 0.0, so it is elided from fusion and never computed. Per the prove-before-flip
    // directive (store memory 019ed336), the per-class weight is flipped on only once a
    // graph-bearing benchmark shows a marginal lift — BEAM is episode-only and cannot measure it.
    // This guard locks the OFF posture so an accidental flip is caught until that follow-up.
    for class in [
        QueryClass::SingleHopFactual,
        QueryClass::MultiHop,
        QueryClass::Temporal,
        QueryClass::Entity,
        QueryClass::Quote,
    ] {
        assert_eq!(
            profile_for(class).weights.authority,
            0.0,
            "{class:?} stages the global-authority weight OFF pending a graph-bearing benchmark",
        );
    }
}
