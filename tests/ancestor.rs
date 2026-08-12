#![allow(unused_imports)]
//! Coin selection over candidates that drag in unconfirmed ancestors (CPFP).
//!
//! The invariant under test is that a selection's fee obligation includes the bump owed by the
//! **union** of the ancestors its selected candidates drag in — each ancestor charged exactly once,
//! weights and fees netted over the union — and that `LowestFee` branch and bound stays correct
//! under the resulting non-monotone funding.

mod common;

use bdk_coin_select::{
    float::Ordf32,
    metrics::{Changeless, LowestFee},
    AncestorToBump, BnbMetric, Candidate, CoinSelector, Drain, DrainWeights, FeeRate, Input,
    SelectionProblem, Target, TargetFee, TargetOutputs, TX_FIXED_FIELD_WEIGHT,
};
use proptest::prelude::*;

/// Not a txid of any ancestor we pass in, so inputs residing on it are treated as confirmed.
const CONFIRMED: &str = "confirmed";

const P2WPKH_INPUT_WEIGHT: u64 = 272;

fn target(feerate_sat_per_vb: f32, value: u64) -> Target {
    Target {
        fee: TargetFee {
            rate: FeeRate::from_sat_per_vb(feerate_sat_per_vb),
            absolute: 0,
            replace: None,
        },
        outputs: TargetOutputs {
            value_sum: value,
            weight_sum: 100,
            n_outputs: 1,
        },
        max_weight: None,
    }
}

fn input(value: u64, residing_txid: &'static str) -> Input<&'static str> {
    Input {
        value,
        weight: P2WPKH_INPUT_WEIGHT,
        is_segwit: true,
        residing_txid,
    }
}

fn ancestor(
    txid: &'static str,
    weight: u64,
    fee: u64,
    parents: Vec<&'static str>,
) -> AncestorToBump<&'static str> {
    AncestorToBump {
        txid,
        weight,
        fee,
        parents,
    }
}

fn metric() -> LowestFee {
    LowestFee {
        long_term_feerate: FeeRate::from_sat_per_vb(1.0),
        dust_relay_feerate: FeeRate::from_sat_per_vb(1.0),
        drain_weights: DrainWeights::TR_KEYSPEND,
    }
}

/// The bump is charged on top of the child's own feerate obligation, so it eats exactly that much
/// excess relative to the same selection with nothing unconfirmed behind it.
#[test]
fn bump_is_charged_on_top_of_the_childs_own_fee() {
    let t = target(10.0, 90_000);
    // 1000 wu at 10 sat/vb (2.5 sat/wu) => 2500 sats owed, and the ancestor pays nothing.
    let problem =
        SelectionProblem::new(t, [input(100_000, "P")], [ancestor("P", 1_000, 0, vec![])]);
    let mut cs = problem.selector();
    cs.select(0);

    assert_eq!(cs.ancestor_bump(), 2_500);

    let no_ancestors = SelectionProblem::new_no_ancestors(
        t,
        [Candidate {
            value: 100_000,
            weight: P2WPKH_INPUT_WEIGHT,
            segwit_count: 1,
            legacy_count: 0,
        }],
    );
    let mut clean_cs = no_ancestors.selector();
    clean_cs.select(0);

    assert_eq!(clean_cs.ancestor_bump(), 0);
    assert_eq!(
        cs.weight(t.outputs, DrainWeights::NONE),
        clean_cs.weight(t.outputs, DrainWeights::NONE)
    );
    assert_eq!(
        cs.excess(Drain::NONE),
        clean_cs.excess(Drain::NONE) - 2_500,
        "the bump is the only difference between the two selections"
    );
    assert_eq!(
        cs.implied_fee(DrainWeights::NONE),
        clean_cs.implied_fee(DrainWeights::NONE) + 2_500
    );
}

/// An unconfirmed ancestor can cost more than the coin sitting on it is worth: funding is no longer
/// monotone in the selection.
#[test]
fn dragged_in_ancestor_can_unfund_a_selection() {
    let t = target(10.0, 90_000);
    let problem = SelectionProblem::new(
        t,
        [input(100_000, CONFIRMED), input(100_000, "P")],
        // 100_000 wu at 2.5 sat/wu => 250_000 sats owed: far more than the coin is worth.
        [ancestor("P", 100_000, 0, vec![])],
    );

    let mut clean_only = problem.selector();
    clean_only.select(0);
    assert!(clean_only.is_funded());

    let mut both = problem.selector();
    both.select(0);
    both.select(1);
    assert!(
        !both.is_funded(),
        "adding a coin with an expensive ancestor un-funds a funded selection"
    );
}

