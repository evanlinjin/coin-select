use crate::{bitset::Bitset, FeeRate};
use alloc::{collections::BTreeMap, vec::Vec};

/// An unconfirmed transaction, stripped to what pricing needs. Parents are indices into the
/// cluster's transaction list; the id-to-index resolution happened in [`ClusterBuilder::build`].
#[derive(Debug, Clone)]
pub(crate) struct MempoolTx {
    pub(crate) weight: u64,
    pub(crate) fee: u64,
    /// *Direct* parents only; transitive closures are computed from these.
    pub(crate) parents: Vec<usize>,
}

/// Builds a [`Cluster`] from transactions keyed by the caller's own ids.
///
/// `Id` is whatever the caller already keys transactions by — a txid, a `[u8; 32]`, anything
/// `Ord + Clone`. This crate deliberately has no `bitcoin` dependency, so it never names a txid
/// type; it just resolves the ids to internal indices once, in [`build`](Self::build).
/// Transactions may be added in any order: a child may name a parent that has not been added yet,
/// as long as it is there by `build` time.
///
/// ```
/// # use bdk_coin_select::ClusterBuilder;
/// let mut builder = ClusterBuilder::new();
/// builder.tx("a", 1_000, 500, []); // id, weight (wu), fee paid (sats), parents
/// builder.tx("b", 400, 0, ["a"]);
/// builder.spent_by("b", 3); // candidate 3 spends an output of "b"
/// let cluster = builder.build().expect("well-formed");
/// ```
#[derive(Debug, Clone)]
pub struct ClusterBuilder<Id> {
    /// (id, weight, fee, parent ids), in insertion order.
    txs: Vec<(Id, u64, u64, Vec<Id>)>,
    /// (candidate index, tx id).
    spends: Vec<(usize, Id)>,
}

impl<Id> Default for ClusterBuilder<Id> {
    fn default() -> Self {
        Self {
            txs: Vec::new(),
            spends: Vec::new(),
        }
    }
}

impl<Id: Ord + Clone> ClusterBuilder<Id> {
    /// A builder with no transactions. Building it yields an empty cluster, which prices every
    /// candidate as ancestor-free.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an unconfirmed transaction: its `weight` in weight units, the `fee` it already pays
    /// in satoshis, and the ids of its *direct* in-cluster parents — transitive ancestors are
    /// derived, which is much of the point of supplying a graph rather than a list. Parents
    /// need not have been added yet.
    pub fn tx(&mut self, id: Id, weight: u64, fee: u64, parents: impl IntoIterator<Item = Id>) {
        self.txs
            .push((id, weight, fee, parents.into_iter().collect()));
    }

    /// Record that the candidate at `candidate_index` (into the slice given to
    /// [`CoinSelector::new`]) spends an output of the transaction `id`.
    ///
    /// Both directions are many: a transaction may be spent by several candidates, and a candidate
    /// may appear more than once when it spends outputs of several transactions — its package is
    /// then the union of their ancestor closures.
    ///
    /// [`CoinSelector::new`]: crate::CoinSelector::new
    pub fn spent_by(&mut self, id: Id, candidate_index: usize) {
        self.spends.push((candidate_index, id));
    }

