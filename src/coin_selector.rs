use super::*;
#[allow(unused)] // some bug in <= 1.48.0 sees this as unused when it isn't
use crate::float::FloatExt;
use crate::{
    bitset::Bitset, bnb::BnbMetric, float::Ordf32, ChangePolicy, FeeRate, SelectionProblem, Target,
};
use alloc::{sync::Arc, vec::Vec};

/// The minimum change amount Bitcoin Core's `SelectCoinsSRD` targets; a sensible default for the
/// `change_lower` argument of [`CoinSelector::select_srd`].
pub const CHANGE_LOWER: u64 = 50_000;

/// [`CoinSelector`] selects/deselects coins from a set of canididate coins.
///
/// You can manually select coins using methods like [`select`], or automatically with methods such
/// as [`bnb_solutions`].
///
/// [`select`]: CoinSelector::select
/// [`bnb_solutions`]: CoinSelector::bnb_solutions
#[derive(Debug, Clone)]
pub struct CoinSelector<'a> {
    problem: &'a SelectionProblem,
    selected: Bitset,
    banned: Bitset,
    candidate_order: Arc<Vec<usize>>,
}

impl<'a> CoinSelector<'a> {
    /// Creates a new coin selector for `problem`.
    ///
    /// The [`SelectionProblem`] is fixed for the life of the selector: target, candidates, and any
    /// ancestor-bump data. Methods refer to candidates by index into
    /// [`SelectionProblem::candidates`].
    ///
    /// The `CoinSelector` does not keep track of the final transaction's output count. The caller
    /// is responsible for including the potential output-count varint weight change in the
    /// corresponding [`DrainWeights`].
    pub fn new(problem: &'a SelectionProblem) -> Self {
        let n = problem.len();
        Self {
            problem,
            selected: Bitset::with_capacity(n),
            banned: Bitset::with_capacity(n),
            candidate_order: Arc::new((0..n).collect::<Vec<_>>()),
        }
    }

    /// What this selector is funding.
    pub fn target(&self) -> Target {
        self.problem.target()
    }

