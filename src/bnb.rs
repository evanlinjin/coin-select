use core::cmp::Reverse;

use crate::{float::Ordf32, Drain, SelectionCache, SelectionView};

use super::CoinSelector;
use alloc::collections::BinaryHeap;

/// An [`Iterator`] that iterates over rounds of branch and bound to minimize the score of the
/// provided [`BnbMetric`].
#[derive(Debug)]
pub(crate) struct BnbIter<'a, M: BnbMetric> {
    queue: BinaryHeap<Branch<'a>>,
    best: Option<Ordf32>,
    /// The `BnBMetric` that will score each selection
    pub(crate) metric: M,
}

impl<'a, M: BnbMetric> Iterator for BnbIter<'a, M> {
    type Item = Option<(CoinSelector<'a>, Ordf32)>;

    fn next(&mut self) -> Option<Self::Item> {
        // {
        //     println!("=========================== {:?}", self.best);
        //     for thing in self.queue.iter() {
        //         println!("{} {:?}", &thing.selector, thing.lower_bound);
        //     }
        //     let _ = std::io::stdin().read_line(&mut alloc::string::String::new());
        // }

        let branch = self.queue.pop()?;
        if let Some(best) = &self.best {
            // If the next thing in queue is not better than our best we're done.
            if *best < branch.lower_bound {
                // println!(
                //     "\t\t(SKIP) branch={} inclusion={} lb={:?}, score={:?}",
                //     branch.selector,
                //     !branch.is_exclusion,
                //     branch.lower_bound,
                //     self.metric.score(&branch.selector),
                // );
                return None;
            }
        }
        // println!(
        //     "\t\t( POP) branch={} inclusion={} lb={:?}, score={:?}",
        //     branch.selector,
        //     !branch.is_exclusion,
        //     branch.lower_bound,
        //     self.metric.score(&branch.selector),
        // );

        let Branch {
            selector,
            cache,
            is_exclusion,
            cursor,
            ..
        } = branch;

        let mut return_val = None;
        if !is_exclusion {
            if let Some(score) = self
                .metric
                .score(&SelectionView::with_cache(&selector, &cache))
            {
                let better = match self.best {
                    Some(best_score) => score < best_score,
                    None => true,
                };
                if better {
                    self.best = Some(score);
                    return_val = Some(score);
                }
            };
        }

        self.insert_new_branches(&selector, &cache, cursor);
        Some(return_val.map(|score| (selector, score)))
    }
}

impl<'a, M: BnbMetric> BnbIter<'a, M> {
    pub(crate) fn new(mut selector: CoinSelector<'a>, metric: M) -> Self {
        let mut iter = BnbIter {
            queue: BinaryHeap::default(),
            best: None,
            metric,
        };

        if iter.metric.requires_ordering_by_descending_value_pwu() {
            selector.sort_candidates_by_descending_value_pwu();
        }

        let cache = SelectionCache::from_selector(&selector);
        iter.consider_adding_to_queue(&selector, &cache, false, 0);

        iter
    }

    fn consider_adding_to_queue(
        &mut self,
        cs: &CoinSelector<'a>,
        cache: &SelectionCache,
        is_exclusion: bool,
        cursor: usize,
    ) {
        let bound = self.metric.bound(&SelectionView::with_cache(cs, cache));
        if let Some(bound) = bound {
            let is_good_enough = match self.best {
                Some(best) => best > bound,
                None => true,
            };
            if is_good_enough {
                let branch = Branch {
                    lower_bound: bound,
                    selector: cs.clone(),
                    cache: cache.clone(),
                    is_exclusion,
                    cursor,
                };
                /*println!(
                    "\t\t(PUSH) branch={} inclusion={} lb={:?} score={:?}",
                    branch.selector,
                    !branch.is_exclusion,
                    branch.lower_bound,
                    self.metric.score(&branch.selector),
                );*/
                self.queue.push(branch);
            } /* else {
                  println!(
                      "\t\t( REJ) branch={} inclusion={} lb={:?} score={:?}",
                      cs,
                      !is_exclusion,
                      bound,
                      self.metric.score(cs),
                  );
              }*/
        } /*else {
              println!(
                  "\t\t(NO B) branch={} inclusion={} score={:?}",
                  cs,
                  !is_exclusion,
                  self.metric.score(cs),
              );
          }*/
    }

