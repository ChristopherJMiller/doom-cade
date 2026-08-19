//! All logic for `arcade-attract`, kept free of egui so it is unit-testable
//! (SPEC §10/§11).
//!
//! The eframe shell in `main.rs` is a thin renderer: it translates key
//! events into [`Input`]s, drives the [`AttractState`] machine via
//! [`step`], and paints whatever state it lands in. Everything with
//! behavior — the idle reel, initials entry, timeouts, the boards cache,
//! the background fetcher, and the DOOM fire simulation — lives here.
//!
//! The binary runs in one of two modes ([`Mode`], from `ARCADE_MODE`):
//!
//! - **Attract** (default): the idle reel loops until the player presses
//!   Start, then the shell prints exactly one line `ARCADE_START`,
//!   flushes, and exits 0. No initials are collected here.
//! - **Initials** (`ARCADE_MODE=initials`, classic arcade post-run flow):
//!   starts directly on the initials wheel with the finished run's score
//!   on screen (`ARCADE_SCORE`, `ARCADE_END_REASON`); on the third
//!   confirm — or after [`ENTRY_TIMEOUT`] of inactivity, padding whatever
//!   was entered via [`pad_initials`] — the shell prints exactly one line
//!   `ARCADE_INITIALS ABC`, flushes, and exits 0.
//!
//! Either way the process never exits without its handoff line.

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

/// Inactivity timeout on the initials-entry screen. In Initials mode this
/// auto-submits (padded) rather than abandoning the score.
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

/// The Attract-mode handoff line (no payload — it just means "player
/// pressed Start, launch the run").
pub const START_LINE: &str = "ARCADE_START";

/// Which flavor this process was launched as (`ARCADE_MODE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Idle reel; exits with `ARCADE_START` when Start is pressed.
    Attract,
    /// Post-run initials entry; exits with `ARCADE_INITIALS ABC`.
    Initials,
}

/// Reads [`Mode`] from `ARCADE_MODE` (anything but `initials` is Attract).
pub fn mode_from_env() -> Mode {
    match std::env::var("ARCADE_MODE").as_deref().map(str::trim) {
        Ok("initials") => Mode::Initials,
        _ => Mode::Attract,
    }
}

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
    /// Start button.
    Start,
}

/// The attract app's state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttractState {
    /// Idle reel: cycling boards and the PRESS START interstitial.
    /// (Attract mode only.)
    Idle {
        /// Index into the reel (see [`reel_screen`]); `0..REEL_LEN`.
        board_idx: usize,
        /// When the current screen was entered (dwell reference).
        since: Instant,
    },
    /// Initials entry (Initials mode only).
    InitialsEntry {
        /// Characters confirmed so far (0..=2; the 3rd confirm goes
        /// straight to [`AttractState::Done`]).
        chars: String,
        /// Index into [`CHARSET`] for the slot being edited.
        cursor_char: usize,
        /// Last input time; [`ENTRY_TIMEOUT`] of inactivity auto-submits.
        last_input: Instant,
    },
    /// Terminal. In Initials mode the payload is the chosen (or padded)
    /// initials; in Attract mode it is empty and means "Start pressed".
    /// Absorbing — no input or tick leaves this state.
    Done(String),
}

