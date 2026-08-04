//! Measurement, not production code: what does searching on the *local* ancestor bump model cost
//! in answer quality?
//!
//! Branch and bound ranks candidates on [`CoinSelector::effective_value_of`], which nets off each
//! candidate's *individual* bump. That is additive, and so over-states what a package owes whenever
//! ancestors are shared -- which is what makes it safe to search on, but means the search optimises
//! a slightly wrong objective. This measures the gap against the exactly-priced optimum. Run with:
//!
//! ```text
//! cargo test --release ancestor_search_experiment -- --ignored --nocapture
//! ```
//!
//! # Two approaches this replaced
//!
//! **Banning** candidates with unconfirmed ancestors, so per-candidate figures could stay
//! ancestor-blind. Measured over 300 random instances per row (n=12, k=3, optimum by brute force),
//! sweeping the share of parents already paying the going rate, the ban cost up to 3.07% in answer
//! quality — and unbanning only the candidates that provably never change the bump recovered all
//! but 0.03% of it:
//!
//! ```text
//!  p(fine)   neutral   ban-cost    selective    sel-rds     no-anc
//!       0%        0%      0.03%        0.03%         51        107
//!      25%       25%      0.95%        0.01%         70        116
//!      50%       50%      1.50%        0.01%         75        103
//!      75%       80%      2.57%        0.00%         92        104
//!     100%      100%      3.07%        0.00%        110        110
//! ```
//!
//! **Keeping the exact model everywhere** and repairing `LowestFee::bound` with a `min_bump`
//! floor — the least bump any reachable superset could pay. It prunes almost nothing: 17-23x the
//! search of an ancestor-free problem, and only 1.0-1.34x better than assuming the bump vanishes
//! entirely. Two reasons compound. `LowestFee::bound` prices a greedy *prefix*, i.e. a superset of
//! the node, so the give-back must cover `max_bump - min_bump` rather than `bump - min_bump`. And
//! `min_bump` at the root is always zero, because the empty selection is reachable and pays
//! nothing — so where pruning matters most the whole bump is surrendered. An oracle bound needed
//! 5-6 rounds where the `min_bump` bound needed 500-650, so the objective was easy to search and
//! the bound was the problem; chasing that was not worth it against a prize this small.
//!
//! [`CoinSelector::effective_value_of`]: crate::CoinSelector::effective_value_of

use crate::{
    float::Ordf32, metrics::LowestFee, BnbMetric, Candidate, Cluster, ClusterBuilder, CoinSelector,
    DrainWeights, FeeRate, Target, TargetFee, TargetOutputs,
};
use alloc::vec::Vec;

/// A deterministic xorshift, so a surprising result can be reproduced from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn in_range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo)
    }
}

struct Instance {
    candidates: Vec<Candidate>,
    cluster: Cluster,
    target: Target,
}

fn metric() -> LowestFee {
    LowestFee {
        long_term_feerate: FeeRate::from_sat_per_vb(5.0),
        dust_relay_feerate: FeeRate::from_sat_per_vb(1.0),
        drain_weights: DrainWeights::TR_KEYSPEND,
    }
}

