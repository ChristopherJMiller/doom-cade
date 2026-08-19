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
//!    actually uses (SPEC §13.1), the events arrive. The pinned build
//!    mirrors the same `ARCADE_EVT` lines to both (pk3/NOTES.md): the FIFO
//!    delivers in real time while piped stdout is block-buffered and lags.
//!    So the first event parsed from the FIFO latches it as the telemetry
//!    transport; from then on stdout telemetry is ignored (it is the same
//!    stream, arriving late — applying both would double-count maps).
//!    Stdout lines still count as liveness for the watchdog.
//! 4. Feed every line through [`protocol::parse_event_line`] into
//!    [`RunState`]. On `player_died`: SIGTERM after 3 s (death-animation
//!    linger), SIGKILL 10 s later if it ignored the SIGTERM. On
//!    `run_complete`: SIGTERM after 2 s, same SIGKILL escalation. On
//!    `run_quit` (the player held Start): SIGTERM after 1 s, same
//!    escalation — the pk3 only announces the quit; killing the engine is
//!    the supervisor's job.
//! 5. Abandonment guards, innermost to outermost — each fires the same
//!    kill path (SIGTERM, then SIGKILL after 10 s) and the run ends as
//!    [`EndReason::Abandoned`] with partial stats kept:
//!    - **Walk-away**: every applied `progress` heartbeat feeds the pure
//!      [`IdleTracker`]; when neither the player position nor the
//!      kills+secrets+items sum has changed for `idle_timeout` (default
//!      180 s), the player has walked away and the partial score is
//!      banked.
//!    - **Telemetry stall**: no telemetry event past the transport gate
//!      (see step 3) for `stall_timeout` (default 300 s). Heartbeats
//!      normally flow every ~2 s in-world, but intermission screens pause
//!      them — hence 300 s, not 60. Disabled when no pk3 is configured:
//!      with no telemetry to expect, silence proves nothing.
//!    - **Watchdog**: *no line at all* (event or chatter, either stream)
//!      for 20 minutes. The outer backstop.
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
use protocol::{parse_event_line, EndReason, Event, MAX_EVENT_LINE_BYTES};
use time::OffsetDateTime;
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, BufReader};
use tokio::net::unix::pipe;
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tracing::{debug, info, trace, warn};

use crate::config::Config;
use crate::idle::IdleTracker;
use crate::run_state::{ApplyOutcome, FinishedRun, RunState};

