use core::ops::{Add, Sub};

/// Fee rate
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
// Internally stored as satoshi per 1000 vbytes, the same unit as Bitcoin Core's `CFeeRate`.
//
// This is an exact integer rather than a float on purpose. The fees derived from it
// ([`implied_fee`](Self::implied_fee) and friends) are whole satoshis that feed
// `CoinSelector::excess` and hence `is_funded`, so a rounding error there doesn't merely pick a
// worse selection — it calls a selection funded when it is short of the target feerate, silently.
//
// The old `f32` computation was exact for everyday transactions and only diverged once the implied
// fee outgrew `f32`'s exact-integer range: from ~2^22 sats for `implied_fee_wu` (reached by e.g. a
// 28k wu tx at 599 sat/vb) and ~2^24 sats for `implied_fee` (a 112k wu tx at 599 sat/vb). Past
// those points it was off by up to 2 sats in *either* direction — including underpaying. Integers
// cost nothing here and remove the threshold entirely.
pub struct FeeRate(u64);

/// Weight units per 1000 vbytes.
pub(crate) const WU_PER_KVB: u128 = 4000;
/// Vbytes per 1000 vbytes.
const VB_PER_KVB: u128 = 1000;

/// `ceil(numerator / denominator)`, saturating at `u64::MAX` instead of wrapping.
fn ceil_div(numerator: u128, denominator: u128) -> u64 {
    let ceiled = (numerator + (denominator - 1)) / denominator;
    if ceiled > u64::MAX as u128 {
        u64::MAX
    } else {
        ceiled as u64
    }
}

impl FeeRate {
    /// A feerate of zero
    pub const ZERO: Self = Self(0);
    /// The default minimum relay fee that bitcoin core uses (1 sat per vbyte). The feerate your transaction has must
    /// be at least this to be forwarded by most nodes on the network.
    pub const DEFAULT_MIN_RELAY: Self = Self(1_000);
    /// The defualt incremental relay fee that bitcoin core uses (1 sat per vbyte). You must pay
    /// this fee over the fee of the transaction(s) you are replacing by through the replace-by-fee
    /// mechanism. This feerate is applied to the transaction that is replacing the old
    /// transactions.
    pub const DEFUALT_RBF_INCREMENTAL_RELAY: Self = Self(1_000);

    /// Create a new instance from a satoshi/kvb value, checking it and rounding it to the nearest
    /// whole satoshi/kvb.
    ///
    /// ## Panics
    ///
    /// Panics if the value is not [normal](https://doc.rust-lang.org/std/primitive.f32.html#method.is_normal) (except if it's a positive zero) or negative.
    fn new_checked(sat_per_kvb: f32) -> Self {
        assert!(sat_per_kvb.is_normal() || sat_per_kvb == 0.0);
        assert!(sat_per_kvb.is_sign_positive());

        // Round to nearest. `core` has no `f32::round`, and the value is known non-negative here,
        // so adding a half and truncating does it.
        Self((sat_per_kvb as f64 + 0.5) as u64)
    }

    /// Create a new instance of [`FeeRate`] given a float fee rate in btc/kvbytes
    ///
    /// ## Panics
    ///
    /// Panics if the value is not [normal](https://doc.rust-lang.org/std/primitive.f32.html#method.is_normal) (except if it's a positive zero) or negative.
    pub fn from_btc_per_kvb(btc_per_kvb: f32) -> Self {
        Self::new_checked(btc_per_kvb * 1e8)
    }

    /// Create a new instance of [`FeeRate`] given a float fee rate in satoshi/vbyte
    ///
    /// ## Panics
    ///
    /// Panics if the value is not [normal](https://doc.rust-lang.org/std/primitive.f32.html#method.is_normal) (except if it's a positive zero) or negative.
    pub fn from_sat_per_vb(sat_per_vb: f32) -> Self {
        Self::new_checked(sat_per_vb * VB_PER_KVB as f32)
    }

    /// Create a new [`FeeRate`] with the default min relay fee value
    #[deprecated(note = "use the DEFAULT_MIN_RELAY constant instead")]
    pub const fn default_min_relay_fee() -> Self {
        Self(1_000)
    }

