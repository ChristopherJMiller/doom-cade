//! All logic for `arcade-attract`, kept free of egui so it is unit-testable
//! (SPEC §10/§11).
//!
//! The eframe shell in `main.rs` is a thin renderer: it translates key
//! events into [`Input`]s, drives the [`AttractState`] machine via
//! [`step`], and paints whatever state it lands in. Everything with
//! behavior — the idle reel, initials entry, timeouts, the boards cache,
//! and the background fetcher — lives here.
//!
//! Handoff contract: when the machine reaches [`AttractState::Done`], the
//! binary prints exactly one line `ARCADE_INITIALS ABC` to stdout, flushes,
//! and exits 0. It never exits otherwise.

use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

pub use protocol::{Board, BoardCategory, BoardEntry, BoardsResponse};

/// The initials-entry character wheel: `A..Z` then `0..9`, wrapping.
pub const CHARSET: [char; 36] = [
    'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S',
    'T', 'U', 'V', 'W', 'X', 'Y', 'Z', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
];

/// Number of initials a player enters.
pub const INITIALS_LEN: usize = 3;

/// How long each screen of the idle reel is shown.
pub const IDLE_DWELL: Duration = Duration::from_secs(8);

/// Inactivity timeout on the initials-entry screen.
pub const ENTRY_TIMEOUT: Duration = Duration::from_secs(20);

/// How often the background fetcher polls `GET /v1/boards`.
pub const FETCH_INTERVAL: Duration = Duration::from_secs(60);

/// Overall HTTP timeout for a single boards fetch.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Screens in the idle reel: the five boards plus one PRESS START
/// interstitial between cycles.
pub const REEL_LEN: usize = BoardCategory::ALL.len() + 1;

/// Default leaderboard base URL when `ARCADE_LEADERBOARD_URL` is unset.
pub const DEFAULT_LEADERBOARD_URL: &str = "http://127.0.0.1:8080";

/// Panel inputs, already decoded from keys (SPEC §10): ArrowUp/ArrowDown →
/// `Up`/`Down`, either Ctrl → `Confirm` (fire button), Space → `Backspace`
/// (use button), Enter → `Start`. Esc maps to nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// Joystick up: next character on the wheel.
    Up,
    /// Joystick down: previous character on the wheel.
    Down,
    /// Button 1 (fire): lock in the current character.
    Confirm,
    /// Button 2 (use): remove the last character.
    Backspace,
    /// Start button: begin initials entry.
    Start,
}

/// The attract app's state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttractState {
    /// Idle reel: cycling boards and the PRESS START interstitial.
    Idle {
        /// Index into the reel (see [`reel_screen`]); `0..REEL_LEN`.
        board_idx: usize,
        /// When the current screen was entered (dwell reference).
        since: Instant,
    },
    /// Initials entry, after Start was pressed.
    InitialsEntry {
        /// Characters confirmed so far (0..=2; the 3rd confirm goes
        /// straight to [`AttractState::Done`]).
        chars: String,
        /// Index into [`CHARSET`] for the slot being edited.
        cursor_char: usize,
        /// Last input time; 20 s of inactivity returns to idle.
        last_input: Instant,
    },
    /// Terminal: initials chosen. The shell prints the handoff line and
    /// exits. Absorbing — no input or tick leaves this state.
    Done(String),
}

impl AttractState {
    /// The state the app starts in.
    pub fn initial(now: Instant) -> Self {
        AttractState::Idle {
            board_idx: 0,
            since: now,
        }
    }
}

/// What a given idle-reel index shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReelScreen {
    /// One of the five leaderboards.
    Board(BoardCategory),
    /// The PRESS START interstitial between board cycles.
    PressStart,
}

/// Maps an idle `board_idx` to the screen it shows: indices `0..5` are the
/// boards in [`BoardCategory::ALL`] order, index 5 is the interstitial.
pub fn reel_screen(board_idx: usize) -> ReelScreen {
    let idx = board_idx % REEL_LEN;
    match BoardCategory::ALL.get(idx) {
        Some(&cat) => ReelScreen::Board(cat),
        None => ReelScreen::PressStart,
    }
}