/// Abandon-and-kill threshold: no output of any kind for this long
/// (SPEC §8.4).
pub const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Linger after `player_died` before SIGTERM (death animation, gibs).
const DEATH_LINGER: Duration = Duration::from_secs(3);
/// Linger after `run_complete` before SIGTERM.
const COMPLETE_LINGER: Duration = Duration::from_secs(2);
/// Linger after `run_quit` before SIGTERM. Short: the player already asked
/// to leave, and the pk3 cannot exit the engine itself.
const QUIT_LINGER: Duration = Duration::from_secs(1);
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

    let mut fifo_lines = CappedLines::new(BufReader::new(fifo));
    let mut stdout_lines = CappedLines::new(BufReader::new(stdout));
    let mut fifo_open = true;
    let mut stdout_open = true;
    let mut pump = EventPump::new(
        RunState::new(session_id, initials, &cfg.cabinet_id, &cfg.iwad_sha256),
        pid,
        cfg.idle_timeout,
        // No pk3 means no telemetry to expect: a telemetry-silence guard
        // would kill every (unscored) run at the stall window, so only the
        // outer any-line watchdog applies.
        cfg.pk3.is_some().then_some(cfg.stall_timeout),
    );

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
            _ = tokio::time::sleep_until(pump.idle_at.unwrap_or_else(far_future)),
                if pump.idle_at.is_some() =>
            {
                warn!(
                    "no movement or scoring for {}s; player walked away; \
                     banking partial score and killing gzdoom",
                    cfg.idle_timeout.as_secs()
                );
                pump.abandon_now();
            }
            _ = tokio::time::sleep_until(pump.stall_at.unwrap_or_else(far_future)),
                if pump.stall_at.is_some() =>
            {
                warn!(
                    "no telemetry for {}s; abandoning run and killing gzdoom",
                    cfg.stall_timeout.as_secs()
                );
                pump.abandon_now();
            }
            _ = tokio::time::sleep_until(pump.watchdog_at) => {
                warn!(
                    "no output for {}s; abandoning run and killing gzdoom",
                    WATCHDOG_TIMEOUT.as_secs()
                );
                pump.abandon_now();
                // Re-arm far enough out that it cannot re-fire before the
                // SIGKILL path settles things.
                pump.watchdog_at = Instant::now() + WATCHDOG_TIMEOUT;
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
    /// Set once any telemetry event has parsed from the FIFO. The pinned
    /// GZDoom mirrors the same `ARCADE_EVT` lines to stdout, block-buffered
    /// and lagged (pk3/NOTES.md §13.1) — once the FIFO is known live,
    /// stdout copies are stale duplicates and must not reach the state
    /// machine (a lagged enter+complete block replayed cross-stream would
    /// double-count a map). Stdout stays a watchdog liveness source.
    fifo_latched: bool,
    /// Walk-away detector, fed by every applied `progress` heartbeat.
    idle: IdleTracker,
    /// When the walk-away guard declares the run abandoned: the idle
    /// tracker's deadline, mirrored into a field so the select loop can
    /// sleep on it. `None` before the first heartbeat and once the run is
    /// decided.
    idle_at: Option<Instant>,
    /// Telemetry-stall window; `None` disables the stall guard entirely
    /// (no pk3 → no telemetry to expect).
    stall_timeout: Option<Duration>,
    /// When the stall guard declares the run abandoned. Re-armed to
    /// now + `stall_timeout` by every parsed telemetry event; `None` when
    /// the guard is disabled or once the run is decided.
    stall_at: Option<Instant>,
    /// Set once an abandonment guard has fired. Blocks the guards from
    /// re-arming: an engine that ignores SIGTERM but keeps heart-beating
    /// must not push its own SIGKILL deadline out forever.
    abandoning: bool,
    /// When to send SIGTERM (armed by a terminal event or the watchdog).
    term_at: Option<Instant>,
    /// When to escalate to SIGKILL.
    kill_at: Option<Instant>,
    /// When the watchdog declares the run abandoned.
    watchdog_at: Instant,
}

impl EventPump {
    /// Creates a pump with every deadline armed from now. `stall_timeout:
    /// None` disables the telemetry-stall guard.
    fn new(
        state: RunState,
        pid: Pid,
        idle_timeout: Duration,
        stall_timeout: Option<Duration>,
    ) -> Self {
        let now = Instant::now();
        EventPump {
            state,
            pid,
            fifo_latched: false,
            idle: IdleTracker::new(idle_timeout),
            idle_at: None,
            stall_timeout,
            stall_at: stall_timeout.map(|t| now + t),
            abandoning: false,
            term_at: None,
            kill_at: None,
            watchdog_at: now + WATCHDOG_TIMEOUT,
        }
    }

