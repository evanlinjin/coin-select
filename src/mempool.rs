use crate::{bitset::Bitset, FeeRate};
use alloc::vec::Vec;

/// An unconfirmed transaction in a [`Cluster`].
#[derive(Debug, Clone)]
pub struct MempoolTx {
    /// Weight in weight units.
    pub weight: u64,
    /// Fee paid, in satoshis.
    pub fee: u64,
    /// Indices into [`Cluster`]'s transaction list of this transaction's *direct* parents.
    ///
    /// Only *direct* parents: the transitive closure is computed for you, which is much of the
    /// point of supplying a graph rather than a list.
    pub parents: Vec<usize>,
}

/// A connected piece of the mempool: the unconfirmed transactions relevant to a selection, and
/// which candidates spend from which.
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
/// If all you have is a flat list of ancestors, express it here: each ancestor becomes a
/// transaction with no parents, and each (ancestor, candidate) pair becomes an entry in
/// `candidate_spends`. You then get the mining step for free.
///
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

/// Error returned when a [`Cluster`] cannot be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterError {
    /// A `parents` entry does not refer to a transaction in the cluster.
    ParentOutOfBounds {
        /// The transaction naming the bad parent.
        tx: usize,
        /// The out-of-bounds parent index.
        parent: usize,
    },
    /// A `candidate_spends` entry does not refer to a transaction in the cluster.
    SpendOutOfBounds {
        /// The candidate index.
        candidate: usize,
        /// The out-of-bounds transaction index.
        tx: usize,
    },
    /// The parent relation contains a cycle, so the transactions cannot all be ancestors of each
    /// other. Real mempool clusters are acyclic.
    Cycle {
        /// A transaction on the cycle.
        tx: usize,
    },
}

impl core::fmt::Display for ClusterError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            ClusterError::ParentOutOfBounds { tx, parent } => write!(
                f,
                "transaction {} names parent {}, which is not in the cluster",
                tx, parent
            ),
            ClusterError::SpendOutOfBounds { candidate, tx } => write!(
                f,
                "candidate {} spends transaction {}, which is not in the cluster",
                candidate, tx
            ),
            ClusterError::Cycle { tx } => {
                write!(f, "the parent relation cycles through transaction {}", tx)
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ClusterError {}

impl Cluster {
    /// Build a cluster from unconfirmed transactions and the candidates that spend them.
    ///
    /// `candidate_spends` pairs a candidate index (into the slice given to
    /// [`CoinSelector::new`]) with the index of the transaction in `txs` whose output it spends.
    /// Both directions are many: a transaction may be spent by several candidates, and a candidate
    /// may appear more than once when it spends outputs of several transactions — its package is
    /// then the union of their ancestor closures.
    ///
    /// # Errors
    ///
    /// [`ClusterError`] if an index is out of bounds or the parent relation cycles.
    ///
    /// [`CoinSelector::new`]: crate::CoinSelector::new
    pub fn new(
        txs: Vec<MempoolTx>,
        candidate_spends: Vec<(usize, usize)>,
    ) -> Result<Self, ClusterError> {
        for (tx_index, tx) in txs.iter().enumerate() {
            for &parent in &tx.parents {
                if parent >= txs.len() {
                    return Err(ClusterError::ParentOutOfBounds {
                        tx: tx_index,
                        parent,
                    });
                }
            }
        }
        for &(candidate, tx) in &candidate_spends {
            if tx >= txs.len() {
                return Err(ClusterError::SpendOutOfBounds { candidate, tx });
            }
        }

        let closures = ancestor_closures(&txs)?;
        Ok(Self {
            txs,
            candidate_spends,
            closures,
        })
    }

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

/// For each transaction, the set containing it and all its transitive ancestors.
fn ancestor_closures(txs: &[MempoolTx]) -> Result<Vec<Bitset>, ClusterError> {
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
                    return Err(ClusterError::Cycle { tx: parent });
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
