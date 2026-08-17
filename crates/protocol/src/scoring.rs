//! The scoring formula and its constants (SPEC §5).
//!
//! DOOM has no native score, so the cabinet defines one:
//!
//! ```text
//! map_score  = kills * 10
//!            + secrets * 100
//!            + items * 5
//!            + completion_bonus (500 if map completed)
//!            + time_bonus (max(0, 600 - seconds_on_map) * 2, only if completed)
//!
//! run_score  = sum(map_score for each map) + depth_bonus (200 * maps_completed)
//! ```
//!
//! Death ends the run; the partial map's kills, secrets, and items still
//! count, but no completion or time bonus is awarded.
//!
//! All arithmetic is saturating and negative inputs are clamped to zero, so
//! no input — however implausible — can panic or wrap. Raw component stats
//! are stored alongside computed scores everywhere (spool DB, leaderboard
//! DB), so the formula can be recomputed retroactively; bump
//! [`SCORING_VERSION`] whenever a constant changes, which starts a new
//! leaderboard season (SPEC §6).

use serde::{Deserialize, Serialize};

/// Version of the scoring formula. Recorded on every submitted run; bump on
/// any change to the constants or formula below so old and new scores are
/// never ranked against each other.
pub const SCORING_VERSION: i64 = 1;

/// The fixed 5-map rotation, in play order (SPEC §4.3).
pub const MAP_ROTATION: &[&str] = &["MAP01", "MAP02", "MAP03", "MAP07", "MAP08"];

/// Identifier for the current rotation, recorded on every submitted run.
/// Changing the rotation means minting a new id, which starts a new season.
pub const MAP_ROTATION_ID: &str = "doom2-m1m2m3m7m8-v1";

/// Points per monster killed.
const POINTS_PER_KILL: i64 = 10;
/// Points per secret found.
const POINTS_PER_SECRET: i64 = 100;
/// Points per item picked up.
const POINTS_PER_ITEM: i64 = 5;
/// Flat bonus for completing a map.
const COMPLETION_BONUS: i64 = 500;
/// The par, in seconds, under which the time bonus accrues.
const TIME_BONUS_PAR_SECONDS: i64 = 600;
/// Points per second under par.
const TIME_BONUS_PER_SECOND: i64 = 2;
/// Flat bonus per completed map, rewarding depth into the rotation.
const DEPTH_BONUS_PER_MAP: i64 = 200;

/// Number of game tics per second (DOOM's fixed simulation rate).
pub const TICS_PER_SECOND: i64 = 35;

/// Raw per-map statistics, as accumulated from telemetry events.
///
/// This is the input to [`map_score`]; the same shape is stored in the
/// spool and leaderboard databases so scores can be recomputed without
/// replaying anything.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MapStats {
    /// Map lump name, e.g. `"MAP01"`.
    pub map: String,
    /// Monsters killed.
    pub kills: i64,
    /// Total monsters on the map.
    pub total_monsters: i64,
    /// Secrets found.
    pub secrets: i64,
    /// Total secrets on the map.
    pub total_secrets: i64,
    /// Items picked up.
    pub items: i64,
    /// Total items on the map.
    pub total_items: i64,
    /// Time spent on the map, in tics (35 tics = 1 second).
    pub tics: i64,
    /// Whether the map was exited normally (vs. the run ending on it).
    pub completed: bool,
}

/// Computes the score for a single map from its raw stats.
///
/// `kills * 10 + secrets * 100 + items * 5`, plus — only if `completed` —
/// a flat 500 completion bonus and a time bonus of
/// `max(0, 600 - seconds_on_map) * 2`, where `seconds_on_map` is
/// `tics / 35` (integer division; 35 tics = 1 second).
///
/// Negative inputs are clamped to 0 before computing, and all arithmetic
/// saturates, so this never panics or wraps on hostile input.
///
/// ```
/// use protocol::{map_score, MapStats};
///
/// let s = MapStats {
///     map: "MAP01".into(),
///     kills: 50, secrets: 2, items: 30,
///     tics: 7000, // 200 seconds
///     completed: true,
///     ..Default::default()
/// };
/// // 500 + 200 + 150 + 500 + (600 - 200) * 2 = 2150
/// assert_eq!(map_score(&s), 2150);
/// ```
pub fn map_score(s: &MapStats) -> i64 {
    let kills = s.kills.max(0);
    let secrets = s.secrets.max(0);
    let items = s.items.max(0);
    let tics = s.tics.max(0);

    let mut score = kills.saturating_mul(POINTS_PER_KILL);
    score = score.saturating_add(secrets.saturating_mul(POINTS_PER_SECRET));
    score = score.saturating_add(items.saturating_mul(POINTS_PER_ITEM));
    if s.completed {
        let seconds_on_map = tics / TICS_PER_SECOND;
        let time_bonus = TIME_BONUS_PAR_SECONDS
            .saturating_sub(seconds_on_map)
            .max(0)
            .saturating_mul(TIME_BONUS_PER_SECOND);
        score = score
            .saturating_add(COMPLETION_BONUS)
            .saturating_add(time_bonus);
    }
    score
}

