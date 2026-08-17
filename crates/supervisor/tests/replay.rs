//! Replay-fixture tests (SPEC §12): recorded gzdoom event streams —
//! engine chatter, telemetry, and interleaved hostile garbage — are fed
//! line by line through `protocol::parse_event_line` into the supervisor's
//! `RunState`, exactly as the live session pump does. This is the main
//! defense against regressions in run handling, and it requires no engine.
//!
//! Each fixture is asserted against a fully hand-computed score, and then
//! replayed *without* the hostile lines to prove they changed nothing.

use protocol::{parse_event_line, EndReason, Event, RunSubmission};
use supervisor::run_state::{ApplyOutcome, RunState};
use time::macros::datetime;
use time::OffsetDateTime;

const COMPLETE_FIXTURE: &str = include_str!("fixtures/run_complete.jsonl");
const DEATH_FIXTURE: &str = include_str!("fixtures/run_death.jsonl");

const COMPLETE_SESSION: &str = "11111111-1111-1111-1111-111111111111";
const DEATH_SESSION: &str = "22222222-2222-2222-2222-222222222222";

const STARTED: OffsetDateTime = datetime!(2026-08-17 12:00:00 UTC);
const ENDED: OffsetDateTime = datetime!(2026-08-17 12:20:00 UTC);

/// How every parsed line of a replay was handled.
#[derive(Debug, Default, PartialEq, Eq)]
struct Counts {
    applied: u32,
    wrong_session: u32,
    duplicate: u32,
    out_of_order: u32,
    run_over: u32,
    /// Lines `parse_event_line` rejected (chatter + hostile garbage).
    ignored: u32,
}

fn replay<'a>(lines: impl Iterator<Item = &'a str>, state: &mut RunState) -> Counts {
    let mut counts = Counts::default();
    for line in lines {
        match parse_event_line(line) {
            None => counts.ignored += 1,
            Some(event) => match state.apply(&event) {
                ApplyOutcome::Applied => counts.applied += 1,
                ApplyOutcome::WrongSession => counts.wrong_session += 1,
                ApplyOutcome::Duplicate => counts.duplicate += 1,
                ApplyOutcome::OutOfOrder => counts.out_of_order += 1,
                ApplyOutcome::RunOver => counts.run_over += 1,
            },
        }
    }
    counts
}

fn finish(state: RunState, reason: EndReason) -> RunSubmission {
    state.finish(reason, STARTED, ENDED).submission
}

// ---------------------------------------------------------------------------
// run_complete.jsonl — a full 5-map clear
// ---------------------------------------------------------------------------

#[test]
fn complete_fixture_replays_to_exact_score() {
    let mut state = RunState::new(COMPLETE_SESSION, "ACE", "cab-1", "sha-fixture");
    let counts = replay(COMPLETE_FIXTURE.lines(), &mut state);

    // Genuine events: run_start + 5×(level_enter + level_complete) +
    // run_complete = 12. Parsed-but-dropped hostiles: 1 wrong-session
    // level_complete, 1 duplicate MAP02 level_complete + 1 duplicate MAP03
    // level_enter, 1 stale MAP01 level_complete while MAP03 was open, and
    // 2 events after the terminal run_complete.
    assert_eq!(counts.applied, 12);
    assert_eq!(counts.wrong_session, 1);
    assert_eq!(counts.duplicate, 2);
    assert_eq!(counts.out_of_order, 1);
    assert_eq!(counts.run_over, 2);

    assert_eq!(state.terminal_reason(), Some(EndReason::Complete));
    let sub = finish(state, EndReason::Complete);

    // Hand-computed per-map scores (kills*10 + secrets*100 + items*5 +
    // 500 completion + max(0, 600 - tics/35)*2 time bonus):
    //   MAP01: 18k 1s 30i 4200t(120s) → 180+100+150+500+(600-120)*2=960 → 1890
    //   MAP02: 40k 2s 25i 7350t(210s) → 400+200+125+500+(600-210)*2=780 → 2005
    //   MAP03: 50k 0s 33i 10500t(300s)→ 500+  0+165+500+(600-300)*2=600 → 1765
    //   MAP07: 20k 0s 12i 5250t(150s) → 200+  0+ 60+500+(600-150)*2=900 → 1660
    //   MAP08: 30k 1s 18i 9800t(280s) → 300+100+ 90+500+(600-280)*2=640 → 1630
    // Sum of map scores: 1890+2005+1765+1660+1630 = 8950.
    // Depth bonus: 5 completed × 200 = 1000. Run score: 8950+1000 = 9950.
    assert_eq!(sub.run_score, 9950);

    let scores: Vec<i64> = sub.maps.iter().map(|m| m.map_score).collect();
    assert_eq!(scores, [1890, 2005, 1765, 1660, 1630]);
    let names: Vec<&str> = sub.maps.iter().map(|m| m.map.as_str()).collect();
    assert_eq!(names, ["MAP01", "MAP02", "MAP03", "MAP07", "MAP08"]);
    assert!(sub.maps.iter().all(|m| m.completed));

    assert_eq!(sub.maps_completed, 5);
    assert_eq!(sub.end_reason, EndReason::Complete);
    // Aggregates: kills 18+40+50+20+30 = 158; secrets 1+2+0+0+1 = 4;
    // items 30+25+33+12+18 = 118; tics 4200+7350+10500+5250+9800 = 37100
    // (matches the run_complete event's total_maptime_tics).
    assert_eq!(sub.kills, 158);
    assert_eq!(sub.secrets, 4);
    assert_eq!(sub.items, 118);
    assert_eq!(sub.total_tics, 37_100);

    assert_eq!(sub.session, COMPLETE_SESSION);
    assert_eq!(sub.initials, "ACE");
    assert_eq!(sub.cabinet_id, "cab-1");
    assert_eq!(sub.iwad_sha256, "sha-fixture");
    assert_eq!(sub.started_at, "2026-08-17T12:00:00Z");
    assert_eq!(sub.ended_at, "2026-08-17T12:20:00Z");
    assert_eq!(sub.scoring_version, protocol::SCORING_VERSION);
    assert_eq!(sub.map_rotation_id, protocol::MAP_ROTATION_ID);
    // Client score agrees with the server-side recomputation.
    assert_eq!(sub.recompute_score(), sub.run_score);
}

