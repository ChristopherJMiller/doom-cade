//! The forever loop (SPEC §8.1): attract → initials → gzdoom run → spool
//! → kick submitter → attract.
//!
//! The loop must never terminate. Each iteration runs inside its own
//! `tokio::spawn`, so a panic anywhere in session handling is caught as a
//! `JoinError` (instead of unwinding the loop) and, like an ordinary
//! error, is logged followed by a 5-second pause before the next
//! iteration. `Restart=always` on the systemd unit is the second line of
//! defense behind this.

use std::sync::Arc;
use std::time::Duration;

use tracing::{error, info, warn};

use crate::attract;
use crate::config::Config;
use crate::session;
use crate::spool::Spool;
use crate::submitter::{Submitter, SubmitterConfig};

/// Pause after a failed or panicked iteration before restarting the loop.
const RESTART_DELAY: Duration = Duration::from_secs(5);
/// Pause after the iwad guard trips, so a permanently missing IWAD cannot
/// hot-spin attract restarts.
const IWAD_GUARD_DELAY: Duration = Duration::from_secs(2);

/// Runs the supervisor forever. Opens the spool (retrying until it
/// succeeds — a cabinet with a broken spool disk should keep retrying,
/// not die), spawns the submitter, then loops sessions.
pub async fn run_forever(cfg: Config) {
    let spool = loop {
        match Spool::open(&cfg.spool_db) {
            Ok(spool) => break Arc::new(spool),
            Err(err) => {
                error!(
                    db = %cfg.spool_db.display(),
                    "cannot open spool: {err:#}; retrying in {}s",
                    RESTART_DELAY.as_secs()
                );
                tokio::time::sleep(RESTART_DELAY).await;
            }
        }
    };
    let submitter = Submitter::spawn(Arc::clone(&spool), SubmitterConfig::from_config(&cfg));
    // Flush anything spooled before the last shutdown.
    submitter.kick();

    loop {
        let iteration = iteration(cfg.clone(), Arc::clone(&spool), submitter.clone());
        match tokio::spawn(iteration).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                error!(
                    "session iteration failed: {err:#}; restarting loop in {}s",
                    RESTART_DELAY.as_secs()
                );
                tokio::time::sleep(RESTART_DELAY).await;
            }
            Err(join_err) => {
                error!(
                    "session iteration panicked: {join_err}; restarting loop in {}s",
                    RESTART_DELAY.as_secs()
                );
                tokio::time::sleep(RESTART_DELAY).await;
            }
        }
    }
}

/// One pass of SPEC §8.1: attract, iwad guard, session, spool, kick.
async fn iteration(cfg: Config, spool: Arc<Spool>, submitter: Submitter) -> anyhow::Result<()> {
    // Step 2: attract until a player enters initials (step 1, the runtime
    // dir purge, happens inside run_session immediately before spawning).
    let initials = attract::acquire_initials(&cfg).await;

    // IWAD guard: never crash-loop into gzdoom when the WAD is absent or
    // unreadable — log loudly and fall back to attract.
    if let Err(err) = std::fs::File::open(&cfg.iwad) {
        error!(
            iwad = %cfg.iwad.display(),
            %err,
            "IWAD absent or unreadable at spawn time; returning to attract"
        );
        tokio::time::sleep(IWAD_GUARD_DELAY).await;
        return Ok(());
    }

    // Step 3: mint the session UUID.
    let session_id = uuid::Uuid::new_v4().to_string();
    info!(session = %session_id, %initials, "starting run");

    // Steps 4–6: spawn gzdoom, pump events, decide the end reason.
    let finished = session::run_session(&cfg, &session_id, &initials).await?;

    // Step 7: the spool is the source of truth — write it before anything
    // touches the network.
    match spool.insert_run(&finished.submission) {
        Ok(true) => info!(session = %session_id, "run spooled"),
        Ok(false) => warn!(session = %session_id, "run was already spooled"),
        Err(err) => {
            // The run would otherwise be lost: dump the payload into the
            // journal so it can be recovered by hand.
            let payload = serde_json::to_string(&finished.submission)
                .unwrap_or_else(|e| format!("<unserializable: {e}>"));
            error!(session = %session_id, %payload, "FAILED to spool run: {err:#}");
        }
    }

    // Step 8: nudge the submitter (async, non-blocking).
    submitter.kick();
    Ok(())
}