    /// The selection problem this selector is solving.
    pub fn problem(&self) -> &'a SelectionProblem {
        self.problem
    }

    /// Build a cached read-only view of the current selection.
    pub fn compute_view(&'a self) -> SelectionView<'a> {
        SelectionView::from_selector(self)
    }

    /// Iterate over all the candidates in their currently sorted order. Each item has the original
    /// index with the candidate.
    pub fn candidates(
        &self,
    ) -> impl DoubleEndedIterator<Item = (usize, Candidate)> + ExactSizeIterator + '_ {
        let cands = self.problem.candidates();
        self.candidate_order.iter().map(move |i| (*i, cands[*i]))
    }

    /// Get the candidate at `index`. `index` refers to its position in
    /// [`SelectionProblem::candidates`].
    pub fn candidate(&self, index: usize) -> Candidate {
        self.problem.candidate(index)
    }

    /// Deselect a candidate at `index`. `index` refers to its position in the original `candidates`
    /// slice of [`SelectionProblem::candidates`].
    pub fn deselect(&mut self, index: usize) -> bool {
        self.selected.remove(index)
    }

    /// Convienince method to pick elements of a slice by the indexes that are currently selected.
    /// Obviously the slice must represent the inputs ordered in the same way as when they were
    /// passed to `Candidates::new`.
    pub fn apply_selection<T>(&self, candidates: &'a [T]) -> impl Iterator<Item = &'a T> + '_ {
        self.selected.iter().map(move |i| &candidates[i])
    }

    /// Select the input at `index`. `index` refers to its position in the original `candidates`
    /// slice of [`SelectionProblem::candidates`].
    pub fn select(&mut self, index: usize) -> bool {
        assert!(index < self.problem.len());
        self.selected.insert(index)
    }

    /// Select the next unselected candidate in the sorted order fo the candidates.
    pub fn select_next(&mut self) -> bool {
        let next = self.unselected_indices().next();
        if let Some(next) = next {
            self.select(next);
            true
        } else {
            false
        }
    }

    /// Ban an input from being selected. Banning the input means it won't show up in [`unselected`]
    /// or [`unselected_indices`]. Note it can still be manually selected.
    ///
    /// `index` refers to its position in the original `candidates` slice of [`SelectionProblem::candidates`].
    ///
    /// [`unselected`]: Self::unselected
    /// [`unselected_indices`]: Self::unselected_indices
    pub fn ban(&mut self, index: usize) {
        self.banned.insert(index);
    }

    /// Gets the list of inputs that have been banned by [`ban`].
    ///
    /// [`ban`]: Self::ban
    pub fn banned(&self) -> &Bitset {
        &self.banned
    }

    /// Is the input at `index` selected. `index` refers to its position in the original
    /// `candidates` slice of [`SelectionProblem::candidates`].
    pub fn is_selected(&self, index: usize) -> bool {
        self.selected.contains(index)
    }

    /// Whether the candidates can cover this `target`'s **value** (net of input fees) — i.e. whether
    /// enough value is reachable for [`is_funded`] to hold. Respects [`ban`]ned candidates.
    ///
    /// This selects all remaining candidates with positive standalone effective value. The current
    /// selection is checked first because transaction framing can make adding such a candidate
    /// reduce the selection's actual excess.
    ///
    /// NOTE: this does **not** account for [`Target::max_weight`] — a `true` result can still be
    /// infeasible under the weight cap. Use [`select_until_target_met`] or branch and bound (both of
    /// which enforce the cap) to actually build a selection.
    ///
    /// This is a heuristic because input-count varints, witness framing, and ancestors make
    /// candidates' marginal effective values selection-dependent.
    ///
    /// [`ban`]: Self::ban
    /// [`is_funded`]: Self::is_funded
    /// [`select_until_target_met`]: Self::select_until_target_met
    pub fn is_fundable(&self) -> bool {
        self.compute_view().is_fundable()
    }

    /// Returns true if no candidates have been selected.
    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    /// The weight of the inputs including the witness header and the varint for the number of
    /// inputs.
    pub fn input_weight(&self) -> u64 {
        let is_segwit_tx = self.selected().any(|(_, wv)| wv.segwit_count > 0);
        let witness_header_extra_weight = is_segwit_tx as u64 * 2;

        let input_count = self
            .selected()
            .map(|(_, wv)| wv.segwit_count + wv.legacy_count)
            .sum::<usize>();
        let input_varint_weight = varint_size(input_count) * 4;

        let selected_weight: u64 = self
            .selected()
            .map(|(_, candidate)| {
                let mut weight = candidate.weight;
                if is_segwit_tx {
                    // Legacy inputs do not have the witness length included in their weight field
                    // so we need to add 1 to each if it's a segwit tx.
                    weight += candidate.legacy_count as u64;
                }
                weight
            })
            .sum();

        input_varint_weight + selected_weight + witness_header_extra_weight
    }

    /// Absolute value sum of all selected inputs.
    pub fn selected_value(&self) -> u64 {
        self.selected
            .iter()
            .map(|index| self.problem.candidates()[index].value)
            .sum()
    }

    /// The unconfirmed ancestors the current selection drags in (indices into
    /// [`SelectionProblem::ancestors`]).
    ///
    /// This is the **union** over the selected candidates, so an ancestor shared by several of them
    /// appears once. Derived from `selected` on demand: deselecting a candidate keeps an ancestor
    /// that another selected candidate still drags in.
    pub fn selected_ancestors(&self) -> Bitset {
        let mut union = Bitset::with_capacity(self.problem.ancestors().len());
        if self.problem.has_ancestors() {
            for cand_index in self.selected.iter() {
                for anc_index in self.problem.drags_in(cand_index).iter() {
                    union.insert(anc_index);
                }
            }
        }
        union
    }

    /// The fee (sats) this selection must pay *on top of* its own feerate obligation so the
    /// unconfirmed ancestors it drags in reach `target.fee.rate` (CPFP).
    ///
    /// Charged over the ancestors this selection drags in, taken **once each** — never by summing
    /// [`SelectionProblem::local_bump`], which would charge a shared ancestor once per candidate.
    /// Weight and fee are netted across them, so an ancestor paying above the rate offsets one paying
    /// below it, and the result saturates at 0 (an ancestor that overpays never funds the child).
    ///
    /// Most ancestors are reachable through a single candidate, and
    /// [`SelectionProblem`] has already folded those into a per-candidate
    /// [`private_ancestors`](SelectionProblem::private_ancestors) pair, so all this does is add them
    /// up. Only ancestors several candidates can reach still need de-duplicating here.
    ///
    /// Note this makes funding **non-monotone**: selecting a candidate that drags in an
    /// underpaying ancestor can lower [`excess`](Self::excess). It also means the bump is not
    /// additive over candidates, and a descendant selection can owe *less* than its parent (by
    /// dragging in an ancestor that already overpays).
    pub fn ancestor_bump(&self) -> u64 {
        if !self.problem.has_ancestors() {
            return 0;
        }

        let (mut weight, mut fee) = (0_u64, 0_u64);
        if self.problem.has_private_ancestors() {
            for cand_index in self.selected.iter() {
                let (private_weight, private_fee) = self.problem.private_ancestors(cand_index);
                weight += private_weight;
                fee += private_fee;
            }
        }

        if self.problem.has_shared_ancestors() {
            let shared = self.selected_shared_ancestors();
            for anc_index in shared.iter() {
                let (shared_weight, shared_fee) = self.problem.ancestors()[anc_index];
                weight += shared_weight;
                fee += shared_fee;
            }
        }

        self.target()
            .fee
            .rate
            .implied_fee_wu(weight)
            .saturating_sub(fee)
    }

    /// The unconfirmed ancestors that are not dragged in yet but could still be, i.e. those of the
    /// [`unselected`](Self::unselected) candidates. Respects [`ban`](Self::ban).
    ///
    /// These are exactly the ancestors a descendant of this selection can add.
    pub fn addable_ancestors(&self) -> Bitset {
        let mut union = Bitset::with_capacity(self.problem.ancestors().len());
        if self.problem.has_ancestors() {
            let already = self.selected_ancestors();
            for cand_index in self.unselected_indices() {
                for anc_index in self.problem.drags_in(cand_index).iter() {
                    if !already.contains(anc_index) {
                        union.insert(anc_index);
                    }
                }
            }
        }
        union
    }

    /// The least [`ancestor_bump`](Self::ancestor_bump) this selection — or any selection extending
    /// it — could still owe.
    ///
    /// This is **not** the bump of the current selection. A later coin can drag in an ancestor that
    /// already overpays the target rate; that surplus nets against the deficit, so a descendant can
    /// owe *less*. This method credits every still-reachable surplus and floors at zero:
    ///
    /// ```text
    /// bump of this selection, and of every selection that adds more coins
    ///     >=  max(0, currently_owed − reachable_surplus)
    /// ```
    ///
    /// where `currently_owed` is `rate · ancestor_weight − ancestor_fee` of this selection, and
    /// `reachable_surplus` is how much still-addable ancestors overpay the target rate.
    ///
    /// Surplus cannot be picked up ancestor by ancestor: ancestors arrive by selecting a
    /// *candidate*, which drags in its whole transitive set. So `reachable_surplus` is accumulated
    /// per group that must arrive together — the split [`SelectionProblem`] already computed:
    ///
    /// - Ancestors only one candidate can reach ([`private_ancestors`]) are netted as a group, and
    ///   contribute only if the group as a whole is in surplus. A chain whose tip overpays but which
    ///   nets to a deficit therefore offers nothing.
    /// - Ancestors several candidates can reach ([`shared_drags_in`]) are credited individually,
    ///   since which candidate brings them — and what else it brings — is not pinned down.
    ///
    /// This is still a relaxation: those groups may not be reachable *together*, and reaching them at
    /// all means adding candidates (and their child weight). Both only push the real figure up. When
    /// nothing reachable overpays, the bound equals the current bump.
    ///
    /// Computed in floating point and floored, so it can sit a fraction of a satoshi below the exact
    /// value — in the safe direction.
    ///
    /// [`private_ancestors`]: SelectionProblem::private_ancestors
    /// [`shared_drags_in`]: SelectionProblem::shared_drags_in
    pub fn ancestor_bump_lower_bound(&self) -> u64 {
        if !self.problem.has_ancestors() {
            return 0;
        }
        let spwu = self.target().fee.rate.spwu() as f64;
        // What a group of ancestors still owes; negative means it pays above the target rate.
        let owes = |(weight, fee): (u64, u64)| weight as f64 * spwu - fee as f64;

        // Ancestors only one candidate can reach are netted as a group, so they need no
        // de-duplicating: what this selection owes for them is a plain sum, and the most a descendant
        // could shed is one group at a time.
        let mut owed = 0.0;
        let mut shed = 0.0;
        if self.problem.has_private_ancestors() {
            for cand_index in self.selected.iter() {
                owed += owes(self.problem.private_ancestors(cand_index));
            }
            for cand_index in self.unselected_indices() {
                shed += (-owes(self.problem.private_ancestors(cand_index))).max(0.0);
            }
        }

        // Only ancestors several candidates can reach have to be gathered up, and they are credited
        // individually since no single candidate owns them.
        if self.problem.has_shared_ancestors() {
            let selected_shared = self.selected_shared_ancestors();
            for anc_index in selected_shared.iter() {
                owed += owes(self.problem.ancestors()[anc_index]);
            }

            let mut addable_shared = Bitset::with_capacity(self.problem.ancestors().len());
            for cand_index in self.unselected_indices() {
                for anc_index in self.problem.shared_drags_in(cand_index).iter() {
                    if !selected_shared.contains(anc_index) {
                        addable_shared.insert(anc_index);
                    }
                }
            }
            for anc_index in addable_shared.iter() {
                shed += (-owes(self.problem.ancestors()[anc_index])).max(0.0);
            }
        }

        let bound = owed - shed;
        if bound <= 0.0 {
            0
        } else {
            bound as u64 // truncating a positive float is the floor, i.e. rounds down
        }
    }

    /// The ancestors this selection drags in that several candidates could have dragged in, taken
    /// once each. Empty unless [`SelectionProblem::has_shared_ancestors`].
    fn selected_shared_ancestors(&self) -> Bitset {
        let mut shared = Bitset::with_capacity(match self.problem.has_shared_ancestors() {
            true => self.problem.ancestors().len(),
            false => 0,
        });
        if self.problem.has_shared_ancestors() {
            for cand_index in self.selected.iter() {
                for anc_index in self.problem.shared_drags_in(cand_index).iter() {
                    shared.insert(anc_index);
                }
            }
        }
        shared
    }

    /// Current weight of transaction implied by the selection.
    ///
    /// If you don't have any drain outputs (only target outputs) just set drain_weights to
    /// [`DrainWeights::NONE`].
    pub fn weight(&self, target_ouputs: TargetOutputs, drain_weight: DrainWeights) -> u64 {
        TX_FIXED_FIELD_WEIGHT
            + self.input_weight()
            + target_ouputs.output_weight_with_drain(drain_weight)
    }

    /// How much the current selection overshoots the value needed to achieve `target`.
    ///
    /// In order for the resulting transaction to be valid this must be 0 or above. If it's above 0
    /// this means the transaction will overpay for what it needs to reach `target`.
    pub fn excess(&self, drain: Drain) -> i64 {
        self.rate_excess(drain)
            .min(self.absolute_excess(drain))
            .min(self.replacement_excess(drain))
    }

    /// How much extra value needs to be selected to reach the self.target().
    pub fn missing(&self) -> u64 {
        let excess = self.excess(Drain::NONE);
        if excess < 0 {
            excess.unsigned_abs()
        } else {
            0
        }
    }

    /// How much the current selection overshoots the value need to satisfy `self.target().fee.rate` and
    /// `self.target().value` (while ignoring `self.target().fee.absolute`).
    ///
    /// The feerate obligation includes the [`ancestor_bump`](Self::ancestor_bump).
    pub fn rate_excess(&self, drain: Drain) -> i64 {
        self.selected_value() as i64
            - self.target().value() as i64
            - drain.value as i64
            - self.implied_fee_from_feerate(drain.weights) as i64
    }

    /// Same as [rate_excess](Self::rate_excess) except `self.target().fee.rate` is applied to the
    /// implied transaction's weight units directly without any conversion to vbytes.
    pub fn rate_excess_wu(&self, drain: Drain) -> i64 {
        self.selected_value() as i64
            - self.target().value() as i64
            - drain.value as i64
            - self.implied_fee_from_feerate_wu(drain.weights) as i64
    }

    /// How much the current selection overshoots the value needed to satisfy `self.target().fee.absolute`
    /// and `self.target().value` (while ignoring `self.target().fee.rate`).
    pub fn absolute_excess(&self, drain: Drain) -> i64 {
        self.selected_value() as i64
            - self.target().value() as i64
            - drain.value as i64
            - self.target().fee.absolute as i64
    }

    /// How much the current selection overshoots the value needed to satisfy RBF's rule 4.
    pub fn replacement_excess(&self, drain: Drain) -> i64 {
        let mut replacement_excess_needed = 0;
        if let Some(replace) = self.target().fee.replace {
            replacement_excess_needed =
                replace.min_fee_to_do_replacement(self.weight(self.target().outputs, drain.weights))
        }
        self.selected_value() as i64
            - self.target().value() as i64
            - drain.value as i64
            - replacement_excess_needed as i64
    }

    /// Same as [replacement_excess](Self::replacement_excess) except the replacement fee
    /// is calculated using weight units directly without any conversion to vbytes.
    pub fn replacement_excess_wu(&self, drain: Drain) -> i64 {
        let mut replacement_excess_needed = 0;
        if let Some(replace) = self.target().fee.replace {
            replacement_excess_needed = replace
                .min_fee_to_do_replacement_wu(self.weight(self.target().outputs, drain.weights))
        }
        self.selected_value() as i64
            - self.target().value() as i64
            - drain.value as i64
            - replacement_excess_needed as i64
    }

    /// The feerate the transaction would have if we were to use this selection of inputs to achieve
    /// the `target`'s value and weight. It is essentially telling you what target feerate you currently have.
    ///
    /// This is the *child* transaction's feerate: the fee and weight of any unconfirmed ancestors
    /// this selection drags in are not included, so it is not the package feerate.
    ///
    /// Returns `None` if the feerate would be negative or infinity.
    pub fn implied_feerate(&self, target_outputs: TargetOutputs, drain: Drain) -> Option<FeeRate> {
        let numerator =
            self.selected_value() as i64 - target_outputs.value_sum as i64 - drain.value as i64;
        let denom = self.weight(target_outputs, drain.weights);
        if numerator < 0 || denom == 0 {
            return None;
        }
        Some(FeeRate::from_sat_per_wu(numerator as f32 / denom as f32))
    }

    /// The fee the current selection and `drain_weight` should pay to satisfy `target_fee`.
    ///
    /// This compares the fee calculated from the target feerate with the fee calculated from the
    /// [`Replace`] constraints and returns the larger of the two.
    ///
    /// The feerate component includes the [`ancestor_bump`](Self::ancestor_bump); the absolute and
    /// replacement components are child-transaction constraints and are left alone.
    ///
    /// `drain_weight` can be 0 to indicate no draining output.
    pub fn implied_fee(&self, drain_weights: DrainWeights) -> u64 {
        let mut implied_fee = self
            .implied_fee_from_feerate(drain_weights)
            .max(self.target().fee.absolute);

        if let Some(replace) = self.target().fee.replace {
            implied_fee = Ord::max(
                implied_fee,
                replace
                    .min_fee_to_do_replacement(self.weight(self.target().outputs, drain_weights)),
            );
        }

        implied_fee
    }

    fn implied_fee_from_feerate(&self, drain_weights: DrainWeights) -> u64 {
        self.target()
            .fee
            .rate
            .implied_fee(self.weight(self.target().outputs, drain_weights))
            + self.ancestor_bump()
    }

    fn implied_fee_from_feerate_wu(&self, drain_weights: DrainWeights) -> u64 {
        self.target()
            .fee
            .rate
            .implied_fee_wu(self.weight(self.target().outputs, drain_weights))
            + self.ancestor_bump()
    }

    /// The actual fee the selection would pay if it was used in a transaction that had
    /// `target_value` value for outputs and change output of `drain_value`.
    ///
    /// This can be negative when the selection is invalid (outputs are greater than inputs).
    pub fn fee(&self, target_value: u64, drain_value: u64) -> i64 {
        self.selected_value() as i64 - target_value as i64 - drain_value as i64
    }

    /// The value of the current selected inputs minus the fee needed to pay for the selected inputs
    ///
    /// Only the selected inputs' own weight is charged; any [`ancestor_bump`](Self::ancestor_bump)
    /// they drag in is not.
    pub fn effective_value(&self, feerate: FeeRate) -> i64 {
        self.selected_value() as i64 - (self.input_weight() as f32 * feerate.spwu()).ceil() as i64
    }

    // /// Waste sum of all selected inputs.
    fn input_waste(&self, feerate: FeeRate, long_term_feerate: FeeRate) -> f32 {
        self.input_weight() as f32 * (feerate.spwu() - long_term_feerate.spwu())
    }

    /// Sorts the candidates by the comparision function.
    ///
    /// The comparision function takes the candidates's index and the [`Candidate`].
    ///
    /// Note this function does not change the index of the candidates after sorting, just the order
    /// in which they will be returned when interating over them in [`candidates`] and [`unselected`].
    ///
    /// [`candidates`]: CoinSelector::candidates
    /// [`unselected`]: CoinSelector::unselected
    pub fn sort_candidates_by<F>(&mut self, mut cmp: F)
    where
        F: FnMut((usize, Candidate), (usize, Candidate)) -> core::cmp::Ordering,
    {
        let candidates = self.problem.candidates();
        Arc::make_mut(&mut self.candidate_order)
            .sort_by(|a, b| cmp((*a, candidates[*a]), (*b, candidates[*b])))
    }

    /// Sorts the candidates by the key function.
    ///
    /// The key function takes the candidates's index and the [`Candidate`].
    ///
    /// Note this function does not change the index of the candidates after sorting, just the order
    /// in which they will be returned when interating over them in [`candidates`] and [`unselected`].
    ///
    /// [`candidates`]: CoinSelector::candidates
    /// [`unselected`]: CoinSelector::unselected
    pub fn sort_candidates_by_key<F, K>(&mut self, mut key_fn: F)
    where
        F: FnMut((usize, Candidate)) -> K,
        K: Ord,
    {
        self.sort_candidates_by(|a, b| key_fn(a).cmp(&key_fn(b)))
    }

    /// Sorts the candidates by descending value per weight unit, tie-breaking with value.
    pub fn sort_candidates_by_descending_value_pwu(&mut self) {
        self.sort_candidates_by_key(|(_, wv)| {
            core::cmp::Reverse((Ordf32(wv.value_pwu()), wv.value))
        });
    }

    /// Shuffle the candidates with Fisher-Yates algorithm.
    ///
    /// `rng` should yield uniform `u64`s.
    pub fn shuffle_candidates(&mut self, mut rng: impl FnMut() -> u64) {
        let candidates = Arc::make_mut(&mut self.candidate_order);
        for i in (1..candidates.len()).rev() {
            let j = (rng() % (i as u64 + 1)) as usize;
            candidates.swap(i, j);
        }
    }

    /// The waste created by the current selection as measured by the [waste metric].
    ///
    /// You can pass in an `excess_discount` which must be between `0.0..1.0`. Passing in `1.0` gives you no discount
    ///
    /// [waste metric]: https://bitcoin.stackexchange.com/questions/113622/what-does-waste-metric-mean-in-the-context-of-coin-selection
    pub fn waste(&self, long_term_feerate: FeeRate, drain: Drain, excess_discount: f32) -> f32 {
        debug_assert!((0.0..=1.0).contains(&excess_discount));
        let mut waste = self.input_waste(self.target().fee.rate, long_term_feerate);

        if drain.is_none() {
            // We don't allow negative excess waste since negative excess just means you haven't
            // satisified target yet in which case you probably shouldn't be calling this function.
            let mut excess_waste = self.excess(drain).max(0) as f32;
            // we allow caller to discount this waste depending on how wasteful excess actually is
            // to them.
            excess_waste *= excess_discount.clamp(0.0, 1.0);
            waste += excess_waste;
        } else {
            waste += drain.weights.waste(
                self.target().fee.rate,
                long_term_feerate,
                self.target().outputs.n_outputs,
            );
        }

        waste
    }

    /// The selected candidates with their index.
    pub fn selected(
        &self,
    ) -> impl ExactSizeIterator<Item = (usize, Candidate)> + DoubleEndedIterator + '_ {
        let cands = self.problem.candidates();
        self.selected.iter().map(move |index| (index, cands[index]))
    }

    /// The unselected candidates with their index.
    ///
    /// The candidates are returned in sorted order. See [`sort_candidates_by`].
    ///
    /// [`sort_candidates_by`]: Self::sort_candidates_by
    pub fn unselected(&self) -> impl DoubleEndedIterator<Item = (usize, Candidate)> + '_ {
        let cands = self.problem.candidates();
        self.unselected_indices().map(move |i| (i, cands[i]))
    }

    /// The weight of the lightest unselected (addable) candidate, or `None` when nothing is left to
    /// add.
    ///
    /// This is a lower bound on the extra input weight any descendant selection must take on to add
    /// more value, which weight-aware branch-and-bound bounds use to reason about `max_weight`.
    pub fn min_input_weight(&self) -> Option<u64> {
        self.unselected()
            .map(|(_, candidate)| candidate.weight)
            .min()
    }

    /// The indices of the selelcted candidates.
    pub fn selected_indices(&self) -> &Bitset {
        &self.selected
    }

    /// The indices of the unselected candidates.
    ///
    /// This excludes candidates that have been selected or [`banned`].
    ///
    /// [`banned`]: Self::ban
    pub fn unselected_indices(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        self.candidate_order
            .iter()
            .copied()
            .filter(move |&index| !(self.selected.contains(index) || self.banned.contains(index)))
    }

    /// Whether there are any unselected candidates left.
    pub fn is_exhausted(&self) -> bool {
        self.unselected_indices().next().is_none()
    }

    /// Whether the tx implied by the current selection plus a drain of `drain_weights` is within
    /// [`Target::max_weight`]. Pass [`DrainWeights::NONE`] for a changeless tx.
    ///
    /// Always `true` when `max_weight` is `None`. Note this is the *anti-monotone* half of
    /// feasibility (adding inputs adds weight), so it is kept separate from the monotone
    /// value-only [`is_funded`](Self::is_funded).
    pub fn is_within_max_weight(&self, drain_weights: DrainWeights) -> bool {
        match self.target().max_weight {
            Some(max_weight) => self.weight(self.target().outputs, drain_weights) <= max_weight,
            None => true,
        }
    }

    /// Whether the selection covers the target value (i.e. [`excess`](Self::excess) is
    /// non-negative), ignoring [`Target::max_weight`].
    ///
    /// This is **monotone** — selecting more never un-meets it — *unless* the problem has
    /// unconfirmed ancestors, in which case adding an input can drag in an ancestor whose bump
    /// exceeds the input's value (see [`ancestor_bump`](Self::ancestor_bump)). It deliberately does
    /// not include the weight cap — see [`is_within_max_weight`](Self::is_within_max_weight).
    pub fn is_funded_with_drain(&self, drain: Drain) -> bool {
        self.excess(drain) >= 0
    }

    /// Whether the selection covers the target **value** (net of input fees), i.e. [`excess`] is
    /// non-negative. Monotone unless the problem has unconfirmed ancestors (see
    /// [`is_funded_with_drain`] and [`ancestor_bump`]), and it deliberately does *not* check
    /// [`Target::max_weight`] — that is the separate, anti-monotone [`is_within_max_weight`]. See
    /// [`is_funded_with_drain`] for the version that accounts for a specific `drain`.
    ///
    /// [`excess`]: Self::excess
    /// [`ancestor_bump`]: Self::ancestor_bump
    /// [`is_within_max_weight`]: Self::is_within_max_weight
    /// [`is_funded_with_drain`]: Self::is_funded_with_drain
    pub fn is_funded(&self) -> bool {
        self.is_funded_with_drain(Drain::NONE)
    }

    /// Select all unselected candidates
    pub fn select_all(&mut self) {
        loop {
            if !self.select_next() {
                break;
            }
        }
    }

    /// The value of the change output should have to drain the excess value while maintaining the
    /// constraints of `target` and respecting `change_policy`.
    ///
    /// If not change output should be added according to policy then it will return `None`.
    pub fn drain_value(&self, change_policy: ChangePolicy) -> Option<u64> {
        let excess = self.excess(Drain {
            weights: change_policy.drain_weights,
            value: 0,
        });
        if excess > change_policy.min_value as i64 {
            debug_assert_eq!(
                self.is_funded(),
                self.is_funded_with_drain(Drain {
                    weights: change_policy.drain_weights,
                    value: excess as u64
                }),
                "if the target is met without a drain it must be met after adding the drain"
            );
            Some(excess as u64)
        } else {
            None
        }
    }

    /// Figures out whether the current selection should have a change output given the
    /// `change_policy`. If it should not, then it will return [`Drain::NONE`]. The value of the
    /// `Drain` will be the same as [`drain_value`].
    ///
    /// If [`is_funded`] returns true for this selection then [`is_funded_with_drain`] will
    /// also be true if you pass in the drain returned from this method.
    ///
    /// [`drain_value`]: Self::drain_value
    /// [`is_funded_with_drain`]: Self::is_funded_with_drain
    /// [`is_funded`]: Self::is_funded
    #[must_use]
    pub fn drain(&self, change_policy: ChangePolicy) -> Drain {
        match self.drain_value(change_policy) {
            Some(value) => Drain {
                weights: change_policy.drain_weights,
                value,
            },
            None => Drain::NONE,
        }
    }

    /// Select all candidates with an *effective value* greater than 0 at the provided `feerate`.
    ///
    /// A candidate if effective if it provides more value than it takes to pay for at `feerate`.
    ///
    /// This looks at each candidate's own value and weight only: a candidate that pays for itself
    /// but drags in an unconfirmed ancestor still counts as effective, even if the resulting
    /// [`ancestor_bump`](Self::ancestor_bump) outweighs it.
    pub fn select_all_effective(&mut self, feerate: FeeRate) {
        for i in 0..self.candidate_order.len() {
            let cand_index = self.candidate_order[i];
            if self.selected.contains(cand_index)
                || self.banned.contains(cand_index)
                || self.problem.candidates()[cand_index].effective_value(feerate) <= 0.0
            {
                continue;
            }
            self.selected.insert(cand_index);
        }
    }

    /// Select candidates until `target` has been met.
    ///
    /// # Errors
    ///
    /// - [`SelectError::InsufficientFunds`] if the candidates can't cover the target value.
    /// - [`SelectError::MaxWeightExceeded`] if the value is met but the resulting selection exceeds
    ///   [`Target::max_weight`]. Note this only reflects *this* in-order greedy selection; a
    ///   different selection might still fit the cap (use branch and bound to search for one).
    ///
    /// With unconfirmed ancestors the same caveat applies to
    /// [`SelectError::InsufficientFunds`]: selecting everything can fail to meet the target while
    /// some subset (one that drags in fewer ancestors) would meet it.
    pub fn select_until_target_met(&mut self) -> Result<(), SelectError> {
        self.select_until(|cs| cs.is_funded()).ok_or_else(|| {
            SelectError::InsufficientFunds(InsufficientFunds {
                missing: self.excess(Drain::NONE).unsigned_abs(),
            })
        })?;
        if !self.is_within_max_weight(DrainWeights::NONE) {
            return Err(SelectError::MaxWeightExceeded);
        }
        Ok(())
    }

    /// Select candidates until some predicate has been satisfied.
    #[must_use]
    pub fn select_until(
        &mut self,
        mut predicate: impl FnMut(&CoinSelector<'a>) -> bool,
    ) -> Option<()> {
        loop {
            if predicate(&*self) {
                break Some(());
            }

            if !self.select_next() {
                break None;
            }
        }
    }

    /// Select candidates in random order ("Single Random Draw") until the change would be at least
    /// `change_lower`.
    ///
    /// Unlike [`run_bnb`] with [`LowestFee`], this doesn't minimize fees — it deliberately produces
    /// a healthy-sized (privacy-friendly) change output, avoiding tiny "toxic" change. It's a port
    /// of Bitcoin Core's `SelectCoinsSRD`; pass [`CHANGE_LOWER`] for Core's value.
    ///
    /// The change *amount* comes out random on its own: because candidates are added in random order
    /// and we stop as soon as the change reaches `change_lower`, the final change is wherever the
    /// last (random) input pushed it — at or above `change_lower`. So, like Core, we use a fixed
    /// lower bound rather than randomizing the self.target().
    ///
    /// On success it returns the [`Drain`] to attach, whose value is the achieved change (at least
    /// `change_lower`). Returns [`SelectError::InsufficientFunds`] if the target plus `change_lower`
    /// can't be met with the available candidates, or [`SelectError::MaxWeightExceeded`] if it can be
    /// met but the resulting selection exceeds the weight cap.
    ///
    /// `rng` shuffles the candidates; it yields uniform `u64`s, e.g. `|| my_rng.next_u64()`. Any
    /// already-selected candidates are kept and counted toward the self.target().
    ///
    /// [`run_bnb`]: Self::run_bnb
    /// [`LowestFee`]: crate::metrics::LowestFee
    // TODO: recover from exceeding `max_weight` by evicting the least-valuable inputs (matching
    // Core's `max_selection_weight`) instead of erroring with `MaxWeightExceeded`. Deferred until
    // the max-weight PR lands.
    pub fn select_srd(
        &mut self,
        drain_weights: DrainWeights,
        change_lower: u64,
        rng: impl FnMut() -> u64,
    ) -> Result<Drain, SelectError> {
        self.shuffle_candidates(rng);

        let mut is_within_max_weight = false;
        let mut excess = 0_i64;

        self.select_until(|cs| {
            is_within_max_weight = cs.is_within_max_weight(drain_weights);
            excess = cs.excess(Drain {
                weights: drain_weights,
                value: 0,
            });
            excess >= change_lower as i64 || !is_within_max_weight
        })
        .ok_or_else(|| {
            SelectError::InsufficientFunds(InsufficientFunds {
                missing: (change_lower as i64 - excess).unsigned_abs(),
            })
        })?;

        if !is_within_max_weight {
            return Err(SelectError::MaxWeightExceeded);
        }

        Ok(Drain {
            weights: drain_weights,
            value: excess as u64,
        })
    }

    /// Return an iterator that can be used to select candidates.
    pub fn select_iter(self) -> SelectIter<'a> {
        SelectIter { cs: self.clone() }
    }

    /// Iterates over rounds of branch and bound to minimize the score of the provided
    /// [`BnbMetric`].
    ///
    /// Not every iteration will return a solution. If a solution is found, we return the selection
    /// and score. Each subsequent solution of the iterator guarantees a higher score than the last.
    ///
    /// Most of the time, you would want to use [`CoinSelector::run_bnb`] instead.
    pub fn bnb_solutions<M: BnbMetric>(
        &self,
        metric: M,
    ) -> impl Iterator<Item = Option<(CoinSelector<'a>, Ordf32)>> {
        crate::bnb::BnbIter::new(self.clone(), metric)
    }

    /// Run branch and bound to minimize the score of the provided [`BnbMetric`].
    ///
    /// The method keeps trying until no better solution can be found, or we reach `max_rounds`. If a
    /// solution is found, the score and the change output ([`Drain`]) that the metric decided on are
    /// returned. Otherwise, we error with [`NoBnbSolution`].
    ///
    /// Use [`CoinSelector::bnb_solutions`] to access the branch and bound iterator directly.
    pub fn run_bnb<M: BnbMetric>(
        &mut self,
        metric: M,
        max_rounds: usize,
    ) -> Result<(Ordf32, Drain), NoBnbSolution> {
        let mut iter = crate::bnb::BnbIter::new(self.clone(), metric);
        let mut rounds = 0_usize;
        let best = iter
            .by_ref()
            .take(max_rounds)
            .inspect(|_| rounds += 1)
            .flatten()
            .last();
        if let Some((selector, score)) = best {
            let drain = iter.metric.drain(&selector.compute_view());
            *self = selector;
            return Ok((score, drain));
        }

        // No solution. If the iterator still has an item we stopped at the round limit and a
        // solution may still exist with a larger `max_rounds`. Otherwise the tree was fully
        // explored, so no selection satisfies the target — a genuine infeasibility, split into
        // value vs weight. (With unconfirmed ancestors `is_fundable` is only a heuristic, so the
        // split between the two can be wrong — the infeasibility itself is not.)
        if iter.next().is_some() {
            assert_eq!(rounds, max_rounds); // still-yielding ⟹ we truncated at the cap
            return Err(NoBnbSolution::RoundLimit { max_rounds, rounds });
        }
        if !self.is_fundable() {
            return Err(NoBnbSolution::InsufficientFunds);
        }
        Err(NoBnbSolution::MaxWeightExceeded)
    }
}

