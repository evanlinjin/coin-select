use bdk_coin_select::{
    Candidate, Cluster, ClusterBuilder, CoinSelector, Drain, DrainWeights, FeeRate, Replace,
    Target, TargetFee, TargetOutputs, TR_KEYSPEND_TXIN_WEIGHT,
};

fn simple_target(feerate: f32) -> Target {
    Target {
        outputs: TargetOutputs {
            value_sum: 100_000,
            weight_sum: 200,
            n_outputs: 1,
        },
        fee: TargetFee::from_feerate(FeeRate::from_sat_per_vb(feerate)),
        max_weight: None,
    }
}

/// (weight, fee, parent positions) — fed to the builder with the position as the id.
type Tx = (u64, u64, Vec<usize>);

fn tx(weight: u64, fee: u64, parents: Vec<usize>) -> Tx {
    (weight, fee, parents)
}

fn cluster(txs: Vec<Tx>, spends: Vec<(usize, usize)>) -> Cluster {
    let mut builder = ClusterBuilder::new();
    for (id, (weight, fee, parents)) in txs.into_iter().enumerate() {
        builder.tx(id, weight, fee, parents);
    }
    for (candidate, tx_id) in spends {
        builder.spent_by(tx_id, candidate);
    }
    builder.build().expect("well-formed")
}

/// One transaction paying far too little, spent by candidate 0: 400 wu = 100 vB, so at 10 sat/vB
/// it owes 1000 but paid 10 => a 990 bump.
fn one_stuck_parent() -> Cluster {
    cluster(vec![tx(400, 10, vec![])], vec![(0, 0)])
}

fn candidates(n: usize, value: u64) -> Vec<Candidate> {
    (0..n)
        .map(|_| Candidate {
            input_count: 1,
            value,
            weight: TR_KEYSPEND_TXIN_WEIGHT,
            is_segwit: true,
        })
        .collect()
}

/// Candidates for a selector that will carry a table. Nothing is written into them: the bump lives
/// on the table, and the selector nets it off.
fn priced(n: usize, value: u64) -> Vec<Candidate> {
    candidates(n, value)
}

#[test]
fn zero_ancestors_backward_compatible() {
    let candidates = candidates(1, 200_000);

    let mut cs = CoinSelector::new(&candidates, simple_target(10.0));
    cs.select(0);

    assert_eq!(cs.selected_ancestor_bump_fee(), 0);
    assert!(
        cs.excess(Drain::NONE) > 0,
        "should meet target without ancestors"
    );
}

#[test]
fn single_ancestor_reduces_excess() {
    let cluster = one_stuck_parent();
    let target = simple_target(10.0);

    let plain = candidates(1, 200_000);
    let mut cs_no_anc = CoinSelector::new(&plain, target);
    cs_no_anc.select(0);
    let excess_no_anc = cs_no_anc.excess(Drain::NONE);

    let with_ancestors = priced(1, 200_000);
    let mut cs = CoinSelector::new(&with_ancestors, target).with_cluster(&cluster);
    cs.select(0);

    assert_eq!(cs.selected_ancestor_bump_fee(), 990);
    assert_eq!(
        excess_no_anc - cs.excess(Drain::NONE),
        990,
        "the bump comes straight off the excess"
    );
}

#[test]
fn shared_ancestors_are_deduplicated() {
    let cluster = cluster(vec![tx(400, 10, vec![])], vec![(0, 0), (1, 0)]);
    let candidates = priced(2, 100_000);

    let mut cs_one = CoinSelector::new(&candidates, simple_target(10.0)).with_cluster(&cluster);
    cs_one.select(0);

    let mut cs_both = CoinSelector::new(&candidates, simple_target(10.0)).with_cluster(&cluster);
    cs_both.select(0);
    cs_both.select(1);

    assert_eq!(
        cs_one.selected_ancestor_bump_fee(),
        cs_both.selected_ancestor_bump_fee(),
        "a shared ancestor is paid for once, however many dependents are selected"
    );
}

