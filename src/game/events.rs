//! The real-time event log: what happened in the game, in order, for the
//! commander to catch up on with `wait_for_events`.

use std::collections::VecDeque;

/// Oldest events are dropped beyond this.
const CAPACITY: usize = 500;
/// The same text isn't logged again within this many tics.
const REPEAT_WINDOW: i32 = 350;

#[derive(Debug, Clone)]
pub struct Event {
    /// Increases by one per event; `wait_for_events` remembers the last seen.
    pub seq: u64,
    /// Game time when it happened, in seconds since the level started.
    pub level_time: f64,
    /// Important events wake the commander up early.
    pub important: bool,
    pub text: String,
    /// Game tic, for de-duplicating repeats.
    tic: i32,
}

#[derive(Default)]
pub struct EventLog {
    events: VecDeque<Event>,
    next_seq: u64,
}

impl EventLog {
    /// Record an event, unless the same text was recorded very recently.
    /// Returns whether it was recorded.
    pub fn push(
        &mut self,
        tic: i32,
        level_time: f64,
        important: bool,
        text: impl Into<String>,
    ) -> bool {
        let text = text.into();
        if self
            .events
            .iter()
            .rev()
            .any(|e| e.text == text && tic - e.tic < REPEAT_WINDOW)
        {
            return false;
        }
        self.next_seq += 1;
        self.events.push_back(Event {
            seq: self.next_seq,
            level_time,
            important,
            text,
            tic,
        });
        if self.events.len() > CAPACITY {
            self.events.pop_front();
        }
        true
    }

    /// Events after `seq`, oldest first.
    pub fn since(&self, seq: u64) -> Vec<Event> {
        self.events
            .iter()
            .filter(|e| e.seq > seq)
            .cloned()
            .collect()
    }

    pub fn has_important_since(&self, seq: u64) -> bool {
        self.events.iter().any(|e| e.seq > seq && e.important)
    }

    /// Sequence number of the newest event (0 if none).
    pub fn latest(&self) -> u64 {
        self.next_seq
    }
}
