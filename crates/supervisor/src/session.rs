//! One player session: runtime dir seeding, gzdoom process lifecycle, and
//! the telemetry event pump (SPEC §8.2/§8.4, §4.6).
//!
//! Flow of [`run_session`]:
//!
//! 1. Purge and re-seed `<runtime>/session/` from the pristine config
//!    template, so no player inherits another's rebinds, autosaves, or
//!    console state.
//! 2. Create `<runtime>/events.fifo` (unlinking any stale one first) and
//!    open it **read+write**. Opening a FIFO read-only blocks until a
//!    writer appears — gzdoom may open its logfile late, or never (wrong
//!    build, `-logfile` unsupported) — and once the writer closed, reads
//!    would return EOF forever. Holding our own write end makes the open
//!    non-blocking and the read end EOF-free for the whole session; the
//!    leftover buffered lines are drained with short timeouts at session
//!    end instead of relying on EOF.
//! 3. Spawn gzdoom with the exact SPEC §8.2 argument vector (plus dev-mode
//!    extras) and read **both** the FIFO and the child's stdout line by
//!    line, concurrently — whichever transport the pinned GZDoom build
//!    actually uses (SPEC §13.1), the events arrive.
//! 4. Feed every line through [`protocol::parse_event_line`] into
//!    [`RunState`]. On `player_died`: SIGTERM after 3 s (death-animation
//!    linger), SIGKILL 10 s later if it ignored the SIGTERM. On
//!    `run_complete`: SIGTERM after 2 s, same SIGKILL escalation.
//! 5. Watchdog: if *no line at all* (event or chatter, either stream)
//!    arrives for 20 minutes, the run is abandoned and the engine is
//!    killed (SIGTERM, then SIGKILL after 10 s).
//! 6. The child is always reaped: a dedicated task owns the [`Child`] and
//!    `wait()`s it, even if the pump errors out early.
//!
//! If the child exits without a terminal event, the run ends as
//! [`EndReason::Abandoned`] with partial stats kept.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context as _;
use nix::sys::signal::{kill, Signal};
use nix::sys::stat::Mode;
use nix::unistd::Pid;
use protocol::{parse_event_line, EndReason, Event};
use time::OffsetDateTime;
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, BufReader, Lines};
use tokio::net::unix::pipe;
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tracing::{debug, info, trace, warn};

use crate::config::Config;
use crate::run_state::{ApplyOutcome, FinishedRun, RunState};

/// Abandon-and-kill threshold: no output of any kind for this long
/// (SPEC §8.4).
pub const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Linger after `player_died` before SIGTERM (death animation, gibs).
const DEATH_LINGER: Duration = Duration::from_secs(3);
/// Linger after `run_complete` before SIGTERM.
const COMPLETE_LINGER: Duration = Duration::from_secs(2);
/// Grace between SIGTERM and SIGKILL escalation.
const KILL_GRACE: Duration = Duration::from_secs(10);
/// Max quiet time while draining buffered lines after child exit.
const DRAIN_QUIET: Duration = Duration::from_millis(200);