/// The sum of per-candidate bumps is what branch and bound searches on, and it must never come out
/// below the package figure the transaction pays — otherwise the search would under-reserve.
#[test]
fn the_local_sum_never_undercuts_the_package() {
    let cluster = cluster(vec![tx(400, 10, vec![])], vec![(0, 0), (1, 0)]);
    let candidates = priced(2, 100_000);

    let mut cs = CoinSelector::new(&candidates, simple_target(10.0)).with_cluster(&cluster);
    cs.select(0);
    cs.select(1);

    let local: u64 = (0..candidates.len())
        .map(|i| cs.ancestor_bump_fee_of(i))
        .sum();
    let combined = cs.selected_ancestor_bump_fee();

    assert_eq!(combined, 990, "the shared ancestor is charged once");
    assert_eq!(local, 1_980, "but each candidate is charged for it alone");
    assert!(local >= combined, "the search must over-reserve, not under");
}

#[test]
fn ancestor_package_above_target_contributes_zero_bump() {
    let cluster = cluster(vec![tx(400, 10_000, vec![])], vec![(0, 0)]);
    let candidates = priced(1, 200_000);

    let mut cs = CoinSelector::new(&candidates, simple_target(10.0)).with_cluster(&cluster);
    cs.select(0);

    assert_eq!(cs.selected_ancestor_bump_fee(), 0);
}

/// The bump is priced at the selector's own target feerate — there is no separate rate to pass,
/// and so no way to price the package at a rate other than the one being aimed for.
#[test]
fn different_feerates_produce_different_bump_fees() {
    let bump_at = |feerate: f32| {
        let cluster = cluster(vec![tx(400, 100, vec![])], vec![(0, 0)]);
        let candidates = priced(1, 200_000);
        let mut cs = CoinSelector::new(&candidates, simple_target(feerate)).with_cluster(&cluster);
        cs.select(0);
        cs.selected_ancestor_bump_fee()
    };

    assert!(
        bump_at(20.0) > bump_at(5.0),
        "a higher feerate owes more of a bump"
    );
}

/// The figure the selection algorithms rank on has to tell the whole truth about what a candidate
/// costs. `Candidate` cannot supply it — a bump only means something at one feerate, and a
/// `Candidate` has nowhere to record which — so it comes from the selector.
#[test]
fn the_selectors_effective_value_includes_the_bump() {
    let cluster = one_stuck_parent();
    let feerate = FeeRate::from_sat_per_vb(10.0);
    let candidates = candidates(1, 200_000);

    let plain = CoinSelector::new(&candidates, simple_target(10.0));
    let with_ancestors = CoinSelector::new(&candidates, simple_target(10.0)).with_cluster(&cluster);

    assert_eq!(with_ancestors.ancestor_bump_fee_of(0), 990);
    assert_eq!(plain.ancestor_bump_fee_of(0), 0, "no table, nothing owed");
    assert_eq!(
        plain.effective_value_of(0, feerate) - with_ancestors.effective_value_of(0, feerate),
        990.0,
        "effective value drops by exactly the bump"
    );
    assert!(with_ancestors.value_pwu_of(0) < plain.value_pwu_of(0));
    assert_eq!(
        candidates[0].effective_value(feerate),
        plain.effective_value_of(0, feerate),
        "`Candidate`'s own figure is the ancestor-blind one"
    );
}

/// `implied_fee` is the exact counterpart of `excess`, so it must carry the ancestor bump. A
/// wallet sizing its change output from it would otherwise underpay the package.
#[test]
fn implied_fee_includes_ancestor_bump() {
    let cluster = one_stuck_parent();
    let target = simple_target(10.0);
    let plain = candidates(1, 200_000);
    let with_ancestors = priced(1, 200_000);

    let mut without = CoinSelector::new(&plain, target);
    without.select(0);
    let mut with = CoinSelector::new(&with_ancestors, target).with_cluster(&cluster);
    with.select(0);

    assert_eq!(
        with.implied_fee(DrainWeights::NONE) - without.implied_fee(DrainWeights::NONE),
        990,
        "implied_fee must carry the ancestor bump"
    );
}