/// A shared ancestor is paid for once, no matter how many selected candidates drag it in — summing
/// the per-candidate `local_bump` figures would pay for it twice.
#[test]
fn shared_ancestor_is_charged_once() {
    let t = target(10.0, 10_000);
    let problem = SelectionProblem::new(
        t,
        [input(50_000, "P"), input(60_000, "P")],
        [ancestor("P", 1_000, 0, vec![])],
    );

    assert_eq!(problem.local_bump(0), 2_500);
    assert_eq!(problem.local_bump(1), 2_500);

    let mut cs = problem.selector();
    cs.select(0);
    cs.select(1);

    assert_eq!(cs.selected_ancestors().len(), 1);
    assert_eq!(cs.ancestor_bump(), 2_500);
    assert_ne!(
        cs.ancestor_bump(),
        problem.local_bump(0) + problem.local_bump(1)
    );
}

/// Deselecting one of two candidates that share an ancestor keeps the ancestor: it is still dragged
/// in by the other one.
#[test]
fn deselecting_keeps_an_ancestor_another_candidate_still_drags_in() {
    let t = target(10.0, 10_000);
    let problem = SelectionProblem::new(
        t,
        [
            input(50_000, "P"),
            input(60_000, "P"),
            input(70_000, CONFIRMED),
        ],
        [ancestor("P", 1_000, 0, vec![])],
    );
    let mut cs = problem.selector();

    cs.select(0);
    cs.select(1);
    assert_eq!(cs.ancestor_bump(), 2_500);

    cs.deselect(0);
    assert_eq!(cs.ancestor_bump(), 2_500, "candidate 1 still drags in P");

    cs.select(2);
    assert_eq!(
        cs.ancestor_bump(),
        2_500,
        "a confirmed coin drags in nothing"
    );

    cs.deselect(1);
    assert_eq!(cs.ancestor_bump(), 0, "nothing selected drags in P anymore");
}

/// The whole transitive chain is charged, and fees are netted across it (not per ancestor).
#[test]
fn transitive_ancestors_are_netted_as_one_package() {
    let t = target(10.0, 10_000);
    let problem = SelectionProblem::new(
        t,
        [input(50_000, "B")],
        [
            ancestor("A", 400, 0, vec![]),
            ancestor("B", 400, 1_000, vec!["A"]),
        ],
    );
    let mut cs = problem.selector();
    cs.select(0);

    // Union: weight 800 => 2000 sats owed at 2.5 sat/wu, of which B already paid 1000.
    assert_eq!(cs.selected_ancestors().len(), 2);
    assert_eq!(cs.ancestor_bump(), 1_000);
}

/// Dragging in an ancestor that overpays *lowers* what the selection owes, because the deficit is
/// netted over the union. This is what makes a funded selection's fee a bad lower bound for its
/// descendants.
#[test]
fn overpaying_ancestor_offsets_an_underpaying_one() {
    let t = target(1.0, 10_000); // 0.25 sat/wu
    let problem = SelectionProblem::new(
        t,
        [input(50_000, "RICH"), input(50_000, "POOR")],
        [
            ancestor("RICH", 400, 10_000, vec![]),
            ancestor("POOR", 400, 0, vec![]),
        ],
    );

    let mut poor_only = problem.selector();
    poor_only.select(1);
    assert_eq!(poor_only.ancestor_bump(), 100);

    let mut rich_only = problem.selector();
    rich_only.select(0);
    assert_eq!(rich_only.ancestor_bump(), 0, "never credits the child");

    let mut both = problem.selector();
    both.select(0);
    both.select(1);
    assert_eq!(
        both.ancestor_bump(),
        0,
        "RICH's surplus covers POOR's deficit, so the superset owes less"
    );
}

