//! Run-submission wire types for `POST /v1/runs` (SPEC §7.2/§7.3).
//!
//! The supervisor writes a [`RunSubmission`] to its local spool after every
//! run and submits it (with retries) to the leaderboard service. Submission
//! is idempotent on `session`. The service recomputes the score from the
//! per-map raw stats via [`RunSubmission::recompute_score`] and rejects on
//! mismatch, so a tampered client cannot inflate a score without also
//! forging plausible stats.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::scoring::{run_score, MapStats};

/// Why a run ended. Stored as lowercase text in the `runs.end_reason`
/// column and serialized in JSON the same way (`"death"`, `"complete"`,
/// `"abandoned"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndReason {
    /// The player died; the run ended on the map they died on.
    Death,
    /// The final map of the rotation was cleared.
    Complete,
    /// The engine exited unexpectedly, the walk-away detector fired, or
    /// the watchdog fired; partial stats were kept.
    Abandoned,
    /// The player deliberately ended the run early (held Start); partial
    /// stats were kept and the score counts.
    Quit,
}

impl EndReason {
    /// The canonical lowercase string form: `"death"`, `"complete"`,
    /// `"abandoned"`, or `"quit"`.
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::Death => "death",
            EndReason::Complete => "complete",
            EndReason::Abandoned => "abandoned",
            EndReason::Quit => "quit",
        }
    }
}

impl fmt::Display for EndReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned by [`EndReason::from_str`] for anything other than the
/// exact strings `"death"`, `"complete"`, `"abandoned"`, or `"quit"`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid end reason {0:?} (expected \"death\", \"complete\", \"abandoned\", or \"quit\")")]
pub struct ParseEndReasonError(pub String);

impl FromStr for EndReason {
    type Err = ParseEndReasonError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "death" => Ok(EndReason::Death),
            "complete" => Ok(EndReason::Complete),
            "abandoned" => Ok(EndReason::Abandoned),
            "quit" => Ok(EndReason::Quit),
            other => Err(ParseEndReasonError(other.to_owned())),
        }
    }
}

/// One map's results within a [`RunSubmission`] — the raw stats plus the
/// score the client computed for the map. Mirrors a `run_maps` row
/// (SPEC §7.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapResult {
    /// Position within the run, starting at 0, in play order.
    pub seq: i64,
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
    /// Whether the map was exited normally.
    pub completed: bool,
    /// Client-computed [`map_score`](crate::scoring::map_score) for this
    /// map. Advisory — the server recomputes from the raw stats.
    pub map_score: i64,
}

/// A complete run, as submitted to `POST /v1/runs`. Mirrors a `runs` row
/// plus its `run_maps` children (SPEC §7.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSubmission {
    /// Session UUID minted by the supervisor. The idempotency key:
    /// re-submitting the same session returns the existing record.
    pub session: String,
    /// Player initials — exactly 3 chars, `[A-Z0-9]`. Validate with
    /// [`validate_initials`] before accepting.
    pub initials: String,
    /// Identifier of the submitting cabinet.
    pub cabinet_id: String,
    /// RFC 3339 timestamp of run start.
    pub started_at: String,
    /// RFC 3339 timestamp of run end.
    pub ended_at: String,
    /// Why the run ended.
    pub end_reason: EndReason,
    /// Number of maps completed (the depth of the run).
    pub maps_completed: i64,
    /// Total kills across all maps.
    pub kills: i64,
    /// Total secrets found across all maps.
    pub secrets: i64,
    /// Total items picked up across all maps.
    pub items: i64,
    /// Total time across all maps, in tics.
    pub total_tics: i64,
    /// Client-computed run score. Advisory — the server recomputes via
    /// [`RunSubmission::recompute_score`] and rejects on mismatch.
    pub run_score: i64,
    /// SHA-256 of the IWAD the run was played on. Part of the season key:
    /// a WAD swap starts a new season instead of corrupting the board.
    pub iwad_sha256: String,
    /// [`SCORING_VERSION`](crate::scoring::SCORING_VERSION) the client
    /// scored with. Part of the season key.
    pub scoring_version: i64,
    /// [`MAP_ROTATION_ID`](crate::scoring::MAP_ROTATION_ID) the run was
    /// played on. Part of the season key.
    pub map_rotation_id: String,
    /// Per-map results, in play order.
    pub maps: Vec<MapResult>,
}

impl RunSubmission {
    /// Recomputes the run score from the submitted per-map **raw stats**,
    /// ignoring the client-supplied `map_score` and `run_score` fields.
    ///
    /// Used by server-side validation (SPEC §7.3): reject the submission
    /// when this does not equal `self.run_score`.
    pub fn recompute_score(&self) -> i64 {
        let stats: Vec<MapStats> = self
            .maps
            .iter()
            .map(|m| MapStats {
                map: m.map.clone(),
                kills: m.kills,
                total_monsters: m.total_monsters,
                secrets: m.secrets,
                total_secrets: m.total_secrets,
                items: m.items,
                total_items: m.total_items,
                tics: m.tics,
                completed: m.completed,
            })
            .collect();
        run_score(&stats)
    }
}

