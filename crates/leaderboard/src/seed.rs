//! Deterministic fake-run generation for `--seed N` (SPEC §12).
//!
//! Produces plausible, fully valid submissions — scores are computed with
//! the real [`protocol::scoring`] functions, so seeded rows pass the same
//! validation as real ones. The RNG is a fixed-seed xorshift so the same
//! `N` always yields the same board, which makes UI iteration reproducible.

use protocol::{map_score, EndReason, MapResult, MapStats, RunSubmission, MAP_ROTATION};
use time::format_description::well_known::Rfc3339;
use time::macros::datetime;
use time::Duration;

/// The fake season's IWAD hash: obviously synthetic, correct length.
pub const SEED_IWAD_SHA256: &str =
    "5eedc0de5eedc0de5eedc0de5eedc0de5eedc0de5eedc0de5eedc0de5eedc0de";

/// xorshift64* — tiny, deterministic, plenty for fake data.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[lo, hi]` (inclusive).
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        debug_assert!(lo <= hi);
        lo + (self.next() % (hi - lo + 1) as u64) as i64
    }
}

/// Plausible per-map totals for the rotation: (monsters, secrets, items).
const MAP_TOTALS: [(i64, i64, i64); 5] = [
    (57, 3, 42),  // MAP01
    (81, 4, 63),  // MAP02
    (99, 5, 71),  // MAP03
    (128, 2, 55), // MAP07
    (112, 3, 66), // MAP08
];

const INITIALS_CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

fn fake_initials(rng: &mut Rng) -> String {
    // Mostly letters, occasional digit — reads like a real cabinet board.
    (0..3)
        .map(|_| {
            let i = if rng.range(0, 9) < 8 {
                rng.range(0, 25)
            } else {
                rng.range(26, 35)
            };
            INITIALS_CHARSET[i as usize] as char
        })
        .collect()
}

/// Generates `n` deterministic, internally consistent fake runs.
pub fn fake_runs(n: u64) -> Vec<RunSubmission> {
    let mut rng = Rng(0xD00D_5EED_0000_0001);
    let base = datetime!(2026-08-17 21:00:00 UTC);
    let mut runs = Vec::with_capacity(n as usize);

    for i in 0..n {
        // Outcome: ~30% full clears, ~60% deaths, ~10% abandoned.
        let roll = rng.range(0, 9);
        let (end_reason, maps_entered) = if roll < 3 {
            (EndReason::Complete, MAP_ROTATION.len())
        } else if roll < 9 {
            (
                EndReason::Death,
                rng.range(1, MAP_ROTATION.len() as i64) as usize,
            )
        } else {
            (EndReason::Abandoned, rng.range(1, 3) as usize)
        };

        let mut stats = Vec::with_capacity(maps_entered);
        for (j, map) in MAP_ROTATION.iter().take(maps_entered).enumerate() {
            let (monsters, secrets_total, items_total) = MAP_TOTALS[j];
            let completed = end_reason == EndReason::Complete || j + 1 < maps_entered;
            // Partial maps (died / walked away) see less of the map.
            let (kill_lo, kill_hi, tics_lo, tics_hi) = if completed {
                (55, 100, 90, 420) // 1.5–7 min per cleared map
            } else {
                (10, 60, 20, 180)
            };
            stats.push(MapStats {
                map: (*map).to_owned(),
                kills: monsters * rng.range(kill_lo, kill_hi) / 100,
                total_monsters: monsters,
                secrets: rng.range(0, if completed { secrets_total } else { 1 }),
                total_secrets: secrets_total,
                items: items_total * rng.range(25, 90) / 100,
                total_items: items_total,
                tics: rng.range(tics_lo, tics_hi) * protocol::TICS_PER_SECOND,
                completed,
            });
        }

        let maps: Vec<MapResult> = stats
            .iter()
            .enumerate()
            .map(|(seq, s)| MapResult {
                seq: seq as i64,
                map: s.map.clone(),
                kills: s.kills,
                total_monsters: s.total_monsters,
                secrets: s.secrets,
                total_secrets: s.total_secrets,
                items: s.items,
                total_items: s.total_items,
                tics: s.tics,
                completed: s.completed,
                map_score: map_score(s),
            })
            .collect();

        let total_tics: i64 = stats.iter().map(|s| s.tics).sum();
        let ended = base - Duration::minutes(i as i64 * 47);
        let started = ended - Duration::seconds(total_tics / protocol::TICS_PER_SECOND);

        runs.push(RunSubmission {
            session: format!("5eed{:04x}-0000-4000-8000-{:012x}", i & 0xffff, i),
            initials: fake_initials(&mut rng),
            cabinet_id: "cab-seed".to_owned(),
            started_at: started.format(&Rfc3339).expect("format seeded time"),
            ended_at: ended.format(&Rfc3339).expect("format seeded time"),
            end_reason,
            maps_completed: stats.iter().filter(|s| s.completed).count() as i64,
            kills: stats.iter().map(|s| s.kills).sum(),
            secrets: stats.iter().map(|s| s.secrets).sum(),
            items: stats.iter().map(|s| s.items).sum(),
            total_tics,
            run_score: protocol::run_score(&stats),
            iwad_sha256: SEED_IWAD_SHA256.to_owned(),
            scoring_version: protocol::SCORING_VERSION,
            map_rotation_id: protocol::MAP_ROTATION_ID.to_owned(),
            maps,
        });
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_runs_are_deterministic_and_consistent() {
        let a = fake_runs(20);
        let b = fake_runs(20);
        assert_eq!(a, b);
        assert_eq!(a.len(), 20);
        for run in &a {
            assert!(
                protocol::validate_initials(&run.initials),
                "{:?}",
                run.initials
            );
            assert_eq!(run.recompute_score(), run.run_score);
            assert_eq!(
                run.maps_completed,
                run.maps.iter().filter(|m| m.completed).count() as i64
            );
            assert_eq!(run.kills, run.maps.iter().map(|m| m.kills).sum::<i64>());
            for m in &run.maps {
                assert!(m.kills <= m.total_monsters);
                assert!(m.secrets <= m.total_secrets);
                assert!(m.items <= m.total_items);
                assert!(m.tics >= 0);
            }
            if run.end_reason == EndReason::Complete {
                assert!(run.maps.iter().all(|m| m.completed));
                assert_eq!(run.maps.len(), MAP_ROTATION.len());
            }
        }
        // Sessions are unique.
        let mut sessions: Vec<_> = a.iter().map(|r| r.session.clone()).collect();
        sessions.sort();
        sessions.dedup();
        assert_eq!(sessions.len(), a.len());
    }
}
