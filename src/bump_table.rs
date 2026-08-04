use crate::{bitset::Bitset, mempool::Cluster, FeeRate};
use alloc::{collections::BTreeMap, vec::Vec};

/// An unconfirmed transaction that may still need bumping, stripped to what pricing needs.
#[derive(Debug, Clone, Copy)]
struct Unit {
    weight: u64,
    fee: u64,
}

/// What spending unconfirmed UTXOs costs, at one fixed feerate.
///
/// Built from a [`Cluster`] — the relevant piece of the mempool as a graph — because pricing CPFP
/// correctly needs to know which transactions a miner would already include. A flat list of
/// ancestors cannot express that, and charges for packages that need no bump at all.
///
/// This answers two different questions, and the difference between them is the whole design:
///
/// - `combined` — what a *set* of candidates owes together, via
///   [`CoinSelector::selected_ancestor_bump_fee`]. Exact: an ancestor
///   two candidates share is paid for once. Every figure this crate reports is built on it, and it
///   is what the transaction actually has to pay.
/// - [`individual`](Self::individual) — what one candidate owes on its own. Additive across
///   candidates, so an over-estimate whenever ancestors are shared, but *local*: it is what
///   [`CoinSelector::effective_value_of`] subtracts, so the figures the selection algorithms rank
///   on tell the whole truth. Those algorithms need locality more than they need accuracy.
///
/// Branch and bound searches on the local figures while the crate reports the combined one — the
/// same split Bitcoin Core draws between `calculateIndividualBumpFees` and
/// `calculateCombinedBumpFee`. Because the combined bump is never larger than the sum of the
/// individual ones, searching on the latter *over*-reserves, so the surplus surfaces as a larger
/// change output rather than going missing from the fee.
///
/// # Feerate
///
/// Both figures are computed for one feerate and are meaningless at any other, since the
/// arithmetic (`feerate * weight - fee_paid`) depends on it. Using one against a mismatched
/// [`Target::fee`] under-prices the package, so [`CoinSelector::selected_ancestor_bump_fee`]
/// checks that they agree.
///
/// [`CoinSelector::effective_value_of`]: crate::CoinSelector::effective_value_of
/// [`CoinSelector::selected_ancestor_bump_fee`]: crate::CoinSelector::selected_ancestor_bump_fee
/// [`Target::fee`]: crate::Target::fee
#[derive(Debug, Clone)]
pub struct BumpTable {
    feerate: FeeRate,
    /// The unconfirmed transactions that may still need bumping: for a cluster, the ones a miner
    /// would leave behind; for a flat ancestor list, all of them.
    units: Vec<Unit>,
    /// Candidate index -> the units its selection drags in, and what it owes alone.
    ///
    /// Sparse — only candidates carrying ancestors — so this stays proportional to the number of
    /// unconfirmed UTXOs rather than to the size of the candidate set. Ascending iteration comes
    /// from the map rather than from construction discipline.
    entries: BTreeMap<usize, (Bitset, u64)>,
}

impl BumpTable {
    /// Build by mining a mock block over `cluster` and charging only for what it leaves behind.
    ///
    /// Transactions a miner would already include at `feerate` are paying their own way and cost
    /// nothing, so a parent already above the target — or one already carried by an overpaying
    /// child in the cluster — correctly contributes zero. Neither is visible to a model that only
    /// knows a flat list of ancestors.
    ///
    /// The template is built once: which transactions a miner includes depends on the cluster and
    /// the feerate, not on which outputs you happen to be asking about.
    pub fn from_cluster(cluster: &Cluster, feerate: FeeRate) -> Self {
        let mined = cluster.mine(feerate);

        // Renumber the survivors so units are dense and the mined transactions simply do not exist.
        let mut unit_of_tx = alloc::vec![usize::MAX; cluster.n_txs()];
        let mut units = Vec::new();
        for (tx, unit) in unit_of_tx.iter_mut().enumerate() {
            if !mined.contains(tx) {
                *unit = units.len();
                units.push(Unit {
                    weight: cluster.tx(tx).weight,
                    fee: cluster.tx(tx).fee,
                });
            }
        }

        let mut entries = BTreeMap::new();
        for candidate in cluster.candidates() {
            let mut drags_in = Bitset::with_capacity(units.len());
            for closure in cluster.package_of(candidate) {
                for tx in closure.iter() {
                    if !mined.contains(tx) {
                        drags_in.insert(unit_of_tx[tx]);
                    }
                }
            }
            let individual = deficit(&drags_in, &units, feerate);
            entries.insert(candidate, (drags_in, individual));
        }

        Self {
            feerate,
            units,
            entries,
        }
    }

    /// The feerate these figures were computed for. They are meaningless at any other.
    pub fn feerate(&self) -> FeeRate {
        self.feerate
    }

    /// The candidates that carry unconfirmed ancestors, ascending.
    pub fn candidates(&self) -> impl Iterator<Item = usize> + '_ {
        self.entries.keys().copied()
    }

    /// What `candidate` owes on its own, ignoring whatever else might be selected.
    ///
    /// Zero for a candidate with no unconfirmed ancestors, or one whose ancestors a miner would
    /// already include. Summing these across a selection over-estimates the combined package
    /// figure whenever ancestors are shared — and that over-estimate is the price of the figure
    /// being additive, which is what the selection algorithms need.
    pub fn individual(&self, candidate: usize) -> u64 {
        self.entries.get(&candidate).map_or(0, |&(_, bump)| bump)
    }

    /// Every candidate's individual bump, ascending by candidate index.
    pub fn individual_bumps(&self) -> impl Iterator<Item = (usize, u64)> + '_ {
        self.entries.iter().map(|(&c, &(_, bump))| (c, bump))
    }

    /// What the candidates in `selected` owe *together*, with shared ancestors counted once.
    ///
    /// This is what the transaction actually has to pay.
    pub(crate) fn combined(&self, selected: &Bitset) -> u64 {
        let mut units = Bitset::with_capacity(self.units.len());
        for (&candidate, (drags_in, _)) in &self.entries {
            if selected.contains(candidate) {
                for unit in drags_in.iter() {
                    units.insert(unit);
                }
            }
        }
        deficit(&units, &self.units, self.feerate)
    }

    /// The largest candidate index this table refers to, or `None` if it refers to none.
    pub(crate) fn max_candidate_index(&self) -> Option<usize> {
        self.entries.keys().next_back().copied()
    }
}

/// What a set of unconfirmed transactions still owes at `feerate`, as a package.
fn deficit(set: &Bitset, units: &[Unit], feerate: FeeRate) -> u64 {
    let (mut weight, mut fee) = (0_u64, 0_u64);
    for i in set.iter() {
        weight += units[i].weight;
        fee += units[i].fee;
    }
    feerate.implied_fee(weight).saturating_sub(fee)
}
