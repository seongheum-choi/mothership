//! Limits on what reaches an agent from GitHub: each delivery once, and a capped number of
//! prompts per session per hour.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

/// Delivery ids remembered to drop GitHub's redeliveries and retries.
const DELIVERIES_KEPT: usize = 1000;

/// GitHub-started prompts a session may take per window before it is paused, so a reply loop
/// the marker misses (the agent posting with plain `gh`) burns out quickly.
pub(super) const PROMPTS_PER_WINDOW: usize = 10;
const WINDOW: Duration = Duration::from_hours(1);

/// Delivery ids already handled, oldest dropped first.
#[derive(Default)]
pub(super) struct Deliveries {
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl Deliveries {
    /// Records `id`; false when it was already recorded. An empty id (never sent by GitHub)
    /// can't be deduplicated and always passes.
    pub(super) fn first_time(&mut self, id: &str) -> bool {
        if id.is_empty() {
            return true;
        }
        if !self.seen.insert(id.to_string()) {
            return false;
        }
        self.order.push_back(id.to_string());
        if self.order.len() > DELIVERIES_KEPT
            && let Some(old) = self.order.pop_front()
        {
            self.seen.remove(&old);
        }
        true
    }
}

/// GitHub-started prompts per session within the last [`WINDOW`].
#[derive(Default)]
pub(super) struct Budget {
    granted: HashMap<String, VecDeque<Instant>>,
    /// When each session was last told it is paused.
    notified: HashMap<String, Instant>,
}

#[derive(Debug, PartialEq)]
pub(super) enum Allowance {
    Granted,
    /// `notify` is true for the first refusal of a pause, so Linear hears of it once.
    Denied {
        notify: bool,
    },
}

impl Budget {
    pub(super) fn take(&mut self, key: &str, now: Instant) -> Allowance {
        let granted = self.granted.entry(key.to_string()).or_default();
        while granted
            .front()
            .is_some_and(|&t| now.duration_since(t) >= WINDOW)
        {
            granted.pop_front();
        }
        if granted.len() < PROMPTS_PER_WINDOW {
            granted.push_back(now);
            return Allowance::Granted;
        }
        let notify = self
            .notified
            .get(key)
            .is_none_or(|&t| now.duration_since(t) >= WINDOW);
        if notify {
            self.notified.insert(key.to_string(), now);
        }
        Allowance::Denied { notify }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deliveries_are_handled_once() {
        let mut seen = Deliveries::default();
        assert!(seen.first_time("a"));
        assert!(!seen.first_time("a"), "redelivery");
        assert!(seen.first_time(""));
        assert!(seen.first_time(""), "no id, nothing to compare");
        for i in 0..DELIVERIES_KEPT {
            assert!(seen.first_time(&i.to_string()));
        }
        assert!(seen.first_time("a"), "oldest id forgotten");
        assert!(!seen.first_time(&(DELIVERIES_KEPT - 1).to_string()));
        assert_eq!(seen.seen.len(), DELIVERIES_KEPT);
    }

    #[test]
    fn budget_pauses_a_session_after_too_many_prompts() {
        let mut budget = Budget::default();
        let t0 = Instant::now();
        for i in 0..PROMPTS_PER_WINDOW {
            let t = t0 + Duration::from_secs(i as u64);
            assert_eq!(budget.take("s", t), Allowance::Granted);
        }
        let later = t0 + Duration::from_secs(60);
        assert_eq!(budget.take("s", later), Allowance::Denied { notify: true });
        assert_eq!(budget.take("s", later), Allowance::Denied { notify: false });
        assert_eq!(budget.take("other", later), Allowance::Granted);
        // The first prompt leaves the window, freeing one slot.
        assert_eq!(budget.take("s", t0 + WINDOW), Allowance::Granted);
        assert_eq!(
            budget.take("s", t0 + WINDOW),
            Allowance::Denied { notify: false }
        );
        assert_eq!(
            budget.take("s", later + WINDOW + Duration::from_secs(10)),
            Allowance::Granted
        );
    }
}
