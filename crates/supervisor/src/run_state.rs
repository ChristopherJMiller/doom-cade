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
//! - **Terminal events are final.** After `PlayerDied`, `RunComplete`, or
//!   `RunQuit` has been applied, every further event is dropped
//!   ([`ApplyOutcome::RunOver`]). A duplicate `PlayerDied` or a
//!   `LevelComplete` arriving after death cannot change anything.
//! - **`RunStart`** marks the run as started. A second `RunStart` is a
//!   duplicate and is dropped. Initials in the event are advisory only —
//!   the authoritative initials come from the attract app via the
//!   constructor; a mismatch is logged.
//! - **`LevelEnter`** opens a new map entry with zeroed stats. A
//!   `LevelEnter` naming *any* map already recorded this run — the open
//!   one (hook double-fire) or an earlier finalized one (a replayed or
//!   lagged copy; the rotation never revisits a map within one run) — is
//!   a duplicate and is dropped without touching the open entry. A
//!   `LevelEnter` for a genuinely *new* map while one is open means the
//!   previous map's `LevelComplete` was lost: the open map is closed as
//!   **not completed** with whatever stats it has (no completion/time
//!   bonus is fabricated) and the new map opens.
//! - **`LevelComplete`** finalizes the open map with the event's
//!   authoritative stats and `completed = true`, but only when the map
//!   name matches the open entry. If no map is open and the event names
//!   *any* already-recorded map, it is a duplicate and is dropped (this is
//!   the double-`LevelComplete` case, including a copy delayed past later
//!   maps — the second copy, even with different stats, cannot
//!   double-count or overwrite). If no map is open and the name is new,
//!   the `LevelEnter` was lost: the event is accepted as a complete map
//!   entry, since its stats are authoritative. If a *different* map is
//!   open than the one named, the event is stale and is dropped
//!   ([`ApplyOutcome::OutOfOrder`]).
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
//! - **`Progress`** (the ~2 s in-map heartbeat) carries *provisional*
//!   stats. For the open map each heartbeat overwrites the provisional
//!   counters wholesale — the latest observation supersedes the last, and
//!   an eventual `LevelComplete`/`PlayerDied` overwrites them with
//!   authoritative values. If no map is open and the named map was never
//!   recorded, the heartbeat **opens** it provisionally (its `LevelEnter`
//!   is late or lost — possible with FIFO/stdout races); the late
//!   `LevelEnter`, when it shows up, drops as a duplicate, keeping the
//!   provisional stats. If a *different* map is open and the named map is
//!   new, the open map's exit event was lost: the open map is closed as
//!   **not completed** and the named map opens provisionally, mirroring
//!   the `LevelEnter` rule. Provisional data must **never** overwrite an
//!   authoritatively closed map: a `Progress` naming any already-finalized
//!   map is stale and is dropped ([`ApplyOutcome::OutOfOrder`]).
//! - **`RunQuit`** (the player held Start) is terminal. The open map is
//!   closed as **not completed**, keeping its best-known provisional
//!   stats; the event's `maptime_tics` replaces the open map's tics only
//!   when it is newer (the quit clock is the later observation — but a
//!   stale event must not rewind it). No completion or time bonus is
//!   fabricated. If the open map's name disagrees with the event, the open
//!   entry is closed as-is and a minimal entry for the named map is
//!   synthesized (unless that map is already recorded — an authoritative
//!   close always stands). If no map is open: a never-recorded map is
//!   synthesized with only the event's tics so the quit location is still
//!   recorded; an already-recorded one (quitting from the intermission
//!   right after its close) is left untouched. The run ends with
//!   [`EndReason::Quit`].
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
    /// A terminal event (death, run-complete, or run-quit) was already
    /// recorded; the run is over and the event was dropped.
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
    /// `PlayerDied`, `Some(Complete)` after `RunComplete`, `Some(Quit)`
    /// after `RunQuit`, else `None`. The session driver maps `None` at
    /// child exit to [`EndReason::Abandoned`].
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
                // Any already-recorded map — the open one (hook
                // double-fire) or an earlier finalized one (a replayed or
                // lagged copy) — is a duplicate: the rotation never
                // revisits a map within one run, so re-opening it could
                // only double-count.
                if self.maps.iter().any(|m| m.map == *map) {
                    debug!(map = %map, "level_enter for an already-recorded map; dropping");
                    return ApplyOutcome::Duplicate;
                }
                if self.open {
                    let current = self.maps.last().expect("open implies a last map");
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
                } else if self.maps.iter().any(|m| m.map == *map) {
                    // The double-LevelComplete case: already finalized —
                    // whether it was the most recent map or one further
                    // back (a copy delayed past later maps must not be
                    // accepted as a fresh entry and double-count).
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

            Event::Progress {
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
                let provisional = MapStats {
                    map: map.clone(),
                    kills: *kills,
                    total_monsters: *total_monsters,
                    secrets: *secrets,
                    total_secrets: *total_secrets,
                    items: *items,
                    total_items: *total_items,
                    tics: *maptime_tics,
                    completed: false,
                };
                if self.open && self.maps.last().is_some_and(|m| m.map == *map) {
                    // Heartbeat for the open map: the latest observation
                    // supersedes the last, wholesale.
                    *self.maps.last_mut().expect("open implies a last map") = provisional;
                    ApplyOutcome::Applied
                } else if self.maps.iter().any(|m| m.map == *map) {
                    // The named map was already finalized (LevelComplete,
                    // PlayerDied, or a forced close): provisional data
                    // never overwrites an authoritative close.
                    warn!(map = %map, "stale progress for an already-finalized map; dropping");
                    ApplyOutcome::OutOfOrder
                } else {
                    if self.open {
                        // The open map's exit event was lost: mirror the
                        // LevelEnter rule — close it without fabricating
                        // any bonus, then open the heartbeat's map.
                        warn!(
                            previous = %self.maps.last().expect("open implies a last map").map,
                            entering = %map,
                            "progress for a new map while another open; closing previous as incomplete"
                        );
                    } else {
                        // Heartbeat before level_enter (FIFO/stdout race):
                        // open the map provisionally so the run still gets
                        // credit for it.
                        warn!(map = %map, "progress before level_enter; opening map provisionally");
                    }
                    self.maps.push(provisional);
                    self.open = true;
                    ApplyOutcome::Applied
                }
            }

            Event::RunQuit {
                map, maptime_tics, ..
            } => {
                if self.open {
                    let current = self.maps.last_mut().expect("open implies a last map");
                    current.completed = false;
                    if current.map == *map {
                        // The quit clock is the later observation, but a
                        // stale event must not rewind the provisional tics.
                        current.tics = current.tics.max(*maptime_tics);
                    } else {
                        warn!(
                            open = %current.map,
                            quit_on = %map,
                            "run_quit names a different map than the open one"
                        );
                        if !self.maps.iter().any(|m| m.map == *map) {
                            self.maps.push(MapStats {
                                map: map.clone(),
                                tics: *maptime_tics,
                                completed: false,
                                ..MapStats::default()
                            });
                        }
                    }
                    self.open = false;
                } else if self.maps.iter().any(|m| m.map == *map) {
                    // Quit from the intermission right after the map's
                    // close: the authoritative close stands untouched.
                    debug!(map = %map, "run_quit for an already-finalized map; keeping its close");
                } else {
                    warn!(map = %map, "run_quit without level_enter; synthesizing map entry");
                    self.maps.push(MapStats {
                        map: map.clone(),
                        tics: *maptime_tics,
                        completed: false,
                        ..MapStats::default()
                    });
                }
                self.terminal = Some(EndReason::Quit);
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
        | Event::RunComplete { session, .. }
        | Event::Progress { session, .. }
        | Event::RunQuit { session, .. } => session,
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

    fn progress(map: &str, kills: i64, secrets: i64, items: i64, tics: i64) -> Event {
        Event::Progress {
            session: SESSION.into(),
            map: map.into(),
            kills,
            total_monsters: 30,
            secrets,
            total_secrets: 2,
            items,
            total_items: 20,
            maptime_tics: tics,
            px: -512,
            py: 768,
        }
    }

    fn run_quit(map: &str, tics: i64) -> Event {
        Event::RunQuit {
            session: SESSION.into(),
            map: map.into(),
            maptime_tics: tics,
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
    fn replayed_level_complete_for_earlier_map_is_dropped() {
        // A duplicate level_complete for a map OTHER than the most recent
        // one (e.g. a lagged copy of MAP01's completion arriving after
        // MAP02 finished) must not be accepted as a fresh entry.
        let mut s = state();
        s.apply(&enter("MAP01"));
        s.apply(&complete("MAP01", 10, 0, 0, 350));
        s.apply(&enter("MAP02"));
        s.apply(&complete("MAP02", 20, 0, 0, 350));
        assert_eq!(
            s.apply(&complete("MAP01", 10, 0, 0, 350)),
            ApplyOutcome::Duplicate
        );
        let sub = finish(s, EndReason::Abandoned);
        assert_eq!(sub.maps.len(), 2, "MAP01 must not appear twice");
        assert_eq!(sub.kills, 30, "kills must not double-count");
        assert_eq!(sub.maps_completed, 2);
    }

    #[test]
    fn replayed_level_enter_for_earlier_map_is_dropped() {
        // A lagged copy of an earlier map's level_enter must neither close
        // the currently open map nor re-open the old one.
        let mut s = state();
        s.apply(&enter("MAP01"));
        s.apply(&complete("MAP01", 10, 0, 0, 350));
        s.apply(&enter("MAP02"));
        assert_eq!(s.apply(&enter("MAP01")), ApplyOutcome::Duplicate);
        // MAP02 is still the open map and completes normally.
        assert_eq!(
            s.apply(&complete("MAP02", 5, 0, 0, 350)),
            ApplyOutcome::Applied
        );
        let sub = finish(s, EndReason::Abandoned);
        assert_eq!(sub.maps.len(), 2);
        assert!(sub.maps[1].completed);
        assert_eq!(sub.maps_completed, 2);
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

    #[test]
    fn progress_updates_open_map_provisionally() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        assert_eq!(
            s.apply(&progress("MAP01", 3, 0, 1, 700)),
            ApplyOutcome::Applied
        );
        // Each heartbeat supersedes the previous one wholesale.
        assert_eq!(
            s.apply(&progress("MAP01", 9, 1, 4, 1800)),
            ApplyOutcome::Applied
        );
        let sub = finish(s, EndReason::Abandoned);
        assert_eq!(sub.maps.len(), 1);
        assert!(!sub.maps[0].completed);
        assert_eq!(sub.maps[0].kills, 9);
        assert_eq!(sub.maps[0].secrets, 1);
        assert_eq!(sub.maps[0].items, 4);
        assert_eq!(sub.maps[0].tics, 1800);
        assert_eq!(sub.maps[0].total_monsters, 30);
        // 90 + 100 + 20, no bonuses.
        assert_eq!(sub.maps[0].map_score, 210);
    }

    #[test]
    fn progress_opens_map_when_level_enter_is_late() {
        let mut s = state();
        // Heartbeat arrives before the level_enter (FIFO/stdout race).
        assert_eq!(
            s.apply(&progress("MAP01", 2, 0, 1, 420)),
            ApplyOutcome::Applied
        );
        // The late level_enter drops as a duplicate: the provisional
        // stats survive instead of being zeroed.
        assert_eq!(s.apply(&enter("MAP01")), ApplyOutcome::Duplicate);
        let sub = finish(s, EndReason::Abandoned);
        assert_eq!(sub.maps.len(), 1);
        assert_eq!(sub.maps[0].kills, 2);
        assert_eq!(sub.maps[0].items, 1);
    }

    #[test]
    fn progress_never_overwrites_an_authoritative_close() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        s.apply(&complete("MAP01", 10, 1, 4, 3500));
        // Stale heartbeat for the closed map, no map open: dropped.
        assert_eq!(
            s.apply(&progress("MAP01", 999, 99, 999, 1)),
            ApplyOutcome::OutOfOrder
        );
        // Same with another map open.
        s.apply(&enter("MAP02"));
        assert_eq!(
            s.apply(&progress("MAP01", 999, 99, 999, 1)),
            ApplyOutcome::OutOfOrder
        );
        let sub = finish(s, EndReason::Abandoned);
        assert_eq!(sub.maps[0].kills, 10);
        assert_eq!(sub.maps[0].tics, 3500);
        assert!(sub.maps[0].completed);
    }

    #[test]
    fn progress_after_terminal_is_dropped() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        s.apply(&died("MAP01", 3, 0, 200));
        assert_eq!(
            s.apply(&progress("MAP01", 999, 99, 999, 9999)),
            ApplyOutcome::RunOver
        );
        let sub = finish(s, EndReason::Death);
        assert_eq!(sub.maps[0].kills, 3);
    }

    #[test]
    fn progress_for_new_map_while_open_closes_previous() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        // MAP01's exit event was lost; MAP02's heartbeat proves the run
        // moved on. MAP01 closes without any fabricated bonus.
        assert_eq!(
            s.apply(&progress("MAP02", 2, 0, 0, 350)),
            ApplyOutcome::Applied
        );
        let sub = finish(s, EndReason::Abandoned);
        assert_eq!(sub.maps.len(), 2);
        assert!(!sub.maps[0].completed);
        assert_eq!(sub.maps[0].map_score, 0);
        assert_eq!(sub.maps[1].map, "MAP02");
        assert_eq!(sub.maps[1].kills, 2);
    }

    #[test]
    fn run_quit_closes_open_map_keeping_provisional_stats() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        s.apply(&complete("MAP01", 10, 1, 4, 3500));
        s.apply(&enter("MAP02"));
        s.apply(&progress("MAP02", 9, 1, 4, 1800));
        // Quit clock is newer than the last heartbeat: it wins.
        assert_eq!(s.apply(&run_quit("MAP02", 1850)), ApplyOutcome::Applied);
        assert_eq!(s.terminal_reason(), Some(EndReason::Quit));
        let sub = finish(s, EndReason::Quit);
        assert_eq!(sub.end_reason, EndReason::Quit);
        assert_eq!(sub.maps_completed, 1);
        assert_eq!(sub.maps.len(), 2);
        assert!(!sub.maps[1].completed);
        assert_eq!(sub.maps[1].kills, 9);
        assert_eq!(sub.maps[1].tics, 1850);
        // MAP01: 100+100+20+500+(600-100)*2=1000 → 1720. MAP02: 90+100+20
        // = 210, no bonuses. Depth bonus 200. Total 2130.
        assert_eq!(sub.run_score, 2130);
    }

    #[test]
    fn run_quit_does_not_rewind_newer_provisional_tics() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        s.apply(&progress("MAP01", 5, 0, 2, 1800));
        // A stale quit clock must not rewind the map time.
        assert_eq!(s.apply(&run_quit("MAP01", 1700)), ApplyOutcome::Applied);
        let sub = finish(s, EndReason::Quit);
        assert_eq!(sub.maps[0].tics, 1800);
    }

    #[test]
    fn run_quit_with_no_open_map_synthesizes_entry() {
        let mut s = state();
        assert_eq!(s.apply(&run_quit("MAP01", 900)), ApplyOutcome::Applied);
        assert_eq!(s.terminal_reason(), Some(EndReason::Quit));
        let sub = finish(s, EndReason::Quit);
        assert_eq!(sub.maps.len(), 1);
        assert_eq!(sub.maps[0].map, "MAP01");
        assert!(!sub.maps[0].completed);
        assert_eq!(sub.maps[0].tics, 900);
        assert_eq!(sub.run_score, 0);
        assert_eq!(sub.maps_completed, 0);
    }

    #[test]
    fn run_quit_after_close_keeps_the_authoritative_close() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        s.apply(&complete("MAP01", 10, 1, 4, 3500));
        // Player quits from the intermission: the completed map must stay
        // completed with its authoritative stats and tics.
        assert_eq!(s.apply(&run_quit("MAP01", 3600)), ApplyOutcome::Applied);
        let sub = finish(s, EndReason::Quit);
        assert_eq!(sub.maps.len(), 1);
        assert!(sub.maps[0].completed);
        assert_eq!(sub.maps[0].tics, 3500);
        assert_eq!(sub.maps_completed, 1);
    }

    #[test]
    fn run_quit_on_unexpected_map_closes_open_and_synthesizes() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        s.apply(&progress("MAP01", 5, 0, 2, 700));
        assert_eq!(s.apply(&run_quit("MAP02", 100)), ApplyOutcome::Applied);
        let sub = finish(s, EndReason::Quit);
        assert_eq!(sub.maps.len(), 2);
        assert!(!sub.maps[0].completed);
        assert_eq!(sub.maps[0].kills, 5); // provisional stats kept
        assert_eq!(sub.maps[1].map, "MAP02");
        assert_eq!(sub.maps[1].tics, 100);
        assert!(!sub.maps[1].completed);
    }

    #[test]
    fn events_after_run_quit_are_dropped() {
        let mut s = state();
        s.apply(&enter("MAP01"));
        assert_eq!(s.apply(&run_quit("MAP01", 500)), ApplyOutcome::Applied);
        assert_eq!(s.apply(&run_quit("MAP01", 500)), ApplyOutcome::RunOver);
        assert_eq!(s.apply(&enter("MAP02")), ApplyOutcome::RunOver);
        assert_eq!(
            s.apply(&complete("MAP01", 50, 5, 50, 100)),
            ApplyOutcome::RunOver
        );
        assert_eq!(
            s.apply(&progress("MAP01", 99, 9, 99, 999)),
            ApplyOutcome::RunOver
        );
        let sub = finish(s, EndReason::Quit);
        assert_eq!(sub.maps.len(), 1);
        assert_eq!(sub.kills, 0);
    }
}