    /// Resolve ids and compute ancestor closures.
    ///
    /// # Errors
    ///
    /// [`ClusterError`], naming the offending ids: a duplicated transaction, a parent or spent
    /// transaction that was never added, or a cycle in the parent relation (real mempool graphs
    /// are acyclic; an id scheme that cycles is a caller bug).
    pub fn build(self) -> Result<Cluster, ClusterError<Id>> {
        let mut index_of = BTreeMap::new();
        for (index, (id, _, _, _)) in self.txs.iter().enumerate() {
            if index_of.insert(id.clone(), index).is_some() {
                return Err(ClusterError::DuplicateTx { tx: id.clone() });
            }
        }

        let txs =
            self.txs
                .iter()
                .map(|(id, weight, fee, parents)| {
                    let parents = parents
                        .iter()
                        .map(|parent| {
                            index_of.get(parent).copied().ok_or_else(|| {
                                ClusterError::UnknownParent {
                                    child: id.clone(),
                                    parent: parent.clone(),
                                }
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(MempoolTx {
                        weight: *weight,
                        fee: *fee,
                        parents,
                    })
                })
                .collect::<Result<Vec<_>, ClusterError<Id>>>()?;

        let candidate_spends = self
            .spends
            .iter()
            .map(|(candidate, id)| {
                let tx = index_of
                    .get(id)
                    .copied()
                    .ok_or_else(|| ClusterError::UnknownSpend {
                        candidate: *candidate,
                        tx: id.clone(),
                    })?;
                Ok((*candidate, tx))
            })
            .collect::<Result<Vec<_>, ClusterError<Id>>>()?;

        let closures = ancestor_closures(&txs).map_err(|index| ClusterError::Cycle {
            tx: self.txs[index].0.clone(),
        })?;

        Ok(Cluster {
            txs,
            candidate_spends,
            closures,
        })
    }
}

/// A connected piece of the mempool: the unconfirmed transactions relevant to a selection, and
/// which candidates spend from which. Built with [`ClusterBuilder`].
///
/// This is the input to [`CoinSelector::with_cluster`]. Supplying the graph rather than a flat
/// ancestor list buys three things a list cannot express:
///
/// - **Transitive closure is computed here**, so a candidate cannot be under-priced by a caller
///   listing only its direct parent.
/// - **Package feerates are visible**, so a transaction a miner would already include costs
///   nothing, and an overpaying child that carries a deficient parent means neither is charged
///   for. A flat list has to charge for everything it is given.
/// - **The bump stays safe to search on.** Branch and bound ranks candidates on their
///   individual bumps ([`CoinSelector::ancestor_bump_fee_of`]), whose sum must never fall below
///   what the package owes together. Mining guarantees that; pooling a flat list does not, because
///   a shared ancestor that *overpays* has its surplus counted once per dependent.
///
/// # Completeness
///
/// Include the descendants and siblings of your ancestors where you know them — a parent already
/// being paid for by *another* child needs no bump, and only a transaction present in the cluster
/// can demonstrate that. Where you don't know them (a child belonging to someone else), the
/// package is priced as if it needs the bump: you overpay, the transaction still confirms. That is
/// the safe direction, and it is the reason this is an optimality limit rather than a correctness
/// one.
///
/// [`CoinSelector::with_cluster`]: crate::CoinSelector::with_cluster
/// [`CoinSelector::ancestor_bump_fee_of`]: crate::CoinSelector::ancestor_bump_fee_of
#[derive(Debug, Clone)]
pub struct Cluster {
    txs: Vec<MempoolTx>,
    /// Candidate index -> index of the transaction whose output it spends.
    candidate_spends: Vec<(usize, usize)>,
    /// Per transaction, itself plus every transitive ancestor.
    closures: Vec<Bitset>,
}

/// Error returned by [`ClusterBuilder::build`], naming the offending transactions by the caller's
/// own ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClusterError<Id> {
    /// The same transaction id was added twice.
    DuplicateTx {
        /// The duplicated id.
        tx: Id,
    },
    /// A transaction names a parent that was never added.
    UnknownParent {
        /// The transaction naming the missing parent.
        child: Id,
        /// The missing parent.
        parent: Id,
    },
    /// A candidate spends a transaction that was never added.
    UnknownSpend {
        /// The candidate index.
        candidate: usize,
        /// The missing transaction.
        tx: Id,
    },
    /// The parent relation contains a cycle, so the transactions cannot all be ancestors of each
    /// other. Real mempool clusters are acyclic.
    Cycle {
        /// A transaction on the cycle.
        tx: Id,
    },
}

impl<Id: core::fmt::Debug> core::fmt::Display for ClusterError<Id> {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            ClusterError::DuplicateTx { tx } => {
                write!(f, "transaction {:?} was added more than once", tx)
            }
            ClusterError::UnknownParent { child, parent } => write!(
                f,
                "transaction {:?} names parent {:?}, which is not in the cluster",
                child, parent
            ),
            ClusterError::UnknownSpend { candidate, tx } => write!(
                f,
                "candidate {} spends transaction {:?}, which is not in the cluster",
                candidate, tx
            ),
            ClusterError::Cycle { tx } => {
                write!(f, "the parent relation cycles through transaction {:?}", tx)
            }
        }
    }
}

#[cfg(feature = "std")]
impl<Id: core::fmt::Debug> std::error::Error for ClusterError<Id> {}

impl Cluster {
    /// The candidates that spend from this cluster, ascending and deduplicated.
    pub fn candidates(&self) -> Vec<usize> {
        let mut out = self
            .candidate_spends
            .iter()
            .map(|&(candidate, _)| candidate)
            .collect::<Vec<_>>();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Every transaction that selecting `candidate` pulls into the package: the one it spends,
    /// plus all of that transaction's ancestors.
    pub(crate) fn package_of(&self, candidate: usize) -> impl Iterator<Item = &Bitset> + '_ {
        self.candidate_spends
            .iter()
            .filter(move |&&(c, _)| c == candidate)
            .map(move |&(_, tx)| &self.closures[tx])
    }