/// Computes the total run score: the sum of every [`map_score`] plus a
/// depth bonus of 200 per completed map.
///
/// An empty slice scores 0. All arithmetic saturates.
///
/// ```
/// use protocol::{run_score, MapStats};
///
/// assert_eq!(run_score(&[]), 0);
/// ```
pub fn run_score(maps: &[MapStats]) -> i64 {
    let base = maps
        .iter()
        .fold(0i64, |acc, m| acc.saturating_add(map_score(m)));
    let completed = maps.iter().filter(|m| m.completed).count() as i64;
    base.saturating_add(completed.saturating_mul(DEPTH_BONUS_PER_MAP))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(kills: i64, secrets: i64, items: i64, tics: i64, completed: bool) -> MapStats {
        MapStats {
            map: "MAP01".into(),
            kills,
            secrets,
            items,
            tics,
            completed,
            ..Default::default()
        }
    }

    #[test]
    fn known_value_table() {
        // (kills, secrets, items, tics, completed, expected) — hand-computed.
        let table: &[(i64, i64, i64, i64, bool, i64)] = &[
            // Nothing at all, not completed.
            (0, 0, 0, 0, false, 0),
            // Nothing at all, completed instantly: 500 + 600*2.
            (0, 0, 0, 0, true, 1700),
            // Kills only, not completed: 7 * 10.
            (7, 0, 0, 1234, false, 70),
            // Secrets only, not completed: 3 * 100.
            (0, 3, 0, 1234, false, 300),
            // Items only, not completed: 11 * 5.
            (0, 0, 11, 1234, false, 55),
            // SPEC-style example: 50 kills, 2 secrets, 30 items, 200 s, done.
            // 500 + 200 + 150 + 500 + (600-200)*2 = 2150.
            (50, 2, 30, 7000, true, 2150),
            // Completed exactly at par (600 s = 21000 tics): no time bonus.
            // 10 + 500 = 510.
            (1, 0, 0, 21000, true, 510),
            // Completed 1 second under par (599 s): bonus 2. 500 + 2 = 502.
            (0, 0, 0, 599 * 35, true, 502),
            // Slow completion (30 min): 20*10 + 500 = 700.
            (20, 0, 0, 30 * 60 * 35, true, 700),
            // Death mid-map keeps kills/secrets/items but no bonuses:
            // 120 + 100 + 25 = 245.
            (12, 1, 5, 400, false, 245),
        ];
        for &(kills, secrets, items, tics, completed, expected) in table {
            let s = stats(kills, secrets, items, tics, completed);
            assert_eq!(
                map_score(&s),
                expected,
                "stats: kills={kills} secrets={secrets} items={items} tics={tics} completed={completed}"
            );
        }
    }

    #[test]
    fn integer_division_on_seconds() {
        // 20999 tics = 599.97 s → integer 599 s → bonus (600-599)*2 = 2.
        assert_eq!(map_score(&stats(0, 0, 0, 20999, true)), 502);
        // 21000 tics = exactly 600 s → bonus 0.
        assert_eq!(map_score(&stats(0, 0, 0, 21000, true)), 500);
        // 34 tics rounds down to 0 s → full bonus 1200.
        assert_eq!(map_score(&stats(0, 0, 0, 34, true)), 1700);
    }

    #[test]
    fn time_bonus_clamps_to_zero_past_par() {
        for tics in [21000, 21001, 100_000, i64::MAX] {
            assert_eq!(map_score(&stats(0, 0, 0, tics, true)), 500, "tics={tics}");
        }
    }

    #[test]
    fn death_run_gets_no_completion_or_time_bonus() {
        let died = stats(50, 2, 30, 7000, false);
        // Only 500 + 200 + 150 — no 500 completion, no 800 time bonus.
        assert_eq!(map_score(&died), 850);
    }

    #[test]
    fn negative_inputs_clamp_to_zero() {
        assert_eq!(map_score(&stats(-5, -1, -100, -35, false)), 0);
        // Negative tics on a completed map count as 0 s → full time bonus.
        assert_eq!(map_score(&stats(-5, -1, -100, i64::MIN, true)), 1700);
    }

    #[test]
    fn saturates_on_absurd_inputs() {
        let absurd = stats(i64::MAX, i64::MAX, i64::MAX, i64::MIN, true);
        assert_eq!(map_score(&absurd), i64::MAX); // no panic, no wrap
        let runs: Vec<MapStats> = (0..10).map(|_| absurd.clone()).collect();
        assert_eq!(run_score(&runs), i64::MAX);
    }

    #[test]
    fn run_score_sums_maps_and_adds_depth_bonus() {
        let m1 = stats(50, 2, 30, 7000, true); // 2150
        let m2 = stats(12, 1, 5, 400, false); // 245, death here
        assert_eq!(run_score(std::slice::from_ref(&m1)), 2150 + 200);
        assert_eq!(run_score(&[m1, m2]), 2150 + 245 + 200);
        assert_eq!(run_score(&[]), 0);
    }

    #[test]
    fn full_clear_hand_computed() {
        // Five completed maps, uniform stats: 10 kills, 1 secret, 4 items,
        // 100 s each. Per map: 100 + 100 + 20 + 500 + (600-100)*2 = 1720.
        // Run: 5*1720 + 5*200 = 9600.
        let maps: Vec<MapStats> = MAP_ROTATION
            .iter()
            .map(|m| MapStats {
                map: (*m).into(),
                kills: 10,
                secrets: 1,
                items: 4,
                tics: 100 * TICS_PER_SECOND,
                completed: true,
                ..Default::default()
            })
            .collect();
        assert_eq!(run_score(&maps), 9600);
    }

    #[test]
    fn rotation_constants() {
        assert_eq!(MAP_ROTATION.len(), 5);
        assert_eq!(MAP_ROTATION[0], "MAP01");
        assert_eq!(MAP_ROTATION[4], "MAP08");
        assert_eq!(SCORING_VERSION, 1);
        assert!(!MAP_ROTATION_ID.is_empty());
    }
}
