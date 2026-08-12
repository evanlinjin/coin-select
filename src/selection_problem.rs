use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::bitset::Bitset;
use crate::{Candidate, CoinSelector, FeeRate, Target};

/// An unconfirmed ancestor that may need bumping to the target feerate (CPFP).
///
/// `Txid` is whatever the caller keys transactions by. This crate has no `bitcoin` dependency.
#[derive(Debug, Clone)]
pub struct AncestorToBump<Txid> {
    /// Caller-chosen id for this transaction.
    pub txid: Txid,
    /// Weight of this transaction in weight units.
    pub weight: u64,
    /// Fee this transaction already pays, in satoshis.
    pub fee: u64,
    /// Direct parents only; transitive ancestors are derived when building a [`SelectionProblem`].
    pub parents: Vec<Txid>,
}

/// One or more UTXOs that must be spent together, described on their own terms.
///
/// Everything here is intrinsic to the coins. Hand these to [`SelectionProblem::new`], which pairs
/// each group with the ancestors it drags in.
pub type InputGroup<Txid> = Vec<Input<Txid>>;

/// A single UTXO, before it is folded into a [`Candidate`].
#[derive(Debug, Clone, Copy)]
pub struct Input<Txid> {
    /// Value of the UTXO in satoshis.
    pub value: u64,
    /// Input weight as for [`Candidate::weight`] (legacy inputs omit the empty-witness byte).
    pub weight: u64,
    /// Whether this input is segwit.
    pub is_segwit: bool,
    /// Transaction that created this UTXO (may be unconfirmed).
    pub residing_txid: Txid,
}

impl<Txid> From<Input<Txid>> for InputGroup<Txid> {
    fn from(input: Input<Txid>) -> Self {
        alloc::vec![input]
    }
}

/// Target, candidates, and (optional) ancestor-bump data for one coin-selection run.
///
/// Build with [`SelectionProblem::new_no_ancestors`] when nothing is unconfirmed, or
/// [`SelectionProblem::new`] when spending unconfirmed UTXOs. Pass a reference to
/// [`CoinSelector::new`].
///
/// Ancestor bump figures are stored here (not on [`Candidate`]) so candidates stay a plain
/// description of inputs. Unknown parent ids are treated as confirmed and ignored. There is no
/// mempool "mine" step — deficits are computed against the full ancestor set and may overestimate
/// what Bitcoin Core would charge.
///
/// What a selection actually owes is [`ancestor_bump`](Self::ancestor_bump) over the **union** of
/// the ancestors its selected candidates drag in; see
/// [`CoinSelector::ancestor_bump`](crate::CoinSelector::ancestor_bump), which is what fee and
/// excess calculations use.
#[derive(Debug, Clone)]
pub struct SelectionProblem {
    target: Target,
    candidates: Vec<Candidate>,
    /// Weight and fee of each ancestor, after txids are dropped.
    ancestors: Vec<(u64, u64)>,
    /// Per-candidate set of ancestor indices dragged in by selecting that candidate.
    drags_in: Vec<Bitset>,
    /// Per-candidate local bump fee (sats) at [`Target::fee`](crate::TargetFee)'s rate.
    local_bump: Vec<u64>,
    /// Whether any candidate drags in at least one ancestor.
    has_ancestors: bool,
}

/// The fee still owed so the ancestors in `set` meet `rate`, over the whole set at once.
///
/// Weights and fees are netted across the set, so an overpaying ancestor subsidizes an underpaying
/// one and the result saturates at 0 (the child is never credited).
fn bump_of(ancestors: &[(u64, u64)], rate: FeeRate, set: &Bitset) -> u64 {
    let (weight, fee) = set.iter().fold((0_u64, 0_u64), |(w, f), anc_i| {
        let (anc_w, anc_f) = ancestors[anc_i];
        (w + anc_w, f + anc_f)
    });
    rate.implied_fee_wu(weight).saturating_sub(fee)
}

impl SelectionProblem {
    /// A problem with no unconfirmed ancestors.
    ///
    /// `candidates` are taken as-is.
    pub fn new_no_ancestors(
        target: Target,
        candidates: impl IntoIterator<Item = Candidate>,
    ) -> Self {
        let candidates: Vec<Candidate> = candidates.into_iter().collect();
        let n = candidates.len();
        Self {
            target,
            candidates,
            ancestors: Vec::new(),
            drags_in: (0..n).map(|_| Bitset::with_capacity(0)).collect(),
            local_bump: alloc::vec![0; n],
            has_ancestors: false,
        }
    }

