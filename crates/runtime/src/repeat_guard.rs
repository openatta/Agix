//! Notices an agent calling one tool with the same input again and again
//! within a turn, and tells the model so at the 3rd, 5th and 8th call — the
//! runtime's way out of a loop, with no prompt text spent until one happens.
//! Advisory only: the call itself still runs.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

const NOTICE_AT: &[u32] = &[3, 5, 8];

#[derive(Default)]
pub(crate) struct RepeatGuard {
    turn: u32,
    seen: HashMap<u64, u32>,
}

impl RepeatGuard {
    /// Count one call in `turn`; the count when it is one worth a notice.
    pub(crate) fn record(
        &mut self,
        turn: u32,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<u32> {
        if turn != self.turn {
            self.turn = turn;
            self.seen.clear();
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (name, input.to_string()).hash(&mut h);
        let count = self.seen.entry(h.finish()).or_default();
        *count += 1;
        NOTICE_AT.contains(count).then_some(*count)
    }
}

pub(crate) fn notice(name: &str, count: u32, interactive: bool) -> String {
    let way_out = match interactive {
        true => "or stop and tell the user what you know and where you are stuck",
        false => "or stop and report what you know, what you tried and what is blocking you",
    };
    format!(
        "<system-reminder>\nYou have called {name} with the same input {count} times this turn. \
         Look at what it returned: it will not change by asking again. Change approach, \
         {way_out}.\n</system-reminder>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_at_three_five_and_eight_within_a_turn() {
        let mut guard = RepeatGuard::default();
        let input = serde_json::json!({"command": "make"});
        let counts: Vec<_> = (0..9).map(|_| guard.record(1, "Bash", &input)).collect();
        assert_eq!(counts.iter().flatten().collect::<Vec<_>>(), [&3, &5, &8]);
        assert_eq!(
            guard.record(1, "Bash", &serde_json::json!({"command": "ls"})),
            None
        );
        assert_eq!(
            guard.record(2, "Bash", &input),
            None,
            "a new turn starts over"
        );
    }

    #[test]
    fn the_way_out_depends_on_who_is_there() {
        assert!(notice("Read", 3, true).contains("tell the user"));
        assert!(notice("Read", 3, false).contains("report what you know"));
    }
}