// Allow this for now due to MSRV
#[allow(clippy::uninlined_format_args)]
impl core::fmt::Display for CoinSelector<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "[")?;
        let mut candidates = self.candidates().peekable();

        while let Some((i, _)) = candidates.next() {
            write!(f, "{}", i)?;
            if self.is_selected(i) {
                write!(f, "✔")?;
            } else if self.banned().contains(i) {
                write!(f, "✘")?
            } else {
                write!(f, "☐")?;
            }

            if candidates.peek().is_some() {
                write!(f, ", ")?;
            }
        }

        write!(f, "]")
    }
}

/// The `SelectIter` allows you to select candidates by calling [`Iterator::next`].
///
/// The [`Iterator::Item`] is a tuple of `(selector, last_selected_index, last_selected_candidate)`.
pub struct SelectIter<'a> {
    cs: CoinSelector<'a>,
}

impl<'a> Iterator for SelectIter<'a> {
    type Item = (CoinSelector<'a>, usize, Candidate);

    fn next(&mut self) -> Option<Self::Item> {
        let (index, wv) = self.cs.unselected().next()?;
        self.cs.select(index);
        Some((self.cs.clone(), index, wv))
    }
}

impl DoubleEndedIterator for SelectIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let (index, wv) = self.cs.unselected().next_back()?;
        self.cs.select(index);
        Some((self.cs.clone(), index, wv))
    }
}

