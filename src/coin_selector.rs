use super::*;
#[allow(unused)] // some bug in <= 1.48.0 sees this as unused when it isn't
use crate::float::FloatExt;
use crate::{
    bitset::Bitset, bnb::BnbMetric, bump_table::BumpTable, float::Ordf32, ChangePolicy, FeeRate,
    Target,
};
use alloc::{sync::Arc, vec::Vec};

/// The minimum change amount Bitcoin Core's `SelectCoinsSRD` targets; a sensible default for the
/// `change_lower` argument of [`CoinSelector::select_srd`].
pub const CHANGE_LOWER: u64 = 50_000;

/// Which ancestor-bump model a [`CoinSelector`] answers with.
///
/// The CPFP bump is a property of the *package*: an ancestor two candidates share is paid for
/// once. That makes it non-additive, and non-additive figures break the selection algorithms,
/// which rank and accumulate per candidate. So there are two models, and a selector commits to one
/// at a time rather than mixing them — a bound computed against one model and scored against the
/// other is not a bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pricing {
    /// The package figure: exact, non-additive. What every public method reports.
    Exact,
    /// The sum of per-candidate figures: additive, and never below the exact one. What branch and
    /// bound searches on.
    Local,
}

/// [`CoinSelector`] selects/deselects coins from a set of canididate coins.
///
/// You can manually select coins using methods like [`select`], or automatically with methods such
/// as [`bnb_solutions`].
///
/// [`select`]: CoinSelector::select
/// [`bnb_solutions`]: CoinSelector::bnb_solutions
#[derive(Debug, Clone)]
pub struct CoinSelector<'a> {
    candidates: &'a [Candidate],
    /// What this selection is trying to fund. Owned rather than passed per call: a selector is
    /// built for one target and evaluated against it throughout, and threading it through every
    /// method made it possible to ask two different questions of the same selection.
    target: Target,
    /// Exact CPFP pricing, built from the caller's [`Cluster`] at the target's feerate (via
    /// [`CoinSelector::with_cluster`]). `Arc` because the selector is cloned at every branch and
    /// bound node, and the table never changes after construction.
    bump_table: Option<Arc<BumpTable>>,
    /// Which of the two ancestor-bump models this selector answers with. See [`Pricing`].
    pricing: Pricing,
    selected: Bitset,
    banned: Bitset,
    candidate_order: Arc<Vec<usize>>,
}

impl<'a> CoinSelector<'a> {
    /// Creates a new coin selector from some candidate inputs and a `base_weight`.
    ///
    /// The `base_weight` is the weight of the transaction without any inputs and without a change
    /// output.
    ///
    /// The `CoinSelector` does not keep track of the final transaction's output count. The caller
    /// is responsible for including the potential output-count varint weight change in the
    /// corresponding [`DrainWeights`].
    ///
    /// Note that methods in `CoinSelector` will refer to inputs by the index in the `candidates`
    /// slice you pass in.
    ///
    /// `target` is fixed for the life of the selector. Everything it reports -- excesses, implied
    /// fees, whether it is funded -- is measured against that one target, so build a second
    /// selector to ask about a second self.target.
    pub fn new(candidates: &'a [Candidate], target: Target) -> Self {
        Self {
            candidates,
            target,
            bump_table: None,
            pricing: Pricing::Exact,
            selected: Bitset::with_capacity(candidates.len()),
            banned: Bitset::with_capacity(candidates.len()),
            candidate_order: Arc::new((0..candidates.len()).collect::<Vec<_>>()),
        }
    }

    /// What this selector is funding. Fixed at construction.
    pub fn target(&self) -> Target {
        self.target
    }