    /// Handles one line from either stream: feeds the watchdog, parses,
    /// applies, feeds the walk-away/stall guards, and arms the kill timers
    /// on terminal events. Telemetry on stdout is dropped once the FIFO
    /// has delivered any event (see [`EventPump::fifo_latched`]).
    fn on_line(&mut self, source: &'static str, line: &str) {
        // ANY line is proof of life, event or not (SPEC §8.4).
        let now = Instant::now();
        self.watchdog_at = now + WATCHDOG_TIMEOUT;
        trace!(source, %line, "line");
        let Some(event) = parse_event_line(line) else {
            return;
        };
        if source == "fifo" {
            self.fifo_latched = true;
        } else if self.fifo_latched {
            debug!(
                ?event,
                "ignoring stdout telemetry; fifo is the live transport"
            );
            return;
        }
        // Any telemetry that gets past the transport gate — whatever the
        // state machine then does with it — proves the live pipeline is
        // still delivering: re-arm the stall guard while it is armed at
        // all. Post-latch stdout copies deliberately do NOT count: if the
        // FIFO dies, the lagged mirror can no longer advance the run, and
        // the stall guard is exactly what ends it.
        if self.stall_at.is_some() {
            self.stall_at = self.stall_timeout.map(|t| now + t);
        }
        match self.state.apply(&event) {
            ApplyOutcome::Applied => match &event {
                Event::PlayerDied { map, .. } => {
                    info!(%map, "player died; SIGTERM in {}s", DEATH_LINGER.as_secs());
                    self.arm_exit(DEATH_LINGER);
                }
                Event::RunComplete { .. } => {
                    info!("run complete; SIGTERM in {}s", COMPLETE_LINGER.as_secs());
                    self.arm_exit(COMPLETE_LINGER);
                }
                Event::RunQuit { map, .. } => {
                    info!(%map, "player quit the run; SIGTERM in {}s", QUIT_LINGER.as_secs());
                    self.arm_exit(QUIT_LINGER);
                }
                Event::Progress {
                    px,
                    py,
                    kills,
                    secrets,
                    items,
                    ..
                } => {
                    debug!(?event, "event applied");
                    if !self.abandoning {
                        let counters = kills.saturating_add(*secrets).saturating_add(*items);
                        self.idle_at = if self.idle.observe(now, *px, *py, counters) {
                            // The window elapsed between heartbeats (e.g.
                            // the loop was busy): fire the guard now.
                            Some(now)
                        } else {
                            self.idle.deadline()
                        };
                    }
                }
                _ => debug!(?event, "event applied"),
            },
            outcome => debug!(?outcome, ?event, "event dropped"),
        }
    }

    /// Arms the SIGTERM/SIGKILL pair `linger` from now and disarms the
    /// walk-away/stall guards: the run is decided, and a guard firing
    /// during the linger would only re-signal a child already being shut
    /// down.
    fn arm_exit(&mut self, linger: Duration) {
        let now = Instant::now();
        self.term_at = Some(now + linger);
        self.kill_at = Some(now + linger + KILL_GRACE);
        self.idle_at = None;
        self.stall_at = None;
    }

    /// The immediate abandonment kill path shared by the walk-away, stall,
    /// and watchdog guards: SIGTERM now, SIGKILL after the grace, and no
    /// guard may fire or re-arm again.
    fn abandon_now(&mut self) {
        send_signal(self.pid, Signal::SIGTERM);
        self.kill_at = Some(Instant::now() + KILL_GRACE);
        self.idle_at = None;
        self.stall_at = None;
        self.abandoning = true;
    }
}

/// Reads lines until the stream stays quiet for [`DRAIN_QUIET`], hits
/// EOF, or errors. Used after child exit, when no more producers exist.
async fn drain_lines<R: AsyncBufRead + Unpin>(
    lines: &mut CappedLines<R>,
    source: &'static str,
    pump: &mut EventPump,
) {
    while let Ok(Ok(Some(line))) = tokio::time::timeout(DRAIN_QUIET, lines.next_line()).await {
        pump.on_line(source, &line);
    }
}

/// Line reader with a hard per-line memory cap.
///
/// `tokio`'s `Lines::next_line` buffers an entire line before returning,
/// so [`protocol::MAX_EVENT_LINE_BYTES`] — checked only inside
/// [`parse_event_line`], i.e. after the line is fully resident — could
/// never bound memory: a newline-less byte stream on the FIFO or stdout
/// (wedged engine, runaway mod printing without line breaks) would grow
/// the supervisor's memory until the OOM killer took down the kiosk's one
/// long-lived process. This reader enforces the cap while reading: once a
/// line exceeds the cap it is dropped (one warning), remaining bytes are
/// discarded in bounded chunks until the next `\n`, and reading resumes.
///
/// `next_line` is cancel-safe (required by the `tokio::select!` pump):
/// partial-line state lives in `self` across `.await`s, and bytes are
/// `consume`d synchronously right after being copied out of `fill_buf`.
struct CappedLines<R> {
    reader: R,
    buf: Vec<u8>,
    /// True while discarding an oversized line's remainder up to `\n`.
    discarding: bool,
}

impl<R: AsyncBufRead + Unpin> CappedLines<R> {
    fn new(reader: R) -> Self {
        CappedLines {
            reader,
            buf: Vec::new(),
            discarding: false,
        }
    }

