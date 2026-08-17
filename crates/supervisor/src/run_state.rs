//! The per-run state machine: folds parsed telemetry [`Event`]s into
//! per-map [`MapStats`] and emits a finished [`RunSubmission`].
//!
//! # Rules
//!
//! The event stream comes from a game engine over a FIFO — lines can be
//! lost, duplicated, or interleaved with garbage, and a stale writer could
//! replay events from an earlier session. Every rule below is written so
//! that no duplicate or out-of-order event can *corrupt* accumulated state;
//! when in doubt, the machine drops the event and never fabricates bonuses.
//!
//! - **Session scoping.** An event whose `session` differs from the one
//!   this `RunState` was created with is logged and **dropped**
//!   ([`ApplyOutcome::WrongSession`]). A stale gzdoom or a hostile writer
//!   on the FIFO cannot touch the current run.
//! - **Terminal events are final.** After `PlayerDied` or `RunComplete`
//!   has been applied, every further event is dropped
//!   ([`ApplyOutcome::RunOver`]). A duplicate `PlayerDied` or a
//!   `LevelComplete` arriving after death cannot change anything.
//! - **`RunStart`** marks the run as started. A second `RunStart` is a
//!   duplicate and is dropped. Initials in the event are advisory only —
//!   the authoritative initials come from the attract app via the
//!   constructor; a mismatch is logged.
//! - **`LevelEnter`** opens a new map entry with zeroed stats. A
//!   `LevelEnter` for the *same* map while it is already open is a
//!   duplicate (hook double-fire) and is dropped. A `LevelEnter` for a
//!   *different* map while one is open means the previous map's
//!   `LevelComplete` was lost: the open map is closed as **not completed**
//!   with whatever stats it has (no completion/time bonus is fabricated)
//!   and the new map opens.
//! - **`LevelComplete`** finalizes the open map with the event's
//!   authoritative stats and `completed = true`, but only when the map
//!   name matches the open entry. If no map is open and the event names
//!   the most recently finalized map, it is a duplicate and is dropped
//!   (this is the double-`LevelComplete` case — the second copy, even with
//!   different stats, cannot double-count or overwrite). If no map is open
//!   and the name is new, the `LevelEnter` was lost: the event is accepted
//!   as a complete map entry, since its stats are authoritative. If a
//!   *different* map is open than the one named, the event is stale and is
//!   dropped ([`ApplyOutcome::OutOfOrder`]).
//! - **`PlayerDied`** finalizes the current map as **not completed**,
//!   taking `kills`, `secrets`, and `maptime_tics` from the event
//!   (authoritative at the moment of death) and keeping items/totals at
//!   their best-known values (telemetry carries no mid-map item counts, so
//!   these are typically zero). If no map is open — the `LevelEnter` was
//!   lost — a map entry is synthesized from the event so the death still
//!   counts. If the open map's name disagrees with the event, the open
//!   entry is closed as-is and the death entry is synthesized from the
//!   event (the event is the later observation). The run becomes terminal.
//! - **`RunComplete`** marks the run terminal-victorious. If a map is
//!   somehow still open (its `LevelComplete` was lost), it is closed as
//!   **not completed** — no bonus is fabricated. The reported
//!   `total_maptime_tics` is kept for cross-checking but the submission's
//!   `total_tics` is always the sum of per-map tics, so it stays
//!   consistent with the per-map array the server validates against.
//!
//! [`RunState::finish`] closes any still-open map as not completed
//! (abandonment mid-map), aggregates totals, computes per-map scores via
//! [`protocol::map_score`] and the run score via [`protocol::run_score`],
//! and emits a [`FinishedRun`].

