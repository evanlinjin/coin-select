use bdk_coin_select::{
    Candidate, Cluster, ClusterBuilder, ClusterError, CoinSelector, FeeRate, Target, TargetFee,
    TargetOutputs, TR_KEYSPEND_TXIN_WEIGHT,
};

const RATE: f32 = 10.0;

fn rate() -> FeeRate {
    FeeRate::from_sat_per_vb(RATE)
}

fn candidates(n: usize) -> Vec<Candidate> {
    (0..n)
        .map(|_| Candidate {
            input_count: 1,
            value: 200_000,
            weight: TR_KEYSPEND_TXIN_WEIGHT,
            is_segwit: true,
        })
        .collect()
}

/// A modest target these fixtures can all fund; the pricing assertions do not depend on it.
fn target() -> Target {
    Target {
        outputs: TargetOutputs {
            value_sum: 100_000,
            weight_sum: 200,
            n_outputs: 1,
        },
        fee: TargetFee::from_feerate(rate()),
        max_weight: None,
    }
}

/// (weight, fee, parent positions) — fed to the builder with the position as the id.
type Tx = (u64, u64, Vec<usize>);

fn tx(weight: u64, fee: u64, parents: Vec<usize>) -> Tx {
    (weight, fee, parents)
}

fn try_cluster(txs: Vec<Tx>, spends: Vec<(usize, usize)>) -> Result<Cluster, ClusterError<usize>> {
    let mut builder = ClusterBuilder::new();
    for (id, (weight, fee, parents)) in txs.into_iter().enumerate() {
        builder.tx(id, weight, fee, parents);
    }
    for (candidate, tx_id) in spends {
        builder.spent_by(tx_id, candidate);
    }
    builder.build()
}

/// The bump a selection of `selection` owes under `cluster`, at the fixture target's feerate.
fn bump(cluster: &Cluster, n_candidates: usize, selection: &[usize]) -> u64 {
    let candidates = candidates(n_candidates);
    let mut cs = CoinSelector::new(&candidates, target()).with_cluster(cluster);
    for &i in selection {
        cs.select(i);
    }
    cs.selected_ancestor_bump_fee()
}

/// A deficient parent with nothing to carry it has to be bumped, in full.
#[test]
fn a_deficient_ancestor_alone_is_charged_in_full() {
    // 1000 wu = 250 vB. At 10 sat/vB it owes 2500 but paid 500.
    let cluster = try_cluster(vec![tx(1_000, 500, vec![])], vec![(0, 0)]).unwrap();

    assert_eq!(bump(&cluster, 1, &[0]), 2_000);
}

/// A deficient parent already carried by an overpaying child that we are *not* spending. A miner
/// includes both, so the parent needs no bump — and only a cluster can show that, since the child
/// is nowhere in this candidate's ancestry.
#[test]
fn a_parent_carried_by_someone_elses_child_needs_no_bump() {
    let cluster = try_cluster(
        vec![
            tx(1_000, 500, vec![]),  // 0: parent, 250 vB, owes 2500, paid 500
            tx(400, 4_000, vec![0]), // 1: its child, 100 vB, pays 4000
        ],
        // We spend the *parent*. The child is someone else's, or another of ours.
        vec![(0, 0)],
    )
    .unwrap();

    // Package {0,1} is 350 vB paying 4500 against 3500 owed, so a miner takes both.
    assert_eq!(
        bump(&cluster, 1, &[0]),
        0,
        "the child already pays for the parent"
    );
}

/// Transitive closure is computed from the graph, so spending a grandchild pulls in the
/// grandparent without the caller having to say so.
#[test]
fn transitive_ancestors_are_pulled_in_automatically() {
    let cluster = try_cluster(
        vec![
            tx(400, 0, vec![]),  // 0: grandparent, 100 vB, pays nothing
            tx(400, 0, vec![0]), // 1: parent
            tx(400, 0, vec![1]), // 2: the tx we spend
        ],
        vec![(0, 2)],
    )
    .unwrap();

    // All three are unmined: 1200 wu = 300 vB, owes 3000, paid 0.
    assert_eq!(bump(&cluster, 1, &[0]), 3_000);
}

