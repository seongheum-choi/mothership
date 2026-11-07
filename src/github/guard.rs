//! Each GitHub delivery reaches an agent once; the hourly prompt cap is in
//! [`crate::review::intake`].

use std::collections::{HashSet, VecDeque};

/// Delivery ids remembered to drop GitHub's redeliveries and retries.
const DELIVERIES_KEPT: usize = 1000;

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
}
