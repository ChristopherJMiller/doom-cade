//! Telemetry events emitted by the in-game ZScript handler (SPEC §4.5).
//!
//! The `arcade-telemetry.pk3` mod prints one JSON object per line to the
//! GZDoom log, prefixed with a fixed sentinel so the supervisor can
//! discriminate telemetry from engine chatter:
//!
//! ```text
//! ARCADE_EVT {"v":1,"event":"level_complete","session":"...","map":"MAP01",...}
//! ```
//!
//! The supervisor feeds every log line through [`parse_event_line`], which
//! returns `Some(Event)` only for well-formed, version-1 telemetry lines and
//! `None` for everything else. The parser must **never** panic: the input is
//! an untrusted byte stream from a game engine, and cvar values echoed into
//! it (session id, player initials) are attacker-influenced.

use serde::{Deserialize, Serialize};

/// Sentinel prefix that marks a log line as a telemetry event.
///
/// Includes the trailing space — the JSON payload begins immediately after
/// it. A line is only telemetry if it starts with this exact string; the
/// sentinel appearing anywhere later in a line means nothing.
pub const EVENT_SENTINEL: &str = "ARCADE_EVT ";

/// Wire-format version this crate understands.
///
/// Every event JSON object carries `"v": 1`. [`parse_event_line`] rejects
/// any line whose `v` is absent, non-integer, or not equal to this value,
/// so a future incompatible pk3 fails safe instead of producing garbage
/// stats.
pub const EVENT_VERSION: i64 = 1;

/// Maximum accepted line length in bytes (64 KiB).
///
/// Real telemetry lines are a few hundred bytes; anything larger is either
/// engine spew or hostile input, and is rejected before any parsing work.
pub const MAX_EVENT_LINE_BYTES: usize = 64 * 1024;

/// A single telemetry event from the game, in emission order over a run:
/// [`RunStart`](Event::RunStart) → ([`LevelEnter`](Event::LevelEnter) →
/// [`LevelComplete`](Event::LevelComplete))\* → either
/// [`PlayerDied`](Event::PlayerDied) or [`RunComplete`](Event::RunComplete).
///
/// Serialized as internally-tagged JSON: the `"event"` field carries the
/// snake_case variant name. All stat fields are raw `LevelLocals` counters;
/// times are in tics (35 tics = 1 second).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Emitted once from the `NewGame` hook when a run begins.
    RunStart {
        /// Session UUID minted by the supervisor (idempotency key).
        session: String,
        /// Player initials as passed via the `arcade_initials` cvar.
        /// Untrusted here — validated server-side against `[A-Z0-9]{3}`.
        initials: String,
        /// Skill level the run was started at (always 3 in production).
        skill: i64,
        /// Unix timestamp (seconds) when the run started.
        ts: i64,
    },
    /// Emitted from `WorldLoaded` each time a map is entered.
    LevelEnter {
        /// Session UUID.
        session: String,
        /// Map lump name, e.g. `"MAP01"`.
        map: String,
        /// Human-readable level name, e.g. `"Entryway"`. Untrusted text
        /// from the WAD — display-only.
        level_name: String,
        /// Unix timestamp (seconds) when the map was entered.
        ts: i64,
    },
    /// Emitted from `WorldUnloaded` when a map is exited normally,
    /// carrying the final stat counters for that map.
    LevelComplete {
        /// Session UUID.
        session: String,
        /// Map lump name, e.g. `"MAP01"`.
        map: String,
        /// Monsters killed on this map.
        kills: i64,
        /// Total monsters on this map.
        total_monsters: i64,
        /// Secrets found on this map.
        secrets: i64,
        /// Total secrets on this map.
        total_secrets: i64,
        /// Items picked up on this map.
        items: i64,
        /// Total items on this map.
        total_items: i64,
        /// Time spent on this map, in tics (35 tics = 1 second).
        maptime_tics: i64,
    },
    /// Emitted from the `PlayerDied` hook. The run is over; the partial
    /// map's kills and secrets still count toward the score, but no
    /// completion or time bonus is awarded (SPEC §5).
    PlayerDied {
        /// Session UUID.
        session: String,
        /// Map the player died on.
        map: String,
        /// Kills at the moment of death.
        kills: i64,
        /// Secrets found at the moment of death.
        secrets: i64,
        /// Time on the map at the moment of death, in tics.
        maptime_tics: i64,
    },
    /// Emitted when the final map of the rotation is cleared — the run
    /// ended in victory.
    RunComplete {
        /// Session UUID.
        session: String,
        /// Total time across all maps, in tics.
        total_maptime_tics: i64,
    },
}