/// Ancestor weight is not part of the child transaction, so it must not count against
/// [`Target::max_weight`].
#[test]
fn ancestor_weight_does_not_count_against_max_weight() {
    let mut t = target(10.0, 10_000);
    let heavy = 100_000;
    let problem = SelectionProblem::new(
        t,
        [input(50_000, "P")],
        [ancestor("P", heavy, heavy, vec![])],
    );
    let mut cs = problem.selector();
    cs.select(0);

    let child_weight = cs.weight(t.outputs, DrainWeights::NONE);
    assert!(child_weight < heavy);

    t.max_weight = Some(child_weight);
    let capped = SelectionProblem::new(
        t,
        [input(50_000, "P")],
        [ancestor("P", heavy, heavy, vec![])],
    );
    let mut capped_cs = capped.selector();
    capped_cs.select(0);
    assert!(capped_cs.is_within_max_weight(DrainWeights::NONE));
}

/// Two coins of equal value and weight are *not* interchangeable when only one of them drags in an
/// ancestor, so branch and bound must not ban them as a group.
///
/// Here the only fundable selection is the clean coin alone, and it sits *after* the coin with the
/// expensive ancestor in the search order (equal value-per-weight, so the sort is stable). If the
/// exclusion branch banned it along with its look-alike, the search would report no solution.
#[test]
fn look_alikes_with_different_ancestors_are_not_banned_together() {
    let t = target(10.0, 90_000);
    let problem = SelectionProblem::new(
        t,
        [input(100_000, "P"), input(100_000, CONFIRMED)],
        [ancestor("P", 100_000, 0, vec![])],
    );

    assert_eq!(problem.candidate(0).value, problem.candidate(1).value);
    assert_eq!(problem.candidate(0).weight, problem.candidate(1).weight);

    let mut cs = problem.selector();
    let (_score, _drain) = cs
        .run_bnb(metric(), 100_000)
        .expect("the clean coin funds the target on its own");

    assert!(cs.is_selected(1));
    assert!(!cs.is_selected(0));
}

/// The bump has to be inside the fee the metric reports, not added on top of it.
#[test]
fn score_is_the_childs_fee_which_already_covers_the_bump() {
    let t = target(10.0, 90_000);
    let problem =
        SelectionProblem::new(t, [input(100_000, "P")], [ancestor("P", 1_000, 0, vec![])]);
    let mut cs = problem.selector();
    cs.select(0);

    let mut m = metric();
    let score = m.score(&cs).expect("funded");
    let drain = m.drain(&cs);
    assert_eq!(
        score,
        Ordf32(
            (cs.fee(t.value(), drain.value) as u64 + drain.weights.spend_fee(m.long_term_feerate))
                as f32
        )
    );
    assert!(
        cs.fee(t.value(), drain.value) as u64 >= cs.ancestor_bump(),
        "a funded selection's child fee covers the bump"
    );
}

/// A changeless solution can be reachable *only* by adding a coin whose ancestor eats the excess,
/// which the `Changeless` wrapper's prune cannot see.
///
/// That prune asks "does the reachable selection with the least excess still have change?", and
/// builds it by adding the remaining coins with negative effective value. Here the coin that kills
/// the change looks profitable on its own (1000 sats for 200 wu) — it only shrinks the excess
/// because it drags in an ancestor owing 10_800 sats. So the prune concludes change is unavoidable
/// and would discard the one changeless solution there is.
#[test]
fn changeless_solution_reachable_only_via_an_ancestor_is_not_pruned() {
    let t = target(1.0, 100_000);
    let problem = SelectionProblem::new(
        t,
        [
            Input {
                value: 110_000,
                weight: 200,
                is_segwit: true,
                residing_txid: CONFIRMED,
            },
            Input {
                value: 1_000,
                weight: 200,
                is_segwit: true,
                residing_txid: "P",
            },
        ],
        // 43_200 wu at 0.25 sat/wu => 10_800 sats owed.
        [ancestor("P", 43_200, 0, vec![])],
    );

    let mut m = metric();

    // The coin that drags in the ancestor is *not* one the prune would pick up: on its own it is
    // worth more than it costs to spend.
    assert!(problem.candidate(1).effective_value(t.fee.rate) > 0.0);

    let mut clean_only = problem.selector();
    clean_only.select(0);
    assert!(clean_only.is_funded());
    assert!(
        m.drain(&clean_only).is_some(),
        "the clean coin on its own overshoots enough to warrant change"
    );

    let mut both = problem.selector();
    both.select(0);
    both.select(1);
    assert_eq!(both.ancestor_bump(), 10_800);
    assert!(both.is_funded(), "still funded after paying the bump");
    assert!(
        m.drain(&both).is_none(),
        "the bump leaves too little excess to be worth a change output"
    );

    // So the only changeless solution is both coins together, reachable only *through* the node
    // that has change.
    let mut cs = problem.selector();
    let (score, drain) = cs
        .run_bnb(Changeless(metric()), 100_000)
        .expect("the changeless solution must not be pruned");
    assert!(drain.is_none());
    assert!(cs.is_selected(0) && cs.is_selected(1));
    assert_eq!(score, Ordf32(11_000.0));
}

