//! Pure walk-away detection over `progress` heartbeats.
//!
//! An arcade cabinet has no "quit to menu": a player who wanders off leaves
//! gzdoom running in-level forever. The pk3's ~2 s [`Progress`] heartbeats
//! carry the player position and the scoring counters, and a player who is
//! actually playing cannot help but change one of them — DOOM has no
//! stand-still-and-score mechanic. [`IdleTracker`] folds those observations
//! into a single question: has *neither* the position nor the counter sum
//! changed for a whole window?
//!
//! The tracker is deliberately pure — no clocks, no timers, no I/O; every
//! call passes `now` in — so the detection rule is unit-testable with
//! synthetic timelines. The session pump owns the wiring: it feeds each
//! applied heartbeat via [`IdleTracker::observe`] and arms a timer at
//! [`IdleTracker::deadline`] so the window elapsing is noticed even between
//! heartbeats.
//!
//! Movement is compared against the *anchor* (the observation that last
//! showed activity), not the previous sample, so sub-epsilon drift can
//! never creep the anchor along and mask true idleness. Positions within
//! [`MOVE_EPSILON_UNITS`] (< 2 map units on both axes — for integer
//! coordinates exactly the set of points less than 2 units away) count as
//! the same spot: an idling player's view bob or a lift cycling underfoot
//! must not read as activity.
//!
//! [`Progress`]: protocol::Event::Progress

use std::time::Duration;

use tokio::time::Instant;

/// Per-axis movement below this many map units counts as standing still.
///
/// With whole-unit coordinates, "both axis deltas < 2" is exactly
/// "Euclidean distance < 2": the only integer offsets with both axes in
/// `{-1, 0, 1}` are at most `sqrt(2)` away, and any offset of 2 on an axis
/// is at least 2 away.
pub const MOVE_EPSILON_UNITS: i64 = 2;

/// The observation that last showed activity: where the player was and
/// what the counters read when something last changed.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    /// When this activity was observed.
    since: Instant,
    /// Player X position, whole map units.
    px: i64,
    /// Player Y position, whole map units.
    py: i64,
    /// The kills+secrets+items sum at the time.
    counters: i64,
}

impl Anchor {
    /// Whether an observation shows activity relative to this anchor:
    /// movement of at least [`MOVE_EPSILON_UNITS`] on either axis, or any
    /// change (in either direction — a new map resets them) of the counter
    /// sum. Deltas are widened to `i128` so hostile coordinates near
    /// `i64::MIN`/`i64::MAX` cannot overflow.
    fn differs(&self, px: i64, py: i64, counters: i64) -> bool {
        let dx = i128::from(px) - i128::from(self.px);
        let dy = i128::from(py) - i128::from(self.py);
        dx.abs() >= i128::from(MOVE_EPSILON_UNITS)
            || dy.abs() >= i128::from(MOVE_EPSILON_UNITS)
            || counters != self.counters
    }
}

/// Detects a player who has walked away mid-level.
///
/// Feed every heartbeat through [`observe`](IdleTracker::observe); it
/// reports idle once neither the position (within a small epsilon) nor the
/// counter sum has changed for at least the configured window. Any
/// activity re-anchors the window from that observation.
#[derive(Debug)]
pub struct IdleTracker {
    /// How long the player may stay static before counting as gone.
    window: Duration,
    /// `None` until the first observation.
    anchor: Option<Anchor>,
}

impl IdleTracker {
    /// Creates a tracker that reports idle after `window` of no activity.
    pub fn new(window: Duration) -> Self {
        IdleTracker {
            window,
            anchor: None,
        }
    }

    /// Feeds one heartbeat observation: the player position in whole map
    /// units and the kills+secrets+items sum. Returns `true` iff nothing
    /// has changed — position within epsilon *and* counters equal — for at
    /// least the window ending at `now`.
    ///
    /// The first observation only sets the anchor and is never idle: the
    /// window is measured between observations, not from tracker creation.
    pub fn observe(&mut self, now: Instant, px: i64, py: i64, counters: i64) -> bool {
        match self.anchor {
            Some(anchor) if !anchor.differs(px, py, counters) => {
                now.saturating_duration_since(anchor.since) >= self.window
            }
            _ => {
                self.anchor = Some(Anchor {
                    since: now,
                    px,
                    py,
                    counters,
                });
                false
            }
        }
    }

