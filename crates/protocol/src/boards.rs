//! Leaderboard API types (SPEC §6/§7.3).
//!
//! `GET /v1/boards` returns a [`BoardsResponse`] with all five boards in
//! one payload — this is what the attract app polls and cycles through on
//! an ~8-second dwell. Every board is scoped to a [`Season`]: changing the
//! IWAD, the scoring formula, or the map rotation starts a fresh board
//! rather than corrupting the existing one.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The five leaderboard categories (SPEC §6).
///
/// Serialized (and used in URLs, e.g. `GET /v1/boards/high-score`) as the
/// kebab-case slug; displayed on the attract screen as the uppercase
/// title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BoardCategory {
    /// Highest `run_score`, all runs.
    HighScore,
    /// Most maps completed, tiebreak on score.
    Deepest,
    /// Lowest total time, completed runs only.
    FastestClear,
    /// Most total kills in a run.
    MostKills,
    /// Most total secrets found in a run.
    SecretHunter,
}

impl BoardCategory {
    /// All five categories, in the order they cycle on the attract screen.
    pub const ALL: [BoardCategory; 5] = [
        BoardCategory::HighScore,
        BoardCategory::Deepest,
        BoardCategory::FastestClear,
        BoardCategory::MostKills,
        BoardCategory::SecretHunter,
    ];

    /// URL/serde slug: `"high-score"`, `"deepest"`, `"fastest-clear"`,
    /// `"most-kills"`, `"secret-hunter"`.
    pub fn slug(self) -> &'static str {
        match self {
            BoardCategory::HighScore => "high-score",
            BoardCategory::Deepest => "deepest",
            BoardCategory::FastestClear => "fastest-clear",
            BoardCategory::MostKills => "most-kills",
            BoardCategory::SecretHunter => "secret-hunter",
        }
    }

    /// Display title for the attract screen: `"HIGH SCORE"`,
    /// `"DEEPEST RUN"`, `"FASTEST CLEAR"`, `"MOST KILLS"`,
    /// `"SECRET HUNTER"`.
    pub fn title(self) -> &'static str {
        match self {
            BoardCategory::HighScore => "HIGH SCORE",
            BoardCategory::Deepest => "DEEPEST RUN",
            BoardCategory::FastestClear => "FASTEST CLEAR",
            BoardCategory::MostKills => "MOST KILLS",
            BoardCategory::SecretHunter => "SECRET HUNTER",
        }
    }
}

impl fmt::Display for BoardCategory {
    /// Displays as the slug (matches the serde form).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

/// Error returned by [`BoardCategory::from_str`] for anything that is not
/// one of the five exact slugs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown board category {0:?} (expected one of: high-score, deepest, fastest-clear, most-kills, secret-hunter)")]
pub struct ParseBoardCategoryError(pub String);

impl FromStr for BoardCategory {
    type Err = ParseBoardCategoryError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "high-score" => Ok(BoardCategory::HighScore),
            "deepest" => Ok(BoardCategory::Deepest),
            "fastest-clear" => Ok(BoardCategory::FastestClear),
            "most-kills" => Ok(BoardCategory::MostKills),
            "secret-hunter" => Ok(BoardCategory::SecretHunter),
            other => Err(ParseBoardCategoryError(other.to_owned())),
        }
    }
}

/// The season a board is scoped to (SPEC §6): the triple that must match
/// for runs to be ranked against each other. Changing any component starts
/// a new season.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Season {
    /// SHA-256 of the IWAD in play.
    pub iwad_sha256: String,
    /// Scoring formula version (see
    /// [`SCORING_VERSION`](crate::scoring::SCORING_VERSION)).
    pub scoring_version: i64,
    /// Map rotation id (see
    /// [`MAP_ROTATION_ID`](crate::scoring::MAP_ROTATION_ID)).
    pub map_rotation_id: String,
}

/// One row on a leaderboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardEntry {
    /// 1-based rank on this board.
    pub rank: i64,
    /// Player initials.
    pub initials: String,
    /// The raw ranked value for this category (score, maps, tics, kills,
    /// or secrets, depending on the board).
    pub value: i64,
    /// Human-readable rendering of `value` — e.g. `"12:34"` (via
    /// [`format_tics_clock`]) on time boards, a plain number elsewhere.
    /// The attract app displays this verbatim.
    pub value_display: String,
    /// The run's score, shown as secondary context on non-score boards.
    pub run_score: i64,
    /// How deep the run got.
    pub maps_completed: i64,
    /// RFC 3339 timestamp of when the run ended.
    pub ended_at: String,
}

/// One complete leaderboard: a category plus its ranked entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Board {
    /// Which category this board ranks.
    pub category: BoardCategory,
    /// Display title, duplicated from [`BoardCategory::title`] so dumb
    /// clients need no lookup table.
    pub title: String,
    /// Entries in rank order (rank 1 first).
    pub entries: Vec<BoardEntry>,
}