/// Parses one log line into an [`Event`], returning `None` for anything
/// that is not a well-formed version-1 telemetry line.
///
/// Processing order:
///
/// 1. Lines longer than [`MAX_EVENT_LINE_BYTES`] are rejected up front,
///    before any allocation or parsing.
/// 2. Trailing `\r`/`\n` characters are trimmed (nothing else — leading
///    whitespace is *not* forgiven).
/// 3. The line must begin with [`EVENT_SENTINEL`] exactly; the sentinel
///    embedded mid-line does not count. The prefix is stripped.
/// 4. The remainder must parse as a JSON object whose `"v"` field is the
///    integer [`EVENT_VERSION`].
/// 5. The object must deserialize into a known [`Event`] variant with all
///    required fields present. Unknown extra fields are ignored (forward
///    compatibility within a version).
///
/// Returns `None` — never panics — for: no sentinel, malformed or partial
/// JSON, non-JSON garbage, an unknown `"event"` tag, a wrong or missing
/// version, missing or mistyped fields, or oversized input. A sentinel
/// string appearing *inside* a JSON string value is just data; it does not
/// confuse the parser.
///
/// # Examples
///
/// ```
/// use protocol::{parse_event_line, Event};
///
/// let line = r#"ARCADE_EVT {"v":1,"event":"run_complete","session":"s","total_maptime_tics":42}"#;
/// assert!(matches!(parse_event_line(line), Some(Event::RunComplete { .. })));
///
/// assert_eq!(parse_event_line("Init: DOOM 2: Hell on Earth"), None);
/// assert_eq!(parse_event_line("ARCADE_EVT {not json"), None);
/// ```
pub fn parse_event_line(line: &str) -> Option<Event> {
    if line.len() > MAX_EVENT_LINE_BYTES {
        return None;
    }
    let line = line.trim_end_matches(['\r', '\n']);
    let json = line.strip_prefix(EVENT_SENTINEL)?;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    if value.get("v").and_then(serde_json::Value::as_i64) != Some(EVENT_VERSION) {
        return None;
    }
    serde_json::from_value(value).ok()
}

