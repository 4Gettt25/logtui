//! Capped ring buffer for incoming lines.
//!
//! Every line ever received gets a stable, monotonically increasing *absolute
//! number* (`u64`). The filter view stores absolute numbers, so it stays
//! correct even when old lines are evicted from the front of the ring.

use std::collections::VecDeque;

pub struct RingBuffer {
    lines: VecDeque<String>,
    cap: usize,
    /// Total lines ever pushed (== absolute number of the next line).
    total: u64,
    /// Lines evicted from the front (== absolute number of the oldest live line).
    dropped: u64,
}

impl RingBuffer {
    pub fn new(cap: usize) -> Self {
        debug_assert!(cap > 0);
        Self {
            lines: VecDeque::with_capacity(cap.min(1 << 20)),
            cap,
            total: 0,
            dropped: 0,
        }
    }

    /// Append a line, evicting the oldest one if at capacity.
    /// Returns the absolute number assigned to the new line.
    pub fn push(&mut self, line: String) -> u64 {
        if self.lines.len() == self.cap {
            self.lines.pop_front();
            self.dropped += 1;
        }
        let abs = self.total;
        self.lines.push_back(line);
        self.total += 1;
        abs
    }

    /// Number of lines currently held.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Number of lines evicted so far.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Fetch a line by absolute number (None if already evicted).
    pub fn get_abs(&self, abs: u64) -> Option<&str> {
        if abs < self.dropped || abs >= self.total {
            return None;
        }
        self.lines.get((abs - self.dropped) as usize).map(String::as_str)
    }

    /// Iterate `(abs_number, line)` over everything currently buffered.
    pub fn iter_abs(&self) -> impl Iterator<Item = (u64, &str)> {
        self.lines
            .iter()
            .enumerate()
            .map(move |(i, s)| (self.dropped + i as u64, s.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_oldest_and_keeps_abs_numbers_stable() {
        let mut b = RingBuffer::new(3);
        assert_eq!(b.push("a".into()), 0);
        assert_eq!(b.push("b".into()), 1);
        assert_eq!(b.push("c".into()), 2);
        assert_eq!(b.push("d".into()), 3); // evicts "a"
        assert_eq!(b.len(), 3);
        assert_eq!(b.dropped(), 1);
        assert_eq!(b.get_abs(0), None);
        assert_eq!(b.get_abs(1), Some("b"));
        assert_eq!(b.get_abs(3), Some("d"));
        let collected: Vec<_> = b.iter_abs().collect();
        assert_eq!(collected, vec![(1, "b"), (2, "c"), (3, "d")]);
    }
}