/// The 12 genuine events of `run_complete.jsonl`, hand-built. Replaying
/// only these must yield a submission identical to replaying the whole
/// fixture — proving the chatter, garbage, wrong-session, duplicate, and
/// out-of-order lines changed nothing.
fn genuine_complete_events() -> Vec<Event> {
    let s = || COMPLETE_SESSION.to_owned();
    let complete = |map: &str,
                    kills,
                    total_monsters,
                    secrets,
                    total_secrets,
                    items,
                    total_items,
                    maptime_tics| {
        Event::LevelComplete {
            session: s(),
            map: map.into(),
            kills,
            total_monsters,
            secrets,
            total_secrets,
            items,
            total_items,
            maptime_tics,
        }
    };
    let enter = |map: &str, level_name: &str, ts| Event::LevelEnter {
        session: s(),
        map: map.into(),
        level_name: level_name.into(),
        ts,
    };
    vec![
        Event::RunStart {
            session: s(),
            initials: "ACE".into(),
            skill: 3,
            ts: 1_755_430_000,
        },
        enter("MAP01", "Entryway", 1_755_430_001),
        complete("MAP01", 18, 20, 1, 1, 30, 36, 4200),
        enter("MAP02", "Underhalls", 1_755_430_122),
        complete("MAP02", 40, 45, 2, 3, 25, 40, 7350),
        enter("MAP03", "The Gantlet", 1_755_430_340),
        complete("MAP03", 50, 58, 0, 4, 33, 50, 10500),
        enter("MAP07", "Dead Simple", 1_755_430_650),
        complete("MAP07", 20, 22, 0, 0, 12, 20, 5250),
        enter("MAP08", "Tricks and Traps", 1_755_430_805),
        complete("MAP08", 30, 41, 1, 2, 18, 29, 9800),
        Event::RunComplete {
            session: s(),
            total_maptime_tics: 37_100,
        },
    ]
}

#[test]
fn hostile_lines_change_nothing_in_complete_fixture() {
    let mut full = RunState::new(COMPLETE_SESSION, "ACE", "cab-1", "sha-fixture");
    replay(COMPLETE_FIXTURE.lines(), &mut full);

    let mut clean = RunState::new(COMPLETE_SESSION, "ACE", "cab-1", "sha-fixture");
    for event in genuine_complete_events() {
        assert_eq!(
            clean.apply(&event),
            ApplyOutcome::Applied,
            "event: {event:?}"
        );
    }

    assert_eq!(
        finish(full, EndReason::Complete),
        finish(clean, EndReason::Complete),
        "hostile fixture lines altered the accumulated run state"
    );
}

// ---------------------------------------------------------------------------
// run_death.jsonl — dies on MAP03
// ---------------------------------------------------------------------------

