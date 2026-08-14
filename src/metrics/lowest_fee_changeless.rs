use crate::{
    float::Ordf32, varint_size, BnbMetric, Drain, DrainWeights, FeeRate, SelectionProblem,
    SelectionView,
};

use super::LowestFee;

/// Metric that minimizes fees while only accepting selections for which [`LowestFee`] chooses no
/// change output.
///
/// This reuses [`LowestFee`]'s change decision, including the future cost of spending change, its
/// dust threshold, and the transaction weight cap. A selection is only valid here when that
/// decision returns no change output. That includes change that is uneconomical or dust, as well as
/// change that cannot fit the weight cap.
///
/// Unlike constraining an arbitrary metric after the fact, this metric has a changeless-specific
/// lower bound. A changeless selection's score is its selected value minus the target value. Since
/// selected value can only increase down a branch, the current no-change fee is a lower bound for
/// every descendant, including when unconfirmed ancestry makes funding non-monotone. For pools of at
/// most 24 candidates, the bound combines that fact with [`LowestFee`]'s funding relaxation. Larger
/// pools retain only the `LowestFee` bound and ordering because the selected-value bound can starve
/// useful branches under a finite round limit.
#[derive(Clone, Copy, Debug)]
pub struct LowestFeeChangeless {
    /// The estimated feerate needed to spend a potential change output later.
    pub long_term_feerate: FeeRate,
    /// The feerate used to determine the dust threshold of a potential change output.
    pub dust_relay_feerate: FeeRate,
    /// The weights of the potential change output.
    pub drain_weights: DrainWeights,
}

/// Every weight unit the input serialization could still gain, however this branch is extended.
///
/// A candidate's own weight is not the whole cost of adding it: the input-count varint can grow, a
/// first segwit input adds the witness header, and in a segwit transaction every legacy input
/// serializes an empty witness. So a candidate whose standalone effective value is positive can
/// still *lower* a selection's excess, which is what stops Core's window cut from porting
/// directly — Core's `GetSelectionAmount()` has no such count-dependent term. This is the loosest
/// upper bound that needs nothing per node: the varint at its largest, the witness header, and one
/// weight unit per candidate in case it is a legacy input.
fn max_future_input_overhead(problem: &SelectionProblem) -> u64 {
    varint_size(problem.len()) * 4 + 2 + problem.len() as u64
}

impl LowestFeeChangeless {
    fn lowest_fee(self) -> LowestFee {
        LowestFee {
            long_term_feerate: self.long_term_feerate,
            dust_relay_feerate: self.dust_relay_feerate,
            drain_weights: self.drain_weights,
        }
    }

    /// The largest excess a selection can carry and still be changeless.
    ///
    /// Read straight off [`LowestFee::drain_value`], which is what decides whether a selection
    /// counts as changeless here: change is refused, and the selection therefore accepted, while
    /// the excess stays at or below the change output's future spend cost, or below its dust
    /// threshold.
    fn changeless_excess_ceiling(&self) -> u64 {
        let spend_cost = self
            .long_term_feerate
            .implied_fee_wu(self.drain_weights.spend_weight);
        let dust_threshold = self.drain_weights.dust_threshold(self.dust_relay_feerate);
        spend_cost.max(dust_threshold.saturating_sub(1))
    }

    /// Whether this selection has overshot that ceiling with no way back for any descendant.
    ///
    /// This is Bitcoin Core's defining `SelectCoinsBnB` cut — it backtracks on
    /// `curr_value > selection_target + cost_of_change` — and, like Core's, it needs no incumbent,
    /// so it fires on the first descent rather than waiting for a solution the search may never
    /// reach.
    ///
    /// It holds only where excess cannot fall again on the way down, which takes three things.
    /// Every undecided candidate must raise the excess. Core gets that from its caller, which
    /// filters the pool; we cannot, because a candidate that costs more than it brings is exactly
    /// what burns an overshoot back down into range for this metric, so the pool is checked rather
    /// than filtered. There must be no unconfirmed ancestors, since one
    /// dragged in later raises the bump and lowers the excess. And there must be no `max_weight`,
    /// since a selection heavy enough that change no longer fits is changeless by that route
    /// whatever its excess.
    ///
    /// ponytail: the last two gates are conservative and cost the ancestry fixtures the cut
    /// entirely. The ancestor one wants an upper bound on the bump a descendant can still take on,
    /// mirroring the floor `ancestor_bump_lower_bound` already tracks; the weight one wants a test
    /// that no descendant can bust the cap with change.
    fn overshot_the_changeless_window(&self, cs: &SelectionView<'_>) -> bool {
        let target = cs.target();
        if !cs.every_undecided_candidate_is_worth_selecting()
            || cs.problem().has_ancestors()
            || target.max_weight.is_some()
        {
            return false;
        }
        let excess = cs.excess(Drain {
            weights: self.drain_weights,
            value: 0,
        });
        let ceiling = self.changeless_excess_ceiling()
            + target
                .fee
                .rate
                .implied_fee_wu(max_future_input_overhead(cs.problem()));
        excess > ceiling as i64
    }
}

impl From<LowestFee> for LowestFeeChangeless {
    fn from(metric: LowestFee) -> Self {
        Self {
            long_term_feerate: metric.long_term_feerate,
            dust_relay_feerate: metric.dust_relay_feerate,
            drain_weights: metric.drain_weights,
        }
    }
}

impl BnbMetric for LowestFeeChangeless {
    fn drain(&mut self, _cs: &SelectionView<'_>) -> Drain {
        Drain::NONE
    }

    fn score(&mut self, cs: &SelectionView<'_>) -> Option<Ordf32> {
        if !cs.is_funded()
            || !cs.is_within_max_weight(DrainWeights::NONE)
            || self.lowest_fee().drain_value(cs).is_some()
        {
            return None;
        }

        Some(Ordf32(
            cs.selected_value().saturating_sub(cs.target().value()) as f32,
        ))
    }

    fn bound(&mut self, cs: &SelectionView<'_>) -> Option<Ordf32> {
        if self.overshot_the_changeless_window(cs) {
            return None;
        }
        let mut lowest_fee = self.lowest_fee();
        let funding_bound = lowest_fee.bound(cs)?;
        if cs.problem().len() > 24 {
            return Some(funding_bound);
        }
        let no_change_fee = Ordf32(cs.selected_value().saturating_sub(cs.target().value()) as f32);
        Some(funding_bound.max(no_change_fee))
    }

    fn requires_ordering_by_descending_value_pwu(&self) -> bool {
        true
    }
}