/// Position of `c` on the character wheel, if it is on it.
pub fn char_index(c: char) -> Option<usize> {
    CHARSET.iter().position(|&x| x == c)
}

fn entry(chars: String, cursor_char: usize, now: Instant) -> AttractState {
    AttractState::InitialsEntry {
        chars,
        cursor_char,
        last_input: now,
    }
}

/// Advances the state machine by one input (`Some(input)`) or one timer
/// tick (`None`) at time `now`. Pure: same inputs, same output.
pub fn step(state: AttractState, input: Option<Input>, now: Instant) -> AttractState {
    match (state, input) {
        // --- Idle reel -------------------------------------------------
        (AttractState::Idle { board_idx, since }, None) => {
            if now.duration_since(since) >= IDLE_DWELL {
                AttractState::Idle {
                    board_idx: (board_idx + 1) % REEL_LEN,
                    since: now,
                }
            } else {
                AttractState::Idle { board_idx, since }
            }
        }
        (AttractState::Idle { .. }, Some(Input::Start)) => entry(String::new(), 0, now),
        (idle @ AttractState::Idle { .. }, Some(_)) => idle,

        // --- Initials entry --------------------------------------------
        (
            AttractState::InitialsEntry {
                chars,
                cursor_char,
                last_input,
            },
            None,
        ) => {
            if now.duration_since(last_input) >= ENTRY_TIMEOUT {
                AttractState::Idle {
                    board_idx: 0,
                    since: now,
                }
            } else {
                AttractState::InitialsEntry {
                    chars,
                    cursor_char,
                    last_input,
                }
            }
        }
        (
            AttractState::InitialsEntry {
                mut chars,
                cursor_char,
                ..
            },
            Some(input),
        ) => match input {
            Input::Up => entry(chars, (cursor_char + 1) % CHARSET.len(), now),
            Input::Down => entry(
                chars,
                (cursor_char + CHARSET.len() - 1) % CHARSET.len(),
                now,
            ),
            Input::Confirm => {
                chars.push(CHARSET[cursor_char]);
                if chars.len() >= INITIALS_LEN {
                    AttractState::Done(chars)
                } else {
                    // Next slot starts on the same character (classic
                    // arcade behavior).
                    entry(chars, cursor_char, now)
                }
            }
            Input::Backspace => match chars.pop() {
                None => AttractState::Idle {
                    board_idx: 0,
                    since: now,
                },
                Some(c) => entry(chars, char_index(c).unwrap_or(0), now),
            },
            // Start during entry is just activity: refresh the timeout.
            Input::Start => entry(chars, cursor_char, now),
        },

        // --- Done is absorbing -----------------------------------------
        (done @ AttractState::Done(_), _) => done,
    }
}

/// Result of one boards fetch: the parsed response or a description of why
/// it failed.
pub type FetchResult = Result<BoardsResponse, String>;

/// Cached leaderboard data. `stale` is set on any fetch failure but the
/// last good `data` is kept, so the reel keeps rendering offline (with the
/// OFFLINE pip).
#[derive(Debug, Default)]
pub struct BoardsCache {
    /// Last successfully fetched response, if any.
    pub data: Option<BoardsResponse>,
    /// When `data` was fetched.
    pub fetched_at: Option<Instant>,
    /// True after a failed fetch (until the next success).
    pub stale: bool,
}

impl BoardsCache {
    /// Folds one fetch result into the cache.
    pub fn apply(&mut self, result: FetchResult, now: Instant) {
        match result {
            Ok(resp) => {
                self.data = Some(resp);
                self.fetched_at = Some(now);
                self.stale = false;
            }
            Err(_) => self.stale = true,
        }
    }

    /// The cached board for `category`, if we have data containing it.
    pub fn board(&self, category: BoardCategory) -> Option<&Board> {
        self.data
            .as_ref()
            .and_then(|d| d.boards.iter().find(|b| b.category == category))
    }
}