    /// Calculate fee rate from `fee` and weight units (`wu`), rounding down.
    pub fn from_wu(fee: u64, wu: usize) -> Self {
        Self(((fee as u128 * WU_PER_KVB) / wu as u128) as u64)
    }

    /// Calculate feerate from `satoshi/wu`.
    pub fn from_sat_per_wu(sats_per_wu: f32) -> Self {
        Self::new_checked(sats_per_wu * WU_PER_KVB as f32)
    }

    /// Calculate fee rate from `fee` and `vbytes`, rounding down.
    pub fn from_vb(fee: u64, vbytes: usize) -> Self {
        Self(((fee as u128 * VB_PER_KVB) / vbytes as u128) as u64)
    }

    /// Return the value as satoshi per 1000 vbytes. This is the exact internal representation;
    /// prefer it over [`spwu`](Self::spwu) and [`as_sat_vb`](Self::as_sat_vb) when you need to do
    /// exact arithmetic.
    pub fn sat_per_kvb(&self) -> u64 {
        self.0
    }

    /// Return the value as satoshi/vbyte.
    pub fn as_sat_vb(&self) -> f32 {
        self.0 as f32 / VB_PER_KVB as f32
    }

    /// Return the value as satoshi/wu.
    pub fn spwu(&self) -> f32 {
        self.0 as f32 / WU_PER_KVB as f32
    }

    /// The fee that the transaction with weight `tx_weight` should pay in order to satisfy the fee rate given by `self`,
    /// where the fee rate is applied to the rounded-up vbytes obtained from `tx_weight`.
    pub fn implied_fee(&self, tx_weight: u64) -> u64 {
        let vbytes = (tx_weight as u128 + 3) / 4;
        ceil_div(vbytes * self.0 as u128, VB_PER_KVB)
    }

    /// Same as [implied_fee](Self::implied_fee) except the fee rate given by `self` is applied to `tx_weight` directly.
    pub fn implied_fee_wu(&self, tx_weight: u64) -> u64 {
        ceil_div(tx_weight as u128 * self.0 as u128, WU_PER_KVB)
    }
}

impl Add<FeeRate> for FeeRate {
    type Output = Self;