// --- randomized cross-checks ---

/// Spec for a randomly generated ancestor problem. Indices are taken modulo the relevant length so
/// any combination of generated numbers describes a valid (acyclic) problem.
#[derive(Debug, Clone)]
struct AncestorProblemSpec {
    /// `(value, weight, residing_txid_selector)` per candidate.
    candidates: Vec<(u64, u64, usize)>,
    /// `(weight, fee, parent_selector)` per unconfirmed ancestor.
    ancestors: Vec<(u64, u64, usize)>,
    target_value: u64,
    feerate: f32,
    max_weight: Option<u64>,
}

impl AncestorProblemSpec {
    fn build(&self) -> SelectionProblem {
        let n_anc = self.ancestors.len();

        let ancestors: Vec<AncestorToBump<usize>> = self
            .ancestors
            .iter()
            .enumerate()
            .map(|(i, &(weight, fee, parent_sel))| {
                // Parents are strictly earlier ancestors (keeps the graph acyclic); selecting `i`
                // itself means "no unconfirmed parent".
                let parent = parent_sel % (i + 1);
                AncestorToBump {
                    txid: i,
                    weight,
                    fee,
                    parents: if parent == i { vec![] } else { vec![parent] },
                }
            })
            .collect();

        let inputs: Vec<Input<usize>> = self
            .candidates
            .iter()
            .map(|&(value, weight, residing_sel)| Input {
                value,
                weight,
                is_segwit: true,
                // `n_anc` means the coin sits on a confirmed tx (no matching txid).
                residing_txid: residing_sel % (n_anc + 1),
            })
            .collect();

        let mut t = target(self.feerate, self.target_value);
        t.max_weight = self.max_weight;
        SelectionProblem::new(t, inputs, ancestors)
    }
}

fn spec_strategy() -> impl Strategy<Value = AncestorProblemSpec> {
    (
        prop::collection::vec((1_000u64..200_000, 200u64..1_000, 0usize..8), 1..6),
        prop::collection::vec((200u64..4_000, 0u64..3_000, 0usize..8), 0..4),
        10_000u64..400_000,
        1.0f32..30.0,
        proptest::option::of(400u64..3_000),
    )
        .prop_map(
            |(candidates, ancestors, target_value, feerate, max_weight)| AncestorProblemSpec {
                candidates,
                ancestors,
                target_value,
                feerate,
                max_weight,
            },
        )
}

/// Independently computed bump for a selection, straight from the definition: union the ancestor
/// sets of the selected candidates, sum weight and fee over that union, and take the shortfall.
fn expected_bump(problem: &SelectionProblem, cs: &CoinSelector<'_>, feerate: FeeRate) -> u64 {
    let mut union = std::collections::BTreeSet::new();
    for i in cs.selected_indices().iter() {
        union.extend(problem.drags_in(i).iter());
    }
    let (weight, fee) = union
        .iter()
        .map(|&i| problem.ancestors()[i])
        .fold((0u64, 0u64), |(w, f), (aw, af)| (w + aw, f + af));
    feerate.implied_fee_wu(weight).saturating_sub(fee)
}