    /// Price CPFP packages exactly, for candidates that spend unconfirmed outputs of `cluster`.
    ///
    /// The pricing table is built here, at this selector's target feerate — the caller cannot
    /// supply figures derived at some other rate, which would silently under-price the package.
    /// This is why the method takes the raw [`Cluster`] rather than anything precomputed.
    ///
    /// Nothing is banned: candidates with unconfirmed ancestors are selected like any other. That
    /// works because the *search* reasons about [`effective_value_of`], which nets off each
    /// candidate's individual bump and is therefore additive, while everything this selector
    /// reports — [`excess`], [`implied_fee`], [`is_funded`], [`drain`] — uses the exact combined
    /// figure, in which an ancestor shared by two candidates is paid for once.
    ///
    /// The two differ, and deliberately: the sum of per-candidate bumps is never below the
    /// combined one, so the search *over*-reserves and the surplus surfaces as a larger change
    /// output rather than a missing fee.
    ///
    /// # Panics
    ///
    /// If the cluster refers to a candidate index out of bounds for the slice passed to
    /// [`CoinSelector::new`].
    ///
    /// [`effective_value_of`]: Self::effective_value_of
    /// [`excess`]: Self::excess
    /// [`implied_fee`]: Self::implied_fee
    /// [`is_funded`]: Self::is_funded
    /// [`drain`]: Self::drain
    pub fn with_cluster(mut self, cluster: &Cluster) -> Self {
        let bump_table = BumpTable::from_cluster(cluster, self.target.fee.rate);
        if let Some(max) = bump_table.max_candidate_index() {
            assert!(
                max < self.candidates.len(),
                "cluster refers to candidate index {} but there are only {} candidates",
                max,
                self.candidates.len()
            );
        }
        self.bump_table = Some(Arc::new(bump_table));
        self
    }

    /// The same selector, answering with the local (per-candidate) bump model.
    ///
    /// Branch and bound searches in this mode so that every figure a metric sees is additive
    /// across candidates, which is what its ranking and its bounds assume. Selections handed back
    /// to the caller are returned to [`Pricing::Exact`].
    pub(crate) fn priced_locally(&self) -> Self {
        let mut cs = self.clone();
        cs.pricing = Pricing::Local;
        cs
    }

    /// The same selector, answering with the exact (package) bump model.
    pub(crate) fn priced_exactly(&self) -> Self {
        let mut cs = self.clone();
        cs.pricing = Pricing::Exact;
        cs
    }

