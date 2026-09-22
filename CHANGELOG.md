# Unreleased

- **Breaking:** Replace `Candidate`'s `input_count` and `is_segwit` fields with `segwit_count` and `legacy_count`, fixing `CoinSelector::input_weight` undercounting candidates that group multiple inputs: in a segwit transaction every legacy input still serializes an empty witness (1 WU), which was previously paid once per candidate instead of once per legacy input, so a group of N legacy inputs came out N-1 WU short. Splitting the count by script type also means a single candidate may now mix legacy and segwit inputs and still be priced exactly. Branch and bound also stops treating candidates of equal value and weight as interchangeable when their segwit/legacy counts differ, which could make it skip a cheaper selection. Replaces `Candidate::new` with `Candidate::new_segwit` and `Candidate::new_legacy`.
- **Breaking:** `CoinSelector` now owns its `Target`. `CoinSelector::new(candidates, target)` takes it and `CoinSelector::target()` returns it, and it is fixed for the life of the selector. Every method that took a `target: Target` argument no longer does, including `excess`, `missing`, `rate_excess`, `implied_fee`, `is_funded`, `is_within_max_weight`, `drain`, `select_until_target_met`, `select_srd`, `run_bnb`, and `bnb_solutions`. The same goes for arguments that merely restated part of the target: `weight` and `implied_feerate` no longer take `TargetOutputs`, `fee` no longer takes `target_value`, and `effective_value` and `select_all_effective` no longer take a `FeeRate`. `BnbMetric`'s methods read the target from the `CoinSelector` they are given — they are now `fn score(&mut self, cs: &CoinSelector<'_>) -> Option<Ordf32>`, `fn bound(&mut self, cs: &CoinSelector<'_>) -> Option<Ordf32>` and `fn drain(&mut self, cs: &CoinSelector<'_>) -> Drain` — so `LowestFee` no longer stores a `target` field. To measure a selection against a second target, use `CoinSelector::with_target(target)`, which copies the selection, the bans and the candidate order over to the new target; `CoinSelector::new` starts from an empty selection.
- **Breaking:** `BnbMetric` metrics now decide the change output themselves. The trait gains a `drain(&mut self, cs) -> Drain` method; call it on a branch-and-bound solution (or the `LowestFee` metric directly) to get the change output the metric optimized against, instead of computing a separate `ChangePolicy`.
- **Breaking:** `CoinSelector::run_bnb` now returns `(Ordf32, Drain)` instead of just `Ordf32`, handing back the change output the metric decided on for the winning selection.
- **Breaking:** `LowestFee` no longer takes a `change_policy`. It now takes `dust_relay_feerate: FeeRate` and `drain_weights: DrainWeights`, and adds change only when doing so lowers the long-term fee and the change would not be dust.
- Add `DrainWeights::dust_threshold(dust_relay_feerate)`, the minimum value a change output with these weights must have to not be dust.
- Add `CoinSelector::select_srd`, a Single Random Draw selector (port of Bitcoin Core's `SelectCoinsSRD`) that adds candidates in random order until the change reaches `change_lower`, producing a healthy-sized (privacy-friendly) change output instead of minimizing fees. Adds the `CHANGE_LOWER` constant for Core's value.
- Search branch and bound depth-first (better-bound child first, backtracking in place) instead of best-first over a heap of cloned branches. Only the current path is held in memory, and under a round cap it reaches complete selections on large pools where the old frontier often ran out of rounds first.
- **Breaking:** Remove the `Changeless` metric and the `BnbMetric` tuple implementations (`impl BnbMetric for ((A, f32), ...)`). Generic metric composition is no longer supported. `LowestFee` decides for itself whether a selection should carry change (adding one only when it lowers the long-term fee, clears the dust threshold, and fits `Target::max_weight`), so a separate changeless objective duplicates that decision and then constrains it. Callers that required a changeless transaction should use `LowestFee` and inspect the returned `Drain`.
- **Breaking:** `CoinSelector::selected_indices` and `CoinSelector::banned` now return `&Bitset` instead of `&BTreeSet<usize>`. `Bitset` exposes `contains`/`len`/`is_empty`/`iter` (#46)
- Replace the internal `Cow<BTreeSet>`/`Cow<[usize]>` selection state with a `Bitset` and an `Arc`-shared candidate order, making the per-branch clones in branch-and-bound substantially cheaper (#46)
- Fix compilation error when building with `--no-default-features` (#36)

# 0.4.0

- Use `u64` for weights instead of u32
- Fix feerate not being rounded up to vbytes #29
- Fix `new_tr_keyspend` weight

# 0.3.0

- Remove `is_target_met_with_change_policy`: it was redundant. If the target is met without a change policy it will always be met with it.
- Remove `min_fee` in favour of `replace` which allows you to replace a transaction
- Remove `Drain` argument from `CoinSelector::select_until_target_met` because adding a drain won't
  change when the target is met.
- No more `base_weight` in `CoinSelector`. Weight of the outputs is tracked in `target`.
- You now account for the number of outputs in both drain and target and their weight.
- Removed waste metric because it was pretty broken and took a lot to maintain