/// A transaction shared by two selected candidates is paid for once.
#[test]
fn a_shared_ancestor_is_charged_once() {
    let cluster = try_cluster(
        vec![
            tx(400, 0, vec![]),  // 0: shared parent
            tx(400, 0, vec![0]), // 1: spent by candidate 0
            tx(400, 0, vec![0]), // 2: spent by candidate 1
        ],
        vec![(0, 1), (1, 2)],
    )
    .unwrap();

    // One candidate: parent + its own tx = 800 wu = 200 vB => 2000.
    assert_eq!(bump(&cluster, 2, &[0]), 2_000);
    assert_eq!(bump(&cluster, 2, &[1]), 2_000);
    // Both: parent counted once => 1200 wu = 300 vB => 3000, not 4000.
    assert_eq!(bump(&cluster, 2, &[0, 1]), 3_000);
}

/// A cluster already paying above the target is mined entirely, so nothing is owed.
#[test]
fn a_cluster_above_the_target_owes_nothing() {
    let cluster = try_cluster(
        vec![tx(400, 10_000, vec![]), tx(400, 10_000, vec![0])],
        vec![(0, 1)],
    )
    .unwrap();

    assert_eq!(bump(&cluster, 1, &[0]), 0);
    assert_eq!(
        bump(&cluster, 1, &[]),
        0,
        "the empty subset always owes zero"
    );
}

/// Mining is package-wise, so a deficient parent is carried by an overpaying descendant we *are*
/// spending — the whole chain comes in together or not at all.
#[test]
fn an_overpaying_descendant_carries_its_deficient_parent() {
    let cluster = try_cluster(
        vec![
            tx(1_000, 500, vec![]),  // 250 vB, owes 2500, paid 500
            tx(400, 4_000, vec![0]), // 100 vB, pays 4000
        ],
        vec![(0, 1)], // we spend the child
    )
    .unwrap();

    assert_eq!(bump(&cluster, 1, &[0]), 0);
}

#[test]
fn cluster_rejects_malformed_input() {
    assert_eq!(
        try_cluster(vec![tx(400, 0, vec![7])], vec![]).unwrap_err(),
        ClusterError::UnknownParent {
            child: 0,
            parent: 7
        }
    );
    assert_eq!(
        try_cluster(vec![tx(400, 0, vec![])], vec![(0, 3)]).unwrap_err(),
        ClusterError::UnknownSpend {
            candidate: 0,
            tx: 3
        }
    );
    assert!(matches!(
        try_cluster(vec![tx(400, 0, vec![1]), tx(400, 0, vec![0])], vec![(0, 0)]),
        Err(ClusterError::Cycle { .. })
    ));

    let mut duplicated = ClusterBuilder::new();
    duplicated.tx("a", 400, 0, []);
    duplicated.tx("a", 500, 0, []);
    assert_eq!(
        duplicated.build().unwrap_err(),
        ClusterError::DuplicateTx { tx: "a" }
    );
}

/// The builder is keyed by the caller's own ids — insertion order does not matter, a child may
/// name a parent that arrives later, and errors come back in the caller's vocabulary.
#[test]
fn builder_accepts_ids_in_any_order() {
    let mut builder = ClusterBuilder::new();
    builder.tx("child", 400, 4_000, ["parent"]); // parent not added yet
    builder.tx("parent", 1_000, 500, []);
    builder.spent_by("child", 0);
    let cluster = builder.build().expect("well-formed");

    // Package {parent, child}: 350 vB paying 4500 against 3500 owed => mined, nothing to bump.
    assert_eq!(bump(&cluster, 1, &[0]), 0);
}

/// Selecting a bump-neutral candidate really does leave the package price alone -- which is the
/// property that makes leaving it unbanned safe.
#[test]
fn selecting_a_bump_neutral_candidate_does_not_move_the_price() {
    let cluster = try_cluster(
        vec![tx(400, 2_000, vec![]), tx(400, 10, vec![])],
        vec![(0, 0), (1, 1)],
    )
    .unwrap();

    // Neutral candidate alone, and added on top of the stuck one: neither changes anything.
    assert_eq!(bump(&cluster, 2, &[]), 0);
    assert_eq!(bump(&cluster, 2, &[0]), 0);
    assert_eq!(bump(&cluster, 2, &[1]), 990);
    assert_eq!(bump(&cluster, 2, &[0, 1]), 990);
}