/// The full `GET /v1/boards` response: all five boards for one season.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardsResponse {
    /// The season the boards are scoped to.
    pub season: Season,
    /// All boards, in [`BoardCategory::ALL`] order.
    pub boards: Vec<Board>,
    /// RFC 3339 timestamp of when the server generated this response.
    pub generated_at: String,
}

/// Formats a tic count as an `"MM:SS"` clock — the `value_display` for
/// time-based boards. 35 tics = 1 second; seconds truncate.
///
/// Negative inputs clamp to `"00:00"`. Runs longer than 99 minutes widen
/// the minute field naturally (`"123:45"`) rather than wrapping.
///
/// ```
/// use protocol::format_tics_clock;
///
/// assert_eq!(format_tics_clock(0), "00:00");
/// assert_eq!(format_tics_clock(35 * 754), "12:34");
/// ```
pub fn format_tics_clock(tics: i64) -> String {
    let total_seconds = tics.max(0) / crate::scoring::TICS_PER_SECOND;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{minutes:02}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_contains_each_category_once() {
        assert_eq!(BoardCategory::ALL.len(), 5);
        for (i, a) in BoardCategory::ALL.iter().enumerate() {
            for b in &BoardCategory::ALL[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn slug_and_title_table() {
        let expected = [
            (BoardCategory::HighScore, "high-score", "HIGH SCORE"),
            (BoardCategory::Deepest, "deepest", "DEEPEST RUN"),
            (
                BoardCategory::FastestClear,
                "fastest-clear",
                "FASTEST CLEAR",
            ),
            (BoardCategory::MostKills, "most-kills", "MOST KILLS"),
            (
                BoardCategory::SecretHunter,
                "secret-hunter",
                "SECRET HUNTER",
            ),
        ];
        for (cat, slug, title) in expected {
            assert_eq!(cat.slug(), slug);
            assert_eq!(cat.title(), title);
            assert_eq!(cat.to_string(), slug);
        }
    }

    #[test]
    fn from_str_round_trips_every_slug() {
        for cat in BoardCategory::ALL {
            assert_eq!(cat.slug().parse::<BoardCategory>(), Ok(cat));
        }
    }

    #[test]
    fn from_str_rejects_non_slugs() {
        for bad in [
            "",
            "HIGH-SCORE",
            "high_score",
            "highscore",
            "high-score ",
            "deepest-run",
        ] {
            assert!(bad.parse::<BoardCategory>().is_err(), "input: {bad:?}");
        }
    }

    #[test]
    fn serde_uses_the_slug() {
        assert_eq!(
            serde_json::to_string(&BoardCategory::HighScore).unwrap(),
            r#""high-score""#
        );
        assert_eq!(
            serde_json::to_string(&BoardCategory::SecretHunter).unwrap(),
            r#""secret-hunter""#
        );
        for cat in BoardCategory::ALL {
            let json = serde_json::to_string(&cat).unwrap();
            assert_eq!(json, format!("{:?}", cat.slug())); // quoted slug
            let back: BoardCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(back, cat);
        }
        assert!(serde_json::from_str::<BoardCategory>(r#""HighScore""#).is_err());
    }

    #[test]
    fn format_tics_clock_known_values() {
        assert_eq!(format_tics_clock(0), "00:00");
        assert_eq!(format_tics_clock(34), "00:00"); // truncates
        assert_eq!(format_tics_clock(35), "00:01");
        assert_eq!(format_tics_clock(35 * 59), "00:59");
        assert_eq!(format_tics_clock(35 * 60), "01:00");
        assert_eq!(format_tics_clock(35 * 754), "12:34");
        assert_eq!(format_tics_clock(35 * 3599), "59:59");
        assert_eq!(format_tics_clock(35 * 3600), "60:00");
        assert_eq!(format_tics_clock(35 * 6000), "100:00"); // widens
    }

    #[test]
    fn format_tics_clock_hostile_values() {
        assert_eq!(format_tics_clock(-1), "00:00");
        assert_eq!(format_tics_clock(i64::MIN), "00:00");
        // Must not panic on the extreme.
        // i64::MAX / 35 = 263524915338707880 s = 4392081922311798 min exactly.
        assert_eq!(format_tics_clock(i64::MAX), "4392081922311798:00");
    }

    #[test]
    fn boards_response_serde_round_trip() {
        let resp = BoardsResponse {
            season: Season {
                iwad_sha256: "abc123".into(),
                scoring_version: 1,
                map_rotation_id: crate::MAP_ROTATION_ID.into(),
            },
            boards: BoardCategory::ALL
                .into_iter()
                .map(|category| Board {
                    category,
                    title: category.title().to_owned(),
                    entries: vec![BoardEntry {
                        rank: 1,
                        initials: "ABC".into(),
                        value: 12345,
                        value_display: "12345".into(),
                        run_score: 12345,
                        maps_completed: 5,
                        ended_at: "2026-08-17T12:00:00Z".into(),
                    }],
                })
                .collect(),
            generated_at: "2026-08-17T12:34:56Z".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: BoardsResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back, resp);
        assert!(json.contains(r#""category":"fastest-clear""#));
    }
}