    fn insert_new_branches(&mut self, cs: &CoinSelector<'a>, cache: &SelectionCache, start: usize) {
        let mut iter = cs.candidates().skip(start);
        let mut cursor = start;
        let (next_index, next) = loop {
            match iter.next() {
                None => return,
                Some((index, candidate)) => {
                    if !cs.is_selected(index) && !cs.banned().contains(index) {
                        break (index, candidate);
                    }
                    cursor += 1;
                }
            }
        };

        let mut inclusion_cs = cs.clone();
        let mut inclusion_cache = cache.clone();
        inclusion_cs.select(next_index);
        inclusion_cache.add(cs.problem(), next_index, next, true);
        self.consider_adding_to_queue(&inclusion_cs, &inclusion_cache, false, cursor + 1);

        // For the exclusion branch, we keep banning candidates that are interchangeable with the one
        // we just excluded: same value and weight, and dragging in exactly the same unconfirmed
        // ancestors (two coins of equal value and weight are *not* interchangeable if one of them
        // drags in an ancestor that needs bumping). Candidates are only compared until the first
        // mismatch, since this exploits them being adjacent in the sorted order.
        let mut exclusion_cs = cs.clone();
        let mut exclusion_cache = cache.clone();
        let to_ban = (
            next.value,
            next.weight,
            next.segwit_count,
            next.legacy_count,
        );
        let to_ban_drags_in = cs.problem().drags_in(next_index);
        exclusion_cs.ban(next_index);
        exclusion_cache.ban(cs.problem(), next_index);
        let mut exclusion_cursor = cursor + 1;
        for (next_index, next) in iter {
            if cs.is_selected(next_index) || cs.banned().contains(next_index) {
                exclusion_cursor += 1;
                continue;
            }
            if (
                next.value,
                next.weight,
                next.segwit_count,
                next.legacy_count,
            ) != to_ban
                || cs.problem().drags_in(next_index) != to_ban_drags_in
            {
                break;
            }
            exclusion_cs.ban(next_index);
            exclusion_cache.ban(cs.problem(), next_index);
            exclusion_cursor += 1;
        }
        self.consider_adding_to_queue(&exclusion_cs, &exclusion_cache, true, exclusion_cursor);
    }
}

#[derive(Debug, Clone)]
struct Branch<'a> {
    lower_bound: Ordf32,
    selector: CoinSelector<'a>,
    cache: SelectionCache,
    is_exclusion: bool,
    cursor: usize,
}

impl Ord for Branch<'_> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        // NOTE: Reverse comparision `lower_bound` because we want a min-heap (by default BinaryHeap
        // is a max-heap).
        // NOTE: We tiebreak equal scores based on whether it's exlusion or not (preferring
        // inclusion). We do this because we want to try and get to evaluating complete selection
        // returning actual scores as soon as possible.
        core::cmp::Ord::cmp(
            &(Reverse(&self.lower_bound), !self.is_exclusion),
            &(Reverse(&other.lower_bound), !other.is_exclusion),
        )
    }
}

impl PartialOrd for Branch<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Branch<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.lower_bound == other.lower_bound
    }
}

impl Eq for Branch<'_> {}

/// A branch and bound metric where we minimize the [`Ordf32`] score.
///
/// This is to be used as input for [`CoinSelector::run_bnb`] or [`CoinSelector::bnb_solutions`].
pub trait BnbMetric {
    /// Get the score of a given selection.
    ///
    /// If this returns `None`, the selection is invalid.
    fn score(&mut self, view: &SelectionView<'_>) -> Option<Ordf32>;

    /// Get the lower bound score using a heuristic.
    ///
    /// This represents the best possible score of all descendant branches (according to the
    /// heuristic).
    ///
    /// If this returns `None`, the current branch and all descendant branches will not have valid
    /// solutions.
    fn bound(&mut self, view: &SelectionView<'_>) -> Option<Ordf32>;

    /// The change output (a.k.a. drain) this metric decides on for the given selection,
    /// or [`Drain::NONE`] if it decides there should be no change.
    ///
    /// Call this on a branch-and-bound solution to get the change output the metric optimized against.
    fn drain(&mut self, view: &SelectionView<'_>) -> Drain;

    /// Returns whether the metric requies we order candidates by descending value per weight unit.
    fn requires_ordering_by_descending_value_pwu(&self) -> bool {
        false
    }
}