/// Leaderboard base URL from `ARCADE_LEADERBOARD_URL`, defaulting to
/// [`DEFAULT_LEADERBOARD_URL`].
pub fn leaderboard_url_from_env() -> String {
    std::env::var("ARCADE_LEADERBOARD_URL")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_LEADERBOARD_URL.to_owned())
}

/// Joins the base URL with the boards path, tolerating a trailing slash.
pub fn boards_url(base: &str) -> String {
    format!("{}/v1/boards", base.trim_end_matches('/'))
}

/// True when an env-var value means "flag set" (the contract is `=1`).
pub fn flag_is_set(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("1"))
}

/// Reads a `=1` flag from the environment.
pub fn env_flag(name: &str) -> bool {
    flag_is_set(std::env::var(name).ok().as_deref())
}

/// One blocking fetch of the boards endpoint.
pub fn fetch_boards(agent: &ureq::Agent, url: &str) -> FetchResult {
    let response = agent.get(url).call().map_err(|e| e.to_string())?;
    response
        .into_json::<BoardsResponse>()
        .map_err(|e| format!("bad boards payload: {e}"))
}

/// Spawns the background fetcher thread: polls `GET {base}/v1/boards`
/// immediately and then every [`FETCH_INTERVAL`], sending each result down
/// the returned channel. The thread exits when the receiver is dropped.
pub fn spawn_fetcher(base_url: String) -> Receiver<FetchResult> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("boards-fetcher".into())
        .spawn(move || {
            let agent = ureq::AgentBuilder::new().timeout(FETCH_TIMEOUT).build();
            let url = boards_url(&base_url);
            loop {
                let result = fetch_boards(&agent, &url);
                if tx.send(result).is_err() {
                    break;
                }
                thread::sleep(FETCH_INTERVAL);
            }
        })
        .expect("spawn boards-fetcher thread");
    rx
}