/// **The inequality the whole design rests on.** Branch and bound searches on the sum of
/// per-candidate bumps while the crate reports the combined package figure. That is only safe if
/// the sum is never *below* the combined figure — otherwise the search would under-reserve and the
/// transaction would come up short. A mock template guarantees it: an ancestor that overpays gets
/// mined out, so whatever two candidates share is itself ancestor-closed and therefore deficient.
#[test]
fn the_local_sum_never_undercuts_the_package_for_any_cluster() {
    // Deterministic xorshift, so a counterexample is reproducible from its seed.
    let mut state = 0x2545F4914F6CDD1D_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for trial in 0..2_000 {
        let n_txs = 1 + (next() % 6) as usize;
        let mut txs = Vec::new();
        for i in 0..n_txs {
            // Parents are always earlier transactions, so the graph is acyclic by construction.
            let mut parents = Vec::new();
            for p in 0..i {
                if next() % 3 == 0 {
                    parents.push(p);
                }
            }
            let weight = 400 + (next() % 1_200) / 4 * 4;
            // Fees straddle the target rate so both mined and stuck transactions occur.
            let fee = (weight / 4) * (next() % 30);
            txs.push(tx(weight, fee, parents));
        }

        let n_candidates = 1 + (next() % 4) as usize;
        let spends: Vec<(usize, usize)> = (0..n_candidates)
            .map(|c| (c, (next() % n_txs as u64) as usize))
            .collect();

        let cluster = try_cluster(txs, spends).expect("acyclic by construction");
        let cands = candidates(n_candidates);
        let cs = CoinSelector::new(&cands, target()).with_cluster(&cluster);

        for mask in 0..(1_u32 << n_candidates) {
            let selection: Vec<usize> =
                (0..n_candidates).filter(|b| mask & (1 << b) != 0).collect();
            let local: u64 = selection.iter().map(|&i| cs.ancestor_bump_fee_of(i)).sum();
            let combined = bump(&cluster, n_candidates, &selection);
            assert!(
                local >= combined,
                "trial {}: local sum {} undercuts the package {} for selection {:?}",
                trial,
                local,
                combined,
                selection
            );
        }
    }
}

/// Branch and bound may now choose candidates with unconfirmed ancestors, so the selection it
/// returns has to be genuinely funded once priced exactly — the search reasons about the local
/// over-estimate, and the difference must land in the change output rather than in a shortfall.
#[test]
fn branch_and_bound_selections_are_funded_when_priced_exactly() {
    use bdk_coin_select::{metrics::LowestFee, DrainWeights, Target, TargetFee, TargetOutputs};

    let cluster = try_cluster(
        vec![
            tx(1_000, 500, vec![]), // stuck parent, shared by candidates 0 and 1
            tx(400, 10, vec![0]),   // stuck child
            tx(400, 4_000, vec![]), // already paying
        ],
        vec![(0, 0), (1, 1), (2, 2)],
    )
    .unwrap();
    let cands = candidates(4);

    let target = Target {
        outputs: TargetOutputs {
            value_sum: 300_000,
            weight_sum: 200,
            n_outputs: 1,
        },
        fee: TargetFee::from_feerate(rate()),
        max_weight: None,
    };
    let metric = LowestFee {
        long_term_feerate: FeeRate::from_sat_per_vb(5.0),
        dust_relay_feerate: FeeRate::from_sat_per_vb(1.0),
        drain_weights: DrainWeights::TR_KEYSPEND,
    };

    let mut cs = CoinSelector::new(&cands, target).with_cluster(&cluster);
    let (_, drain) = cs.run_bnb(metric, 100_000).expect("a solution exists");

    assert!(
        cs.is_funded_with_drain(drain),
        "the returned selection must fund the target under exact pricing"
    );
    assert!(
        cs.is_selected(0) || cs.is_selected(1) || cs.is_selected(2) || cs.is_selected(3),
        "something was selected"
    );
    // The drain handed back is sized by the exact package cost, not the search's over-estimate.
    assert_eq!(
        drain.value,
        cs.drain_value(bdk_coin_select::ChangePolicy {
            min_value: 0,
            drain_weights: DrainWeights::TR_KEYSPEND,
        })
        .unwrap_or(0),
        "run_bnb's drain must match the exactly-priced drain value"
    );
}