    fn add(self, rhs: FeeRate) -> Self::Output {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl Sub<FeeRate> for FeeRate {
    type Output = Self;

    /// Saturates at [`FeeRate::ZERO`] — a negative feerate isn't representable.
    fn sub(self, rhs: FeeRate) -> Self::Output {
        Self(self.0.saturating_sub(rhs.0))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// The unit conversions must round-trip through the documented accessors.
    #[test]
    fn unit_conversions() {
        assert_eq!(FeeRate::from_sat_per_vb(1.0), FeeRate::DEFAULT_MIN_RELAY);
        assert_eq!(FeeRate::from_sat_per_wu(0.25), FeeRate::DEFAULT_MIN_RELAY);
        assert_eq!(FeeRate::from_btc_per_kvb(1e-5), FeeRate::DEFAULT_MIN_RELAY);
        assert_eq!(FeeRate::from_vb(10, 10), FeeRate::DEFAULT_MIN_RELAY);
        assert_eq!(FeeRate::from_wu(10, 40), FeeRate::DEFAULT_MIN_RELAY);
        assert_eq!(FeeRate::DEFAULT_MIN_RELAY.as_sat_vb(), 1.0);
        assert_eq!(FeeRate::DEFAULT_MIN_RELAY.spwu(), 0.25);
        assert_eq!(FeeRate::DEFAULT_MIN_RELAY.sat_per_kvb(), 1000);
    }

    /// `implied_fee` charges whole (rounded-up) vbytes; `implied_fee_wu` charges weight directly.
    #[test]
    fn implied_fees_round_up() {
        let ten_sat_vb = FeeRate::from_sat_per_vb(10.0);
        // 9 wu -> 3 vbytes -> 30 sats
        assert_eq!(ten_sat_vb.implied_fee(9), 30);
        assert_eq!(ten_sat_vb.implied_fee(12), 30);
        assert_eq!(ten_sat_vb.implied_fee(13), 40);
        // applied to weight directly: 9 wu * 2.5 sat/wu = 22.5 -> 23
        assert_eq!(ten_sat_vb.implied_fee_wu(9), 23);
        assert_eq!(ten_sat_vb.implied_fee_wu(12), 30);

        assert_eq!(FeeRate::ZERO.implied_fee(1000), 0);
        assert_eq!(FeeRate::ZERO.implied_fee_wu(1000), 0);
    }

    /// The whole point of the integer representation: the fee is exactly `ceil(weight * feerate)`
    /// with no float rounding drift, at *any* magnitude.
    ///
    /// The range here deliberately runs past where `f32` loses whole satoshis (implied fees above
    /// ~2^22 / ~2^24 sats); the old computation was off by up to 2 sats there, in either direction.
    #[test]
    fn implied_fees_are_exact() {
        for sat_vb in 1..600_u64 {
            let feerate = FeeRate::from_sat_per_vb(sat_vb as f32);
            for weight in (200..400_000_u64).step_by(311) {
                assert_eq!(
                    feerate.implied_fee(weight),
                    ((weight + 3) / 4) * sat_vb,
                    "implied_fee({}) at {} sat/vb",
                    weight,
                    sat_vb
                );
                assert_eq!(
                    feerate.implied_fee_wu(weight),
                    (weight * sat_vb + 3) / 4,
                    "implied_fee_wu({}) at {} sat/vb",
                    weight,
                    sat_vb
                );
            }
        }
    }

    /// A feerate large enough to overflow `u64` in the intermediate product must saturate rather
    /// than wrap to a tiny fee (which would read as "this selection is funded").
    #[test]
    fn implied_fee_saturates_instead_of_wrapping() {
        let absurd = FeeRate::from_sat_per_vb(1e18);
        assert_eq!(absurd.implied_fee(400_000), u64::MAX);
        assert_eq!(absurd.implied_fee_wu(400_000), u64::MAX);
    }

    /// The invariant the integer representation exists to protect: `is_funded` must flip exactly at
    /// the satoshi that covers the target feerate — never one or two sats early, which would call a
    /// selection funded while the transaction is short of its feerate.
    ///
    /// Probes the boundary itself, where an off-by-a-couple-sats fee is decisive. The sweep runs
    /// out to large weights and high feerates on purpose: that is where the old `f32` fee lost
    /// whole satoshis and moved this boundary.
    #[test]
    fn is_funded_flips_exactly_at_the_required_fee() {
        use crate::{Candidate, CoinSelector, DrainWeights, Target, TargetFee, TargetOutputs};

        let outputs = TargetOutputs {
            value_sum: 99_000,
            weight_sum: 400,
            n_outputs: 2,
        };

        for weight in (200..500_000_u64).step_by(4_093) {
            for sat_vb in [1u64, 3, 10, 50, 135, 337, 599] {
                let target = Target {
                    fee: TargetFee::from_feerate(FeeRate::from_sat_per_vb(sat_vb as f32)),
                    outputs,
                    max_weight: None,
                };

                // The tx weight doesn't depend on the candidate's value, so compute the required
                // fee once, independently, in exact integer arithmetic.
                let probe = [Candidate {
                    value: 0,
                    weight,
                    input_count: 1,
                    is_segwit: true,
                }];
                let mut cs = CoinSelector::new(&probe);
                cs.select(0);
                let tx_weight = cs.weight(outputs, DrainWeights::NONE);
                let required = (((tx_weight as u128 + 3) / 4) * sat_vb as u128) as u64;
                let exact = outputs.value_sum + required;

                for (value, want_funded) in [(exact - 1, false), (exact, true), (exact + 1, true)] {
                    let candidates = [Candidate {
                        value,
                        weight,
                        input_count: 1,
                        is_segwit: true,
                    }];
                    let mut cs = CoinSelector::new(&candidates);
                    cs.select(0);
                    assert_eq!(
                        cs.is_funded(target),
                        want_funded,
                        "weight={} sat/vb={} value={} (exact boundary {}, required fee {})",
                        weight,
                        sat_vb,
                        value,
                        exact,
                        required
                    );
                }
            }
        }
    }

    #[test]
    fn sub_saturates_at_zero() {
        assert_eq!(
            FeeRate::from_sat_per_vb(1.0) - FeeRate::from_sat_per_vb(5.0),
            FeeRate::ZERO
        );
    }
}