/// A cluster where `p_fine_pct`% of parents already pay at or above the cs.target() feerate (so a miner
/// takes them and they cost nothing), and the rest are stuck below it.
fn instance(rng: &mut Rng, n: usize, k: usize, p_fine_pct: u64) -> Option<Instance> {
    let feerate = FeeRate::from_sat_per_vb(10.0);

    let candidates = (0..n)
        .map(|_| Candidate {
            input_count: 1,
            value: rng.in_range(20_000, 200_000),
            weight: crate::TR_KEYSPEND_TXIN_WEIGHT,
            is_segwit: true,
        })
        .collect::<Vec<_>>();

    // (weight, fee, parent positions); position doubles as the builder id.
    let mut txs: Vec<(u64, u64, Vec<usize>)> = Vec::new();
    let mut spends: Vec<(usize, usize)> = Vec::new();

    let push_tx =
        |txs: &mut Vec<(u64, u64, Vec<usize>)>, rng: &mut Rng, parents: Vec<usize>| -> usize {
            let weight = rng.in_range(400, 1_600) / 4 * 4;
            // Fee as a multiple of what the cs.target() feerate would require: "fine" parents pay 1.0-3.0x
            // and get mined, "stuck" ones pay 0.05-0.8x and need bumping.
            let mult_pct = if rng.in_range(0, 100) < p_fine_pct {
                rng.in_range(100, 300)
            } else {
                rng.in_range(5, 80)
            };
            let fee = (weight / 4) * 10 * mult_pct / 100;
            txs.push((weight, fee, parents));
            txs.len() - 1
        };

    for c in 0..k {
        let tx = match c {
            // Candidate 1 shares candidate 0's parent, so shared-ancestor dedup is in play --
            // exactly where local and exact pricing diverge.
            1 if !spends.is_empty() => spends[0].1,
            // Candidate 2 sits one level deeper, so transitive closure is in play.
            2 => {
                let grandparent = push_tx(&mut txs, rng, alloc::vec![]);
                push_tx(&mut txs, rng, alloc::vec![grandparent])
            }
            _ => push_tx(&mut txs, rng, alloc::vec![]),
        };
        spends.push((c, tx));
    }

    let cluster = {
        let mut builder = ClusterBuilder::new();
        for (id, (weight, fee, parents)) in txs.into_iter().enumerate() {
            builder.tx(id, weight, fee, parents);
        }
        for (candidate, tx_id) in spends {
            builder.spent_by(tx_id, candidate);
        }
        builder.build().ok()?
    };

    Some(Instance {
        candidates,
        cluster,
        target: Target {
            outputs: TargetOutputs {
                value_sum: rng.in_range(100_000, 400_000),
                weight_sum: 200,
                n_outputs: 1,
            },
            fee: TargetFee::from_feerate(feerate),
            max_weight: None,
        },
    })
}

/// An exactly-priced selector: what the caller sees, and what scores are compared on.
fn exact<'a>(inst: &'a Instance) -> CoinSelector<'a> {
    CoinSelector::new(&inst.candidates, inst.target).with_cluster(&inst.cluster)
}

/// The best exactly-priced score over every subset.
fn brute_force(inst: &Instance) -> Option<Ordf32> {
    let mut best: Option<Ordf32> = None;
    let mut m = metric();
    for mask in 0..(1_u32 << inst.candidates.len()) {
        let mut cs = exact(inst);
        for i in 0..inst.candidates.len() {
            if mask & (1 << i) != 0 {
                cs.select(i);
            }
        }
        if let Some(score) = m.score(&cs) {
            if best.map_or(true, |b| score < b) {
                best = Some(score);
            }
        }
    }
    best
}

/// The best exactly-priced score reachable when only `allowed` candidates may be selected.
fn brute_force_allowing(inst: &Instance, allowed: &[usize]) -> Option<Ordf32> {
    let mut best: Option<Ordf32> = None;
    let mut m = metric();
    for mask in 0..(1_u32 << inst.candidates.len()) {
        if (0..inst.candidates.len()).any(|i| mask & (1 << i) != 0 && !allowed.contains(&i)) {
            continue;
        }
        let mut cs = exact(inst);
        for i in 0..inst.candidates.len() {
            if mask & (1 << i) != 0 {
                cs.select(i);
            }
        }
        if let Some(score) = m.score(&cs) {
            if best.map_or(true, |b| score < b) {
                best = Some(score);
            }
        }
    }
    best
}

/// The alternative policy: keep exact pricing everywhere and ban the candidates that actually owe
/// something, leaving the rest selectable. A candidate owes nothing exactly when its unmined
/// ancestor closure is empty, so this needs no subset enumeration -- `individual(c) == 0` decides
/// it.
fn selectable_under_ban(inst: &Instance) -> Vec<usize> {
    (0..inst.candidates.len())
        .filter(|&c| exact(inst).ancestor_bump_fee_of(c) == 0)
        .collect()
}

/// The same candidates with every ancestor bump zeroed — an ancestor-free problem of the same
/// shape, used to separate what *local pricing* costs from what branch and bound costs anyway.
fn without_ancestors(inst: &Instance) -> CoinSelector<'_> {
    CoinSelector::new(&inst.candidates, inst.target)
}