/// Error type that occurs when the target amount cannot be met.
#[derive(Clone, Debug, Copy, PartialEq, Eq)]
pub struct InsufficientFunds {
    /// The missing amount in satoshis.
    pub missing: u64,
}

impl core::fmt::Display for InsufficientFunds {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "Insufficient funds. Missing {} sats.", self.missing)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for InsufficientFunds {}

/// Error returned by [`CoinSelector::select_until_target_met`].
#[derive(Clone, Debug, Copy, PartialEq, Eq)]
pub enum SelectError {
    /// The candidates can't cover the target value.
    InsufficientFunds(InsufficientFunds),
    /// The value target is met, but the resulting selection exceeds [`Target::max_weight`].
    MaxWeightExceeded,
}

impl From<InsufficientFunds> for SelectError {
    fn from(e: InsufficientFunds) -> Self {
        SelectError::InsufficientFunds(e)
    }
}

impl core::fmt::Display for SelectError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            SelectError::InsufficientFunds(e) => write!(f, "{}", e),
            SelectError::MaxWeightExceeded => {
                write!(
                    f,
                    "Selection meets the target value but exceeds `max_weight`."
                )
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SelectError {}

/// Error returned by [`CoinSelector::run_bnb`] when it yields no solution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoBnbSolution {
    /// The candidates can't cover the target value, so no selection is possible.
    ///
    /// With unconfirmed ancestors this is decided by the heuristic [`CoinSelector::is_fundable`], so
    /// it may be reported where [`MaxWeightExceeded`](Self::MaxWeightExceeded) fits better, and vice
    /// versa. Either way the search was exhaustive: there is no solution.
    InsufficientFunds,
    /// Some selection covers the target value, but every one of them exceeds
    /// [`Target::max_weight`].
    ///
    /// Only reachable with a metric that enforces the cap (e.g. [`LowestFee`]); a cap-blind metric
    /// returns an over-cap selection rather than failing.
    ///
    /// [`LowestFee`]: crate::metrics::LowestFee
    MaxWeightExceeded,
    /// The round limit was reached before the search finished — a solution may still exist with a
    /// larger `max_rounds`.
    RoundLimit {
        /// Maximum rounds set by the caller.
        max_rounds: usize,
        /// Number of branch-and-bound rounds performed.
        rounds: usize,
    },
}