    /// Build candidates from input groups and the unconfirmed ancestors they may drag in.
    ///
    /// For each input group, the residing txids and their transitive parents (restricted to
    /// `ancestors_to_bump`) form that candidate's `drags_in` set. `local_bump` is the fee still
    /// owed so those ancestors meet `target.fee.rate`, as if this were the only selected
    /// candidate — see [`local_bump`](Self::local_bump) for why that figure must not be summed.
    pub fn new<Txid, G, A>(target: Target, input_groups: G, ancestors_to_bump: A) -> Self
    where
        Txid: Copy + Ord + Eq,
        G: IntoIterator,
        G::Item: Into<InputGroup<Txid>>,
        A: IntoIterator,
        A::Item: Into<AncestorToBump<Txid>>,
    {
        let ancestors: Vec<AncestorToBump<Txid>> =
            ancestors_to_bump.into_iter().map(Into::into).collect();

        let txid_to_anc: BTreeMap<Txid, usize> = ancestors
            .iter()
            .enumerate()
            .map(|(i, a)| (a.txid, i))
            .collect();

        let n_anc = ancestors.len();
        let anc_weight_fee: Vec<(u64, u64)> = ancestors.iter().map(|a| (a.weight, a.fee)).collect();
        let mut candidates = Vec::new();
        let mut drags_in = Vec::new();
        let mut local_bump = Vec::new();
        let mut has_ancestors = false;

        for input_group in input_groups {
            let mut cand = Candidate {
                value: 0,
                weight: 0,
                segwit_count: 0,
                legacy_count: 0,
            };
            let mut dragged = Bitset::with_capacity(n_anc);

            for input in input_group.into() {
                cand.value += input.value;
                cand.weight += input.weight;
                match input.is_segwit {
                    true => cand.segwit_count += 1,
                    false => cand.legacy_count += 1,
                }

                let mut txid_stack = alloc::vec![input.residing_txid];
                while let Some(txid) = txid_stack.pop() {
                    if let Some(&anc_i) = txid_to_anc.get(&txid) {
                        if dragged.insert(anc_i) {
                            txid_stack.extend(ancestors[anc_i].parents.iter().copied());
                        }
                    }
                }
            }

            has_ancestors |= !dragged.is_empty();
            local_bump.push(bump_of(&anc_weight_fee, target.fee.rate, &dragged));
            candidates.push(cand);
            drags_in.push(dragged);
        }

        Self {
            target,
            candidates,
            ancestors: anc_weight_fee,
            drags_in,
            local_bump,
            has_ancestors,
        }
    }

    /// What this problem is funding.
    pub fn target(&self) -> Target {
        self.target
    }

    /// All candidates, in construction order.
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    /// Candidate at `index`.
    pub fn candidate(&self, index: usize) -> Candidate {
        self.candidates[index]
    }

    /// Number of candidates.
    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    /// Whether there are no candidates.
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// Ancestor units as `(weight, fee)` pairs.
    pub fn ancestors(&self) -> &[(u64, u64)] {
        &self.ancestors
    }

    /// Whether any candidate drags in an unconfirmed ancestor.
    ///
    /// `false` means every fee calculation reduces to the plain (child-only) case, which lets
    /// branch and bound use the tighter bounds that assume monotone funding.
    pub fn has_ancestors(&self) -> bool {
        self.has_ancestors
    }

    /// Ancestor indices dragged in by selecting candidate `index`.
    pub fn drags_in(&self, index: usize) -> &Bitset {
        &self.drags_in[index]
    }

    /// The fee still owed so the ancestors in `set` meet [`Target::fee`](crate::TargetFee)'s rate.
    ///
    /// `set` indexes [`ancestors`](Self::ancestors). Weight and fee are netted over the whole set,
    /// so each ancestor is charged exactly once no matter how many candidates drag it in, and an
    /// overpaying ancestor offsets an underpaying one. Saturates at 0.
    pub fn ancestor_bump(&self, set: &Bitset) -> u64 {
        bump_of(&self.ancestors, self.target.fee.rate, set)
    }