/// Rounds, and how far branch and bound lands from the brute-force optimum, on the ancestor-free
/// problem. Any gap here is the search's own, not local pricing's.
fn baseline(inst: &Instance, max_rounds: usize) -> (usize, f64) {
    let n = inst.candidates.len();
    let rounds = without_ancestors(inst)
        .bnb_solutions(metric())
        .take(max_rounds)
        .count();

    let mut best: Option<Ordf32> = None;
    let mut m = metric();
    for mask in 0..(1_u32 << n) {
        let mut cs = without_ancestors(inst);
        for i in 0..n {
            if mask & (1 << i) != 0 {
                cs.select(i);
            }
        }
        if let Some(score) = m.score(&cs) {
            if best.map_or(true, |b| score < b) {
                best = Some(score);
            }
        }
    }

    let mut cs = without_ancestors(inst);
    let gap = match (cs.run_bnb(metric(), max_rounds), best) {
        (Ok(_), Some(optimum)) => metric()
            .score(&cs)
            .map_or(0.0, |s| (s.0 - optimum.0) as f64 / optimum.0 as f64),
        _ => 0.0,
    };
    (rounds, gap)
}

#[test]
#[ignore = "measurement, not a correctness test; run with --nocapture"]
fn measure_local_search_quality() {
    const N: usize = 12;
    const K: usize = 3;
    const TRIALS: usize = 300;
    const MAX_ROUNDS: usize = 100_000;

    std::println!(
        "n={}, k={}, {} trials per row, brute force = {} subsets\n",
        N,
        K,
        TRIALS,
        1 << N
    );
    std::println!(
        "{:>8} {:>10} {:>10} {:>10} {:>12} {:>10}",
        "p(fine)",
        "local-gap",
        "ban-gap",
        "ban-fails",
        "suboptimal",
        "base-gap"
    );

    for p_fine in [0_u64, 25, 50, 75, 100] {
        let mut rng = Rng(0xD1B54A32D192ED03 ^ (p_fine + 1) << 32);
        let mut trials = 0;
        let mut gap = 0f64;
        let mut suboptimal = 0;
        let (mut bnb_rounds, mut plain_rounds) = (0usize, 0usize);
        let mut base_gap = 0f64;
        let mut ban_gap = 0f64;
        let mut ban_fails = 0;

        while trials < TRIALS {
            let inst = match instance(&mut rng, N, K, p_fine) {
                Some(inst) => inst,
                None => continue,
            };
            let optimum = match brute_force(&inst) {
                Some(optimum) => optimum,
                None => continue,
            };
            trials += 1;

            bnb_rounds += exact(&inst)
                .bnb_solutions(metric())
                .take(MAX_ROUNDS)
                .count();
            let (rounds, gap_without) = baseline(&inst, MAX_ROUNDS);
            plain_rounds += rounds;
            base_gap += gap_without;

            // The alternative policy: what can it reach, and can it fund at all?
            match brute_force_allowing(&inst, &selectable_under_ban(&inst)) {
                Some(banned_opt) => ban_gap += (banned_opt.0 - optimum.0) as f64 / optimum.0 as f64,
                // A funding selection exists, but not one this policy may reach: automatic
                // selection would report insufficient funds. Not a quality loss -- a failure.
                None => ban_fails += 1,
            }

            let mut cs = exact(&inst);
            if cs.run_bnb(metric(), MAX_ROUNDS).is_ok() {
                // Re-score what branch and bound chose against the *exact* model. Its own score is
                // the local one, which is a search artifact rather than what the caller pays.
                if let Some(score) = metric().score(&cs) {
                    gap += (score.0 - optimum.0) as f64 / optimum.0 as f64;
                    if score > optimum {
                        suboptimal += 1;
                    }
                }
            }
        }

        let t = trials as f64;
        std::println!(
            "{:>7}% {:>9.3}% {:>9.3}% {:>9.1}% {:>11.1}% {:>9.3}%",
            p_fine,
            100.0 * gap / t,
            100.0 * ban_gap / t,
            100.0 * ban_fails as f64 / t,
            100.0 * suboptimal as f64 / t,
            100.0 * base_gap / t,
        );
        let _ = (bnb_rounds, plain_rounds);
    }

    std::println!(
        "\np(fine): share of parents already paying the cs.target() rate. local-gap: how much worse branch\n\
         and bound's answer is than the exactly-priced optimum. base-gap: the same measured on\n\
         the ancestor-free problem, i.e. the search's own error rather than local pricing's.\n\
         suboptimal: share of instances where it is worse at all. bnb-rds/no-anc: mean rounds, and the same instance with the\n\
         ancestor bumps zeroed."
    );
}

