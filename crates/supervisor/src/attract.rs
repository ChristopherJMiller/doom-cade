//! Spawning and supervising the attract app (SPEC §11).
//!
//! The attract binary runs in one of two modes, selected via `ARCADE_MODE`:
//!
//! - **Attract** (default): the idle reel. When the player presses Start it
//!   prints exactly one line `ARCADE_START`, flushes, and exits 0.
//! - **Initials** (`ARCADE_MODE=initials`): the post-run initials wheel,
//!   shown with the finished run's score (`ARCADE_SCORE`) and end reason
//!   (`ARCADE_END_REASON`). It prints exactly one line
//!   `ARCADE_INITIALS ABC`, flushes, and exits 0 — entering three
//!   characters or timing out and auto-padding, so it always hands off.
//!
//! The supervisor reads stdout line by line, ignoring everything that is
//! not the expected handoff line. [`wait_for_start`] restarts a crashed
//! attract forever (nothing is at stake yet); [`acquire_initials`] retries
//! only a few times and then falls back to `AAA` — a finished score must
//! never be held hostage by a broken attract binary.

use std::process::Stdio;
use std::time::Duration;

use anyhow::Context as _;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::Config;

/// Prefix of the initials handoff line, trailing space included.
pub const INITIALS_PREFIX: &str = "ARCADE_INITIALS ";

/// The attract-mode handoff line (Start pressed; no payload).
pub const START_LINE: &str = "ARCADE_START";

/// Initials used when the post-run attract cannot produce any (crash
/// loop): the score still gets on the board.
pub const FALLBACK_INITIALS: &str = "AAA";

/// Attempts at collecting post-run initials before falling back.
const INITIALS_ATTEMPTS: u32 = 3;

/// Delay before restarting attract after it exits without a handoff.
const RESTART_DELAY: Duration = Duration::from_secs(2);
/// How long attract gets to exit after closing stdout before being killed.
const EXIT_GRACE: Duration = Duration::from_secs(10);

/// Parses one attract stdout line, returning validated initials.
///
/// The line must be exactly `ARCADE_INITIALS ` followed by three `[A-Z0-9]`
/// characters ([`protocol::validate_initials`]); trailing CR/LF is
/// forgiven. Anything else — chatter, lowercase, wrong length — is `None`.
pub fn parse_initials_line(line: &str) -> Option<String> {
    let rest = line
        .trim_end_matches(['\r', '\n'])
        .strip_prefix(INITIALS_PREFIX)?;
    protocol::validate_initials(rest).then(|| rest.to_owned())
}

/// Recognizes the attract-mode Start handoff line.
pub fn is_start_line(line: &str) -> bool {
    line.trim_end_matches(['\r', '\n']) == START_LINE
}

/// What one attract lifetime should produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttractMode {
    Attract,
    Initials,
}

/// Runs the idle attract (restarting it as needed) until the player
/// presses Start. Never returns an error — a cabinet with a broken attract
/// binary shows nothing, but the supervisor stays alive and keeps
/// retrying.
pub async fn wait_for_start(cfg: &Config) {
    loop {
        match run_attract_once(cfg, AttractMode::Attract, None, None).await {
            Ok(Some(_)) => {
                info!("attract handed off: start pressed");
                return;
            }
            Ok(None) => warn!("attract exited without start; restarting in 2s"),
            Err(err) => warn!("attract failed: {err:#}; restarting in 2s"),
        }
        tokio::time::sleep(RESTART_DELAY).await;
    }
}

/// Runs the post-run initials screen for a finished run and returns the
/// entered (or auto-padded) initials. Retries a few times on failure, then
/// falls back to [`FALLBACK_INITIALS`] so the score is never lost.
pub async fn acquire_initials(cfg: &Config, score: i64, end_reason: &str) -> String {
    for attempt in 1..=INITIALS_ATTEMPTS {
        match run_attract_once(cfg, AttractMode::Initials, Some(score), Some(end_reason)).await {
            Ok(Some(initials)) => {
                info!(%initials, "attract handed off initials");
                return initials;
            }
            Ok(None) => warn!(attempt, "initials attract exited without handoff"),
            Err(err) => warn!(attempt, "initials attract failed: {err:#}"),
        }
        tokio::time::sleep(RESTART_DELAY).await;
    }
    warn!(
        fallback = FALLBACK_INITIALS,
        "initials screen unavailable; submitting fallback initials"
    );
    FALLBACK_INITIALS.to_owned()
}

