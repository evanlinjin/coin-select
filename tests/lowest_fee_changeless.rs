mod common;
use bdk_coin_select::{
    float::Ordf32, metrics::LowestFeeChangeless, BnbMetric, Candidate, DrainWeights, FeeRate,
    SelectionProblem, Target, TargetFee, TargetOutputs,
};
#[cfg(not(debug_assertions))]
use proptest::{prelude::*, proptest, test_runner::*};
#[cfg(not(debug_assertions))]
use rand::{Rng, RngCore};

#[test]
fn funded_changeful_branch_is_bounded_by_its_no_change_fee() {
    let target = Target {
        outputs: TargetOutputs {
            n_outputs: 1,
            value_sum: 100_000,
            weight_sum: 100,
        },
        fee: TargetFee::from_feerate(FeeRate::from_sat_per_vb(1.0)),
        max_weight: None,
    };
    let problem = SelectionProblem::new_no_ancestors(
        target,
        [Candidate {
            value: 110_000,
            weight: 200,
            segwit_count: 1,
            legacy_count: 0,
        }],
    );
    let mut selector = problem.selector();
    selector.select(0);
    let mut metric = LowestFeeChangeless {
        long_term_feerate: FeeRate::ZERO,
        dust_relay_feerate: FeeRate::ZERO,
        drain_weights: DrainWeights {
            output_weight: 100,
            spend_weight: 100,
            n_outputs: 1,
        },
    };
    let view = selector.compute_view();

    assert!(metric.score(&view).is_none(), "this selection wants change");
    assert_eq!(metric.bound(&view), Some(Ordf32(10_000.0)));
}

#[test]
fn mixed_serialization_overhead_does_not_prune_exact_solution() {
    let target = Target {
        outputs: TargetOutputs {
            n_outputs: 0,
            value_sum: 1_000,
            weight_sum: 0,
        },
        fee: TargetFee::from_feerate(FeeRate::from_sat_per_vb(4.0)),
        max_weight: None,
    };
    let candidates = [
        Candidate {
            value: 1_201,
            weight: 158,
            segwit_count: 1,
            legacy_count: 0,
        },
        Candidate {
            value: 167,
            weight: 164,
            segwit_count: 0,
            legacy_count: 3,
        },
    ];
    let problem = SelectionProblem::new_no_ancestors(target, candidates);
    let mut selector = problem.selector();
    let metric = LowestFeeChangeless {
        long_term_feerate: FeeRate::ZERO,
        dust_relay_feerate: FeeRate::ZERO,
        drain_weights: DrainWeights::NONE,
    };

    let mut expected = problem.selector();
    expected.select_all();
    assert_eq!(expected.excess(bdk_coin_select::Drain::NONE), 0);
    assert!(metric.clone().score(&expected.compute_view()).is_some());

    selector.run_bnb(metric, 100).expect("exact solution");
    assert_eq!(
        selector.selected_indices().iter().collect::<Vec<_>>(),
        [0, 1]
    );
    assert_eq!(selector.excess(bdk_coin_select::Drain::NONE), 0);
}

#[cfg(not(debug_assertions))]
fn test_wv(mut rng: impl RngCore) -> impl Iterator<Item = Candidate> {
    core::iter::repeat_with(move || {
        let value = rng.random_range(0..1_000);
        Candidate {
            value,
            weight: rng.random_range(0..100),
            segwit_count: rng.random_range(1..2),
            legacy_count: 0,
        }
    })
}

#[cfg(not(debug_assertions))]
proptest! {
    #![proptest_config(ProptestConfig::default())]

    #[test]
    fn compare_against_benchmarks(
        n_candidates in 0..15_usize,        // candidates (n)
        target_value in 500..1_000_000_u64,   // target value (sats)
        n_target_outputs in 1..150_usize,    // the number of outputs we're funding
        target_weight in 0..10_000_u32,         // the sum of the weight of the outputs (wu)
        replace in common::maybe_replace(0..10_000u64), // The weight of the transaction we're replacing
        feerate in 1.0..100.0_f32,          // feerate (sats/vb)
        drain_weight in 100..=500_u32,      // drain weight (wu)
        drain_spend_weight in 1..=2000_u32, // drain spend weight (wu)
        n_drain_outputs in 1..150usize,     // the number of drain outputs
    ) {
        println!("=======================================");
        let mut rng = TestRng::deterministic_rng(RngAlgorithm::ChaCha);
        let feerate = FeeRate::from_sat_per_vb(feerate);
        let drain_weights = DrainWeights {
            output_weight: drain_weight as u64,
            spend_weight: drain_spend_weight as u64,
            n_outputs: n_drain_outputs,
        };

        let wv = test_wv(&mut rng);
        let candidates = wv.take(n_candidates).collect::<Vec<_>>();


        let target = Target {
            outputs: TargetOutputs {
                n_outputs: n_target_outputs,
                value_sum: target_value,
                weight_sum: target_weight as u64,
            },
            fee: TargetFee {
                rate: feerate,
                replace,
                ..TargetFee::ZERO
            },
            max_weight: None,
        };
        let problem = SelectionProblem::new_no_ancestors(target, candidates.iter().copied());
        let make_metric = || {
            LowestFeeChangeless {
                long_term_feerate: feerate,
                dust_relay_feerate: FeeRate::from_sat_per_vb(1.0),
                drain_weights,
            }
        };

        let mut exhaustive_cs = problem.selector();
        let mut exhaustive_metric = make_metric();
        let expected = common::exhaustive_search(&mut exhaustive_cs, &mut exhaustive_metric);

        let mut bnb_cs = problem.selector();
        let found = common::bnb_search(&mut bnb_cs, make_metric(), usize::MAX);

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