use protocol::{
    map_score, run_score, EndReason, Event, MapResult, MapStats, RunSubmission, MAP_ROTATION_ID,
    SCORING_VERSION,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tracing::{debug, warn};

/// What [`RunState::apply`] did with an event. Anything but
/// [`Applied`](ApplyOutcome::Applied) means the event was dropped without
/// mutating per-map stats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The event advanced the state machine.
    Applied,
    /// The event belonged to a different session and was dropped.
    WrongSession,
    /// The event repeated something already recorded and was dropped.
    Duplicate,
    /// The event contradicted the current position in the run (e.g. a
    /// stale `LevelComplete` for a map other than the open one) and was
    /// dropped.
    OutOfOrder,
    /// A terminal event (death or run-complete) was already recorded; the
    /// run is over and the event was dropped.
    RunOver,
}

/// A finished run: the wire-format submission plus how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishedRun {
    /// The complete submission, ready for the spool and `POST /v1/runs`.
    pub submission: RunSubmission,
    /// Why the run ended (also recorded inside `submission.end_reason`).
    pub end_reason: EndReason,
}

/// Accumulates one run's worth of telemetry. See the [module
/// docs](self) for the full rule set.
#[derive(Debug)]
pub struct RunState {
    session: String,
    initials: String,
    cabinet_id: String,
    iwad_sha256: String,
    maps: Vec<MapStats>,
    /// Whether `maps.last()` is an in-progress (entered, not finalized)
    /// entry.
    open: bool,
    run_started: bool,
    terminal: Option<EndReason>,
    /// `total_maptime_tics` as reported by `RunComplete`, kept only to log
    /// divergence from the per-map sum.
    reported_total_tics: Option<i64>,
}

impl RunState {
    /// Creates the state machine for one session. `initials` are the
    /// authoritative ones from the attract app; `cabinet_id` and
    /// `iwad_sha256` are stamped onto the final submission.
    pub fn new(session: &str, initials: &str, cabinet_id: &str, iwad_sha256: &str) -> Self {
        RunState {
            session: session.to_owned(),
            initials: initials.to_owned(),
            cabinet_id: cabinet_id.to_owned(),
            iwad_sha256: iwad_sha256.to_owned(),
            maps: Vec::new(),
            open: false,
            run_started: false,
            terminal: None,
            reported_total_tics: None,
        }
    }

    /// The terminal reason recorded so far: `Some(Death)` after
    /// `PlayerDied`, `Some(Complete)` after `RunComplete`, else `None`.
    /// The session driver maps `None` at child exit to
    /// [`EndReason::Abandoned`].
    pub fn terminal_reason(&self) -> Option<EndReason> {
        self.terminal
    }