/// Nothing is banned: a candidate with unconfirmed ancestors is reachable by the automatic
/// algorithms like any other, because its cost is visible in the figures they rank on.
#[test]
fn ancestor_candidates_are_selectable() {
    let cluster = one_stuck_parent();
    let candidates = priced(1, 200_000);
    let cs = CoinSelector::new(&candidates, simple_target(10.0)).with_cluster(&cluster);

    assert!(cs.banned().is_empty(), "nothing is banned any more");
    assert_eq!(cs.unselected_indices().collect::<Vec<_>>(), vec![0]);
    assert_eq!(cs.candidates_with_ancestors().collect::<Vec<_>>(), vec![0]);

    let mut greedy = cs.clone();
    greedy
        .select_until_target_met()
        .expect("the ancestor candidate covers the target even after its bump");
    assert!(greedy.is_selected(0));
    assert_eq!(greedy.selected_ancestor_bump_fee(), 990);
}

/// `excess == selected_value - target.value() - drain.value - implied_fee` must hold for every
/// combination of fee constraints. This pins the whole `*_excess` family against `implied_fee`, so
/// a bump added to one but not the other cannot go unnoticed.
#[test]
fn excess_and_implied_fee_agree() {
    // (transactions, which candidate spends which) -- no ancestors, one stuck parent, and a stuck
    // parent for candidate 0 alongside an already-paying one for candidate 1.
    type Fixture = (Vec<Tx>, Vec<(usize, usize)>);
    let clusters: [Fixture; 3] = [
        (vec![], vec![]),
        (vec![tx(400, 10, vec![])], vec![(0, 0)]),
        (
            vec![tx(400, 10, vec![]), tx(1_000, 100_000, vec![])],
            vec![(0, 0), (1, 1)],
        ),
    ];

    for (txs, spends) in &clusters {
        for absolute in [0_u64, 5_000, 500_000] {
            for replace in [None, Some(Replace::new(1_000))] {
                for feerate in [1.0_f32, 10.0, 50.0] {
                    let cluster = cluster(txs.clone(), spends.clone());
                    let mut candidates = candidates(2, 200_000);
                    candidates[1].value = 50_000;

                    for drain in [
                        Drain::NONE,
                        Drain {
                            weights: DrainWeights::TR_KEYSPEND,
                            value: 20_000,
                        },
                    ] {
                        let target = Target {
                            outputs: TargetOutputs {
                                value_sum: 100_000,
                                weight_sum: 200,
                                n_outputs: 1,
                            },
                            fee: TargetFee {
                                rate: FeeRate::from_sat_per_vb(feerate),
                                replace,
                                absolute,
                            },
                            max_weight: None,
                        };

                        for selection in [vec![], vec![0], vec![1], vec![0, 1]] {
                            let mut cs =
                                CoinSelector::new(&candidates, target).with_cluster(&cluster);
                            for i in &selection {
                                cs.select(*i);
                            }

                            assert_eq!(
                                cs.excess(drain),
                                cs.selected_value() as i64
                                    - target.value() as i64
                                    - drain.value as i64
                                    - cs.implied_fee(drain.weights) as i64,
                                "identity broken: n_txs={} absolute={} replace={} \
                                 feerate={} drain={} selection={:?}",
                                txs.len(),
                                absolute,
                                replace.is_some(),
                                feerate,
                                drain.value,
                                selection,
                            );
                        }
                    }
                }
            }
        }
    }
}

/// The bump is computed at the target feerate, so per-candidate figures asked at any other rate
/// would mix rates. The methods that take a feerate of their own catch that.
#[test]
#[should_panic(expected = "the ancestor bump is computed at the target feerate")]
fn selecting_all_effective_rejects_a_mismatched_feerate() {
    let cluster = one_stuck_parent();
    let candidates = priced(1, 200_000);

    let mut cs = CoinSelector::new(&candidates, simple_target(10.0)).with_cluster(&cluster);
    cs.select_all_effective(FeeRate::from_sat_per_vb(20.0));
}

/// The same figures at the target's own feerate are fine, and the bump is visible in the ranking.
#[test]
fn selecting_all_effective_works_at_the_target_feerate() {
    let cluster = one_stuck_parent();
    let candidates = priced(1, 200_000);

    let mut cs = CoinSelector::new(&candidates, simple_target(10.0)).with_cluster(&cluster);
    cs.select_all_effective(FeeRate::from_sat_per_vb(10.0));

    assert!(cs.is_selected(0), "still worth its bump at 200_000 sats");
    assert_eq!(cs.selected_ancestor_bump_fee(), 990);
}