    /// Local (per-candidate) bump fee for candidate `index`, in satoshis.
    ///
    /// This is what candidate `index` would owe *on its own*. It is informational only: these
    /// figures must never be summed over a selection, because candidates sharing an ancestor would
    /// each pay for it. Use [`ancestor_bump`](Self::ancestor_bump) over the union instead (which is
    /// what [`CoinSelector`] does).
    pub fn local_bump(&self, index: usize) -> u64 {
        self.local_bump[index]
    }

    /// A [`CoinSelector`] over this problem.
    pub fn selector(&self) -> CoinSelector<'_> {
        CoinSelector::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FeeRate, TargetFee, TargetOutputs};

    fn target(feerate_sat_vb: f32) -> Target {
        Target {
            fee: TargetFee {
                rate: FeeRate::from_sat_per_vb(feerate_sat_vb),
                absolute: 0,
                replace: None,
            },
            outputs: TargetOutputs {
                value_sum: 0,
                weight_sum: 0,
                n_outputs: 0,
            },
            max_weight: None,
        }
    }

    #[test]
    fn no_ancestors_round_trip() {
        let cands = [
            Candidate::new_segwit(100_000, 100),
            Candidate::new_legacy(50_000, 200),
        ];
        let p = SelectionProblem::new_no_ancestors(target(10.0), cands);
        assert_eq!(p.len(), 2);
        assert_eq!(p.candidate(0).value, 100_000);
        assert_eq!(p.candidate(1).legacy_count, 1);
        assert!(p.ancestors().is_empty());
        assert_eq!(p.local_bump(0), 0);
        assert_eq!(p.local_bump(1), 0);
        assert!(p.drags_in(0).is_empty());
    }

    #[test]
    fn transitive_parents() {
        // UTXO on C; C parents B; B parents A. All unconfirmed.
        let ancestors = [
            AncestorToBump {
                txid: "A",
                weight: 400,
                fee: 0,
                parents: vec![],
            },
            AncestorToBump {
                txid: "B",
                weight: 400,
                fee: 0,
                parents: vec!["A"],
            },
            AncestorToBump {
                txid: "C",
                weight: 400,
                fee: 0,
                parents: vec!["B"],
            },
        ];
        let inputs = [Input {
            value: 10_000,
            weight: 272,
            is_segwit: true,
            residing_txid: "C",
        }];
        let p = SelectionProblem::new(target(10.0), inputs, ancestors);
        assert_eq!(p.len(), 1);
        let dragged: Vec<_> = p.drags_in(0).iter().collect();
        assert_eq!(dragged, vec![0, 1, 2]); // A, B, C
    }

    #[test]
    fn shared_ancestor_in_both_drags_in() {
        let ancestors = [AncestorToBump {
            txid: "P",
            weight: 1_000,
            fee: 0,
            parents: vec![],
        }];
        let inputs = [
            Input {
                value: 10_000,
                weight: 272,
                is_segwit: true,
                residing_txid: "P",
            },
            Input {
                value: 20_000,
                weight: 272,
                is_segwit: true,
                residing_txid: "P",
            },
        ];
        let p = SelectionProblem::new(target(10.0), inputs, ancestors);
        assert!(p.drags_in(0).contains(0));
        assert!(p.drags_in(1).contains(0));
        assert_eq!(p.local_bump(0), p.local_bump(1));
        assert!(p.local_bump(0) > 0);
    }

    #[test]
    fn overpaying_ancestor_zero_bump() {
        // weight 400 wu at 1 sat/vb => ~100 sats implied; fee already 10_000
        let ancestors = [AncestorToBump {
            txid: "P",
            weight: 400,
            fee: 10_000,
            parents: vec![],
        }];
        let inputs = [Input {
            value: 10_000,
            weight: 272,
            is_segwit: true,
            residing_txid: "P",
        }];
        let p = SelectionProblem::new(target(1.0), inputs, ancestors);
        assert_eq!(p.local_bump(0), 0);
    }

    #[test]
    fn unknown_parent_ignored() {
        let ancestors = [AncestorToBump {
            txid: "child",
            weight: 400,
            fee: 0,
            parents: vec!["confirmed_parent"],
        }];
        let inputs = [Input {
            value: 10_000,
            weight: 272,
            is_segwit: true,
            residing_txid: "child",
        }];
        let p = SelectionProblem::new(target(10.0), inputs, ancestors);
        let dragged: Vec<_> = p.drags_in(0).iter().collect();
        assert_eq!(dragged, vec![0]); // only child
    }
}