    /// Applies one event, returning what happened to it. Never panics;
    /// dropped events leave the state byte-for-byte unchanged.
    pub fn apply(&mut self, event: &Event) -> ApplyOutcome {
        // Rule: session scoping comes first — foreign events cannot even
        // reach the terminal check.
        if event_session(event) != self.session {
            warn!(
                session = %self.session,
                event_session = %event_session(event),
                "dropping event for a different session"
            );
            return ApplyOutcome::WrongSession;
        }
        // Rule: terminal events are final.
        if self.terminal.is_some() {
            debug!(?event, "dropping event after run already ended");
            return ApplyOutcome::RunOver;
        }

        match event {
            Event::RunStart { initials, .. } => {
                if self.run_started {
                    return ApplyOutcome::Duplicate;
                }
                self.run_started = true;
                if *initials != self.initials {
                    warn!(
                        expected = %self.initials,
                        got = %initials,
                        "run_start initials disagree with attract; keeping attract's"
                    );
                }
                ApplyOutcome::Applied
            }

            Event::LevelEnter { map, .. } => {
                if self.open {
                    let current = self.maps.last().expect("open implies a last map");
                    if current.map == *map {
                        return ApplyOutcome::Duplicate;
                    }
                    // Lost LevelComplete: close the previous map without
                    // fabricating any bonus.
                    warn!(
                        previous = %current.map,
                        entering = %map,
                        "level_enter while another map open; closing previous as incomplete"
                    );
                    self.open = false;
                }
                self.maps.push(MapStats {
                    map: map.clone(),
                    ..MapStats::default()
                });
                self.open = true;
                ApplyOutcome::Applied
            }

            Event::LevelComplete {
                map,
                kills,
                total_monsters,
                secrets,
                total_secrets,
                items,
                total_items,
                maptime_tics,
                ..
            } => {
                let completed = MapStats {
                    map: map.clone(),
                    kills: *kills,
                    total_monsters: *total_monsters,
                    secrets: *secrets,
                    total_secrets: *total_secrets,
                    items: *items,
                    total_items: *total_items,
                    tics: *maptime_tics,
                    completed: true,
                };
                if self.open {
                    let current = self.maps.last_mut().expect("open implies a last map");
                    if current.map == *map {
                        *current = completed;
                        self.open = false;
                        ApplyOutcome::Applied
                    } else {
                        warn!(
                            open = %current.map,
                            event_map = %map,
                            "stale level_complete for a map other than the open one; dropping"
                        );
                        ApplyOutcome::OutOfOrder
                    }
                } else if self.maps.last().is_some_and(|m| m.map == *map) {
                    // The double-LevelComplete case: already finalized.
                    ApplyOutcome::Duplicate
                } else {
                    // Lost LevelEnter: the stats are authoritative, so the
                    // completed map is accepted as a fresh entry.
                    warn!(map = %map, "level_complete without level_enter; accepting");
                    self.maps.push(completed);
                    ApplyOutcome::Applied
                }
            }

            Event::PlayerDied {
                map,
                kills,
                secrets,
                maptime_tics,
                ..
            } => {
                if self.open && self.maps.last().is_some_and(|m| m.map == *map) {
                    // Normal death: overwrite the counters the event
                    // carries; items/totals stay best-known.
                    let current = self.maps.last_mut().expect("open implies a last map");
                    current.kills = *kills;
                    current.secrets = *secrets;
                    current.tics = *maptime_tics;
                    current.completed = false;
                } else {
                    if self.open {
                        warn!(
                            open = %self.maps.last().expect("open implies a last map").map,
                            died_on = %map,
                            "player_died names a different map than the open one"
                        );
                    } else {
                        warn!(map = %map, "player_died without level_enter; synthesizing map entry");
                    }
                    self.maps.push(MapStats {
                        map: map.clone(),
                        kills: *kills,
                        secrets: *secrets,
                        tics: *maptime_tics,
                        completed: false,
                        ..MapStats::default()
                    });
                }
                self.open = false;
                self.terminal = Some(EndReason::Death);
                ApplyOutcome::Applied
            }

            Event::RunComplete {
                total_maptime_tics, ..
            } => {
                if self.open {
                    warn!(
                        map = %self.maps.last().expect("open implies a last map").map,
                        "run_complete with a map still open; closing it as incomplete"
                    );
                    self.open = false;
                }
                self.reported_total_tics = Some(*total_maptime_tics);
                self.terminal = Some(EndReason::Complete);
                ApplyOutcome::Applied
            }
        }
    }