/// Returns `true` iff `s` is a valid set of player initials: exactly three
/// characters, each an ASCII uppercase letter `A`–`Z` or digit `0`–`9`.
///
/// Deliberately strict — no lowercase, no whitespace, no Unicode
/// lookalikes, no control characters. Hand-rolled (byte length 3 with all
/// bytes in the allowed ASCII set implies exactly 3 chars), no regex.
///
/// ```
/// use protocol::validate_initials;
///
/// assert!(validate_initials("ABC"));
/// assert!(validate_initials("X99"));
/// assert!(!validate_initials("abc"));
/// assert!(!validate_initials("AB"));
/// assert!(!validate_initials("ABCD"));
/// ```
pub fn validate_initials(s: &str) -> bool {
    s.len() == 3
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_reason_display_and_from_str_round_trip() {
        for reason in [
            EndReason::Death,
            EndReason::Complete,
            EndReason::Abandoned,
            EndReason::Quit,
        ] {
            assert_eq!(reason.to_string().parse::<EndReason>(), Ok(reason));
        }
        assert_eq!("death".parse(), Ok(EndReason::Death));
        assert_eq!("complete".parse(), Ok(EndReason::Complete));
        assert_eq!("abandoned".parse(), Ok(EndReason::Abandoned));
        assert_eq!("quit".parse(), Ok(EndReason::Quit));
    }

    #[test]
    fn end_reason_from_str_rejects_everything_else() {
        for bad in [
            "",
            "Death",
            "DEATH",
            "death ",
            " death",
            "Quit",
            "quit ",
            "complete\n",
        ] {
            assert!(bad.parse::<EndReason>().is_err(), "input: {bad:?}");
        }
    }

    #[test]
    fn end_reason_serde_is_lowercase() {
        assert_eq!(
            serde_json::to_string(&EndReason::Death).unwrap(),
            r#""death""#
        );
        assert_eq!(
            serde_json::to_string(&EndReason::Abandoned).unwrap(),
            r#""abandoned""#
        );
        let parsed: EndReason = serde_json::from_str(r#""complete""#).unwrap();
        assert_eq!(parsed, EndReason::Complete);
        assert!(serde_json::from_str::<EndReason>(r#""Complete""#).is_err());
    }

    #[test]
    fn initials_accepts_valid() {
        for good in ["ABC", "AAA", "ZZZ", "000", "999", "A1Z", "X0X", "0AB"] {
            assert!(validate_initials(good), "input: {good:?}");
        }
    }

    #[test]
    fn initials_rejects_lowercase() {
        assert!(!validate_initials("abc"));
        assert!(!validate_initials("Abc"));
        assert!(!validate_initials("ABc"));
    }

    #[test]
    fn initials_rejects_wrong_length() {
        assert!(!validate_initials(""));
        assert!(!validate_initials("A"));
        assert!(!validate_initials("AB"));
        assert!(!validate_initials("ABCD"));
        assert!(!validate_initials("ABCDEFGHIJ"));
    }

    #[test]
    fn initials_rejects_unicode_lookalikes() {
        // Greek capital Alpha/Beta, Cyrillic Es — visually "ABC".
        assert!(!validate_initials("\u{0391}\u{0392}\u{0421}"));
        // Fullwidth Latin capitals.
        assert!(!validate_initials("\u{FF21}\u{FF22}\u{FF23}"));
        // Three-byte string that is a single multi-byte char.
        assert!(!validate_initials("\u{20AC}")); // €, 3 bytes in UTF-8
                                                 // Combining mark riding on valid letters.
        assert!(!validate_initials("AB\u{0301}"));
    }

    #[test]
    fn initials_rejects_embedded_nul_and_controls() {
        assert!(!validate_initials("A\0C"));
        assert!(!validate_initials("\0\0\0"));
        assert!(!validate_initials("A\tC"));
        assert!(!validate_initials("A C"));
        assert!(!validate_initials("AB\n"));
        assert!(!validate_initials("A-C"));
        assert!(!validate_initials("a1!"));
    }

    fn sample_submission() -> RunSubmission {
        let maps = vec![
            MapResult {
                seq: 0,
                map: "MAP01".into(),
                kills: 50,
                total_monsters: 60,
                secrets: 2,
                total_secrets: 3,
                items: 30,
                total_items: 40,
                tics: 7000,
                completed: true,
                map_score: 2150,
            },
            MapResult {
                seq: 1,
                map: "MAP02".into(),
                kills: 12,
                total_monsters: 70,
                secrets: 1,
                total_secrets: 2,
                items: 5,
                total_items: 30,
                tics: 400,
                completed: false,
                map_score: 245,
            },
        ];
        RunSubmission {
            session: "11111111-2222-3333-4444-555555555555".into(),
            initials: "ABC".into(),
            cabinet_id: "cab-1".into(),
            started_at: "2026-08-17T12:00:00Z".into(),
            ended_at: "2026-08-17T12:09:30Z".into(),
            end_reason: EndReason::Death,
            maps_completed: 1,
            kills: 62,
            secrets: 3,
            items: 35,
            total_tics: 7400,
            run_score: 2595,
            iwad_sha256: "deadbeef".into(),
            scoring_version: crate::SCORING_VERSION,
            map_rotation_id: crate::MAP_ROTATION_ID.into(),
            maps,
        }
    }

    #[test]
    fn recompute_score_matches_hand_computed() {
        // MAP01 completed: 2150. MAP02 death: 245. Depth bonus: 200.
        let sub = sample_submission();
        assert_eq!(sub.recompute_score(), 2595);
        assert_eq!(sub.recompute_score(), sub.run_score);
    }

    #[test]
    fn recompute_score_ignores_client_claimed_scores() {
        let mut sub = sample_submission();
        sub.run_score = 999_999_999;
        sub.maps[0].map_score = 888_888;
        // Recomputation works from the raw stats only, exposing the lie.
        assert_eq!(sub.recompute_score(), 2595);
        assert_ne!(sub.recompute_score(), sub.run_score);
    }

    #[test]
    fn submission_serde_round_trip() {
        let sub = sample_submission();
        let json = serde_json::to_string(&sub).unwrap();
        let back: RunSubmission = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sub);
        // end_reason serializes lowercase inside the struct too.
        assert!(json.contains(r#""end_reason":"death""#));
    }
}
