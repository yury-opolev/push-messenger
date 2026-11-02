//! An agent's recent states, for the details window.

use std::collections::VecDeque;

/// How many entries a [`History`] keeps.
pub const HISTORY_CAP: usize = 50;

/// The last [`HISTORY_CAP`] states, oldest first: `(Unix ms, state label)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    entries: VecDeque<(i64, String)>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an entry, dropping the oldest beyond [`HISTORY_CAP`].
    pub fn push(&mut self, at_ms: i64, label: impl Into<String>) {
        if self.entries.len() == HISTORY_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back((at_ms, label.into()));
    }

    /// The entries, oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &(i64, String)> + ExactSizeIterator {
        self.entries.iter()
    }

    /// The newest entry.
    pub fn last(&self) -> Option<&(i64, String)> {
        self.entries.back()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_keep_their_order() {
        let mut history = History::new();
        assert!(history.is_empty());
        history.push(1, "starting");
        history.push(2, "connected, idle");
        let entries: Vec<_> = history.iter().cloned().collect();
        assert_eq!(
            entries,
            [
                (1, "starting".to_owned()),
                (2, "connected, idle".to_owned())
            ]
        );
        assert_eq!(history.last(), Some(&(2, "connected, idle".to_owned())));
    }

    #[test]
    fn only_the_last_fifty_are_kept() {
        let mut history = History::new();
        for n in 0..120 {
            history.push(n, format!("state {n}"));
        }
        assert_eq!(history.len(), HISTORY_CAP);
        let times: Vec<i64> = history.iter().map(|(at, _)| *at).collect();
        assert_eq!(times, (70..120).collect::<Vec<_>>());
        assert_eq!(history.last().unwrap().1, "state 119");
    }
}