/// The case the sweep above cannot reach: a wallet where *most* candidates carry unconfirmed
/// ancestors, so banning them removes most of the pool. Run at `p(fine)=0`, where every parent is
/// stuck and every ancestor-carrying candidate is therefore banned — the worst case for that
/// policy, and the one that decides whether banning is viable at all.
#[test]
#[ignore = "measurement, not a correctness test; run with --nocapture"]
fn measure_when_most_candidates_are_unconfirmed() {
    const N: usize = 12;
    const TRIALS: usize = 300;
    const MAX_ROUNDS: usize = 100_000;

    std::println!("n={}, p(fine)=0, {} trials per row\n", N, TRIALS);
    std::println!(
        "{:>3} {:>10} {:>10} {:>11} {:>12}",
        "k",
        "local-gap",
        "ban-gap",
        "ban-fails",
        "suboptimal"
    );

    for k in [1_usize, 2, 4, 6, 8, 10, 12] {
        let mut rng = Rng(0x853C49E6748FEA9B ^ (k as u64) << 32);
        let mut trials = 0;
        let (mut gap, mut ban_gap) = (0f64, 0f64);
        let (mut ban_fails, mut suboptimal) = (0, 0);

        while trials < TRIALS {
            let inst = match instance(&mut rng, N, k, 0) {
                Some(inst) => inst,
                None => continue,
            };
            let optimum = match brute_force(&inst) {
                Some(optimum) => optimum,
                None => continue,
            };
            trials += 1;

            match brute_force_allowing(&inst, &selectable_under_ban(&inst)) {
                Some(banned_opt) => ban_gap += (banned_opt.0 - optimum.0) as f64 / optimum.0 as f64,
                None => ban_fails += 1,
            }

            let mut cs = exact(&inst);
            if cs.run_bnb(metric(), MAX_ROUNDS).is_ok() {
                if let Some(score) = metric().score(&cs) {
                    gap += (score.0 - optimum.0) as f64 / optimum.0 as f64;
                    if score > optimum {
                        suboptimal += 1;
                    }
                }
            }
        }

        let t = trials as f64;
        std::println!(
            "{:>3} {:>9.3}% {:>9.3}% {:>10.1}% {:>11.1}%",
            k,
            100.0 * gap / t,
            100.0 * ban_gap / t,
            100.0 * ban_fails as f64 / t,
            100.0 * suboptimal as f64 / t,
        );
    }

    std::println!(
        "\nk: how many of the {} candidates carry unconfirmed ancestors. ban-fails: share of\n\
         instances where a funding selection exists but the banning policy cannot reach one --\n\
         automatic selection would report insufficient funds.",
        N
    );
}