    /// Consumes the state and emits the finished run.
    ///
    /// `end_reason` is decided by the session driver (death, complete, or
    /// abandoned when the child exited without a terminal event);
    /// `started_at`/`ended_at` are wall-clock timestamps captured by the
    /// driver and are formatted as RFC 3339.
    ///
    /// A still-open map (abandonment mid-map) is closed as not completed.
    /// Totals are per-map sums with saturating arithmetic; the raw stats
    /// are submitted unclamped — implausibility filtering is the server's
    /// job (SPEC §7.3).
    pub fn finish(
        mut self,
        end_reason: EndReason,
        started_at: OffsetDateTime,
        ended_at: OffsetDateTime,
    ) -> FinishedRun {
        if self.open {
            // Abandoned mid-map: whatever partial stats exist stay, but the
            // map is not completed.
            self.open = false;
            if let Some(last) = self.maps.last_mut() {
                last.completed = false;
            }
        }

        let maps_completed = self.maps.iter().filter(|m| m.completed).count() as i64;
        let kills = self
            .maps
            .iter()
            .fold(0i64, |a, m| a.saturating_add(m.kills));
        let secrets = self
            .maps
            .iter()
            .fold(0i64, |a, m| a.saturating_add(m.secrets));
        let items = self
            .maps
            .iter()
            .fold(0i64, |a, m| a.saturating_add(m.items));
        let total_tics = self.maps.iter().fold(0i64, |a, m| a.saturating_add(m.tics));
        if let Some(reported) = self.reported_total_tics {
            if reported != total_tics {
                debug!(
                    reported,
                    summed = total_tics,
                    "run_complete total_maptime_tics differs from per-map sum; keeping sum"
                );
            }
        }
        let score = run_score(&self.maps);
        let maps = self
            .maps
            .iter()
            .enumerate()
            .map(|(seq, m)| MapResult {
                seq: seq as i64,
                map: m.map.clone(),
                kills: m.kills,
                total_monsters: m.total_monsters,
                secrets: m.secrets,
                total_secrets: m.total_secrets,
                items: m.items,
                total_items: m.total_items,
                tics: m.tics,
                completed: m.completed,
                map_score: map_score(m),
            })
            .collect();

        let submission = RunSubmission {
            session: self.session,
            initials: self.initials,
            cabinet_id: self.cabinet_id,
            started_at: rfc3339(started_at),
            ended_at: rfc3339(ended_at),
            end_reason,
            maps_completed,
            kills,
            secrets,
            items,
            total_tics,
            run_score: score,
            iwad_sha256: self.iwad_sha256,
            scoring_version: SCORING_VERSION,
            map_rotation_id: MAP_ROTATION_ID.to_owned(),
            maps,
        };
        FinishedRun {
            submission,
            end_reason,
        }
    }
}

/// The `session` field common to every event variant.
fn event_session(event: &Event) -> &str {
    match event {
        Event::RunStart { session, .. }
        | Event::LevelEnter { session, .. }
        | Event::LevelComplete { session, .. }
        | Event::PlayerDied { session, .. }
        | Event::RunComplete { session, .. } => session,
    }
}