/// Runs one full gzdoom session for `session_id`/`initials` and returns
/// the finished run. Errors only on setup failures (bad runtime dir,
/// spawn failure); once gzdoom is up, every outcome — death, clear,
/// crash, watchdog — comes back as a `FinishedRun`.
pub async fn run_session(
    cfg: &Config,
    session_id: &str,
    initials: &str,
) -> anyhow::Result<FinishedRun> {
    let started_at = OffsetDateTime::now_utc();
    let session_dir = cfg.runtime_dir.join("session");
    seed_session_dir(cfg, &session_dir)?;
    let fifo_path = cfg.runtime_dir.join("events.fifo");
    make_fifo(&fifo_path)?;
    // Read+write: see the module docs for why not read-only.
    let fifo = pipe::OpenOptions::new()
        .read_write(true)
        .open_receiver(&fifo_path)
        .with_context(|| format!("opening event fifo {}", fifo_path.display()))?;

    let mut cmd = Command::new(&cfg.gzdoom_bin);
    cmd.arg("-iwad").arg(&cfg.iwad);
    match &cfg.pk3 {
        Some(pk3) => {
            cmd.arg("-file").arg(pk3);
        }
        None => warn!("ARCADE_PK3 not set; running without telemetry — this run cannot score"),
    }
    cmd.arg("-config").arg(session_dir.join("gzdoom.ini"));
    cmd.arg("-savedir").arg(session_dir.join("saves"));
    cmd.arg("-skill").arg("3");
    cmd.arg("-warp").arg("1");
    cmd.arg("+set").arg("arcade_session").arg(session_id);
    cmd.arg("+set").arg("arcade_initials").arg(initials);
    cmd.arg("+set").arg("vid_fps").arg("0");
    cmd.arg("+logfile").arg(&fifo_path);
    if cfg.dev {
        cmd.arg("-width").arg("1280");
        cmd.arg("-height").arg("720");
        cmd.arg("+vid_fullscreen").arg("0");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning gzdoom ({})", cfg.gzdoom_bin.display()))?;
    let pid = Pid::from_raw(child.id().context("gzdoom pid unavailable")? as i32);
    info!(%pid, session = %session_id, "gzdoom spawned");
    let stdout = child.stdout.take().expect("stdout was piped");

    // Hand the child to a dedicated reaper so it is wait()ed no matter how
    // the pump below exits.
    let (exit_tx, mut exit_rx) = oneshot::channel();
    tokio::spawn(async move {
        let status = child.wait().await;
        let _ = exit_tx.send(status);
    });

    let mut fifo_lines = BufReader::new(fifo).lines();
    let mut stdout_lines = BufReader::new(stdout).lines();
    let mut fifo_open = true;
    let mut stdout_open = true;
    let mut pump = EventPump {
        state: RunState::new(session_id, initials, &cfg.cabinet_id, &cfg.iwad_sha256),
        pid,
        term_at: None,
        kill_at: None,
        watchdog_at: Instant::now() + WATCHDOG_TIMEOUT,
    };

    loop {
        tokio::select! {
            // NOTE: tokio::select! evaluates every branch expression even
            // when its `if` guard is false (it just does not poll it), so
            // the sleep_until args must be total — hence unwrap_or(far).
            line = fifo_lines.next_line(), if fifo_open => match line {
                Ok(Some(line)) => pump.on_line("fifo", &line),
                Ok(None) => {
                    debug!("event fifo reached EOF");
                    fifo_open = false;
                }
                Err(err) => {
                    warn!(%err, "error reading event fifo");
                    fifo_open = false;
                }
            },
            line = stdout_lines.next_line(), if stdout_open => match line {
                Ok(Some(line)) => pump.on_line("stdout", &line),
                Ok(None) => {
                    debug!("gzdoom stdout closed");
                    stdout_open = false;
                }
                Err(err) => {
                    warn!(%err, "error reading gzdoom stdout");
                    stdout_open = false;
                }
            },
            status = &mut exit_rx => {
                match status {
                    Ok(Ok(status)) => info!(%status, "gzdoom exited"),
                    Ok(Err(err)) => warn!(%err, "waiting for gzdoom"),
                    Err(_) => warn!("gzdoom reaper vanished"),
                }
                break;
            }
            _ = tokio::time::sleep_until(pump.term_at.unwrap_or_else(far_future)),
                if pump.term_at.is_some() =>
            {
                info!("sending SIGTERM to gzdoom");
                send_signal(pump.pid, Signal::SIGTERM);
                pump.term_at = None;
            }
            _ = tokio::time::sleep_until(pump.kill_at.unwrap_or_else(far_future)),
                if pump.kill_at.is_some() =>
            {
                warn!("gzdoom ignored SIGTERM; sending SIGKILL");
                send_signal(pump.pid, Signal::SIGKILL);
                pump.kill_at = None;
            }
            _ = tokio::time::sleep_until(pump.watchdog_at) => {
                warn!(
                    "no output for {}s; abandoning run and killing gzdoom",
                    WATCHDOG_TIMEOUT.as_secs()
                );
                send_signal(pump.pid, Signal::SIGTERM);
                let now = Instant::now();
                pump.kill_at = Some(now + KILL_GRACE);
                // Re-arm far enough out that it cannot re-fire before the
                // SIGKILL path settles things.
                pump.watchdog_at = now + WATCHDOG_TIMEOUT;
            }
        }
    }

    // The child is gone, but the fifo (which we also hold a write end of)
    // never signals EOF — bounded-drain whatever it still buffers, e.g. a
    // level_complete flushed during engine shutdown.
    if fifo_open {
        drain_lines(&mut fifo_lines, "fifo", &mut pump).await;
    }
    if stdout_open {
        drain_lines(&mut stdout_lines, "stdout", &mut pump).await;
    }
    if let Err(err) = std::fs::remove_file(&fifo_path) {
        debug!(%err, "removing event fifo");
    }

    let end_reason = pump.state.terminal_reason().unwrap_or(EndReason::Abandoned);
    if end_reason == EndReason::Abandoned {
        warn!(session = %session_id, "gzdoom ended without a terminal event; run abandoned");
    }
    let ended_at = OffsetDateTime::now_utc();
    let finished = pump.state.finish(end_reason, started_at, ended_at);
    info!(
        session = %session_id,
        end_reason = %finished.end_reason,
        score = finished.submission.run_score,
        maps_completed = finished.submission.maps_completed,
        "run finished"
    );
    Ok(finished)
}

/// Mutable pump state shared by the select arms: the run state machine
/// plus the pending signal deadlines.
struct EventPump {
    state: RunState,
    pid: Pid,
    /// When to send SIGTERM (armed by a terminal event or the watchdog).
    term_at: Option<Instant>,
    /// When to escalate to SIGKILL.
    kill_at: Option<Instant>,
    /// When the watchdog declares the run abandoned.
    watchdog_at: Instant,
}

impl EventPump {
    /// Handles one line from either stream: feeds the watchdog, parses,
    /// applies, and arms the kill timers on terminal events.
    fn on_line(&mut self, source: &'static str, line: &str) {
        // ANY line is proof of life, event or not (SPEC §8.4).
        self.watchdog_at = Instant::now() + WATCHDOG_TIMEOUT;
        trace!(source, %line, "line");
        let Some(event) = parse_event_line(line) else {
            return;
        };
        match self.state.apply(&event) {
            ApplyOutcome::Applied => match &event {
                Event::PlayerDied { map, .. } => {
                    info!(%map, "player died; SIGTERM in {}s", DEATH_LINGER.as_secs());
                    let now = Instant::now();
                    self.term_at = Some(now + DEATH_LINGER);
                    self.kill_at = Some(now + DEATH_LINGER + KILL_GRACE);
                }
                Event::RunComplete { .. } => {
                    info!("run complete; SIGTERM in {}s", COMPLETE_LINGER.as_secs());
                    let now = Instant::now();
                    self.term_at = Some(now + COMPLETE_LINGER);
                    self.kill_at = Some(now + COMPLETE_LINGER + KILL_GRACE);
                }
                _ => debug!(?event, "event applied"),
            },
            outcome => debug!(?outcome, ?event, "event dropped"),
        }
    }
}

/// Reads lines until the stream stays quiet for [`DRAIN_QUIET`], hits
/// EOF, or errors. Used after child exit, when no more producers exist.
async fn drain_lines<R: AsyncBufRead + Unpin>(
    lines: &mut Lines<R>,
    source: &'static str,
    pump: &mut EventPump,
) {
    while let Ok(Ok(Some(line))) = tokio::time::timeout(DRAIN_QUIET, lines.next_line()).await {
        pump.on_line(source, &line);
    }
}

/// Purges and re-seeds the per-session runtime dir: fresh `gzdoom.ini`
/// from the pristine template (or empty if none is configured) and an
/// empty `saves/`.
fn seed_session_dir(cfg: &Config, session_dir: &Path) -> anyhow::Result<()> {
    if session_dir.exists() {
        std::fs::remove_dir_all(session_dir)
            .with_context(|| format!("purging session dir {}", session_dir.display()))?;
    }
    std::fs::create_dir_all(session_dir.join("saves"))
        .with_context(|| format!("creating session dir {}", session_dir.display()))?;
    let ini = session_dir.join("gzdoom.ini");
    match &cfg.config_template {
        Some(template) => {
            // An unreadable template should not brick the cabinet: fall
            // back to an empty ini (gzdoom fills in defaults) and complain.
            if let Err(err) = std::fs::copy(template, &ini) {
                warn!(
                    template = %template.display(),
                    %err,
                    "cannot copy config template; seeding empty gzdoom.ini"
                );
                std::fs::write(&ini, b"").context("writing empty gzdoom.ini")?;
            }
        }
        None => {
            debug!("no ARCADE_CONFIG_TEMPLATE; seeding empty gzdoom.ini");
            std::fs::write(&ini, b"").context("writing empty gzdoom.ini")?;
        }
    }
    Ok(())
}

/// Creates the event FIFO, unlinking any stale one from a previous crash
/// first.
fn make_fifo(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => debug!(path = %path.display(), "unlinked stale event fifo"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("unlinking stale fifo {}", path.display()))
        }
    }
    nix::unistd::mkfifo(path, Mode::S_IRUSR | Mode::S_IWUSR)
        .with_context(|| format!("mkfifo {}", path.display()))?;
    Ok(())
}

/// A deadline that never fires within a session's lifetime; placeholder
/// argument for disabled `sleep_until` branches (see the select note).
fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(86_400)
}

/// Sends a signal, logging (not failing) on errors — ESRCH just means the
/// child already exited, which is fine.
fn send_signal(pid: Pid, signal: Signal) {
    match kill(pid, signal) {
        Ok(()) => debug!(%pid, ?signal, "signal sent"),
        Err(err) => debug!(%pid, ?signal, %err, "signal not delivered"),
    }
}