    /// Returns the next newline-terminated line (lossy UTF-8, without the
    /// terminator; a trailing `\r` is stripped), the final unterminated
    /// line at EOF, or `None` at EOF.
    async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                // EOF. A partial oversized line was already dropped.
                self.discarding = false;
                if self.buf.is_empty() {
                    return Ok(None);
                }
                let line = String::from_utf8_lossy(&self.buf).into_owned();
                self.buf.clear();
                return Ok(Some(line));
            }
            match available.iter().position(|&b| b == b'\n') {
                Some(pos) => {
                    if !self.discarding {
                        self.buf.extend_from_slice(&available[..pos]);
                    }
                    self.reader.consume(pos + 1);
                    if self.discarding {
                        // Oversized line fully skipped; resync on the
                        // next line.
                        self.discarding = false;
                        continue;
                    }
                    if self.buf.last() == Some(&b'\r') {
                        self.buf.pop();
                    }
                    let line = String::from_utf8_lossy(&self.buf).into_owned();
                    self.buf.clear();
                    return Ok(Some(line));
                }
                None => {
                    let n = available.len();
                    if !self.discarding {
                        self.buf.extend_from_slice(available);
                    }
                    self.reader.consume(n);
                    if self.buf.len() > MAX_EVENT_LINE_BYTES {
                        warn!(
                            bytes = self.buf.len(),
                            "dropping oversized line (no newline within the cap)"
                        );
                        self.buf.clear();
                        self.buf.shrink_to_fit();
                        self.discarding = true;
                    }
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::format_event_line;
    use time::macros::datetime;

    const SESSION: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    fn pump() -> EventPump {
        EventPump::new(
            RunState::new(SESSION, "ABC", "cab-test", "sha-test"),
            Pid::from_raw(0),
            Duration::from_secs(180),
            Some(Duration::from_secs(300)),
        )
    }

    fn enter(map: &str) -> String {
        format_event_line(&Event::LevelEnter {
            session: SESSION.into(),
            map: map.into(),
            level_name: map.into(),
            ts: 0,
        })
    }

    fn complete(map: &str, kills: i64, tics: i64) -> String {
        format_event_line(&Event::LevelComplete {
            session: SESSION.into(),
            map: map.into(),
            kills,
            total_monsters: kills + 5,
            secrets: 0,
            total_secrets: 0,
            items: 0,
            total_items: 0,
            maptime_tics: tics,
        })
    }

    fn run_complete() -> String {
        format_event_line(&Event::RunComplete {
            session: SESSION.into(),
            total_maptime_tics: 7000,
        })
    }

    fn progress(map: &str, kills: i64, px: i64, py: i64) -> String {
        format_event_line(&Event::Progress {
            session: SESSION.into(),
            map: map.into(),
            kills,
            total_monsters: 30,
            secrets: 0,
            total_secrets: 2,
            items: 0,
            total_items: 20,
            maptime_tics: 700,
            px,
            py,
        })
    }

    fn run_quit(map: &str) -> String {
        format_event_line(&Event::RunQuit {
            session: SESSION.into(),
            map: map.into(),
            maptime_tics: 1850,
        })
    }

    fn finish(pump: EventPump) -> protocol::RunSubmission {
        let reason = pump.state.terminal_reason().unwrap_or(EndReason::Abandoned);
        pump.state
            .finish(
                reason,
                datetime!(2026-08-17 12:00:00 UTC),
                datetime!(2026-08-17 12:15:00 UTC),
            )
            .submission
    }

    /// The dual-transport failure scenario: the FIFO delivers events in
    /// real time while gzdoom's pipe-buffered stdout later flushes a stale
    /// block containing copies of the same events. Once the FIFO has
    /// latched, stdout telemetry must not reach the state machine.
    #[test]
    fn stale_stdout_telemetry_is_ignored_once_fifo_latches() {
        let mut p = pump();
        // FIFO delivers promptly: enter/complete MAP01, enter MAP02.
        p.on_line("fifo", &enter("MAP01"));
        p.on_line("fifo", &complete("MAP01", 10, 3500));
        p.on_line("fifo", &enter("MAP02"));
        // Stdout's ~4KB block flush arrives late with its stale copies.
        p.on_line("stdout", &enter("MAP01"));
        p.on_line("stdout", &complete("MAP01", 10, 3500));
        // A stdout-only event never seen on the FIFO must ALSO be dropped
        // (proves the gate is per-transport, not just per-content dedupe).
        p.on_line("stdout", &complete("MAP07", 99, 100));
        // The run finishes over the FIFO.
        p.on_line("fifo", &complete("MAP02", 20, 3500));
        p.on_line("fifo", &run_complete());

        assert_eq!(p.state.terminal_reason(), Some(EndReason::Complete));
        let sub = finish(p);
        let names: Vec<&str> = sub.maps.iter().map(|m| m.map.as_str()).collect();
        assert_eq!(names, ["MAP01", "MAP02"], "no map may be double-counted");
        assert!(
            sub.maps.iter().all(|m| m.completed),
            "MAP02 must not be closed incomplete by the stale stdout block"
        );
        assert_eq!(sub.maps_completed, 2);
        assert_eq!(sub.kills, 30, "kills must not double-count");
    }

    /// The stdout fallback transport still works when the FIFO never
    /// produces telemetry (e.g. `-logfile` unsupported): only parsed FIFO
    /// events latch, not chatter.
    #[test]
    fn stdout_telemetry_applies_while_fifo_is_silent() {
        let mut p = pump();
        p.on_line("fifo", "Init: DOOM 2: Hell on Earth"); // chatter: no latch
        p.on_line("stdout", &enter("MAP01"));
        p.on_line("stdout", &complete("MAP01", 10, 3500));
        let sub = finish(p);
        assert_eq!(sub.maps.len(), 1);
        assert_eq!(sub.maps_completed, 1);
        assert_eq!(sub.kills, 10);
    }

    /// `run_quit` must arm the prompt SIGTERM/SIGKILL pair (the pk3 only
    /// announces the quit; the supervisor kills the engine) and disarm the
    /// abandonment guards.
    #[test]
    fn run_quit_arms_prompt_shutdown_and_disarms_guards() {
        let mut p = pump();
        p.on_line("fifo", &enter("MAP01"));
        p.on_line("fifo", &progress("MAP01", 3, 100, -200));
        assert!(p.idle_at.is_some());
        assert!(p.stall_at.is_some());
        let before = Instant::now();
        p.on_line("fifo", &run_quit("MAP01"));
        assert_eq!(p.state.terminal_reason(), Some(EndReason::Quit));
        let term_at = p.term_at.expect("SIGTERM must be armed");
        let kill_at = p.kill_at.expect("SIGKILL must be armed");
        assert!(term_at >= before + QUIT_LINGER);
        assert!(term_at <= Instant::now() + QUIT_LINGER);
        assert_eq!(kill_at, term_at + KILL_GRACE);
        assert!(p.idle_at.is_none(), "walk-away guard must be disarmed");
        assert!(p.stall_at.is_none(), "stall guard must be disarmed");
        let sub = finish(p);
        assert_eq!(sub.end_reason, EndReason::Quit);
    }

    /// `progress` heartbeats drive the walk-away deadline: static input
    /// leaves it in place, movement pushes it out.
    #[test]
    fn progress_feeds_the_walkaway_guard() {
        let mut p = pump();
        assert_eq!(p.idle_at, None, "no heartbeat yet, no idle deadline");
        p.on_line("fifo", &enter("MAP01"));
        assert_eq!(p.idle_at, None, "only progress feeds the tracker");
        p.on_line("fifo", &progress("MAP01", 3, 100, -200));
        let first = p.idle_at.expect("first heartbeat arms the deadline");
        // Static heartbeat: the deadline must not move.
        p.on_line("fifo", &progress("MAP01", 3, 100, -200));
        assert_eq!(p.idle_at, Some(first));
        // Movement: the deadline re-anchors later.
        p.on_line("fifo", &progress("MAP01", 3, 500, -200));
        assert!(p.idle_at.expect("still armed") >= first);
        // More kills at the same spot is also activity.
        p.on_line("fifo", &progress("MAP01", 7, 500, -200));
        assert!(p.idle_at.is_some());
    }

    /// Events that the state machine drops must not reset the walk-away
    /// window — a stale writer cannot keep a dead run "active".
    #[test]
    fn dropped_progress_does_not_feed_the_walkaway_guard() {
        let mut p = pump();
        let foreign = format_event_line(&Event::Progress {
            session: "some-other-session".into(),
            map: "MAP01".into(),
            kills: 3,
            total_monsters: 30,
            secrets: 0,
            total_secrets: 2,
            items: 0,
            total_items: 20,
            maptime_tics: 700,
            px: 100,
            py: -200,
        });
        p.on_line("fifo", &foreign);
        assert_eq!(p.idle_at, None);
    }

    /// Engine chatter is watchdog food but must not re-arm the stall
    /// guard: only parsed telemetry proves the pk3 pipeline is alive.
    #[test]
    fn chatter_does_not_rearm_the_stall_guard() {
        let mut p = pump();
        let armed = p.stall_at.expect("stall guard starts armed");
        p.on_line("fifo", "Init: DOOM 2: Hell on Earth");
        p.on_line("stdout", "Picked up a shotgun.");
        assert_eq!(
            p.stall_at,
            Some(armed),
            "chatter must not touch the stall deadline"
        );
        p.on_line("fifo", &enter("MAP01"));
        assert!(p.stall_at.expect("still armed") >= armed);
    }

    /// After an abandonment guard fires, further static heartbeats must
    /// not re-arm the guards (or the SIGKILL could be pushed out forever).
    #[test]
    fn guards_do_not_rearm_after_abandonment() {
        let mut p = pump();
        p.on_line("fifo", &enter("MAP01"));
        p.on_line("fifo", &progress("MAP01", 3, 100, -200));
        // Simulate a fired guard by hand — calling abandon_now() here
        // would kill(pid 0, SIGTERM) our own process group.
        let kill_at = Instant::now() + KILL_GRACE;
        p.kill_at = Some(kill_at);
        p.idle_at = None;
        p.stall_at = None;
        p.abandoning = true;
        p.on_line("fifo", &progress("MAP01", 3, 100, -200));
        assert_eq!(p.idle_at, None, "walk-away must stay disarmed");
        assert_eq!(p.stall_at, None, "stall must stay disarmed");
        assert_eq!(p.kill_at, Some(kill_at), "SIGKILL deadline must not move");
    }

    async fn collect_lines(data: &[u8]) -> Vec<String> {
        let mut lines = CappedLines::new(BufReader::new(data));
        let mut out = Vec::new();
        while let Some(line) = lines.next_line().await.expect("read") {
            out.push(line);
        }
        out
    }

    #[tokio::test]
    async fn capped_lines_drops_oversized_line_and_resyncs() {
        let huge = "x".repeat(MAX_EVENT_LINE_BYTES * 3);
        let data = format!("before\n{huge}\nafter\r\n");
        assert_eq!(collect_lines(data.as_bytes()).await, ["before", "after"]);
    }

    #[tokio::test]
    async fn capped_lines_passes_lines_at_the_cap() {
        // Exactly MAX_EVENT_LINE_BYTES must still get through, so the
        // parser cap stays the effective limit.
        let at_cap = "y".repeat(MAX_EVENT_LINE_BYTES);
        let data = format!("{at_cap}\nnext\n");
        let lines = collect_lines(data.as_bytes()).await;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), MAX_EVENT_LINE_BYTES);
        assert_eq!(lines[1], "next");
    }

    #[tokio::test]
    async fn capped_lines_bounds_memory_on_newline_free_stream() {
        // A multi-cap newline-less stream (the OOM scenario): yields no
        // line and the accumulation buffer never outgrows the cap by more
        // than one refill chunk.
        let data = vec![b'z'; MAX_EVENT_LINE_BYTES * 4];
        let mut lines = CappedLines::new(BufReader::new(&data[..]));
        assert_eq!(lines.next_line().await.expect("read"), None);
        assert!(
            lines.buf.capacity() <= MAX_EVENT_LINE_BYTES + 8192,
            "line buffer grew to {} bytes",
            lines.buf.capacity()
        );
    }

    #[tokio::test]
    async fn capped_lines_returns_final_unterminated_line() {
        assert_eq!(collect_lines(b"tail-no-newline").await, ["tail-no-newline"]);
    }
}