/// Formats a timestamp as RFC 3339, falling back to the epoch on the
/// (practically impossible) formatting failure rather than panicking.
fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const SESSION: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    fn state() -> RunState {
        RunState::new(SESSION, "ABC", "cab-test", "sha-test")
    }

    fn enter(map: &str) -> Event {
        Event::LevelEnter {
            session: SESSION.into(),
            map: map.into(),
            level_name: format!("{map} name"),
            ts: 0,
        }
    }

    fn complete(map: &str, kills: i64, secrets: i64, items: i64, tics: i64) -> Event {
        Event::LevelComplete {
            session: SESSION.into(),
            map: map.into(),
            kills,
            total_monsters: kills + 5,
            secrets,
            total_secrets: secrets + 1,
            items,
            total_items: items + 10,
            maptime_tics: tics,
        }
    }

    fn died(map: &str, kills: i64, secrets: i64, tics: i64) -> Event {
        Event::PlayerDied {
            session: SESSION.into(),
            map: map.into(),
            kills,
            secrets,
            maptime_tics: tics,
        }
    }

    fn run_complete(total: i64) -> Event {
        Event::RunComplete {
            session: SESSION.into(),
            total_maptime_tics: total,
        }
    }

    fn finish(state: RunState, reason: EndReason) -> RunSubmission {
        state
            .finish(
                reason,
                datetime!(2026-08-17 12:00:00 UTC),
                datetime!(2026-08-17 12:15:00 UTC),
            )
            .submission
    }

    #[test]
    fn happy_path_single_map() {
        let mut s = state();
        assert_eq!(
            s.apply(&Event::RunStart {
                session: SESSION.into(),
                initials: "ABC".into(),
                skill: 3,
                ts: 0,
            }),
            ApplyOutcome::Applied
        );
        assert_eq!(s.apply(&enter("MAP01")), ApplyOutcome::Applied);
        // 10 kills, 1 secret, 4 items, 100 s: 100 + 100 + 20 + 500 + 1000 = 1720.
        assert_eq!(
            s.apply(&complete("MAP01", 10, 1, 4, 3500)),
            ApplyOutcome::Applied
        );
        assert_eq!(s.apply(&run_complete(3500)), ApplyOutcome::Applied);
        assert_eq!(s.terminal_reason(), Some(EndReason::Complete));
        let sub = finish(s, EndReason::Complete);
        assert_eq!(sub.maps_completed, 1);
        // 1720 + depth bonus 200.
        assert_eq!(sub.run_score, 1920);
        assert_eq!(sub.started_at, "2026-08-17T12:00:00Z");
        assert_eq!(sub.ended_at, "2026-08-17T12:15:00Z");
        assert_eq!(sub.cabinet_id, "cab-test");
        assert_eq!(sub.iwad_sha256, "sha-test");
    }

    #[test]
    fn duplicate_level_complete_is_dropped() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        assert_eq!(
            s.apply(&complete("MAP01", 10, 1, 4, 3500)),
            ApplyOutcome::Applied
        );
        // Second copy — even with inflated stats — must not double-count
        // or overwrite.
        assert_eq!(
            s.apply(&complete("MAP01", 999, 99, 999, 1)),
            ApplyOutcome::Duplicate
        );
        let sub = finish(s, EndReason::Abandoned);
        assert_eq!(sub.maps.len(), 1);
        assert_eq!(sub.kills, 10);
        assert_eq!(sub.maps[0].tics, 3500);
    }

    #[test]
    fn player_died_without_level_enter_synthesizes_map() {
        let mut s = state();
        assert_eq!(s.apply(&died("MAP01", 7, 1, 700)), ApplyOutcome::Applied);
        assert_eq!(s.terminal_reason(), Some(EndReason::Death));
        let sub = finish(s, EndReason::Death);
        assert_eq!(sub.maps.len(), 1);
        assert_eq!(sub.maps[0].map, "MAP01");
        assert!(!sub.maps[0].completed);
        assert_eq!(sub.maps[0].kills, 7);
        assert_eq!(sub.maps[0].secrets, 1);
        assert_eq!(sub.maps[0].items, 0); // best-known: never observed
        assert_eq!(sub.maps_completed, 0);
        // 70 + 100 + 0, no bonuses, no depth bonus.
        assert_eq!(sub.run_score, 170);
    }

    #[test]
    fn wrong_session_events_are_dropped() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        let foreign = Event::LevelComplete {
            session: "some-other-session".into(),
            map: "MAP01".into(),
            kills: 999,
            total_monsters: 999,
            secrets: 99,
            total_secrets: 99,
            items: 999,
            total_items: 999,
            maptime_tics: 1,
        };
        assert_eq!(s.apply(&foreign), ApplyOutcome::WrongSession);
        let sub = finish(s, EndReason::Abandoned);
        assert_eq!(sub.kills, 0);
        assert!(!sub.maps[0].completed);
    }

    #[test]
    fn events_after_death_are_dropped() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        assert_eq!(s.apply(&died("MAP01", 3, 0, 200)), ApplyOutcome::Applied);
        // A late (or duplicate) terminal, a completion, a new map: all dead.
        assert_eq!(s.apply(&died("MAP01", 3, 0, 200)), ApplyOutcome::RunOver);
        assert_eq!(
            s.apply(&complete("MAP01", 50, 5, 50, 100)),
            ApplyOutcome::RunOver
        );
        assert_eq!(s.apply(&enter("MAP02")), ApplyOutcome::RunOver);
        assert_eq!(s.apply(&run_complete(200)), ApplyOutcome::RunOver);
        let sub = finish(s, EndReason::Death);
        assert_eq!(sub.maps.len(), 1);
        assert_eq!(sub.kills, 3);
        assert_eq!(sub.maps_completed, 0);
    }

    #[test]
    fn events_after_run_complete_are_dropped() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        s.apply(&complete("MAP01", 1, 0, 0, 35));
        assert_eq!(s.apply(&run_complete(35)), ApplyOutcome::Applied);
        assert_eq!(s.apply(&enter("MAP02")), ApplyOutcome::RunOver);
        assert_eq!(s.apply(&run_complete(35)), ApplyOutcome::RunOver);
        let sub = finish(s, EndReason::Complete);
        assert_eq!(sub.maps.len(), 1);
    }

    #[test]
    fn duplicate_level_enter_is_dropped() {
        let mut s = state();
        assert_eq!(s.apply(&enter("MAP01")), ApplyOutcome::Applied);
        assert_eq!(s.apply(&enter("MAP01")), ApplyOutcome::Duplicate);
        let sub = finish(s, EndReason::Abandoned);
        assert_eq!(sub.maps.len(), 1);
    }

    #[test]
    fn level_enter_while_open_closes_previous_without_bonuses() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        // MAP01's level_complete was lost; MAP02 begins.
        assert_eq!(s.apply(&enter("MAP02")), ApplyOutcome::Applied);
        s.apply(&complete("MAP02", 5, 0, 0, 350));
        let sub = finish(s, EndReason::Abandoned);
        assert_eq!(sub.maps.len(), 2);
        assert!(!sub.maps[0].completed); // no fabricated completion
        assert_eq!(sub.maps[0].map_score, 0); // zero stats, zero score
        assert!(sub.maps[1].completed);
        assert_eq!(sub.maps_completed, 1);
    }

    #[test]
    fn stale_level_complete_for_other_map_is_dropped() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        s.apply(&complete("MAP01", 10, 0, 0, 350));
        s.apply(&enter("MAP02"));
        // Replay of MAP01's completion while MAP02 is open: dropped.
        assert_eq!(
            s.apply(&complete("MAP01", 777, 7, 777, 7)),
            ApplyOutcome::OutOfOrder
        );
        let sub = finish(s, EndReason::Abandoned);
        assert_eq!(sub.maps.len(), 2);
        assert_eq!(sub.maps[0].kills, 10);
    }

    #[test]
    fn level_complete_without_enter_is_accepted() {
        // Documented leniency: a lost level_enter does not discard the
        // authoritative completion stats.
        let mut s = state();
        assert_eq!(
            s.apply(&complete("MAP01", 10, 1, 4, 3500)),
            ApplyOutcome::Applied
        );
        let sub = finish(s, EndReason::Abandoned);
        assert_eq!(sub.maps.len(), 1);
        assert!(sub.maps[0].completed);
        assert_eq!(sub.maps_completed, 1);
    }

    #[test]
    fn duplicate_run_start_is_dropped() {
        let mut s = state();
        let start = Event::RunStart {
            session: SESSION.into(),
            initials: "ABC".into(),
            skill: 3,
            ts: 0,
        };
        assert_eq!(s.apply(&start), ApplyOutcome::Applied);
        assert_eq!(s.apply(&start), ApplyOutcome::Duplicate);
    }

    #[test]
    fn finish_closes_open_map_as_incomplete() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        s.apply(&complete("MAP01", 10, 0, 0, 350));
        s.apply(&enter("MAP02"));
        // Child died with MAP02 still open: abandoned mid-map.
        let sub = finish(s, EndReason::Abandoned);
        assert_eq!(sub.end_reason, EndReason::Abandoned);
        assert_eq!(sub.maps.len(), 2);
        assert!(!sub.maps[1].completed);
        assert_eq!(sub.maps_completed, 1);
    }

    #[test]
    fn player_died_on_unexpected_map_closes_open_and_synthesizes() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        assert_eq!(s.apply(&died("MAP02", 4, 0, 100)), ApplyOutcome::Applied);
        let sub = finish(s, EndReason::Death);
        assert_eq!(sub.maps.len(), 2);
        assert!(!sub.maps[0].completed);
        assert_eq!(sub.maps[1].map, "MAP02");
        assert_eq!(sub.maps[1].kills, 4);
    }
}