#[test]
fn death_fixture_replays_to_exact_score() {
    let mut state = RunState::new(DEATH_SESSION, "DIE", "cab-1", "sha-fixture");
    let counts = replay(DEATH_FIXTURE.lines(), &mut state);

    // Genuine: run_start + 3 level_enter + 2 level_complete + player_died
    // = 7. Dropped: 1 wrong-session run_start; after death, 1 duplicate
    // player_died + 1 too-late MAP03 level_complete (with full-clear
    // stats that must NOT count).
    assert_eq!(counts.applied, 7);
    assert_eq!(counts.wrong_session, 1);
    assert_eq!(counts.duplicate, 0);
    assert_eq!(counts.out_of_order, 0);
    assert_eq!(counts.run_over, 2);

    assert_eq!(state.terminal_reason(), Some(EndReason::Death));
    let sub = finish(state, EndReason::Death);

    // Hand-computed:
    //   MAP01 completed: 15k 0s 20i 5600t(160s)
    //     → 150+0+100+500+(600-160)*2=880 → 1630
    //   MAP02 completed: 38k 1s 22i 8400t(240s)
    //     → 380+100+110+500+(600-240)*2=720 → 1810
    //   MAP03 death:     12k 1s (items unknown → 0) 2100t, no bonuses
    //     → 120+100+0 → 220
    // Sum: 1630+1810+220 = 3660. Depth bonus: 2 × 200 = 400.
    // Run score: 3660+400 = 4060.
    assert_eq!(sub.run_score, 4060);

    let scores: Vec<i64> = sub.maps.iter().map(|m| m.map_score).collect();
    assert_eq!(scores, [1630, 1810, 220]);
    assert_eq!(sub.maps.len(), 3);
    assert_eq!(sub.maps[2].map, "MAP03");
    assert!(!sub.maps[2].completed);
    assert_eq!(sub.maps[2].kills, 12); // death-event stats, not the fake late completion

    assert_eq!(sub.maps_completed, 2);
    assert_eq!(sub.end_reason, EndReason::Death);
    // kills 15+38+12 = 65; secrets 0+1+1 = 2; items 20+22+0 = 42;
    // tics 5600+8400+2100 = 16100.
    assert_eq!(sub.kills, 65);
    assert_eq!(sub.secrets, 2);
    assert_eq!(sub.items, 42);
    assert_eq!(sub.total_tics, 16_100);
    assert_eq!(sub.initials, "DIE");
    assert_eq!(sub.recompute_score(), sub.run_score);
}

/// The 7 genuine events of `run_death.jsonl`.
fn genuine_death_events() -> Vec<Event> {
    let s = || DEATH_SESSION.to_owned();
    vec![
        Event::RunStart {
            session: s(),
            initials: "DIE".into(),
            skill: 3,
            ts: 1_755_440_000,
        },
        Event::LevelEnter {
            session: s(),
            map: "MAP01".into(),
            level_name: "Entryway".into(),
            ts: 1_755_440_001,
        },
        Event::LevelComplete {
            session: s(),
            map: "MAP01".into(),
            kills: 15,
            total_monsters: 20,
            secrets: 0,
            total_secrets: 1,
            items: 20,
            total_items: 36,
            maptime_tics: 5600,
        },
        Event::LevelEnter {
            session: s(),
            map: "MAP02".into(),
            level_name: "Underhalls".into(),
            ts: 1_755_440_165,
        },
        Event::LevelComplete {
            session: s(),
            map: "MAP02".into(),
            kills: 38,
            total_monsters: 45,
            secrets: 1,
            total_secrets: 3,
            items: 22,
            total_items: 40,
            maptime_tics: 8400,
        },
        Event::LevelEnter {
            session: s(),
            map: "MAP03".into(),
            level_name: "The Gantlet".into(),
            ts: 1_755_440_410,
        },
        Event::PlayerDied {
            session: s(),
            map: "MAP03".into(),
            kills: 12,
            secrets: 1,
            maptime_tics: 2100,
        },
    ]
}

#[test]
fn hostile_lines_change_nothing_in_death_fixture() {
    let mut full = RunState::new(DEATH_SESSION, "DIE", "cab-1", "sha-fixture");
    replay(DEATH_FIXTURE.lines(), &mut full);

    let mut clean = RunState::new(DEATH_SESSION, "DIE", "cab-1", "sha-fixture");
    for event in genuine_death_events() {
        assert_eq!(
            clean.apply(&event),
            ApplyOutcome::Applied,
            "event: {event:?}"
        );
    }

    assert_eq!(
        finish(full, EndReason::Death),
        finish(clean, EndReason::Death),
        "hostile fixture lines altered the accumulated run state"
    );
}

// ---------------------------------------------------------------------------
// Fixture hygiene
// ---------------------------------------------------------------------------

#[test]
fn fixtures_contain_noise_and_hostile_material() {
    // Guard against the fixtures being "cleaned up" into pure event
    // streams: they must keep exercising the discard paths.
    for fixture in [COMPLETE_FIXTURE, DEATH_FIXTURE] {
        let unparseable = fixture
            .lines()
            .filter(|l| parse_event_line(l).is_none())
            .count();
        assert!(
            unparseable >= 8,
            "fixture lost its chatter/garbage lines ({unparseable} left)"
        );
    }
    // Both fixtures carry at least one well-formed foreign-session event.
    assert!(COMPLETE_FIXTURE.contains("99999999-9999-9999-9999-999999999999"));
    assert!(DEATH_FIXTURE.contains("33333333-3333-3333-3333-333333333333"));
}
