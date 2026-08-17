//! `arcade-supervisor` — the session loop for the DOOM arcade cabinet
//! (SPEC §8).
//!
//! The supervisor is the only long-lived process under the `cage` kiosk
//! compositor. Forever, it:
//!
//! 1. spawns [`arcade-attract`](attract) and waits for the player to enter
//!    initials,
//! 2. mints a session UUID and [spawns gzdoom](session) with a pristine
//!    per-session config, reading the telemetry event stream,
//! 3. folds events into a [`RunState`](run_state::RunState) and, when the
//!    run ends (death, clear, abandonment, or watchdog), computes the score,
//! 4. writes the finished run to the local [SQLite spool](spool) — the
//!    source of truth on the cabinet — and
//! 5. kicks the background [submitter](submitter), which POSTs pending runs
//!    to the leaderboard with retries and exponential backoff.
//!
//! All logic lives in this library so tests can drive it without spawning
//! real processes; `src/main.rs` is a thin binary shell. The replay-fixture
//! integration tests in `tests/` (SPEC §12) exercise the full
//! parse-and-accumulate path against recorded event streams.

pub mod attract;
pub mod config;
pub mod main_loop;
pub mod run_state;
pub mod session;
pub mod spool;
pub mod submitter;