/// One attract lifetime: spawn in `mode`, read stdout to EOF, reap, and
/// return the handoff payload if the expected line was seen (`Some("")`
/// for the start line).
async fn run_attract_once(
    cfg: &Config,
    mode: AttractMode,
    score: Option<i64>,
    end_reason: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let mut cmd = Command::new(&cfg.attract_bin);
    cmd.env("ARCADE_LEADERBOARD_URL", &cfg.leaderboard_url);
    if let Some(public_url) = &cfg.public_url {
        cmd.env("ARCADE_PUBLIC_URL", public_url);
    }
    if let AttractMode::Initials = mode {
        cmd.env("ARCADE_MODE", "initials");
        if let Some(score) = score {
            cmd.env("ARCADE_SCORE", score.to_string());
        }
        if let Some(reason) = end_reason {
            cmd.env("ARCADE_END_REASON", reason);
        }
    }
    if cfg.iwad_unverified {
        cmd.env("ARCADE_IWAD_UNVERIFIED", "1");
    }
    if cfg.dev {
        cmd.env("ARCADE_WINDOWED", "1");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning attract ({})", cfg.attract_bin.display()))?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let mut lines = BufReader::new(stdout).lines();

    let mut payload: Option<String> = None;
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let parsed = match mode {
                    AttractMode::Attract => is_start_line(&line).then(String::new),
                    AttractMode::Initials => parse_initials_line(&line),
                };
                match parsed {
                    Some(value) => {
                        if payload.is_none() {
                            payload = Some(value);
                        } else {
                            warn!(%line, "attract printed more than one handoff line; keeping first");
                        }
                    }
                    None => debug!(%line, "attract chatter"),
                }
            }
            Ok(None) => break, // EOF: attract closed stdout / exited
            Err(err) => {
                warn!(%err, "error reading attract stdout");
                break;
            }
        }
    }

    // Always reap. Per the contract it exits right after the handoff; give
    // it a grace period, then kill.
    match tokio::time::timeout(EXIT_GRACE, child.wait()).await {
        Ok(Ok(status)) => debug!(%status, "attract exited"),
        Ok(Err(err)) => warn!(%err, "waiting for attract"),
        Err(_) => {
            warn!("attract did not exit after closing stdout; killing it");
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_contract_line() {
        assert_eq!(
            parse_initials_line("ARCADE_INITIALS ABC").as_deref(),
            Some("ABC")
        );
        assert_eq!(
            parse_initials_line("ARCADE_INITIALS X99").as_deref(),
            Some("X99")
        );
        assert_eq!(
            parse_initials_line("ARCADE_INITIALS 000").as_deref(),
            Some("000")
        );
        // Trailing newline junk from line-splitting is forgiven.
        assert_eq!(
            parse_initials_line("ARCADE_INITIALS ABC\r").as_deref(),
            Some("ABC")
        );
    }

    #[test]
    fn rejects_invalid_initials() {
        for bad in [
            "ARCADE_INITIALS abc",  // lowercase
            "ARCADE_INITIALS AB",   // too short
            "ARCADE_INITIALS ABCD", // too long
            "ARCADE_INITIALS A B",  // space
            "ARCADE_INITIALS A-C",  // punctuation
            "ARCADE_INITIALS ",     // empty
            "ARCADE_INITIALS",      // no space, no payload
            " ARCADE_INITIALS ABC", // leading junk
            "initials ABC",         // wrong sentinel
            "boards fetched: 5",    // ordinary chatter
            "",
        ] {
            assert_eq!(parse_initials_line(bad), None, "input: {bad:?}");
        }
    }

    #[test]
    fn recognizes_the_start_line() {
        assert!(is_start_line("ARCADE_START"));
        assert!(is_start_line("ARCADE_START\r"));
        for bad in [
            "ARCADE_START pressed",
            " ARCADE_START",
            "ARCADE_STARTED",
            "ARCADE_INITIALS ABC",
            "",
        ] {
            assert!(!is_start_line(bad), "input: {bad:?}");
        }
    }
}
