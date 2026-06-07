//! Integration tests for Reciprocal Rank Fusion (03 §2).
//!
//! Pure over synthetic rankings — no store or embedder needed. The determinism
//! tests are the point: fusion must be permutation-invariant and tie-break
//! deterministically (03 §6).

use aionforge_retrieval::{
    Contribution, DEFAULT_RRF_K, FusedCandidate, RankedCandidate, Signal, SignalRanking,
    WeightedRanking, fuse,
};
use aionforge_store::NodeId;

fn node(n: u64) -> NodeId {
    NodeId::new(n)
}

/// A ranking of the given nodes, best-first; the raw score is filler (fusion ignores
/// it).
fn ranking(signal: Signal, nodes: &[u64]) -> SignalRanking {
    SignalRanking {
        signal,
        candidates: nodes
            .iter()
            .enumerate()
            .map(|(rank, &n)| RankedCandidate {
                node: node(n),
                rank,
                score: 1.0 - rank as f64 * 0.01,
            })
            .collect(),
    }
}

fn weighted(weight: f64, signal: Signal, nodes: &[u64]) -> WeightedRanking {
    WeightedRanking::new(weight, ranking(signal, nodes))
}

fn order(fused: &[FusedCandidate]) -> Vec<NodeId> {
    fused.iter().map(|c| c.node).collect()
}

#[test]
fn rrf_rewards_agreement_across_signals() {
    // Node 1 is top of both lists; node 2 is only in lexical; node 3 only in dense.
    let lexical = weighted(1.0, Signal::Lexical, &[1, 2]);
    let dense = weighted(1.0, Signal::Dense, &[1, 3]);

    let fused = fuse(&[lexical, dense], DEFAULT_RRF_K);

    assert_eq!(
        fused[0].node,
        node(1),
        "the doc both signals rank should win"
    );
    assert_eq!(
        fused[0].contributions.len(),
        2,
        "node 1 has two contributions"
    );
    // Node 1: 1/61 + 1/61; nodes 2 and 3: 1/62 each.
    let n1 = 2.0 / 61.0;
    assert!(
        (fused[0].score - n1).abs() < 1e-12,
        "score {}",
        fused[0].score
    );
}

#[test]
fn rrf_score_uses_one_based_rank_and_the_constant() {
    // A single signal, single doc at rank 0 -> weight / (k + 1).
    let fused = fuse(&[weighted(2.0, Signal::Lexical, &[7])], DEFAULT_RRF_K);
    assert_eq!(fused.len(), 1);
    let expected = 2.0 / (DEFAULT_RRF_K + 1.0);
    assert!(
        (fused[0].score - expected).abs() < 1e-12,
        "score {}",
        fused[0].score
    );
}

#[test]
fn fusion_is_invariant_to_the_order_signals_are_supplied() {
    let lexical = weighted(0.6, Signal::Lexical, &[1, 2, 3]);
    let dense = weighted(1.0, Signal::Dense, &[3, 1, 4]);
    let recency = weighted(0.3, Signal::Recency, &[2, 4, 1]);

    let abc = fuse(
        &[lexical.clone(), dense.clone(), recency.clone()],
        DEFAULT_RRF_K,
    );
    let cba = fuse(
        &[recency.clone(), dense.clone(), lexical.clone()],
        DEFAULT_RRF_K,
    );
    let bac = fuse(&[dense, lexical, recency], DEFAULT_RRF_K);

    // Byte-identical output, including scores and contribution order, regardless of
    // the order the rankings were supplied.
    assert_eq!(abc, cba, "permuted input must yield identical output");
    assert_eq!(abc, bac, "permuted input must yield identical output");
}

#[test]
fn a_zero_weight_signal_is_elided_entirely() {
    let dense = weighted(1.0, Signal::Dense, &[1, 2]);
    // Graph is switched off and uniquely ranks node 99.
    let graph = weighted(0.0, Signal::Graph, &[99, 1]);

    let fused = fuse(&[dense, graph], DEFAULT_RRF_K);

    let nodes = order(&fused);
    assert!(
        !nodes.contains(&node(99)),
        "a zero-weight-only doc must not appear"
    );
    // Node 1 was also in dense, so it appears — but with no graph contribution.
    let one = fused
        .iter()
        .find(|c| c.node == node(1))
        .expect("node 1 present");
    assert!(
        one.contributions.iter().all(|c| c.signal != Signal::Graph),
        "an elided signal leaves no contribution",
    );
}

#[test]
fn ties_break_by_node_id_ascending() {
    // Two docs each ranked first by one equally weighted signal -> identical scores.
    let lexical = weighted(1.0, Signal::Lexical, &[5]);
    let dense = weighted(1.0, Signal::Dense, &[2]);

    let fused = fuse(&[lexical, dense], DEFAULT_RRF_K);

    assert_eq!(fused.len(), 2);
    assert!(
        (fused[0].score - fused[1].score).abs() < 1e-12,
        "scores should tie"
    );
    assert_eq!(
        order(&fused),
        vec![node(2), node(5)],
        "ties order by node id"
    );
}

#[test]
fn contributions_are_in_canonical_signal_order() {
    // Supply dense before lexical; node 1 is in both.
    let dense = weighted(1.0, Signal::Dense, &[1]);
    let lexical = weighted(1.0, Signal::Lexical, &[1]);

    let fused = fuse(&[dense, lexical], DEFAULT_RRF_K);

    let signals: Vec<Signal> = fused[0].contributions.iter().map(|c| c.signal).collect();
    assert_eq!(
        signals,
        vec![Signal::Lexical, Signal::Dense],
        "contributions sort by canonical signal order, not input order",
    );
}

#[test]
fn empty_input_fuses_to_nothing() {
    assert!(fuse(&[], DEFAULT_RRF_K).is_empty());
    // A signal that returned no candidates also contributes nothing.
    let empty = weighted(1.0, Signal::Lexical, &[]);
    assert!(fuse(&[empty], DEFAULT_RRF_K).is_empty());
}

#[test]
fn contribution_term_is_documented_shape() {
    // Two signals at different ranks, distinct weights, to pin the arithmetic.
    let lexical = weighted(1.0, Signal::Lexical, &[10, 11]); // node 11 at rank 1
    let dense = weighted(0.5, Signal::Dense, &[11]); // node 11 at rank 0

    let fused = fuse(&[lexical, dense], DEFAULT_RRF_K);
    let eleven = fused.iter().find(|c| c.node == node(11)).expect("node 11");
    // 1.0/(60+1+1) + 0.5/(60+0+1)
    let expected = 1.0 / 62.0 + 0.5 / 61.0;
    assert!(
        (eleven.score - expected).abs() < 1e-12,
        "score {}",
        eleven.score
    );
    let _ = Contribution {
        signal: Signal::Lexical,
        rank: 0,
        weight: 1.0,
    }; // Contribution is part of the public surface.
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "k_const must be positive")]
fn a_non_positive_constant_trips_the_debug_contract() {
    let _ = fuse(&[weighted(1.0, Signal::Lexical, &[1])], 0.0);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "weight must be non-negative")]
fn a_negative_weight_trips_the_debug_contract() {
    let _ = fuse(&[weighted(-1.0, Signal::Lexical, &[1])], DEFAULT_RRF_K);
}
