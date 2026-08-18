//! Background score submitter (SPEC §8.3).
//!
//! A single tokio task that, on every [`kick`](Submitter::kick) and every
//! 60 seconds regardless, POSTs pending spool rows to the leaderboard's
//! `POST /v1/runs`, oldest first. A 2xx — including the 200 the service
//! returns for an idempotent duplicate — marks the row submitted. A
//! *transient* failure (network trouble, 5xx, 408/429) is recorded on the
//! row and retried with exponential backoff: 5 s doubling to a 5 min cap,
//! reset on success. A *permanent* rejection (any other 4xx, e.g. a
//! validation 422 — re-POSTing the identical payload can never succeed)
//! quarantines the row via [`Spool::mark_failed`] so it stops consuming
//! the server's per-cabinet rate-limit budget and cannot starve newer
//! runs forever. One failing run does not starve the rest: every pending
//! row is attempted each pass, and the backoff applies between passes.
//!
//! HTTP is done with `ureq` inside `spawn_blocking` rather than a full
//! async client: the leaderboard lives on the same machine or LAN over
//! plain `http://`, so a synchronous, TLS-free client keeps the
//! dependency tree (and the attack surface of the cabinet image) small.
//! `spawn_blocking` keeps the blocking socket work off the async runtime
//! threads.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::spool::Spool;

/// Interval between unprompted submission sweeps.
pub const SUBMIT_INTERVAL: Duration = Duration::from_secs(60);
/// Initial backoff after a failed pass.
pub const BACKOFF_BASE: Duration = Duration::from_secs(5);
/// Backoff ceiling (~5 min, SPEC §8.3).
pub const BACKOFF_CAP: Duration = Duration::from_secs(300);
/// Per-request timeout for the POST.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Submitter configuration, distilled from [`Config`].
#[derive(Debug, Clone)]
pub struct SubmitterConfig {
    /// Leaderboard base URL (no trailing `/v1/runs`).
    pub leaderboard_url: String,
    /// Bearer-token file, read (and trimmed) before every pass so a
    /// rotated secret is picked up without a restart. `None` → no
    /// `Authorization` header (dev mode).
    pub token_file: Option<PathBuf>,
}

impl SubmitterConfig {
    /// Extracts the submitter's settings from the supervisor config.
    pub fn from_config(cfg: &Config) -> Self {
        SubmitterConfig {
            leaderboard_url: cfg.leaderboard_url.clone(),
            token_file: cfg.token_file.clone(),
        }
    }
}

/// Handle to the background submitter task. Cloneable; all clones kick
/// the same task.
#[derive(Clone)]
pub struct Submitter {
    notify: Arc<Notify>,
}

impl Submitter {
    /// Spawns the background task. It runs for the life of the process.
    pub fn spawn(spool: Arc<Spool>, cfg: SubmitterConfig) -> Submitter {
        let notify = Arc::new(Notify::new());
        tokio::spawn(run(spool, cfg, Arc::clone(&notify)));
        Submitter { notify }
    }

    /// Nudges the task to sweep now (non-blocking). Called by the main
    /// loop right after spooling a run. A kick during an active sweep is
    /// remembered (single stored permit), so nothing is lost.
    pub fn kick(&self) {
        self.notify.notify_one();
    }
}

async fn run(spool: Arc<Spool>, cfg: SubmitterConfig, notify: Arc<Notify>) {
    let runs_url = format!("{}/v1/runs", cfg.leaderboard_url.trim_end_matches('/'));
    let mut backoff = BACKOFF_BASE;
    loop {
        tokio::select! {
            _ = notify.notified() => debug!("submitter kicked"),
            _ = tokio::time::sleep(SUBMIT_INTERVAL) => {}
        }
        // Sweep until the spool is drained; on failures, keep retrying
        // with backoff (kicks are unnecessary while we are in here).
        loop {
            let pending = match spool.pending() {
                Ok(p) => p,
                Err(err) => {
                    error!("reading spool: {err:#}");
                    break;
                }
            };
            if pending.is_empty() {
                backoff = BACKOFF_BASE;
                break;
            }
            let token = cfg.token_file.as_deref().and_then(read_token);
            let mut any_failure = false;
            for run in pending {
                let url = runs_url.clone();
                let tok = token.clone();
                let payload = run.payload.clone();
                let result =
                    tokio::task::spawn_blocking(move || post_run(&url, tok.as_deref(), &payload))
                        .await;
                match result {
                    Ok(Ok(status)) => {
                        info!(session = %run.session, status, "run submitted");
                        if let Err(err) = spool.mark_submitted(&run.session) {
                            error!(session = %run.session, "marking submitted: {err:#}");
                        }
                        backoff = BACKOFF_BASE;
                    }
                    Ok(Err(PostError::Permanent(err))) => {
                        // Retrying an identical payload cannot succeed;
                        // quarantine it so it stops burning rate-limit
                        // budget and blocking newer runs (kept on disk
                        // for inspection).
                        warn!(session = %run.session, attempts = run.attempts + 1,
                              "submission permanently rejected; quarantining: {err}");
                        if let Err(db_err) = spool.mark_failed(&run.session, &err) {
                            error!(session = %run.session, "quarantining run: {db_err:#}");
                            any_failure = true;
                        }
                    }
                    Ok(Err(PostError::Transient(err))) => {
                        warn!(session = %run.session, attempts = run.attempts + 1,
                              "submission failed: {err}");
                        if let Err(db_err) = spool.record_failure(&run.session, &err) {
                            error!(session = %run.session, "recording failure: {db_err:#}");
                        }
                        any_failure = true;
                    }
                    Err(join_err) => {
                        // The blocking task itself died — treat like any
                        // other failure and keep the supervisor alive.
                        error!(session = %run.session, "submit task panicked: {join_err}");
                        any_failure = true;
                    }
                }
            }
            if any_failure {
                debug!(?backoff, "backing off before retrying pending runs");
                tokio::time::sleep(backoff).await;
                backoff = next_backoff(backoff);
            }
            // No failure: loop re-queries pending, finds it empty, resets
            // the backoff, and breaks back to waiting on kick/interval.
        }
    }
}