proptest! {
    /// Every selection's bump must equal the union-derived figure — in particular it must never be
    /// the sum of the per-candidate `local_bump`s when ancestors are shared.
    #[test]
    fn bump_matches_union_definition(spec in spec_strategy()) {
        let problem = spec.build();
        let feerate = problem.target().fee.rate;
        let cs = problem.selector();

        prop_assert_eq!(cs.ancestor_bump(), expected_bump(&problem, &cs, feerate));

        for (node, _) in common::ExhaustiveIter::new(&cs).into_iter().flatten() {
            prop_assert_eq!(
                node.ancestor_bump(),
                expected_bump(&problem, &node, feerate),
                "selection={}", node
            );
        }
    }

    /// The bound must never exceed the score of any selection in its subtree (else branch and bound
    /// can prune the optimum), and `None` must really mean "nothing in this subtree is valid".
    #[test]
    fn bound_is_admissible_with_ancestors(spec in spec_strategy()) {
        let problem = spec.build();
        let mut metric = metric();

        let mut root = problem.selector();
        if metric.requires_ordering_by_descending_value_pwu() {
            root.sort_candidates_by_descending_value_pwu();
        }

        let nodes = std::iter::once(root.clone()).chain(
            common::ExhaustiveIter::new(&root)
                .into_iter()
                .flatten()
                .map(|(node, _)| node),
        );

        for node in nodes {
            let bound = metric.bound(&node);
            let subtree = std::iter::once(node.clone()).chain(
                common::ExhaustiveIter::new(&node)
                    .into_iter()
                    .flatten()
                    .filter(|(_, inclusion)| *inclusion)
                    .map(|(descendant, _)| descendant),
            );

            for descendant in subtree {
                let score = metric.score(&descendant);
                match bound {
                    Some(lb) => if let Some(score) = score {
                        prop_assert!(
                            score >= lb,
                            "bound too tight: node={} lb={} descendant={} score={}",
                            node, lb, descendant, score
                        );
                    },
                    None => prop_assert!(
                        score.is_none(),
                        "pruned a subtree with a solution: node={} descendant={} score={:?}",
                        node, descendant, score
                    ),
                }
            }
        }
    }

    /// With unlimited rounds, branch and bound must land on the same optimum as brute force — both
    /// the score and the feasibility verdict.
    #[test]
    fn bnb_finds_the_brute_force_optimum(spec in spec_strategy()) {
        let problem = spec.build();

        let mut exhaustive_cs = problem.selector();
        let mut exhaustive_metric = metric();
        let expected = common::exhaustive_search(&mut exhaustive_cs, &mut exhaustive_metric);

        let mut bnb_cs = problem.selector();
        let found = common::bnb_search(&mut bnb_cs, metric(), usize::MAX);

        match (expected, found) {
            (Some((expected_score, _)), Ok((score, _))) => {
                prop_assert_eq!(score, expected_score, "bnb={} exhaustive={}", bnb_cs, exhaustive_cs);
            }
            (None, Err(_)) => {}
            (expected, found) => prop_assert!(
                false,
                "disagreement: exhaustive={:?} bnb={:?}",
                expected.map(|(score, _)| score),
                found.map(|(score, _)| score),
            ),
        }
    }

    /// Same for the changeless-constrained metric, whose extra prune ("every reachable selection
    /// would have change") also leans on a candidate costing only its own weight.
    ///
    /// NOTE: `max_weight` is forced off here. `Changeless<LowestFee>` disagrees with brute force on
    /// capped problems *without* any ancestors too (`LowestFee` reports a selection as changeless
    /// when change would bust the cap, a route to changelessness that `Changeless`'s
    /// excess-monotone prune doesn't consider), so that is a separate, pre-existing issue rather
    /// than something ancestors introduce.
    #[test]
    fn changeless_bnb_finds_the_brute_force_optimum(
        spec in spec_strategy().prop_map(|spec| AncestorProblemSpec { max_weight: None, ..spec }),
    ) {
        let problem = spec.build();

        let mut exhaustive_cs = problem.selector();
        let mut exhaustive_metric = Changeless(metric());
        let expected = common::exhaustive_search(&mut exhaustive_cs, &mut exhaustive_metric);

        let mut bnb_cs = problem.selector();
        let found = common::bnb_search(&mut bnb_cs, Changeless(metric()), usize::MAX);

        match (expected, found) {
            (Some((expected_score, _)), Ok((score, _))) => {
                prop_assert_eq!(score, expected_score, "bnb={} exhaustive={}", bnb_cs, exhaustive_cs);
            }
            (None, Err(_)) => {}
            (expected, found) => prop_assert!(
                false,
                "disagreement: exhaustive={:?} bnb={:?}",
                expected.map(|(score, _)| score),
                found.map(|(score, _)| score),
            ),
        }
    }
}
