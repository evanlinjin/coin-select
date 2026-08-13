use crate::{float::Ordf32, BnbMetric, Drain, DrainWeights, FeeRate, SelectionView};

use super::LowestFee;

/// Metric that minimizes fees while only accepting selections for which [`LowestFee`] chooses no
/// change output.
///
/// This reuses [`LowestFee`]'s change decision, including the future cost of spending change, its
/// dust threshold, and the transaction weight cap. A selection is only valid here when that
/// decision returns no change output.
///
/// Unlike constraining an arbitrary metric after the fact, this metric has a changeless-specific
/// lower bound. A changeless selection's score is its selected value minus the target value. Since
/// selected value can only increase down a branch, the current no-change fee is a lower bound for
/// every descendant, including when unconfirmed ancestry makes funding non-monotone. The bound
/// combines that fact with [`LowestFee`]'s funding relaxation.
#[derive(Clone, Copy, Debug)]
pub struct LowestFeeChangeless {
    /// The estimated feerate needed to spend a potential change output later.
    pub long_term_feerate: FeeRate,
    /// The feerate used to determine the dust threshold of a potential change output.
    pub dust_relay_feerate: FeeRate,
    /// The weights of the potential change output.
    pub drain_weights: DrainWeights,
}

impl LowestFeeChangeless {
    fn lowest_fee(self) -> LowestFee {
        LowestFee {
            long_term_feerate: self.long_term_feerate,
            dust_relay_feerate: self.dust_relay_feerate,
            drain_weights: self.drain_weights,
        }
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