/// Inserts a space between every character for the chunky letter-spaced
/// arcade look; existing word gaps widen to three spaces.
///
/// ```
/// assert_eq!(attract::spaced("HIGH SCORE"), "H I G H   S C O R E");
/// ```
pub fn spaced(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for (i, c) in text.chars().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn fresh_entry(now: Instant) -> AttractState {
        step(AttractState::initial(now), Some(Input::Start), now)
    }

    fn cursor_of(state: &AttractState) -> usize {
        match state {
            AttractState::InitialsEntry { cursor_char, .. } => *cursor_char,
            other => panic!("expected InitialsEntry, got {other:?}"),
        }
    }

    #[test]
    fn charset_layout() {
        assert_eq!(CHARSET.len(), 36);
        assert_eq!(CHARSET[0], 'A');
        assert_eq!(CHARSET[25], 'Z');
        assert_eq!(CHARSET[26], '0');
        assert_eq!(CHARSET[35], '9');
        for (i, a) in CHARSET.iter().enumerate() {
            assert_eq!(char_index(*a), Some(i));
        }
        assert_eq!(char_index('a'), None);
        assert_eq!(char_index(' '), None);
    }

    #[test]
    fn up_cycles_full_charset_and_wraps() {
        let now = t0();
        let mut state = fresh_entry(now);
        assert_eq!(cursor_of(&state), 0); // starts on 'A'
        for expected in 1..CHARSET.len() {
            state = step(state, Some(Input::Up), now);
            assert_eq!(cursor_of(&state), expected);
        }
        // 36th Up wraps 9 -> A.
        state = step(state, Some(Input::Up), now);
        assert_eq!(cursor_of(&state), 0);
    }

    #[test]
    fn down_cycles_full_charset_and_wraps() {
        let now = t0();
        let mut state = fresh_entry(now);
        // Single Down from 'A' wraps straight to '9'.
        state = step(state, Some(Input::Down), now);
        assert_eq!(cursor_of(&state), 35);
        assert_eq!(CHARSET[cursor_of(&state)], '9');
        // 35 more Downs walk the whole wheel back to 'A'.
        for expected in (0..35).rev() {
            state = step(state, Some(Input::Down), now);
            assert_eq!(cursor_of(&state), expected);
        }
        assert_eq!(cursor_of(&state), 0);
    }

    #[test]
    fn three_confirms_yield_done_with_the_right_string() {
        let now = t0();
        let mut state = fresh_entry(now);
        // Slot 1: confirm 'A'.
        state = step(state, Some(Input::Confirm), now);
        // Slot 2: Up once ('A' -> 'B'; cursor carried over), confirm.
        state = step(state, Some(Input::Up), now);
        state = step(state, Some(Input::Confirm), now);
        // Slot 3: Down twice from 'B' wraps through 'A' to '9', confirm.
        state = step(state, Some(Input::Down), now);
        state = step(state, Some(Input::Down), now);
        state = step(state, Some(Input::Confirm), now);
        assert_eq!(state, AttractState::Done("AB9".to_owned()));
    }

    #[test]
    fn confirm_carries_cursor_to_next_slot() {
        let now = t0();
        let mut state = fresh_entry(now);
        for _ in 0..3 {
            state = step(state, Some(Input::Up), now); // 'D'
        }
        state = step(state, Some(Input::Confirm), now);
        match &state {
            AttractState::InitialsEntry {
                chars, cursor_char, ..
            } => {
                assert_eq!(chars, "D");
                assert_eq!(*cursor_char, 3); // still on 'D'
            }
            other => panic!("expected InitialsEntry, got {other:?}"),
        }
    }

    #[test]
    fn backspace_pops_and_restores_cursor() {
        let now = t0();
        let mut state = fresh_entry(now);
        state = step(state, Some(Input::Up), now); // 'B'
        state = step(state, Some(Input::Confirm), now); // chars = "B"
        state = step(state, Some(Input::Down), now);
        state = step(state, Some(Input::Down), now); // cursor on '9'
        state = step(state, Some(Input::Backspace), now);
        match &state {
            AttractState::InitialsEntry {
                chars, cursor_char, ..
            } => {
                assert_eq!(chars, "");
                assert_eq!(CHARSET[*cursor_char], 'B'); // re-editing the popped char
            }
            other => panic!("expected InitialsEntry, got {other:?}"),
        }
    }

    #[test]
    fn backspace_on_empty_returns_to_idle() {
        let now = t0();
        let state = fresh_entry(now);
        let state = step(state, Some(Input::Backspace), now);
        assert!(matches!(state, AttractState::Idle { board_idx: 0, .. }));
    }

    #[test]
    fn entry_times_out_to_idle_after_20s() {
        let now = t0();
        let state = fresh_entry(now);
        // 19.9s: still in entry.
        let state = step(state, None, now + Duration::from_millis(19_900));
        assert!(matches!(state, AttractState::InitialsEntry { .. }));
        // 20s: back to idle.
        let state = step(state, None, now + Duration::from_secs(20));
        assert!(matches!(state, AttractState::Idle { board_idx: 0, .. }));
    }

    #[test]
    fn inputs_refresh_the_entry_timeout() {
        let now = t0();
        let state = fresh_entry(now);
        let state = step(state, Some(Input::Up), now + Duration::from_secs(15));
        // 30s after entry started, but only 15s after the last input.
        let state = step(state, None, now + Duration::from_secs(30));
        assert!(matches!(state, AttractState::InitialsEntry { .. }));
        // 35s after entry: 20s since last input — timeout.
        let state = step(state, None, now + Duration::from_secs(35));
        assert!(matches!(state, AttractState::Idle { .. }));
    }

    #[test]
    fn dwell_advances_reel_round_robin() {
        let now = t0();
        let mut state = AttractState::initial(now);
        // Sub-dwell tick: nothing moves.
        let ticked = step(state.clone(), None, now + Duration::from_millis(7_900));
        assert_eq!(ticked, state);

        let mut t = now;
        let mut seen = Vec::new();
        for _ in 0..REEL_LEN {
            match &state {
                AttractState::Idle { board_idx, .. } => seen.push(reel_screen(*board_idx)),
                other => panic!("expected Idle, got {other:?}"),
            }
            t += IDLE_DWELL;
            state = step(state, None, t);
        }
        // One full cycle: the five boards in ALL order, then PRESS START.
        let expected: Vec<ReelScreen> = BoardCategory::ALL
            .into_iter()
            .map(ReelScreen::Board)
            .chain(std::iter::once(ReelScreen::PressStart))
            .collect();
        assert_eq!(seen, expected);
        // ...and it wraps back to board 0.
        assert!(matches!(state, AttractState::Idle { board_idx: 0, .. }));
    }

    #[test]
    fn start_enters_initials_entry_and_other_idle_inputs_do_nothing() {
        let now = t0();
        let idle = AttractState::initial(now);
        for input in [Input::Up, Input::Down, Input::Confirm, Input::Backspace] {
            assert_eq!(step(idle.clone(), Some(input), now), idle);
        }
        let state = step(idle, Some(Input::Start), now);
        assert_eq!(
            state,
            AttractState::InitialsEntry {
                chars: String::new(),
                cursor_char: 0,
                last_input: now,
            }
        );
    }

    #[test]
    fn done_is_absorbing() {
        let now = t0();
        let done = AttractState::Done("ACE".to_owned());
        for input in [
            Some(Input::Up),
            Some(Input::Down),
            Some(Input::Confirm),
            Some(Input::Backspace),
            Some(Input::Start),
            None,
        ] {
            assert_eq!(
                step(done.clone(), input, now + Duration::from_secs(60)),
                done
            );
        }
    }

    #[test]
    fn cache_keeps_data_and_flags_stale_on_failure() {
        let now = t0();
        let mut cache = BoardsCache::default();
        assert!(cache.data.is_none() && !cache.stale);

        // Failure with no data: stale, still no data.
        cache.apply(Err("connection refused".into()), now);
        assert!(cache.stale && cache.data.is_none());

        // Success: data cached, stale cleared.
        let resp = BoardsResponse {
            season: protocol::Season {
                iwad_sha256: "abc".into(),
                scoring_version: 1,
                map_rotation_id: protocol::MAP_ROTATION_ID.into(),
            },
            boards: vec![Board {
                category: BoardCategory::HighScore,
                title: BoardCategory::HighScore.title().to_owned(),
                entries: vec![],
            }],
            generated_at: "2026-08-17T00:00:00Z".into(),
        };
        cache.apply(Ok(resp.clone()), now);
        assert!(!cache.stale);
        assert_eq!(cache.data.as_ref(), Some(&resp));
        assert!(cache.board(BoardCategory::HighScore).is_some());
        assert!(cache.board(BoardCategory::Deepest).is_none());

        // Later failure: stale set, cached data kept.
        cache.apply(Err("timeout".into()), now);
        assert!(cache.stale);
        assert_eq!(cache.data.as_ref(), Some(&resp));
    }

    #[test]
    fn reel_screen_mapping() {
        assert_eq!(REEL_LEN, 6);
        for (i, cat) in BoardCategory::ALL.into_iter().enumerate() {
            assert_eq!(reel_screen(i), ReelScreen::Board(cat));
        }
        assert_eq!(reel_screen(5), ReelScreen::PressStart);
        assert_eq!(reel_screen(6), ReelScreen::Board(BoardCategory::HighScore));
    }

    #[test]
    fn url_and_flag_helpers() {
        assert_eq!(boards_url("http://x:1"), "http://x:1/v1/boards");
        assert_eq!(boards_url("http://x:1/"), "http://x:1/v1/boards");
        assert!(flag_is_set(Some("1")));
        assert!(flag_is_set(Some(" 1 ")));
        assert!(!flag_is_set(Some("0")));
        assert!(!flag_is_set(Some("true")));
        assert!(!flag_is_set(Some("")));
        assert!(!flag_is_set(None));
    }

    #[test]
    fn spaced_letterspacing() {
        assert_eq!(spaced(""), "");
        assert_eq!(spaced("A"), "A");
        assert_eq!(spaced("ABC"), "A B C");
        assert_eq!(spaced("HIGH SCORE"), "H I G H   S C O R E");
    }
}
