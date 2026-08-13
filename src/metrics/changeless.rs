use crate::{bnb::BnbMetric, float::Ordf32, Drain, SelectionView};

/// Constrains an `inner` metric to only changeless solutions.
///
/// A selection is scored by `inner` only if the inner metric decides it should *not* have a change
/// output (see [`BnbMetric::drain`]); otherwise it is treated as invalid. This lets you find, for
/// example, the lowest-fee changeless solution via `Changeless<LowestFee>`.
#[derive(Clone, Copy, Debug)]
pub struct Changeless<M>(
    /// The inner metric that scores changeless solutions and owns the change decision.
    pub M,
);

impl<M: BnbMetric> BnbMetric for Changeless<M> {
    fn drain(&mut self, _cs: &SelectionView<'_>) -> Drain {
        // by definition a changeless selection never has a change output
        Drain::NONE
    }

    fn score(&mut self, cs: &SelectionView<'_>) -> Option<Ordf32> {
        // Reject selections that have change. We don't need an explicit target-met check: `inner`
        // returns `None` for invalid (e.g. not-target-met) selections.
        //
        // NOTE: for metrics whose `score` recomputes the drain (e.g. `LowestFee`), this evaluates
        // the drain decision twice per node. Sharing it would mean threading the drain into
        // `score`, which we avoid to keep metrics composable.
        if self.0.drain(cs).is_some() {
            return None;
        }
        self.0.score(cs)
    }

    fn bound(&mut self, cs: &SelectionView<'_>) -> Option<Ordf32> {
        // The changeless-constrained optimum is no better than the inner metric's unconstrained
        // optimum, so the inner bound is a valid lower bound. Change-unavoidability pruning is not
        // generally sound because candidate marginal fees depend on vbyte rounding, RBF, framing,
        // and ancestry.
        self.0.bound(cs)
    }

    fn requires_ordering_by_descending_value_pwu(&self) -> bool {
        self.0.requires_ordering_by_descending_value_pwu()
    }

    fn deduplicate_equivalent_candidates(&self) -> bool {
        self.0.deduplicate_equivalent_candidates()
    }
}