/// Diagnostic: is `LowestFee::bound` actually a lower bound once candidates carry ancestor bumps?
///
/// At `k = 1` there is no ancestor sharing, so the local and exact models coincide and branch and
/// bound should find the optimum outright -- `base-gap` is zero. Any gap there has to come from the
/// bound over-estimating and pruning the optimal branch, so check it against an oracle: the exact
/// minimum score over every selection still reachable from each node.
#[test]
#[ignore = "diagnostic; run with --nocapture"]
fn diagnose_bound_admissibility() {
    const N: usize = 12;
    const TRIALS: usize = 400;

    struct Checked<'a> {
        inner: LowestFee,
        n: usize,
        violations: &'a mut usize,
        worst: &'a mut f32,
        calls: &'a mut usize,
        /// Violations split by which branch of `LowestFee::bound` produced them.
        funded_branch: &'a mut usize,
        prefix_branch: &'a mut usize,
        /// Of the funded-branch violations, how many had a bumped candidate already selected.
        with_bumped_selected: &'a mut usize,
        /// Violations where the greedy prefix happened to land on the cs.target() exactly, which
        /// `LowestFee::bound` short-circuits by returning the prefix's own score.
        exact_prefix: &'a mut usize,
    }

    impl BnbMetric for Checked<'_> {
        fn drain(&mut self, cs: &CoinSelector<'_>) -> crate::Drain {
            self.inner.drain(cs)
        }
        fn score(&mut self, cs: &CoinSelector<'_>) -> Option<Ordf32> {
            self.inner.score(cs)
        }
        fn requires_ordering_by_descending_value_pwu(&self) -> bool {
            self.inner.requires_ordering_by_descending_value_pwu()
        }
        fn bound(&mut self, cs: &CoinSelector<'_>) -> Option<Ordf32> {
            let bound = self.inner.bound(cs)?;
            *self.calls += 1;

            // Exact minimum over everything reachable from here, in the same pricing model the
            // metric is being run in.
            let free = (0..self.n)
                .filter(|&i| !cs.is_selected(i) && !cs.banned().contains(i))
                .collect::<Vec<_>>();
            let mut best: Option<Ordf32> = None;
            for sub in 0..(1_u32 << free.len()) {
                let mut t = cs.clone();
                for (j, &i) in free.iter().enumerate() {
                    if sub & (1 << j) != 0 {
                        t.select(i);
                    }
                }
                if let Some(s) = self.inner.score(&t) {
                    if best.map_or(true, |b| s < b) {
                        best = Some(s);
                    }
                }
            }

            if let Some(oracle) = best {
                if bound > oracle {
                    *self.violations += 1;
                    if cs.is_funded() {
                        *self.funded_branch += 1;
                    } else {
                        *self.prefix_branch += 1;
                    }
                    if (0..self.n).any(|i| cs.is_selected(i) && cs.ancestor_bump_fee_of(i) > 0) {
                        *self.with_bumped_selected += 1;
                    }
                    // Re-walk the greedy prefix to see whether it hit the cs.target() dead on.
                    if let Some((prefix, _, _)) =
                        cs.clone().select_iter().find(|(c, _, _)| c.is_funded())
                    {
                        if prefix.excess(crate::Drain::NONE) == 0 {
                            *self.exact_prefix += 1;
                        }
                    }
                    let over = (bound.0 - oracle.0) / oracle.0;
                    if over > *self.worst {
                        *self.worst = over;
                    }
                }
            }
            Some(bound)
        }
    }

    // `bumps = false` zeroes every ancestor bump, leaving an ancestor-free problem of the same
    // shape. If the bound over-estimates there too, this is not something ancestors introduced.
    for (k, bumps) in [(1_usize, false), (1, true), (3, false), (3, true)] {
        let mut rng = Rng(0x27D4EB2F165667C5 ^ (k as u64) << 32);
        let (mut violations, mut calls, mut worst) = (0usize, 0usize, 0f32);
        let (mut funded_branch, mut prefix_branch, mut with_bumped) = (0usize, 0usize, 0usize);
        let mut exact_prefix = 0usize;
        let mut trials = 0;

        while trials < TRIALS {
            let inst = match instance(&mut rng, N, k, 0) {
                Some(inst) => inst,
                None => continue,
            };
            if brute_force(&inst).is_none() {
                continue;
            }
            trials += 1;

            let cs = if bumps {
                exact(&inst)
            } else {
                without_ancestors(&inst)
            };

            let checked = Checked {
                inner: metric(),
                n: N,
                violations: &mut violations,
                worst: &mut worst,
                calls: &mut calls,
                funded_branch: &mut funded_branch,
                prefix_branch: &mut prefix_branch,
                with_bumped_selected: &mut with_bumped,
                exact_prefix: &mut exact_prefix,
            };
            let _ = cs.bnb_solutions(checked).take(100_000).count();
        }

        std::println!(
            "k={} bumps={:<5}: {:>6} calls, {:>4} over-estimates ({:.2}%), worst {:>6.1}% \
             | funded {:>4}, prefix {:>4}, bumped-selected {:>4}, exact-prefix {:>4}",
            k,
            bumps,
            calls,
            violations,
            100.0 * violations as f64 / calls.max(1) as f64,
            100.0 * worst,
            funded_branch,
            prefix_branch,
            with_bumped,
            exact_prefix,
        );
    }
}
