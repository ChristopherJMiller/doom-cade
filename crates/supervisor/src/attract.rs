//! Spawning and supervising the attract app (SPEC §11).
//!
//! Contract with `arcade-attract`: it runs fullscreen forever (the idle
//! reel loops internally) and, when a player finishes initials entry, it
//! prints exactly one line
//!
//! ```text
//! ARCADE_INITIALS ABC
//! ```
//!
//! to stdout, flushes, and exits 0. It never exits otherwise. The
//! supervisor therefore reads stdout line by line, ignoring everything
//! that is not a valid initials line, and treats an exit *without*
//! initials (crash, misconfiguration) as a failure: it logs and restarts
//! attract after 2 seconds, forever. There is no error path out of
//! [`acquire_initials`] — a cabinet with a broken attract binary shows
//! nothing, but the supervisor stays alive and keeps retrying.

use std::process::Stdio;
use std::time::Duration;

use anyhow::Context as _;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::Config;

/// Prefix of the initials handoff line, trailing space included.
pub const INITIALS_PREFIX: &str = "ARCADE_INITIALS ";

/// Delay before restarting attract after it exits without initials.
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

/// Runs attract (restarting it as needed) until a player enters initials,
/// and returns them. Never returns an error — see the module docs.
pub async fn acquire_initials(cfg: &Config) -> String {
    loop {
        match run_attract_once(cfg).await {
            Ok(Some(initials)) => {
                info!(%initials, "attract handed off initials");
                return initials;
            }
            Ok(None) => {
                warn!("attract exited without initials; restarting in 2s");
            }
            Err(err) => {
                warn!("attract failed: {err:#}; restarting in 2s");
            }
        }
        tokio::time::sleep(RESTART_DELAY).await;
    }
}

/// One attract lifetime: spawn, read stdout to EOF, reap, and return the
/// initials if a valid handoff line was seen.
async fn run_attract_once(cfg: &Config) -> anyhow::Result<Option<String>> {
    let mut cmd = Command::new(&cfg.attract_bin);
    cmd.env("ARCADE_LEADERBOARD_URL", &cfg.leaderboard_url);
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

    let mut initials: Option<String> = None;
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match parse_initials_line(&line) {
                Some(parsed) => {
                    if initials.is_none() {
                        initials = Some(parsed);
                    } else {
                        warn!(%line, "attract printed more than one initials line; keeping first");
                    }
                }
                None => debug!(%line, "attract chatter"),
            },
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
    Ok(initials)
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
}