    /// The candidates that have unconfirmed ancestors, by index into the original `candidates`
    /// slice passed to [`CoinSelector::new`].
    ///
    /// These are selectable like any other candidate; the [ancestor bump fee] is priced into every
    /// excess calculation when they are chosen. This is a query about *pricing*, not about
    /// selectability.
    ///
    /// [ancestor bump fee]: Self::selected_ancestor_bump_fee
    pub fn candidates_with_ancestors(&self) -> impl Iterator<Item = usize> + '_ {
        self.bump_table.iter().flat_map(|t| t.candidates())
    }

    /// Iterate over all the candidates in their currently sorted order. Each item has the original
    /// index with the candidate.
    pub fn candidates(
        &self,
    ) -> impl DoubleEndedIterator<Item = (usize, Candidate)> + ExactSizeIterator + '_ {
        self.candidate_order
            .iter()
            .map(move |i| (*i, self.candidates[*i]))
    }

    /// Get the candidate at `index`. `index` refers to its position in the original `candidates`
    /// slice passed into [`CoinSelector::new`].
    pub fn candidate(&self, index: usize) -> Candidate {
        self.candidates[index]
    }

    /// Deselect a candidate at `index`. `index` refers to its position in the original `candidates`
    /// slice passed into [`CoinSelector::new`].
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
    /// slice passed into [`CoinSelector::new`].
    pub fn select(&mut self, index: usize) -> bool {
        assert!(index < self.candidates.len());
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
    /// `index` refers to its position in the original `candidates` slice passed into [`CoinSelector::new`].
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
    /// `candidates` slice passed into [`CoinSelector::new`].
    pub fn is_selected(&self, index: usize) -> bool {
        self.selected.contains(index)
    }

    /// Whether the candidates can cover this `target`'s **value** (net of input fees) — i.e. whether
    /// enough value is reachable for [`is_funded`] to hold. Respects [`ban`]ned candidates.
    ///
    /// Selecting *all* effective inputs maximizes the value available, so if that can't meet the
    /// target value, nothing can. Monotone, hence exact.
    ///
    /// NOTE: this does **not** account for [`Target::max_weight`] — a `true` result can still be
    /// infeasible under the weight cap. Use [`select_until_target_met`] or branch and bound (both of
    /// which enforce the cap) to actually build a selection.
    ///
    /// [`ban`]: Self::ban
    /// [`is_funded`]: Self::is_funded
    /// [`select_until_target_met`]: Self::select_until_target_met
    pub fn is_fundable(&self) -> bool {
        let mut test = self.clone();
        test.select_all_effective(self.target.fee.rate);
        test.is_funded()
    }

    /// Returns true if no candidates have been selected.
    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    /// The weight of the inputs including the witness header and the varint for the number of
    /// inputs.
    pub fn input_weight(&self) -> u64 {
        let is_segwit_tx = self.selected().any(|(_, wv)| wv.is_segwit);
        let witness_header_extra_weight = is_segwit_tx as u64 * 2;

        let input_count = self.selected().map(|(_, wv)| wv.input_count).sum::<usize>();
        let input_varint_weight = varint_size(input_count) * 4;

        let selected_weight: u64 = self
            .selected()
            .map(|(_, candidate)| {
                let mut weight = candidate.weight;
                if is_segwit_tx && !candidate.is_segwit {
                    // non-segwit candidates do not have the witness length field included in their
                    // weight field so we need to add 1 here if it's in a segwit tx.
                    weight += 1;
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
            .map(|index| self.candidates[index].value)
            .sum()
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

    /// The extra fee this selection owes on top of its own weight, so that its unconfirmed
    /// ancestors reach the target feerate as a package (CPFP).
    ///
    /// Which of the two models answers depends on how this selector is priced. Everything a
    /// caller can reach uses the **exact** figure — the combined package bump, in which an
    /// ancestor shared by two selected candidates is paid for once, and which is what the
    /// transaction actually owes.
    ///
    /// Branch and bound switches internally to the **local** figure — the sum of the selected
    /// candidates' [`ancestor_bump_fee_of`] — additive, and therefore never below the exact one,
    /// so its ranking and bounds can rely on it.
    ///
    /// Zero when no [`Cluster`] was supplied.
    ///
    /// [`ancestor_bump_fee_of`]: Self::ancestor_bump_fee_of
    pub fn selected_ancestor_bump_fee(&self) -> u64 {
        let bump_table = match &self.bump_table {
            Some(bump_table) => bump_table,
            None => return 0,
        };
        match self.pricing {
            Pricing::Local => self.selected.iter().map(|i| bump_table.individual(i)).sum(),
            Pricing::Exact => bump_table.combined(&self.selected),
        }
    }

    /// What selecting the candidate at `index` alone would owe to bring its unconfirmed ancestors
    /// up to the target feerate, in satoshis. Zero without a [`Cluster`], or for a candidate with
    /// no unconfirmed ancestors.
    ///
    /// Additive across candidates, and never in total below what the package actually owes --
    /// which is what makes it safe to search on; see [`with_cluster`](Self::with_cluster).
    pub fn ancestor_bump_fee_of(&self, index: usize) -> u64 {
        self.bump_table.as_ref().map_or(0, |t| t.individual(index))
    }

    /// [`Candidate::effective_value`] less what that candidate's unconfirmed ancestors cost.
    ///
    /// This is the figure to rank candidates on. `Candidate` cannot compute it: a bump is only
    /// meaningful at the feerate it was derived for, and a `Candidate` has nowhere to record
    /// which -- so it lives here, where the target (and hence the feerate the bump was built at)
    /// is known and can be checked.
    ///
    /// # Panics
    ///
    /// If a [`Cluster`] is attached and `feerate` is not the target's feerate — the bump is
    /// computed at the target rate, and netting it off a differently-rated figure mixes rates.
    pub fn effective_value_of(&self, index: usize, feerate: FeeRate) -> f32 {
        if self.bump_table.is_some() {
            assert_eq!(
                feerate, self.target.fee.rate,
                "the ancestor bump is computed at the target feerate; asking at another mixes rates"
            );
        }
        self.candidates[index].effective_value(feerate) - self.ancestor_bump_fee_of(index) as f32
    }

    /// [`Candidate::value_pwu`] less what that candidate's unconfirmed ancestors cost, spread over
    /// its weight. Needs no feerate of its own: the attached table fixes one.
    pub fn value_pwu_of(&self, index: usize) -> f32 {
        let candidate = self.candidates[index];
        candidate
            .value
            .saturating_sub(self.ancestor_bump_fee_of(index)) as f32
            / candidate.weight as f32
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

    /// How much extra value needs to be selected to reach the self.target.
    pub fn missing(&self) -> u64 {
        let excess = self.excess(Drain::NONE);
        if excess < 0 {
            excess.unsigned_abs()
        } else {
            0
        }
    }

    /// How much the current selection overshoots the value need to satisfy `self.target.fee.rate` and
    /// `self.target.value` (while ignoring `self.target.fee.absolute`).
    pub fn rate_excess(&self, drain: Drain) -> i64 {
        self.selected_value() as i64
            - self.target.value() as i64
            - drain.value as i64
            - self.implied_package_fee_from_feerate(drain.weights) as i64
    }

    /// Same as [rate_excess](Self::rate_excess) except `self.target.fee.rate` is applied to the
    /// implied transaction's weight units directly without any conversion to vbytes.
    pub fn rate_excess_wu(&self, drain: Drain) -> i64 {
        self.selected_value() as i64
            - self.target.value() as i64
            - drain.value as i64
            - self.implied_package_fee_from_feerate_wu(drain.weights) as i64
    }

    /// How much the current selection overshoots the value needed to satisfy `self.target.fee.absolute`
    /// and `self.target.value` (while ignoring `self.target.fee.rate`).
    pub fn absolute_excess(&self, drain: Drain) -> i64 {
        self.selected_value() as i64
            - self.target.value() as i64
            - drain.value as i64
            - self.target.fee.absolute as i64
    }

    /// How much the current selection overshoots the value needed to satisfy RBF's rule 4.
    pub fn replacement_excess(&self, drain: Drain) -> i64 {
        self.selected_value() as i64
            - self.target.value() as i64
            - drain.value as i64
            - self.implied_package_fee_from_replacement(drain.weights) as i64
    }

    /// Same as [replacement_excess](Self::replacement_excess) except the replacement fee
    /// is calculated using weight units directly without any conversion to vbytes.
    pub fn replacement_excess_wu(&self, drain: Drain) -> i64 {
        self.selected_value() as i64
            - self.target.value() as i64
            - drain.value as i64
            - self.implied_package_fee_from_replacement_wu(drain.weights) as i64
    }

    /// The feerate the transaction would have if we were to use this selection of inputs to achieve
    /// the `target`'s value and weight. It is essentially telling you what target feerate you currently have.
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
    /// This is the largest of the fees implied by `self.target.fee.rate`, `self.target.fee.absolute` and the
    /// [`Replace`] constraints. The feerate and replacement fees include any [ancestor bump fee];
    /// `self.target.fee.absolute` is a minimum fee floor rather than an additive charge, so it does not.
    ///
    /// This is the exact counterpart of [`excess`](Self::excess):
    /// `excess == selected_value - target.value() - drain.value - implied_fee`.
    ///
    /// `drain_weight` can be 0 to indicate no draining output.
    ///
    /// [ancestor bump fee]: Self::selected_ancestor_bump_fee
    pub fn implied_fee(&self, drain_weights: DrainWeights) -> u64 {
        self.implied_package_fee_from_feerate(drain_weights)
            .max(self.target.fee.absolute)
            .max(self.implied_package_fee_from_replacement(drain_weights))
    }

    /// The fee implied by `self.target.fee.rate` for the whole CPFP package — this transaction plus any
    /// unconfirmed ancestors — i.e. the fee for the transaction's own weight plus the [ancestor
    /// bump fee].
    ///
    /// The bump is folded in here rather than at each call site because every caller needs it.
    ///
    /// [ancestor bump fee]: Self::selected_ancestor_bump_fee
    fn implied_package_fee_from_feerate(&self, drain_weights: DrainWeights) -> u64 {
        self.target
            .fee
            .rate
            .implied_fee(self.weight(self.target.outputs, drain_weights))
            + self.selected_ancestor_bump_fee()
    }

    /// Same as [`implied_package_fee_from_feerate`](Self::implied_package_fee_from_feerate) except `self.target.fee.rate`
    /// is applied to weight units directly without any conversion to vbytes.
    fn implied_package_fee_from_feerate_wu(&self, drain_weights: DrainWeights) -> u64 {
        self.target
            .fee
            .rate
            .implied_fee_wu(self.weight(self.target.outputs, drain_weights))
            + self.selected_ancestor_bump_fee()
    }

    /// The fee needed for the whole CPFP package to satisfy RBF's rule 4, i.e. the replacement fee
    /// plus the [ancestor bump fee]. No replacement (`self.target.fee.replace` is `None`) still leaves
    /// the bump to pay.
    ///
    /// [ancestor bump fee]: Self::selected_ancestor_bump_fee
    fn implied_package_fee_from_replacement(&self, drain_weights: DrainWeights) -> u64 {
        let replacement_fee = match self.target.fee.replace {
            Some(replace) => {
                replace.min_fee_to_do_replacement(self.weight(self.target.outputs, drain_weights))
            }
            None => 0,
        };
        replacement_fee + self.selected_ancestor_bump_fee()
    }

    /// Same as [`implied_package_fee_from_replacement`](Self::implied_package_fee_from_replacement) except the
    /// replacement fee is calculated using weight units directly without any conversion to vbytes.
    fn implied_package_fee_from_replacement_wu(&self, drain_weights: DrainWeights) -> u64 {
        let replacement_fee = match self.target.fee.replace {
            Some(replace) => replace
                .min_fee_to_do_replacement_wu(self.weight(self.target.outputs, drain_weights)),
            None => 0,
        };
        replacement_fee + self.selected_ancestor_bump_fee()
    }

    /// The actual fee the selection would pay if it was used in a transaction that had
    /// `target_value` value for outputs and change output of `drain_value`.
    ///
    /// This can be negative when the selection is invalid (outputs are greater than inputs).
    pub fn fee(&self, target_value: u64, drain_value: u64) -> i64 {
        self.selected_value() as i64 - target_value as i64 - drain_value as i64
    }

    /// The value of the current selected inputs minus the fee needed to pay for the selected inputs
    /// and any ancestor bump fee.
    ///
    /// # Panics
    ///
    /// If a [`Cluster`] is attached and `feerate` is not the target's feerate; see
    /// [`effective_value_of`](Self::effective_value_of).
    pub fn effective_value(&self, feerate: FeeRate) -> i64 {
        if self.bump_table.is_some() {
            assert_eq!(
                feerate, self.target.fee.rate,
                "the ancestor bump is computed at the target feerate; asking at another mixes rates"
            );
        }
        self.selected_value() as i64
            - (self.input_weight() as f32 * feerate.spwu()).ceil() as i64
            - self.selected_ancestor_bump_fee() as i64
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
        let candidates = &self.candidates;
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
        let pwu = (0..self.candidates.len())
            .map(|i| Ordf32(self.value_pwu_of(i)))
            .collect::<Vec<_>>();
        self.sort_candidates_by_key(|(i, wv)| core::cmp::Reverse((pwu[i], wv.value)));
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
        let mut waste = self.input_waste(self.target.fee.rate, long_term_feerate);

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
                self.target.fee.rate,
                long_term_feerate,
                self.target.outputs.n_outputs,
            );
        }

        waste
    }

    /// The selected candidates with their index.
    pub fn selected(
        &self,
    ) -> impl ExactSizeIterator<Item = (usize, Candidate)> + DoubleEndedIterator + '_ {
        self.selected
            .iter()
            .map(move |index| (index, self.candidates[index]))
    }

    /// The unselected candidates with their index.
    ///
    /// The candidates are returned in sorted order. See [`sort_candidates_by`].
    ///
    /// [`sort_candidates_by`]: Self::sort_candidates_by
    pub fn unselected(&self) -> impl DoubleEndedIterator<Item = (usize, Candidate)> + '_ {
        self.unselected_indices()
            .map(move |i| (i, self.candidates[i]))
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
        match self.target.max_weight {
            Some(max_weight) => self.weight(self.target.outputs, drain_weights) <= max_weight,
            None => true,
        }
    }

    /// Whether the selection covers the target value (i.e. [`excess`](Self::excess) is
    /// non-negative), ignoring [`Target::max_weight`].
    ///
    /// This is **monotone**: selecting more never un-meets it. It deliberately does *not* include
    /// the weight cap — see [`is_within_max_weight`](Self::is_within_max_weight).
    pub fn is_funded_with_drain(&self, drain: Drain) -> bool {
        self.excess(drain) >= 0
    }

    /// Whether the selection covers the target **value** (net of input fees), i.e. [`excess`] is
    /// non-negative. **Monotone** (selecting more never un-meets it), and it deliberately does
    /// *not* check [`Target::max_weight`] — that is the separate, anti-monotone
    /// [`is_within_max_weight`]. See [`is_funded_with_drain`] for the version that
    /// accounts for a specific `drain`.
    ///
    /// [`effective_value_of`]: Self::effective_value_of
    /// [`excess`]: Self::excess
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
    pub fn select_all_effective(&mut self, feerate: FeeRate) {
        // `effective_value_of` asserts `feerate` matches the target when a cluster is attached.
        for i in 0..self.candidate_order.len() {
            let cand_index = self.candidate_order[i];
            if self.selected.contains(cand_index)
                || self.banned.contains(cand_index)
                || self.effective_value_of(cand_index, feerate) <= 0.0
            {
                continue;
            }
            self.select(cand_index);
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
    /// lower bound rather than randomizing the self.target.
    ///
    /// On success it returns the [`Drain`] to attach, whose value is the achieved change (at least
    /// `change_lower`). Returns [`SelectError::InsufficientFunds`] if the target plus `change_lower`
    /// can't be met with the available candidates, or [`SelectError::MaxWeightExceeded`] if it can be
    /// met but the resulting selection exceeds the weight cap.
    ///
    /// `rng` shuffles the candidates; it yields uniform `u64`s, e.g. `|| my_rng.next_u64()`. Any
    /// already-selected candidates are kept and counted toward the self.target.
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
            // `selector` is already exactly priced (see `BnbIter::next`), so the drain the caller
            // gets is sized by the true package cost rather than by the search's over-estimate.
            let drain = iter.metric.drain(&selector);
            *self = selector;
            return Ok((score, drain));
        }

        // No solution. If the iterator still has an item we stopped at the round limit and a
        // solution may still exist with a larger `max_rounds`. Otherwise the tree was fully
        // explored, so no selection satisfies the target — a genuine infeasibility, split into
        // value vs weight.
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
/// This can either be a single UTXO, or a group of UTXOs that should be spent together.
#[derive(Debug, Clone, Copy)]
pub struct Candidate {
    /// Total value of the UTXO(s) that this [`Candidate`] represents.
    pub value: u64,
    /// Total weight of including this/these UTXO(s).
    /// `txin` fields: `prevout`, `nSequence`, `scriptSigLen`, `scriptSig`, `scriptWitnessLen`,
    /// `scriptWitness` should all be included.
    pub weight: u64,
    /// Total number of inputs; so we can calculate extra `varint` weight due to `vin` len changes.
    pub input_count: usize,
    /// Whether this [`Candidate`] contains at least one segwit spend.
    pub is_segwit: bool,
}

impl Candidate {
    /// Create a [`Candidate`] input that spends a single taproot keyspend output.
    pub fn new_tr_keyspend(value: u64) -> Self {
        let weight = TR_KEYSPEND_SATISFACTION_WEIGHT;
        Self::new(value, weight, true)
    }

    /// Create a new [`Candidate`] that represents a single input.
    ///
    /// `satisfaction_weight` is the weight of `scriptSigLen + scriptSig + scriptWitnessLen +
    /// scriptWitness`.
    pub fn new(value: u64, satisfaction_weight: u64, is_segwit: bool) -> Candidate {
        let weight = TXIN_BASE_WEIGHT + satisfaction_weight;
        Candidate {
            value,
            weight,
            input_count: 1,
            is_segwit,
        }
    }

    /// Effective value of this input candidate: `actual_value - input_weight * feerate (sats/wu)`.
    ///
    /// Note this knows nothing about unconfirmed ancestors. Where candidates may have them, rank
    /// on [`CoinSelector::effective_value_of`] instead, which nets off what the CPFP package costs.
    pub fn effective_value(&self, feerate: FeeRate) -> f32 {
        self.value as f32 - (self.weight as f32 * feerate.spwu())
    }

    /// Value per weight unit.
    ///
    /// As with [`effective_value`](Self::effective_value), this ignores unconfirmed ancestors; see
    /// [`CoinSelector::value_pwu_of`].
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