/// Formats an [`Event`] as a complete telemetry line: sentinel prefix plus
/// compact JSON carrying `"v": 1`. No trailing newline is appended.
///
/// The exact inverse of [`parse_event_line`] — used by tests and by dev
/// tooling that mimics the pk3 (the real emitter is ZScript and builds its
/// JSON by hand).
///
/// ```
/// use protocol::{format_event_line, parse_event_line, Event};
///
/// let ev = Event::RunComplete { session: "s".into(), total_maptime_tics: 42 };
/// let line = format_event_line(&ev);
/// assert_eq!(parse_event_line(&line), Some(ev));
/// ```
pub fn format_event_line(event: &Event) -> String {
    let mut value = serde_json::to_value(event).expect("Event serializes infallibly");
    if let serde_json::Value::Object(map) = &mut value {
        map.insert("v".to_owned(), serde_json::Value::from(EVENT_VERSION));
    }
    format!("{EVENT_SENTINEL}{value}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_variants() -> Vec<Event> {
        vec![
            Event::RunStart {
                session: "11111111-2222-3333-4444-555555555555".into(),
                initials: "ABC".into(),
                skill: 3,
                ts: 1_755_400_000,
            },
            Event::LevelEnter {
                session: "s".into(),
                map: "MAP01".into(),
                level_name: "Entryway".into(),
                ts: 1_755_400_010,
            },
            Event::LevelComplete {
                session: "s".into(),
                map: "MAP01".into(),
                kills: 19,
                total_monsters: 20,
                secrets: 1,
                total_secrets: 1,
                items: 40,
                total_items: 44,
                maptime_tics: 4200,
            },
            Event::PlayerDied {
                session: "s".into(),
                map: "MAP07".into(),
                kills: 12,
                secrets: 0,
                maptime_tics: 999,
            },
            Event::RunComplete {
                session: "s".into(),
                total_maptime_tics: 31_500,
            },
        ]
    }

    #[test]
    fn round_trips_every_variant() {
        for ev in all_variants() {
            let line = format_event_line(&ev);
            assert!(line.starts_with(EVENT_SENTINEL), "line: {line}");
            assert_eq!(parse_event_line(&line), Some(ev.clone()), "line: {line}");
        }
    }

    #[test]
    fn round_trips_with_crlf_and_lf_endings() {
        for ev in all_variants() {
            let line = format_event_line(&ev);
            assert_eq!(parse_event_line(&format!("{line}\n")), Some(ev.clone()));
            assert_eq!(parse_event_line(&format!("{line}\r\n")), Some(ev.clone()));
        }
    }

    #[test]
    fn parses_hand_written_spec_example() {
        let line = concat!(
            r#"ARCADE_EVT {"v":1,"event":"level_complete","session":"abc","map":"MAP01","#,
            r#""kills":5,"total_monsters":10,"secrets":0,"total_secrets":2,"#,
            r#""items":3,"total_items":9,"maptime_tics":700}"#,
        );
        let ev = parse_event_line(line).expect("should parse");
        assert_eq!(
            ev,
            Event::LevelComplete {
                session: "abc".into(),
                map: "MAP01".into(),
                kills: 5,
                total_monsters: 10,
                secrets: 0,
                total_secrets: 2,
                items: 3,
                total_items: 9,
                maptime_tics: 700,
            }
        );
    }

    #[test]
    fn rejects_empty_line() {
        assert_eq!(parse_event_line(""), None);
        assert_eq!(parse_event_line("\n"), None);
        assert_eq!(parse_event_line("\r\n"), None);
    }

    #[test]
    fn rejects_sentinel_alone() {
        assert_eq!(parse_event_line("ARCADE_EVT "), None);
        assert_eq!(parse_event_line("ARCADE_EVT \n"), None);
        // Sentinel without its trailing space is not the sentinel.
        assert_eq!(parse_event_line("ARCADE_EVT"), None);
        assert_eq!(
            parse_event_line(
                r#"ARCADE_EVT{"v":1,"event":"run_complete","session":"s","total_maptime_tics":1}"#
            ),
            None
        );
    }

    #[test]
    fn rejects_partial_json() {
        assert_eq!(
            parse_event_line(r#"ARCADE_EVT {"v":1,"event":"run_com"#),
            None
        );
        assert_eq!(parse_event_line("ARCADE_EVT {"), None);
        assert_eq!(parse_event_line(r#"ARCADE_EVT {"v":1,"#), None);
    }

    #[test]
    fn rejects_non_json_garbage() {
        assert_eq!(parse_event_line("ARCADE_EVT hello world"), None);
        assert_eq!(parse_event_line("ARCADE_EVT \x01\x02\x03"), None);
        assert_eq!(parse_event_line("Init: DOOM 2: Hell on Earth"), None);
        assert_eq!(parse_event_line("script warning: line 12"), None);
    }

    #[test]
    fn rejects_valid_json_wrong_shape() {
        // Not an object.
        assert_eq!(parse_event_line("ARCADE_EVT [1,2,3]"), None);
        assert_eq!(parse_event_line("ARCADE_EVT 42"), None);
        assert_eq!(parse_event_line("ARCADE_EVT null"), None);
        assert_eq!(parse_event_line(r#"ARCADE_EVT "run_start""#), None);
        // Object with no event tag.
        assert_eq!(parse_event_line(r#"ARCADE_EVT {"v":1}"#), None);
        // Known event, missing required fields.
        assert_eq!(
            parse_event_line(r#"ARCADE_EVT {"v":1,"event":"run_start"}"#),
            None
        );
        assert_eq!(
            parse_event_line(r#"ARCADE_EVT {"v":1,"event":"run_complete","session":"s"}"#),
            None
        );
        // Known event, mistyped field.
        assert_eq!(
            parse_event_line(
                r#"ARCADE_EVT {"v":1,"event":"run_complete","session":"s","total_maptime_tics":"lots"}"#
            ),
            None
        );
        // Unknown event tag.
        assert_eq!(
            parse_event_line(r#"ARCADE_EVT {"v":1,"event":"warp_drive","session":"s"}"#),
            None
        );
    }

    #[test]
    fn rejects_wrong_version() {
        let tail = r#""event":"run_complete","session":"s","total_maptime_tics":1}"#;
        assert_eq!(
            parse_event_line(&format!(r#"ARCADE_EVT {{"v":2,{tail}"#)),
            None
        );
        assert_eq!(
            parse_event_line(&format!(r#"ARCADE_EVT {{"v":0,{tail}"#)),
            None
        );
        assert_eq!(
            parse_event_line(&format!(r#"ARCADE_EVT {{"v":-1,{tail}"#)),
            None
        );
        assert_eq!(
            parse_event_line(&format!(r#"ARCADE_EVT {{"v":"1",{tail}"#)),
            None
        );
        assert_eq!(
            parse_event_line(&format!(r#"ARCADE_EVT {{"v":1.5,{tail}"#)),
            None
        );
        assert_eq!(
            parse_event_line(&format!(r#"ARCADE_EVT {{"v":null,{tail}"#)),
            None
        );
        // Missing version entirely.
        assert_eq!(parse_event_line(&format!("ARCADE_EVT {{{tail}")), None);
    }

    #[test]
    fn rejects_sentinel_embedded_mid_line() {
        let good = format_event_line(&Event::RunComplete {
            session: "s".into(),
            total_maptime_tics: 1,
        });
        assert_eq!(parse_event_line(&format!("junk {good}")), None);
        assert_eq!(parse_event_line(&format!(" {good}")), None);
        assert_eq!(parse_event_line(&format!("\t{good}")), None);
        assert_eq!(parse_event_line(&format!("[log] {good}")), None);
    }

    #[test]
    fn sentinel_inside_json_string_parses_as_outer_event() {
        // A hostile level name (or initials cvar) containing a full fake
        // telemetry line must not smuggle in a different event.
        let inner =
            r#"ARCADE_EVT {"v":1,"event":"run_complete","session":"evil","total_maptime_tics":0}"#;
        let ev = Event::LevelEnter {
            session: "s".into(),
            map: "MAP01".into(),
            level_name: inner.to_owned(),
            ts: 7,
        };
        let line = format_event_line(&ev);
        assert_eq!(parse_event_line(&line), Some(ev));

        // Same thing hand-escaped, to be independent of our own formatter.
        let raw = concat!(
            r#"ARCADE_EVT {"v":1,"event":"level_enter","session":"s","map":"MAP01","#,
            r#""level_name":"ARCADE_EVT {\"v\":1,\"event\":\"run_complete\",\"session\":\"evil\",\"total_maptime_tics\":0}","#,
            r#""ts":7}"#,
        );
        match parse_event_line(raw) {
            Some(Event::LevelEnter { level_name, .. }) => {
                assert!(level_name.starts_with(EVENT_SENTINEL));
            }
            other => panic!("expected outer LevelEnter, got {other:?}"),
        }
    }

    #[test]
    fn rejects_oversized_lines() {
        // 100 KiB of garbage.
        let big = format!("{}{}", EVENT_SENTINEL, "x".repeat(100 * 1024));
        assert_eq!(parse_event_line(&big), None);
        // A structurally valid event bloated past the cap must also be
        // rejected — the length check comes first.
        let ev = Event::RunStart {
            session: "s".repeat(100 * 1024),
            initials: "ABC".into(),
            skill: 3,
            ts: 0,
        };
        let line = format_event_line(&ev);
        assert!(line.len() > MAX_EVENT_LINE_BYTES);
        assert_eq!(parse_event_line(&line), None);
        // At or under the cap is fine.
        assert!(parse_event_line(&"y".repeat(MAX_EVENT_LINE_BYTES)).is_none());
    }

    #[test]
    fn rejects_nul_bytes() {
        assert_eq!(parse_event_line("\0"), None);
        assert_eq!(parse_event_line("\0ARCADE_EVT {\"v\":1}"), None);
        assert_eq!(parse_event_line("ARCADE_EVT \0{}"), None);
        // Raw (unescaped) NUL inside a JSON string is invalid JSON.
        assert_eq!(
            parse_event_line(
                "ARCADE_EVT {\"v\":1,\"event\":\"run_complete\",\"session\":\"a\0b\",\"total_maptime_tics\":1}"
            ),
            None
        );
    }

    #[test]
    fn escaped_nul_inside_string_is_just_data() {
        // A JSON-escaped NUL (\u0000) is legal JSON and must land in the String untouched;
        // downstream validation (initials, etc.) deals with it.
        let line = r#"ARCADE_EVT {"v":1,"event":"run_complete","session":"a\u0000b","total_maptime_tics":1}"#;
        assert_eq!(
            parse_event_line(line),
            Some(Event::RunComplete {
                session: "a\0b".into(),
                total_maptime_tics: 1,
            })
        );
    }

    #[test]
    fn ignores_unknown_extra_fields() {
        let line = r#"ARCADE_EVT {"v":1,"event":"run_complete","session":"s","total_maptime_tics":1,"future_field":true}"#;
        assert!(parse_event_line(line).is_some());
    }
}