/// Doubles the backoff, clamped to [`BACKOFF_CAP`].
pub fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(BACKOFF_CAP)
}

/// Reads and trims the bearer token; `None` when the file is missing,
/// unreadable, or blank.
fn read_token(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let token = raw.trim().to_owned();
            if token.is_empty() {
                warn!(path = %path.display(), "token file is empty; POSTing without auth");
                None
            } else {
                Some(token)
            }
        }
        Err(err) => {
            warn!(path = %path.display(), %err, "cannot read token file; POSTing without auth");
            None
        }
    }
}

/// How a submission attempt failed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PostError {
    /// The server rejected this payload outright (a 4xx other than
    /// 408/429): re-POSTing the identical bytes can never succeed, so the
    /// row must be quarantined instead of retried forever.
    Permanent(String),
    /// Anything else — network trouble, 5xx, timeout (408), rate limit
    /// (429): worth retrying later.
    Transient(String),
}

/// Whether an HTTP status is a permanent rejection of this payload.
/// 408 (timeout) and 429 (rate limit) are about the *moment*, not the
/// payload; every other 4xx condemns the payload itself.
fn is_permanent_status(status: u16) -> bool {
    (400..500).contains(&status) && status != 408 && status != 429
}

/// POSTs one payload; `Ok(status)` on any 2xx (a 200 for an idempotent
/// duplicate counts as success), `Err(PostError)` otherwise. Blocking —
/// call from `spawn_blocking` only.
fn post_run(url: &str, token: Option<&str>, payload: &str) -> Result<u16, PostError> {
    let agent = ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build();
    let mut request = agent.post(url).set("Content-Type", "application/json");
    if let Some(token) = token {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    match request.send_string(payload) {
        Ok(response) => {
            let status = response.status();
            if (200..300).contains(&status) {
                Ok(status)
            } else if is_permanent_status(status) {
                Err(PostError::Permanent(format!("unexpected status {status}")))
            } else {
                Err(PostError::Transient(format!("unexpected status {status}")))
            }
        }
        Err(ureq::Error::Status(code, response)) => {
            let body: String = response
                .into_string()
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect();
            let msg = format!("http {code}: {body}");
            if is_permanent_status(code) {
                Err(PostError::Permanent(msg))
            } else {
                Err(PostError::Transient(msg))
            }
        }
        Err(err) => Err(PostError::Transient(format!("transport error: {err}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let mut d = BACKOFF_BASE;
        let mut seen = Vec::new();
        for _ in 0..10 {
            seen.push(d.as_secs());
            d = next_backoff(d);
        }
        assert_eq!(seen, [5, 10, 20, 40, 80, 160, 300, 300, 300, 300]);
    }

    #[test]
    fn token_is_trimmed() {
        let dir = std::env::temp_dir().join(format!("token-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "  sekrit-token\n\n").unwrap();
        assert_eq!(read_token(&path).as_deref(), Some("sekrit-token"));
        std::fs::write(&path, "   \n").unwrap();
        assert_eq!(read_token(&path), None);
        assert_eq!(read_token(&dir.join("missing")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runs_url_is_derived_from_base() {
        // Mirrors the format! in run(): trailing slash tolerated.
        for base in ["http://127.0.0.1:8080", "http://127.0.0.1:8080/"] {
            let url = format!("{}/v1/runs", base.trim_end_matches('/'));
            assert_eq!(url, "http://127.0.0.1:8080/v1/runs");
        }
    }

    #[test]
    fn permanent_status_classification() {
        for permanent in [400, 401, 403, 404, 409, 422] {
            assert!(is_permanent_status(permanent), "{permanent}");
        }
        for transient in [408, 429, 500, 502, 503, 504] {
            assert!(!is_permanent_status(transient), "{transient}");
        }
    }

    /// Serves exactly one connection with a canned HTTP status, on a
    /// background thread, returning the URL to POST to.
    fn one_shot_server(status_line: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let (mut stream, _) = listener.accept().unwrap();
            // Drain the request headers (enough for ureq to accept the
            // response as belonging to its request).
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = "{}";
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        format!("http://{addr}/v1/runs")
    }

    #[test]
    fn post_run_classifies_422_as_permanent() {
        // The poison-row scenario: a validation 422 must be a permanent
        // rejection (quarantined), not retried forever.
        let url = one_shot_server("422 Unprocessable Entity");
        match post_run(&url, None, "{}") {
            Err(PostError::Permanent(msg)) => assert!(msg.contains("422"), "{msg}"),
            other => panic!("expected permanent rejection, got {other:?}"),
        }
    }

    #[test]
    fn post_run_classifies_5xx_and_429_as_transient() {
        let url = one_shot_server("503 Service Unavailable");
        match post_run(&url, None, "{}") {
            Err(PostError::Transient(msg)) => assert!(msg.contains("503"), "{msg}"),
            other => panic!("expected transient failure, got {other:?}"),
        }
        let url = one_shot_server("429 Too Many Requests");
        match post_run(&url, None, "{}") {
            Err(PostError::Transient(msg)) => assert!(msg.contains("429"), "{msg}"),
            other => panic!("expected transient failure, got {other:?}"),
        }
    }
}