    pub(crate) fn n_txs(&self) -> usize {
        self.txs.len()
    }

    pub(crate) fn tx(&self, index: usize) -> &MempoolTx {
        &self.txs[index]
    }

    /// Build a mock block template at `feerate` and return the transactions it includes.
    ///
    /// This is the same greedy shape Bitcoin Core's `MiniMiner` uses: repeatedly take the
    /// remaining ancestor package with the highest package feerate, and stop once even the best
    /// package pays below `feerate`. Anything mined is already paying its own way and so needs no
    /// bump; what remains is what a CPFP child has to cover.
    ///
    /// Note this is deliberately *package*-wise rather than transaction-wise. A transaction paying
    /// below `feerate` on its own is still mined when a descendant in the cluster carries it,
    /// which is exactly the case a per-ancestor rule gets wrong.
    pub(crate) fn mine(&self, feerate: FeeRate) -> Bitset {
        let n = self.txs.len();
        let mut mined = Bitset::with_capacity(n);
        let mut n_mined = 0;

        while n_mined < n {
            // The remaining ancestor package with the highest feerate, compared as a ratio so no
            // rounding to vbytes creeps into the ordering.
            let mut best: Option<(usize, u64, u64)> = None;
            for tx_index in 0..n {
                if mined.contains(tx_index) {
                    continue;
                }
                let (weight, fee) = self.remaining_package(tx_index, &mined);
                let better = match best {
                    None => true,
                    // fee/weight > best_fee/best_weight, cross-multiplied. u128 because a
                    // fee x weight product overflows u64 at realistic extremes.
                    Some((_, best_weight, best_fee)) => {
                        (fee as u128) * (best_weight as u128)
                            > (best_fee as u128) * (weight as u128)
                    }
                };
                if better {
                    best = Some((tx_index, weight, fee));
                }
            }

            let (tx_index, weight, fee) = match best {
                Some(best) => best,
                None => break,
            };
            // Once the best remaining package pays below the target, so does every other, and a
            // miner would stop here.
            if fee < feerate.implied_fee_wu(weight) {
                break;
            }
            for ancestor in self.closures[tx_index].iter() {
                if mined.insert(ancestor) {
                    n_mined += 1;
                }
            }
        }

        mined
    }

    /// Total weight and fee of `tx`'s ancestor package, skipping anything already mined.
    fn remaining_package(&self, tx: usize, mined: &Bitset) -> (u64, u64) {
        let mut weight = 0;
        let mut fee = 0;
        for ancestor in self.closures[tx].iter() {
            if !mined.contains(ancestor) {
                weight += self.txs[ancestor].weight;
                fee += self.txs[ancestor].fee;
            }
        }
        (weight, fee)
    }
}

/// For each transaction, the set containing it and all its transitive ancestors. `Err` carries the
/// index of a transaction on a cycle.
fn ancestor_closures(txs: &[MempoolTx]) -> Result<Vec<Bitset>, usize> {
    let n = txs.len();
    let mut closures = Vec::with_capacity(n);
    for _ in 0..n {
        closures.push(Bitset::with_capacity(n));
    }

    // Iterative post-order DFS: a transaction's closure is itself plus the union of its parents'.
    // `done` marks a finished closure, `on_stack` catches a cycle.
    let mut done = Bitset::with_capacity(n);
    let mut on_stack = Bitset::with_capacity(n);
    let mut stack: Vec<(usize, usize)> = Vec::new();

    for root in 0..n {
        if done.contains(root) {
            continue;
        }
        stack.push((root, 0));
        on_stack.insert(root);

        while let Some(&mut (tx, ref mut next_parent)) = stack.last_mut() {
            if *next_parent < txs[tx].parents.len() {
                let parent = txs[tx].parents[*next_parent];
                *next_parent += 1;
                if on_stack.contains(parent) {
                    return Err(parent);
                }
                if !done.contains(parent) {
                    stack.push((parent, 0));
                    on_stack.insert(parent);
                }
                continue;
            }

            // Every parent is finished, so this closure can be completed.
            let mut closure = Bitset::with_capacity(n);
            closure.insert(tx);
            for &parent in &txs[tx].parents {
                for ancestor in closures[parent].iter() {
                    closure.insert(ancestor);
                }
            }
            closures[tx] = closure;
            done.insert(tx);
            on_stack.remove(tx);
            stack.pop();
        }
    }

    Ok(closures)
}