// Allow this for now due to MSRV
#[allow(clippy::uninlined_format_args)]
impl core::fmt::Display for NoBnbSolution {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NoBnbSolution::InsufficientFunds => {
                write!(
                    f,
                    "no bnb solution: candidates cannot cover the target value"
                )
            }
            NoBnbSolution::MaxWeightExceeded => {
                write!(
                    f,
                    "no bnb solution: no selection meets the target within max_weight"
                )
            }
            NoBnbSolution::RoundLimit { max_rounds, rounds } => write!(
                f,
                "no bnb solution found after {} rounds (max rounds is {})",
                rounds, max_rounds
            ),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for NoBnbSolution {}

/// A `Candidate` represents an input candidate for [`CoinSelector`].
///
/// This can either be a single UTXO, or a group of UTXOs that should be spent together. A group
/// may mix legacy and segwit inputs; set [`legacy_count`] and [`segwit_count`] accordingly.
///
/// [`legacy_count`]: Candidate::legacy_count
/// [`segwit_count`]: Candidate::segwit_count
#[derive(Debug, Clone, Copy)]
pub struct Candidate {
    /// Total value of the UTXO(s) that this [`Candidate`] represents.
    pub value: u64,
    /// Total weight of including this/these UTXO(s).
    /// `txin` fields: `prevout`, `nSequence`, `scriptSigLen`, `scriptSig`, `scriptWitnessLen`,
    /// `scriptWitness` should all be included.
    ///
    /// For legacy inputs, do *not* include the `scriptWitnessLen` byte: a legacy input only
    /// serializes an (empty) witness when the transaction has a witness section, and
    /// [`CoinSelector::input_weight`] adds that 1 WU per legacy input once any segwit input is
    /// selected.
    pub weight: u64,
    /// Total number of segwit inputs.
    ///
    /// If any selected candidate has a non-zero `segwit_count`, the transaction serializes a
    /// witness section (marker + flag, 2 WU) and every input — including legacy ones — pays for
    /// a witness.
    pub segwit_count: usize,
    /// Total number of legacy (non-segwit) inputs.
    ///
    /// Each legacy input serializes an empty witness (1 WU) when the transaction has a witness
    /// section; [`CoinSelector::input_weight`] prices this per legacy input, so grouped legacy
    /// inputs are counted exactly.
    pub legacy_count: usize,
}

impl Candidate {
    /// Create a [`Candidate`] input that spends a single taproot keyspend output.
    pub fn new_tr_keyspend(value: u64) -> Self {
        let weight = TR_KEYSPEND_SATISFACTION_WEIGHT;
        Self::new_segwit(value, weight)
    }

