use crate::{varint_size, FeeRate, TR_KEYSPEND_TXIN_WEIGHT, TR_SPK_WEIGHT, TXOUT_BASE_WEIGHT};

/// Represents the weight costs of a drain (a.k.a. change) output.
///
/// May also represent multiple outputs.
#[derive(Default, Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct DrainWeights {
    /// The weight of including this drain output.
    ///
    /// This must not take into account any weight change from varint output count.
    pub output_weight: u64,
    /// The weight of spending this drain output (in the future).
    pub spend_weight: u64,
    /// The total number of outputs that the drain will use
    pub n_outputs: usize,
}

impl DrainWeights {
    /// `DrainWeights` for an output that will be spent with a taproot keyspend
    pub const TR_KEYSPEND: Self = Self {
        output_weight: TXOUT_BASE_WEIGHT + TR_SPK_WEIGHT,
        spend_weight: TR_KEYSPEND_TXIN_WEIGHT,
        n_outputs: 1,
    };

    /// `DrainWeights` for no drain at all
    pub const NONE: Self = Self {
        output_weight: 0,
        spend_weight: 0,
        n_outputs: 0,
    };

    /// The output weight this drain adds to the transaction, including the extra varint weight
    /// from growing the output count past `n_target_outputs`.
    fn added_output_weight(&self, n_target_outputs: usize) -> u64 {
        let extra_varint_weight =
            (varint_size(n_target_outputs + self.n_outputs) - varint_size(n_target_outputs)) * 4;
        self.output_weight + extra_varint_weight
    }

    /// The waste of adding this drain to a transaction according to the [waste metric].
    ///
    /// To get the precise answer you need to pass in the number of non-drain outputs (`n_target_outputs`) that you're
    /// adding to the transaction so we can include the cost of increasing the varint size of the output length.
    ///
    /// This is a search heuristic, so it stays a float. Where the answer is a whole number of
    /// satoshis, the crate uses the exact integer counterpart `waste_ceil` instead.
    ///
    /// [waste metric]: https://bitcoin.stackexchange.com/questions/113622/what-does-waste-metric-mean-in-the-context-of-coin-selection
    pub fn waste(
        &self,
        feerate: FeeRate,
        long_term_feerate: FeeRate,
        n_target_outputs: usize,
    ) -> f32 {
        self.added_output_weight(n_target_outputs) as f32 * feerate.spwu()
            + self.spend_weight as f32 * long_term_feerate.spwu()
    }

    /// Exact-integer [`waste`](Self::waste) rounded **down**.
    ///
    /// This feeds `LowestFee`'s lower bound, which stays admissible only while every credit it
    /// gives is an under-estimate of the real cost — so this must never overstate. The real cost
    /// rounds each fee component up, so flooring the exact sum is always at or below it.
    pub(crate) fn waste_floor(
        &self,
        feerate: FeeRate,
        long_term_feerate: FeeRate,
        n_target_outputs: usize,
    ) -> u64 {
        let scaled = self.added_output_weight(n_target_outputs) as u128
            * feerate.sat_per_kvb() as u128
            + self.spend_weight as u128 * long_term_feerate.sat_per_kvb() as u128;
        (scaled / crate::feerate::WU_PER_KVB) as u64
    }

    /// Exact-integer counterpart of [`waste`](Self::waste): the satoshis it costs to add this
    /// drain, rounding each fee component up.
    fn waste_ceil(
        &self,
        feerate: FeeRate,
        long_term_feerate: FeeRate,
        n_target_outputs: usize,
    ) -> u64 {
        feerate.implied_fee_wu(self.added_output_weight(n_target_outputs))
            + self.spend_fee(long_term_feerate)
    }

    /// The fee you will pay to spend these change output(s) in the future.
    pub fn spend_fee(&self, long_term_feerate: FeeRate) -> u64 {
        long_term_feerate.implied_fee_wu(self.spend_weight)
    }

    /// The minimum value a change output with these weights must have to not be considered dust
    /// according to `dust_relay_feerate`.
    ///
    /// A change output is dust when the fee to relay it plus the fee to later spend it exceeds its
    /// value, so the threshold is `dust_relay_feerate` applied to the output weight plus the spend
    /// weight.
    pub fn dust_threshold(&self, dust_relay_feerate: FeeRate) -> u64 {
        dust_relay_feerate.implied_fee_wu(self.output_weight + self.spend_weight)
    }
}

/// A drain (A.K.A. change) output.
/// Technically it could represent multiple outputs.
///
/// This is returned from [`CoinSelector::drain`]. Note if `drain` returns a drain where `is_none()`
/// returns true then **no change should be added** to the transaction.
///
/// [`CoinSelector::drain`]: crate::CoinSelector::drain
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Drain {
    /// Weight of adding drain output and spending the drain output.
    pub weights: DrainWeights,
    /// The value that should be assigned to the drain.
    pub value: u64,
}

impl Drain {
    /// The drain which represents no drain at all. We could but don't use `Option` because this
    /// causes friction internally, instead we just use a `Drain` with all 0 values.
    pub const NONE: Self = Drain {
        weights: DrainWeights::NONE,
        value: 0,
    };

    /// is the "none" drain
    pub fn is_none(&self) -> bool {
        self == &Drain::NONE
    }

    /// Is not the "none" drain
    pub fn is_some(&self) -> bool {
        !self.is_none()
    }
}

/// Describes when a change output (although it could represent several) should be added that drains
/// the excess in the coin selection. It includes the `drain_weights` to account for the cost of
/// adding this outupt(s).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChangePolicy {
    /// The minimum amount of excess there needs to be add a change output.
    pub min_value: u64,
    /// The weights of the drain that would be added according to the policy.
    pub drain_weights: DrainWeights,
}

impl ChangePolicy {
    /// Construct a change policy that creates change when the change value is greater than
    /// `min_value`.
    pub fn min_value(drain_weights: DrainWeights, min_value: u64) -> Self {
        Self {
            drain_weights,
            min_value,
        }
    }

    /// Construct a change policy that creates change when it would reduce the transaction waste
    /// given that `min_value` is respected.
    pub fn min_value_and_waste(
        drain_weights: DrainWeights,
        min_value: u64,
        target_feerate: FeeRate,
        long_term_feerate: FeeRate,
    ) -> Self {
        // The output waste of a changeless solution is the excess.
        let waste_with_change = drain_weights.waste_ceil(
            target_feerate,
            long_term_feerate,
            0, /* ignore varint cost for now */
        );

        Self {
            drain_weights,
            min_value: waste_with_change.max(min_value),
        }
    }
}