    /// The earliest instant at which [`observe`](IdleTracker::observe)
    /// would report idle absent further activity, or `None` before the
    /// first observation. The session pump sleeps until this so a stream
    /// of static heartbeats is caught at exactly the window — and so is a
    /// player whose heartbeats stop entirely after they stopped moving.
    pub fn deadline(&self) -> Option<Instant> {
        self.anchor.map(|a| a.since + self.window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(180);

    /// A tracker plus a fixed timeline origin, so tests feed deterministic
    /// offsets instead of racing the real clock.
    fn setup() -> (IdleTracker, Instant) {
        (IdleTracker::new(WINDOW), Instant::now())
    }

    #[test]
    fn triggers_at_exactly_the_window_with_static_input() {
        let (mut t, base) = setup();
        // Heartbeats every 2 s, nothing changing.
        assert!(!t.observe(base, 100, -200, 5));
        for secs in (2..180).step_by(2) {
            assert!(
                !t.observe(base + Duration::from_secs(secs), 100, -200, 5),
                "idle before the window elapsed (at {secs}s)"
            );
        }
        // One tick shy of the window: still not idle.
        assert!(!t.observe(base + WINDOW - Duration::from_millis(1), 100, -200, 5));
        // Exactly the window: idle.
        assert!(t.observe(base + WINDOW, 100, -200, 5));
        // And it keeps reporting idle after.
        assert!(t.observe(base + WINDOW + Duration::from_secs(2), 100, -200, 5));
    }

    #[test]
    fn does_not_trigger_when_position_wanders() {
        let (mut t, base) = setup();
        // Position moves a few units every heartbeat; counters frozen.
        // Span the timeline well past two windows.
        for (i, secs) in (0..500).step_by(2).enumerate() {
            let px = 100 + (i as i64) * 5;
            assert!(
                !t.observe(base + Duration::from_secs(secs), px, -200, 5),
                "wandering player flagged idle at {secs}s"
            );
        }
    }

    #[test]
    fn does_not_trigger_when_counters_move() {
        let (mut t, base) = setup();
        // Player camping a spot but still killing things (turret play).
        for (i, secs) in (0..500).step_by(2).enumerate() {
            assert!(
                !t.observe(base + Duration::from_secs(secs), 100, -200, i as i64),
                "scoring player flagged idle at {secs}s"
            );
        }
    }

    #[test]
    fn sub_epsilon_jitter_still_counts_as_idle() {
        let (mut t, base) = setup();
        // View-bob-scale jitter: ±1 unit around the anchor never re-anchors.
        let jitter = [(0, 0), (1, 0), (0, -1), (-1, 1), (1, 1)];
        assert!(!t.observe(base, 100, -200, 5));
        for (i, secs) in (2..180).step_by(2).enumerate() {
            let (jx, jy) = jitter[i % jitter.len()];
            assert!(!t.observe(base + Duration::from_secs(secs), 100 + jx, -200 + jy, 5));
        }
        assert!(t.observe(base + WINDOW, 100, -200, 5));
    }

    #[test]
    fn exactly_epsilon_movement_resets_the_window() {
        let (mut t, base) = setup();
        assert!(!t.observe(base, 100, -200, 5));
        // A 2-unit step on one axis is real movement: re-anchors.
        let moved_at = base + Duration::from_secs(90);
        assert!(!t.observe(moved_at, 102, -200, 5));
        // A full window after the ORIGINAL anchor: not idle, the clock
        // restarted at the move.
        assert!(!t.observe(base + WINDOW, 102, -200, 5));
        // A full window after the move: idle.
        assert!(t.observe(moved_at + WINDOW, 102, -200, 5));
    }

    #[test]
    fn counter_decrease_counts_as_activity() {
        // A new map resets kills/secrets/items — the sum dropping must
        // re-anchor, not read as 180 s of standing still.
        let (mut t, base) = setup();
        assert!(!t.observe(base, 100, -200, 14));
        assert!(!t.observe(base + Duration::from_secs(90), 100, -200, 0));
        assert!(!t.observe(base + WINDOW, 100, -200, 0));
        assert!(t.observe(base + Duration::from_secs(90) + WINDOW, 100, -200, 0));
    }

    #[test]
    fn deadline_tracks_the_anchor() {
        let (mut t, base) = setup();
        assert_eq!(t.deadline(), None);
        t.observe(base, 100, -200, 5);
        assert_eq!(t.deadline(), Some(base + WINDOW));
        // Static heartbeat: deadline unchanged.
        t.observe(base + Duration::from_secs(2), 100, -200, 5);
        assert_eq!(t.deadline(), Some(base + WINDOW));
        // Activity: deadline moves out.
        let moved_at = base + Duration::from_secs(4);
        t.observe(moved_at, 500, -200, 5);
        assert_eq!(t.deadline(), Some(moved_at + WINDOW));
    }

    #[test]
    fn hostile_coordinates_do_not_overflow() {
        let (mut t, base) = setup();
        assert!(!t.observe(base, i64::MIN, i64::MAX, 0));
        // A jump across the whole i64 range is just very fast movement.
        assert!(!t.observe(base + WINDOW, i64::MAX, i64::MIN, 0));
        assert!(t.observe(base + WINDOW + WINDOW, i64::MAX, i64::MIN, 0));
    }

    #[test]
    fn time_going_backwards_is_not_idle() {
        // A now earlier than the anchor (clock weirdness) saturates to
        // zero elapsed rather than panicking or reporting idle.
        let (mut t, base) = setup();
        assert!(!t.observe(base + WINDOW, 100, -200, 5));
        assert!(!t.observe(base, 100, -200, 5));
    }
}