    /// Create a new [`Candidate`] that represents a single segwit input.
    ///
    /// `satisfaction_weight` is the additional weight (in weight units) required to satisfy the input
    /// beyond [`TXIN_BASE_WEIGHT`] (e.g. `scriptWitnessLen + scriptWitness` in WU at 1 WU/byte, plus
    /// any `scriptSig` data and extra `scriptSigLen` varint bytes if nested/wrapped segwit).
    ///
    /// Note that [`TXIN_BASE_WEIGHT`] already accounts for the outpoint, `nSequence`, and 1 byte for
    /// `scriptSigLen`.
    pub fn new_segwit(value: u64, satisfaction_weight: u64) -> Candidate {
        let weight = TXIN_BASE_WEIGHT + satisfaction_weight;
        Candidate {
            value,
            weight,
            segwit_count: 1,
            legacy_count: 0,
        }
    }

    /// Create a new [`Candidate`] that represents a single legacy (non-segwit) input.
    ///
    /// `satisfaction_weight` is the additional weight (in weight units) required to satisfy the input
    /// beyond [`TXIN_BASE_WEIGHT`] (e.g. `scriptSig` at 4 WU/byte, plus 4 WU per extra `scriptSigLen`
    /// varint byte if `scriptSig` exceeds 252 bytes).
    ///
    /// Note that [`TXIN_BASE_WEIGHT`] already accounts for the outpoint, `nSequence`, and 1 byte for
    /// `scriptSigLen`.
    pub fn new_legacy(value: u64, satisfaction_weight: u64) -> Candidate {
        let weight = TXIN_BASE_WEIGHT + satisfaction_weight;
        Candidate {
            value,
            weight,
            segwit_count: 0,
            legacy_count: 1,
        }
    }

    /// Effective value of this input candidate: `actual_value - input_weight * feerate (sats/wu)`.
    pub fn effective_value(&self, feerate: FeeRate) -> f32 {
        self.value as f32 - (self.weight as f32 * feerate.spwu())
    }

    /// Value per weight unit
    pub fn value_pwu(&self) -> f32 {
        self.value as f32 / self.weight as f32
    }

    /// The amount of *effective value* you receive per weight unit from adding this candidate as an
    /// input.
    pub fn effective_value_pwu(&self, feerate: FeeRate) -> f32 {
        self.value_pwu() - feerate.spwu()
    }

    /// The (minimum) fee you'd have to pay to add this input to a transaction as implied by the
    /// `feerate`.
    pub fn implied_fee(&self, feerate: FeeRate) -> f32 {
        self.weight as f32 * feerate.spwu()
    }

    /// The amount of fee you have to pay per satoshi of value you add from this input.
    ///
    /// The value is always positive but values below 1.0 mean the input has negative [*effective
    /// value*](Self::effective_value) at this `feerate`.
    pub fn fee_per_value(&self, feerate: FeeRate) -> f32 {
        self.implied_fee(feerate) / self.value as f32
    }
}