impl AttractState {
    /// The state the app starts in for `mode`.
    pub fn initial(mode: Mode, now: Instant) -> Self {
        match mode {
            Mode::Attract => AttractState::Idle {
                board_idx: 0,
                since: now,
            },
            Mode::Initials => entry(String::new(), 0, now),
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

/// Pads a partial entry to [`INITIALS_LEN`] with `'A'` (the auto-submit
/// used when the initials screen times out: whatever was locked in stands,
/// the rest defaults — the score is never abandoned).
pub fn pad_initials(chars: &str) -> String {
    let mut out: String = chars.chars().take(INITIALS_LEN).collect();
    while out.len() < INITIALS_LEN {
        out.push('A');
    }
    out
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
pub fn step(state: AttractState, input: Option<Input>, now: Instant, mode: Mode) -> AttractState {
    match (state, input) {
        // --- Idle reel (Attract mode) ----------------------------------
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
        (AttractState::Idle { .. }, Some(Input::Start)) => match mode {
            // Start launches the run; initials come after it.
            Mode::Attract => AttractState::Done(String::new()),
            // Defensive: Idle is unreachable in Initials mode.
            Mode::Initials => entry(String::new(), 0, now),
        },
        (idle @ AttractState::Idle { .. }, Some(_)) => idle,

        // --- Initials entry (Initials mode) ----------------------------
        (
            AttractState::InitialsEntry {
                chars,
                cursor_char,
                last_input,
            },
            None,
        ) => {
            if now.duration_since(last_input) >= ENTRY_TIMEOUT {
                match mode {
                    // Auto-submit what's there — never abandon the score.
                    Mode::Initials => AttractState::Done(pad_initials(&chars)),
                    Mode::Attract => AttractState::Idle {
                        board_idx: 0,
                        since: now,
                    },
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
                None => match mode {
                    // Nothing to undo post-run: just refresh the timer.
                    Mode::Initials => entry(chars, cursor_char, now),
                    Mode::Attract => AttractState::Idle {
                        board_idx: 0,
                        since: now,
                    },
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

/// Extracts the host portion of a plain `http://host[:port][/path]` URL.
/// Deliberately minimal (no IPv6-bracket or userinfo support — LAN http
/// URLs only); returns `None` when the shape is unrecognizable.
pub fn url_host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("http://")
        .or(url.strip_prefix("https://"))?;
    let end = rest.find([':', '/']).unwrap_or(rest.len());
    let host = &rest[..end];
    (!host.is_empty()).then_some(host)
}

/// True when the URL's host is loopback — an address useless to a visitor
/// standing at the cabinet with a phone.
pub fn is_loopback_url(url: &str) -> bool {
    matches!(url_host(url), Some("127.0.0.1" | "localhost" | "::1"))
}

/// Replaces the host in a plain http URL, keeping scheme, port, and path.
pub fn swap_host(url: &str, new_host: &str) -> Option<String> {
    let host = url_host(url)?;
    let scheme_len = url.find("://")? + 3;
    let host_start = scheme_len;
    let host_end = host_start + host.len();
    Some(format!(
        "{}{}{}",
        &url[..host_start],
        new_host,
        &url[host_end..]
    ))
}

/// Best-effort primary LAN IP: the classic UDP "connect" trick — no packet
/// is sent; the OS just picks the source address it would route from.
/// `None` when there is no route (offline cabinet).
pub fn detect_lan_ip() -> Option<std::net::IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("1.1.1.1:80").ok()?;
    let ip = sock.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// The URL visitors should use for the leaderboard, shown on the idle
/// screen. Resolution order:
///
/// 1. `ARCADE_PUBLIC_URL` set → shown verbatim (the VM sets this to the
///    host-forwarded port; a cabinet fronted by an external leaderboard
///    could too).
/// 2. `ARCADE_LEADERBOARD_URL` already non-loopback → that IS the public
///    URL (off-cabinet leaderboard).
/// 3. Loopback leaderboard → swap the detected LAN IP into it (the
///    cabinet serves the same port on the LAN).
/// 4. No route / nothing detectable → `None` (show nothing rather than a
///    lie).
pub fn public_board_url() -> Option<String> {
    if let Ok(explicit) = std::env::var("ARCADE_PUBLIC_URL") {
        let explicit = explicit.trim().to_owned();
        if !explicit.is_empty() {
            return Some(explicit);
        }
    }
    let board_url = leaderboard_url_from_env();
    if !is_loopback_url(&board_url) {
        return Some(board_url);
    }
    let ip = detect_lan_ip()?;
    swap_host(&board_url, &ip.to_string())
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

// ---------------------------------------------------------------------------
// DOOM fire — the classic PSX-era cellular automaton, shared with the web
// leaderboard's canvas version: heat rises from a permanently-hot bottom
// row, decaying and jittering sideways as it climbs. Pure state + RGBA
// conversion here; the shell uploads the buffer as an egui texture.

/// Default fire buffer width (pixels; upscaled with nearest filtering).
pub const FIRE_W: usize = 240;
/// Default fire buffer height.
pub const FIRE_H: usize = 64;
/// Number of heat levels (0 = cold/transparent, `HEAT_MAX` = white-hot).
pub const HEAT_MAX: u8 = 36;

/// One step of simulated DOOM fire.
pub struct FireSim {
    w: usize,
    h: usize,
    heat: Vec<u8>,
    palette: [[u8; 3]; 37],
    rng: u64,
}

impl FireSim {
    /// A cold buffer with the bottom row lit.
    pub fn new(w: usize, h: usize) -> Self {
        assert!(w > 0 && h > 1, "fire buffer must be at least 1x2");
        let mut heat = vec![0u8; w * h];
        heat[(h - 1) * w..].fill(HEAT_MAX);
        Self {
            w,
            h,
            heat,
            palette: Self::build_palette(),
            rng: 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// Heat ramp: void → blood → ember → fire → flare → white, matching the
    /// web page's canvas fire.
    fn build_palette() -> [[u8; 3]; 37] {
        const STOPS: [[i32; 3]; 14] = [
            [7, 7, 7],
            [31, 7, 7],
            [71, 15, 7],
            [103, 31, 7],
            [143, 39, 7],
            [175, 63, 7],
            [199, 71, 7],
            [223, 87, 7],
            [215, 103, 15],
            [207, 127, 15],
            [199, 151, 31],
            [191, 175, 47],
            [223, 207, 111],
            [255, 255, 255],
        ];
        let mut pal = [[0u8; 3]; 37];
        for (i, entry) in pal.iter_mut().enumerate() {
            let t = i as f32 / 36.0 * (STOPS.len() - 1) as f32;
            let a = t.floor() as usize;
            let b = (a + 1).min(STOPS.len() - 1);
            let f = t - a as f32;
            for k in 0..3 {
                entry[k] =
                    (STOPS[a][k] as f32 + (STOPS[b][k] - STOPS[a][k]) as f32 * f).round() as u8;
            }
        }
        pal
    }

    fn rand(&mut self) -> u32 {
        // xorshift64* — deterministic, dependency-free, plenty for flames.
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }

    /// Advances the automaton one frame (call ~20 times a second).
    pub fn step(&mut self) {
        for y in 1..self.h {
            for x in 0..self.w {
                let src = y * self.w + x;
                let r = (self.rand() % 3) as usize;
                // dst = src - w - r + 1, clamped to the buffer start.
                let dst = (src + 1).saturating_sub(self.w + r);
                let decay = (r & 1) as u8;
                self.heat[dst] = self.heat[src].saturating_sub(decay);
            }
        }
    }

    /// Buffer width in pixels.
    pub fn width(&self) -> usize {
        self.w
    }

    /// Buffer height in pixels.
    pub fn height(&self) -> usize {
        self.h
    }

    /// The heat field as premultiplied-friendly RGBA (cold pixels fully
    /// transparent), row-major, `width * height * 4` bytes.
    pub fn rgba(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.heat.len() * 4);
        for &h in &self.heat {
            let c = self.palette[h.min(HEAT_MAX) as usize];
            out.extend_from_slice(&[c[0], c[1], c[2], if h == 0 { 0 } else { 255 }]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn fresh_entry(now: Instant) -> AttractState {
        AttractState::initial(Mode::Initials, now)
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
    fn initial_states_by_mode() {
        let now = t0();
        assert!(matches!(
            AttractState::initial(Mode::Attract, now),
            AttractState::Idle { board_idx: 0, .. }
        ));
        assert!(matches!(
            AttractState::initial(Mode::Initials, now),
            AttractState::InitialsEntry { .. }
        ));
    }

    #[test]
    fn up_cycles_full_charset_and_wraps() {
        let now = t0();
        let mut state = fresh_entry(now);
        assert_eq!(cursor_of(&state), 0); // starts on 'A'
        for expected in 1..CHARSET.len() {
            state = step(state, Some(Input::Up), now, Mode::Initials);
            assert_eq!(cursor_of(&state), expected);
        }
        // 36th Up wraps 9 -> A.
        state = step(state, Some(Input::Up), now, Mode::Initials);
        assert_eq!(cursor_of(&state), 0);
    }

    #[test]
    fn down_cycles_full_charset_and_wraps() {
        let now = t0();
        let mut state = fresh_entry(now);
        // Single Down from 'A' wraps straight to '9'.
        state = step(state, Some(Input::Down), now, Mode::Initials);
        assert_eq!(cursor_of(&state), 35);
        assert_eq!(CHARSET[cursor_of(&state)], '9');
        // 35 more Downs walk the whole wheel back to 'A'.
        for expected in (0..35).rev() {
            state = step(state, Some(Input::Down), now, Mode::Initials);
            assert_eq!(cursor_of(&state), expected);
        }
        assert_eq!(cursor_of(&state), 0);
    }

    #[test]
    fn three_confirms_yield_done_with_the_right_string() {
        let now = t0();
        let mut state = fresh_entry(now);
        // Slot 1: confirm 'A'.
        state = step(state, Some(Input::Confirm), now, Mode::Initials);
        // Slot 2: Up once ('A' -> 'B'; cursor carried over), confirm.
        state = step(state, Some(Input::Up), now, Mode::Initials);
        state = step(state, Some(Input::Confirm), now, Mode::Initials);
        // Slot 3: Down twice from 'B' wraps through 'A' to '9', confirm.
        state = step(state, Some(Input::Down), now, Mode::Initials);
        state = step(state, Some(Input::Down), now, Mode::Initials);
        state = step(state, Some(Input::Confirm), now, Mode::Initials);
        assert_eq!(state, AttractState::Done("AB9".to_owned()));
    }

    #[test]
    fn confirm_carries_cursor_to_next_slot() {
        let now = t0();
        let mut state = fresh_entry(now);
        for _ in 0..3 {
            state = step(state, Some(Input::Up), now, Mode::Initials); // 'D'
        }
        state = step(state, Some(Input::Confirm), now, Mode::Initials);
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
        state = step(state, Some(Input::Up), now, Mode::Initials); // 'B'
        state = step(state, Some(Input::Confirm), now, Mode::Initials); // chars = "B"
        state = step(state, Some(Input::Down), now, Mode::Initials);
        state = step(state, Some(Input::Down), now, Mode::Initials); // cursor on '9'
        state = step(state, Some(Input::Backspace), now, Mode::Initials);
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
    fn backspace_on_empty_stays_in_entry_post_run() {
        // Post-run there is no idle reel to fall back to: undo on an empty
        // entry is just activity.
        let now = t0();
        let state = fresh_entry(now);
        let state = step(state, Some(Input::Backspace), now, Mode::Initials);
        assert!(matches!(state, AttractState::InitialsEntry { .. }));
    }

    #[test]
    fn entry_timeout_auto_submits_padded() {
        let now = t0();
        let mut state = fresh_entry(now);
        // Lock in 'C', then walk away.
        state = step(state, Some(Input::Up), now, Mode::Initials);
        state = step(state, Some(Input::Up), now, Mode::Initials);
        state = step(state, Some(Input::Confirm), now, Mode::Initials);
        // 19.9s after the last input: still on the wheel.
        let state = step(
            state,
            None,
            now + Duration::from_millis(19_900),
            Mode::Initials,
        );
        assert!(matches!(state, AttractState::InitialsEntry { .. }));
        // 20s: auto-submit, padded with 'A'.
        let state = step(state, None, now + Duration::from_secs(20), Mode::Initials);
        assert_eq!(state, AttractState::Done("CAA".to_owned()));
    }

    #[test]
    fn entry_timeout_with_nothing_entered_submits_aaa() {
        let now = t0();
        let state = fresh_entry(now);
        let state = step(state, None, now + ENTRY_TIMEOUT, Mode::Initials);
        assert_eq!(state, AttractState::Done("AAA".to_owned()));
    }

    #[test]
    fn inputs_refresh_the_entry_timeout() {
        let now = t0();
        let state = fresh_entry(now);
        let state = step(
            state,
            Some(Input::Up),
            now + Duration::from_secs(15),
            Mode::Initials,
        );
        // 30s after entry started, but only 15s after the last input.
        let state = step(state, None, now + Duration::from_secs(30), Mode::Initials);
        assert!(matches!(state, AttractState::InitialsEntry { .. }));
        // 35s after entry: 20s since last input — auto-submit.
        let state = step(state, None, now + Duration::from_secs(35), Mode::Initials);
        assert!(matches!(state, AttractState::Done(_)));
    }

    #[test]
    fn pad_initials_pads_with_a() {
        assert_eq!(pad_initials(""), "AAA");
        assert_eq!(pad_initials("X"), "XAA");
        assert_eq!(pad_initials("XY"), "XYA");
        assert_eq!(pad_initials("XYZ"), "XYZ");
        assert_eq!(pad_initials("XYZW"), "XYZ"); // over-long input clamped
    }

    #[test]
    fn dwell_advances_reel_round_robin() {
        let now = t0();
        let mut state = AttractState::initial(Mode::Attract, now);
        // Sub-dwell tick: nothing moves.
        let ticked = step(
            state.clone(),
            None,
            now + Duration::from_millis(7_900),
            Mode::Attract,
        );
        assert_eq!(ticked, state);

        let mut t = now;
        let mut seen = Vec::new();
        for _ in 0..REEL_LEN {
            match &state {
                AttractState::Idle { board_idx, .. } => seen.push(reel_screen(*board_idx)),
                other => panic!("expected Idle, got {other:?}"),
            }
            t += IDLE_DWELL;
            state = step(state, None, t, Mode::Attract);
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
    fn start_in_attract_mode_is_terminal_and_other_inputs_do_nothing() {
        let now = t0();
        let idle = AttractState::initial(Mode::Attract, now);
        for input in [Input::Up, Input::Down, Input::Confirm, Input::Backspace] {
            assert_eq!(step(idle.clone(), Some(input), now, Mode::Attract), idle);
        }
        // Start hands off immediately: the run launches, initials come
        // after it (classic arcade order).
        let state = step(idle, Some(Input::Start), now, Mode::Attract);
        assert_eq!(state, AttractState::Done(String::new()));
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
            for mode in [Mode::Attract, Mode::Initials] {
                assert_eq!(
                    step(done.clone(), input, now + Duration::from_secs(60), mode),
                    done
                );
            }
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
    fn url_host_and_loopback_detection() {
        assert_eq!(url_host("http://127.0.0.1:8080"), Some("127.0.0.1"));
        assert_eq!(url_host("http://10.20.30.41:8080/v1"), Some("10.20.30.41"));
        assert_eq!(
            url_host("https://boards.corp.example"),
            Some("boards.corp.example")
        );
        assert_eq!(url_host("ftp://x"), None);
        assert_eq!(url_host("http://"), None);
        assert!(is_loopback_url("http://127.0.0.1:8080"));
        assert!(is_loopback_url("http://localhost:8080"));
        assert!(!is_loopback_url("http://10.0.0.5:8080"));
        assert!(!is_loopback_url("http://boards.corp.example"));
    }

    #[test]
    fn swap_host_keeps_scheme_port_and_path() {
        assert_eq!(
            swap_host("http://127.0.0.1:8080", "10.20.30.41").as_deref(),
            Some("http://10.20.30.41:8080")
        );
        assert_eq!(
            swap_host("http://localhost:8080/v1/boards", "192.168.1.7").as_deref(),
            Some("http://192.168.1.7:8080/v1/boards")
        );
        assert_eq!(
            swap_host("http://127.0.0.1", "10.0.0.2").as_deref(),
            Some("http://10.0.0.2")
        );
        assert_eq!(swap_host("nonsense", "10.0.0.2"), None);
    }

    #[test]
    fn spaced_letterspacing() {
        assert_eq!(spaced(""), "");
        assert_eq!(spaced("A"), "A");
        assert_eq!(spaced("ABC"), "A B C");
        assert_eq!(spaced("HIGH SCORE"), "H I G H   S C O R E");
    }

    #[test]
    fn fire_burns_upward_and_stays_bounded() {
        let mut sim = FireSim::new(60, 40);
        // Bottom row starts (and stays) white-hot.
        let bottom = |s: &FireSim| (0..60).map(|x| s.heat[39 * 60 + x] as u32).sum::<u32>();
        assert_eq!(bottom(&sim), 60 * HEAT_MAX as u32);
        for _ in 0..200 {
            sim.step();
        }
        assert_eq!(bottom(&sim), 60 * HEAT_MAX as u32);
        // Flames climbed: the row just above the base is hot...
        let row_sum =
            |s: &FireSim, y: usize| (0..60).map(|x| s.heat[y * 60 + x] as u32).sum::<u32>();
        assert!(row_sum(&sim, 38) > 0, "no heat directly above the base");
        // ...and heat decays with altitude (top far cooler than base).
        assert!(
            row_sum(&sim, 0) < row_sum(&sim, 38),
            "fire did not decay with height"
        );
        // All values stay within the palette.
        assert!(sim.heat.iter().all(|&h| h <= HEAT_MAX));
    }

    #[test]
    fn fire_rgba_shape_and_alpha() {
        let mut sim = FireSim::new(8, 6);
        sim.step();
        let rgba = sim.rgba();
        assert_eq!(rgba.len(), 8 * 6 * 4);
        // Bottom-row pixels are opaque white-hot; a cold pixel is
        // transparent.
        let bottom_px = &rgba[(5 * 8) * 4..(5 * 8) * 4 + 4];
        assert_eq!(bottom_px[3], 255);
        assert_eq!(&bottom_px[..3], &[255, 255, 255]);
        let top_px = &rgba[..4];
        assert_eq!(top_px[3], 0, "cold pixel should be transparent");
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(
            match std::env::var("ARCADE_MODE_TEST_UNSET").as_deref() {
                Ok("initials") => Mode::Initials,
                _ => Mode::Attract,
            },
            Mode::Attract
        );
    }
}
