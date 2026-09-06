//! A cancelable timer wheel (binary heap of deadlines) used by the manager
//! event loop for start/stop timeouts, restart delays, and timer elapses.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimerKind {
    /// TimeoutStartSec.
    StartTimeout,
    /// TimeoutStopSec / final SIGKILL grace.
    StopTimeout,
    /// Restart= delay between runs.
    RestartDelay,
    /// An OnCalendar elapse (holds the calendar-spec index).
    CalendarElapse(usize),
    /// A monotonic elapse (OnBootSec / OnUnitActiveSec / ...).
    MonotonicElapse,
}

#[derive(Debug, Clone)]
pub struct TimerEntry {
    pub when: Instant,
    pub id: u64,
    pub kind: TimerKind,
    pub unit: String,
}

#[derive(Debug, Default)]
pub struct TimerWheel {
    heap: BinaryHeap<Reverse<(Instant, u64)>>,
    entries: HashMap<u64, TimerEntry>,
    next_id: u64,
}

impl TimerWheel {
    pub fn schedule(&mut self, when: Instant, kind: TimerKind, unit: &str) -> u64 {
        self.next_id += 1;
        let entry = TimerEntry {
            when,
            id: self.next_id,
            kind,
            unit: unit.to_string(),
        };
        self.heap.push(Reverse((entry.when, entry.id)));
        let id = entry.id;
        self.entries.insert(id, entry);
        id
    }

    pub fn cancel(&mut self, id: u64) {
        self.entries.remove(&id);
    }

    /// Cancel every scheduled entry for `unit`. Makes re-arming idempotent:
    /// without this, each re-arm (one per unit state change) accumulates
    /// duplicate deadlines that all fire in the same poll iteration.
    pub fn cancel_by_unit(&mut self, unit: &str) {
        self.entries.retain(|_, e| e.unit != unit);
    }

    /// Cancel scheduled entries for `unit` matching a specific `kind`. Used
    /// when a single (unit, kind) pair must have at most one live deadline —
    /// e.g. StartTimeout/StopTimeout, where the fire path is guarded by the
    /// unit's `ActiveState` so a stale entry is harmless functionally but
    /// leaks heap slots every re-arm cycle.
    pub fn cancel_by_kind(&mut self, unit: &str, kind: TimerKind) {
        let unit = unit.to_string();
        self.entries
            .retain(|_, e| !(e.unit == unit && e.kind == kind));
    }

    /// Earliest deadline still scheduled, pruning stale entries.
    pub fn next_deadline(&mut self) -> Option<Instant> {
        while let Some(&Reverse((_, id))) = self.heap.peek() {
            if self.entries.contains_key(&id) {
                return self.entries.get(&id).map(|e| e.when);
            }
            self.heap.pop();
        }
        None
    }

    /// Pop all entries due at or before `now`, dropping stale ones.
    pub fn pop_due(&mut self, now: Instant) -> Vec<TimerEntry> {
        let mut out = Vec::new();
        while let Some(&Reverse((when, id))) = self.heap.peek() {
            if when > now {
                break;
            }
            self.heap.pop();
            if let Some(e) = self.entries.remove(&id) {
                out.push(e);
            }
        }
        out
    }

    /// Whether any *service* timer (calendar or monotonic elapse) is still
    /// scheduled. Internal bookkeeping timers (start/stop timeouts, restart
    /// delays) are excluded: they are transient and should never keep the
    /// manager "busy" once its job queue is empty.
    pub fn has_service_timers(&self) -> bool {
        self.entries.values().any(|e| {
            matches!(
                e.kind,
                TimerKind::CalendarElapse(_) | TimerKind::MonotonicElapse
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn schedule_and_fire() {
        let mut w = TimerWheel::default();
        let base = now();
        w.schedule(
            base + Duration::from_millis(10),
            TimerKind::RestartDelay,
            "a",
        );
        w.schedule(
            base + Duration::from_millis(5),
            TimerKind::StartTimeout,
            "b",
        );
        assert_eq!(w.next_deadline().unwrap(), base + Duration::from_millis(5));
        let due = w.pop_due(base + Duration::from_millis(6));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].unit, "b");
        let due = w.pop_due(base + Duration::from_millis(100));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].unit, "a");
    }

    #[test]
    fn cancel_skips_stale() {
        let mut w = TimerWheel::default();
        let base = now();
        let id = w.schedule(base, TimerKind::RestartDelay, "a");
        w.schedule(base, TimerKind::StopTimeout, "b");
        w.cancel(id);
        assert_eq!(w.pop_due(base + Duration::from_millis(1)).len(), 1);
        assert_eq!(w.pop_due(base + Duration::from_millis(2)).len(), 0);
    }

    /// Re-arming the same `(unit, kind)` pair must not accumulate entries;
    /// the second schedule must replace (in effect) the first, not stack on
    /// top of it. Regression for the open finding (REVIEW.md, this run)
    /// where `arm_start_timeout` / `arm_stop_timeout` would re-schedule
    /// without cancelling the prior deadline.
    #[test]
    fn cancel_by_kind_replaces_prior_deadline() {
        let mut w = TimerWheel::default();
        let base = now();
        w.schedule(base, TimerKind::StartTimeout, "u");
        w.cancel_by_kind("u", TimerKind::StartTimeout);
        w.schedule(
            base + Duration::from_millis(5),
            TimerKind::StartTimeout,
            "u",
        );
        // Only the post-cancel entry should fire; the first was dropped.
        let due = w.pop_due(base + Duration::from_millis(10));
        assert_eq!(due.len(), 1, "exactly one StartTimeout must remain");
        assert_eq!(due[0].unit, "u");
    }

    /// Different `kind` entries for the same unit must NOT be cancelled by
    /// `cancel_by_kind` for a different `kind` — it is a (unit, kind) pair
    /// cancel, not a unit-only cancel.
    #[test]
    fn cancel_by_kind_is_scoped_to_kind() {
        let mut w = TimerWheel::default();
        let base = now();
        w.schedule(base, TimerKind::StartTimeout, "u");
        w.schedule(base, TimerKind::StopTimeout, "u");
        w.cancel_by_kind("u", TimerKind::StartTimeout);
        let due = w.pop_due(base + Duration::from_millis(1));
        assert_eq!(due.len(), 1);
        assert!(matches!(due[0].kind, TimerKind::StopTimeout));
    }
}
