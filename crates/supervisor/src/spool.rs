//! The local run spool — the source of truth on the cabinet (SPEC §8.3).
//!
//! Every finished run is written here *first*; the background
//! [submitter](crate::submitter) then POSTs pending rows to the
//! leaderboard and marks them submitted on 2xx. Because `POST /v1/runs`
//! is idempotent on `session`, a row may safely be submitted more than
//! once after an ambiguous failure.
//!
//! Schema:
//!
//! ```sql
//! CREATE TABLE spooled_runs (
//!   session      TEXT PRIMARY KEY,   -- run session UUID (idempotency key)
//!   payload      TEXT NOT NULL,      -- RunSubmission as JSON
//!   created_at   TEXT NOT NULL,      -- RFC 3339, when spooled
//!   submitted_at TEXT,               -- RFC 3339, NULL until a 2xx
//!   attempts     INTEGER NOT NULL DEFAULT 0,
//!   last_error   TEXT,
//!   failed_at    TEXT                -- RFC 3339, set on permanent rejection
//! );
//! ```
//!
//! A row with `failed_at` set was permanently rejected by the server (a
//! non-retryable 4xx such as a validation 422): it is excluded from
//! [`Spool::pending`] so it cannot burn the rate-limit budget forever and
//! starve newer runs, but it is kept on disk for operator inspection.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Context as _;
use protocol::RunSubmission;
use rusqlite::{params, Connection, OptionalExtension as _};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS spooled_runs (
  session      TEXT PRIMARY KEY,
  payload      TEXT NOT NULL,
  created_at   TEXT NOT NULL,
  submitted_at TEXT,
  attempts     INTEGER NOT NULL DEFAULT 0,
  last_error   TEXT,
  failed_at    TEXT
);
";

/// A run awaiting submission, as returned by [`Spool::pending`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRun {
    /// The run's session UUID.
    pub session: String,
    /// The serialized [`RunSubmission`] JSON, POSTed verbatim.
    pub payload: String,
    /// How many submission attempts have failed so far.
    pub attempts: i64,
}

/// One full spool row, for tests and operational introspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpoolEntry {
    /// The run's session UUID.
    pub session: String,
    /// The serialized [`RunSubmission`] JSON.
    pub payload: String,
    /// When the run was spooled (RFC 3339).
    pub created_at: String,
    /// When the run was accepted by the leaderboard, if it has been.
    pub submitted_at: Option<String>,
    /// Failed submission attempts so far.
    pub attempts: i64,
    /// The most recent submission error, if any.
    pub last_error: Option<String>,
    /// When the run was permanently rejected by the leaderboard, if it
    /// was. Set rows no longer appear in [`Spool::pending`].
    pub failed_at: Option<String>,
}

/// Handle to the spool database. `Send + Sync` (the connection sits behind
/// a mutex), so it is shared between the main loop and the submitter task
/// via `Arc<Spool>`. All methods are synchronous and fast (single-row
/// local SQLite operations); the submitter keeps its slow network work in
/// `spawn_blocking`, not here.
pub struct Spool {
    conn: Mutex<Connection>,
}

impl Spool {
    /// Opens (creating if needed) the spool at `path`, applying the schema
    /// and WAL mode. Parent directories are created. The literal path
    /// `:memory:` yields an in-memory spool (tests).
    pub fn open(path: &Path) -> anyhow::Result<Spool> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating spool dir {}", parent.display()))?;
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening spool db {}", path.display()))?;
        // WAL keeps the (rare) concurrent reader from blocking writes;
        // harmless no-op for :memory:.
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .context("setting journal_mode")?;
        conn.execute_batch(SCHEMA)
            .context("applying spool schema")?;
        // Migration for spools created before the failed_at column existed
        // (CREATE TABLE IF NOT EXISTS does not extend an existing table).
        let has_failed_at: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('spooled_runs')
                 WHERE name = 'failed_at'",
                [],
                |row| row.get(0),
            )
            .context("inspecting spool schema")?;
        if has_failed_at == 0 {
            conn.execute("ALTER TABLE spooled_runs ADD COLUMN failed_at TEXT", [])
                .context("adding failed_at column")?;
        }
        Ok(Spool {
            conn: Mutex::new(conn),
        })
    }

    /// Spools a finished run. Idempotent on `session` (`INSERT OR
    /// IGNORE`): re-inserting an already-spooled — or already-submitted —
    /// session changes nothing and returns `Ok(false)`. Returns `Ok(true)`
    /// when the row was newly inserted.
    pub fn insert_run(&self, submission: &RunSubmission) -> anyhow::Result<bool> {
        let payload = serde_json::to_string(submission).context("serializing submission")?;
        let conn = self.lock();
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO spooled_runs (session, payload, created_at)
                 VALUES (?1, ?2, ?3)",
                params![submission.session, payload, now_rfc3339()],
            )
            .context("inserting spooled run")?;
        Ok(inserted == 1)
    }

    /// All runs not yet submitted, oldest first (insertion order — `rowid`
    /// breaks ties within one `created_at` second, and RFC 3339 strings
    /// with mixed sub-second precision do not sort reliably on their own).
    /// Permanently rejected rows (`failed_at` set) are excluded, so they
    /// cannot starve newer runs of the server's rate-limit budget.
    pub fn pending(&self) -> anyhow::Result<Vec<PendingRun>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT session, payload, attempts FROM spooled_runs
                 WHERE submitted_at IS NULL AND failed_at IS NULL
                 ORDER BY created_at ASC, rowid ASC",
            )
            .context("preparing pending query")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(PendingRun {
                    session: row.get(0)?,
                    payload: row.get(1)?,
                    attempts: row.get(2)?,
                })
            })
            .context("querying pending runs")?;
        let mut pending = Vec::new();
        for row in rows {
            pending.push(row.context("reading pending row")?);
        }
        Ok(pending)
    }

    /// Marks a run as accepted by the leaderboard. Idempotent; marking an
    /// unknown session is a no-op.
    pub fn mark_submitted(&self, session: &str) -> anyhow::Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE spooled_runs SET submitted_at = ?2 WHERE session = ?1",
            params![session, now_rfc3339()],
        )
        .context("marking run submitted")?;
        Ok(())
    }

    /// Records a failed submission attempt: bumps `attempts` and stores a
    /// (truncated) error description.
    pub fn record_failure(&self, session: &str, error: &str) -> anyhow::Result<()> {
        let error: String = error.chars().take(500).collect();
        let conn = self.lock();
        conn.execute(
            "UPDATE spooled_runs
             SET attempts = attempts + 1, last_error = ?2
             WHERE session = ?1",
            params![session, error],
        )
        .context("recording submission failure")?;
        Ok(())
    }

    /// Quarantines a permanently rejected run: bumps `attempts`, stores
    /// the (truncated) rejection, and sets `failed_at` so the row leaves
    /// [`Spool::pending`]. The row itself is kept for inspection.
    pub fn mark_failed(&self, session: &str, error: &str) -> anyhow::Result<()> {
        let error: String = error.chars().take(500).collect();
        let conn = self.lock();
        conn.execute(
            "UPDATE spooled_runs
             SET attempts = attempts + 1, last_error = ?2, failed_at = ?3
             WHERE session = ?1",
            params![session, error, now_rfc3339()],
        )
        .context("marking run permanently failed")?;
        Ok(())
    }

    /// Fetches one full row, for tests and debugging.
    pub fn entry(&self, session: &str) -> anyhow::Result<Option<SpoolEntry>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT session, payload, created_at, submitted_at, attempts, last_error, failed_at
             FROM spooled_runs WHERE session = ?1",
            params![session],
            |row| {
                Ok(SpoolEntry {
                    session: row.get(0)?,
                    payload: row.get(1)?,
                    created_at: row.get(2)?,
                    submitted_at: row.get(3)?,
                    attempts: row.get(4)?,
                    last_error: row.get(5)?,
                    failed_at: row.get(6)?,
                })
            },
        )
        .optional()
        .context("querying spool entry")
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A poisoned mutex means another thread panicked mid-query; the
        // connection itself is still usable and the supervisor must never
        // die over it.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::EndReason;

    fn spool() -> Spool {
        Spool::open(Path::new(":memory:")).expect("open in-memory spool")
    }

    fn submission(session: &str) -> RunSubmission {
        RunSubmission {
            session: session.to_owned(),
            initials: "ABC".into(),
            cabinet_id: "cab-test".into(),
            started_at: "2026-08-17T12:00:00Z".into(),
            ended_at: "2026-08-17T12:10:00Z".into(),
            end_reason: EndReason::Death,
            maps_completed: 1,
            kills: 10,
            secrets: 1,
            items: 4,
            total_tics: 3500,
            run_score: 1920,
            iwad_sha256: "sha-test".into(),
            scoring_version: protocol::SCORING_VERSION,
            map_rotation_id: protocol::MAP_ROTATION_ID.into(),
            maps: vec![],
        }
    }

    #[test]
    fn insert_is_idempotent() {
        let spool = spool();
        let sub = submission("s-1");
        assert!(spool.insert_run(&sub).unwrap());
        // Double insert: ignored, even with a different payload.
        let mut tampered = sub.clone();
        tampered.run_score = 999_999;
        assert!(!spool.insert_run(&tampered).unwrap());

        let pending = spool.pending().unwrap();
        assert_eq!(pending.len(), 1);
        // The original payload survived the second insert.
        let stored: RunSubmission = serde_json::from_str(&pending[0].payload).unwrap();
        assert_eq!(stored, sub);
    }

    #[test]
    fn pending_orders_oldest_first() {
        let spool = spool();
        spool.insert_run(&submission("s-old")).unwrap();
        spool.insert_run(&submission("s-mid")).unwrap();
        spool.insert_run(&submission("s-new")).unwrap();
        let sessions: Vec<String> = spool
            .pending()
            .unwrap()
            .into_iter()
            .map(|p| p.session)
            .collect();
        assert_eq!(sessions, ["s-old", "s-mid", "s-new"]);
    }

    #[test]
    fn mark_submitted_removes_from_pending() {
        let spool = spool();
        spool.insert_run(&submission("s-1")).unwrap();
        spool.insert_run(&submission("s-2")).unwrap();
        spool.mark_submitted("s-1").unwrap();
        let pending = spool.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].session, "s-2");
        // The submitted row still exists, with a timestamp.
        let entry = spool.entry("s-1").unwrap().unwrap();
        assert!(entry.submitted_at.is_some());
        // Re-inserting a submitted session is still a no-op.
        assert!(!spool.insert_run(&submission("s-1")).unwrap());
        assert_eq!(spool.pending().unwrap().len(), 1);
    }

    #[test]
    fn record_failure_increments_attempts() {
        let spool = spool();
        spool.insert_run(&submission("s-1")).unwrap();
        spool.record_failure("s-1", "connection refused").unwrap();
        spool
            .record_failure("s-1", "http 503: unavailable")
            .unwrap();
        let entry = spool.entry("s-1").unwrap().unwrap();
        assert_eq!(entry.attempts, 2);
        assert_eq!(entry.last_error.as_deref(), Some("http 503: unavailable"));
        assert!(entry.submitted_at.is_none());
        // Failures do not remove the run from pending.
        assert_eq!(spool.pending().unwrap()[0].attempts, 2);
    }

    #[test]
    fn long_errors_are_truncated() {
        let spool = spool();
        spool.insert_run(&submission("s-1")).unwrap();
        let huge = "x".repeat(10_000);
        spool.record_failure("s-1", &huge).unwrap();
        let entry = spool.entry("s-1").unwrap().unwrap();
        assert_eq!(entry.last_error.unwrap().len(), 500);
    }

    #[test]
    fn on_disk_spool_persists_across_reopen() {
        let dir = std::env::temp_dir().join(format!("spool-test-{}", uuid::Uuid::new_v4()));
        let db = dir.join("nested").join("spool.sqlite");
        {
            let spool = Spool::open(&db).unwrap();
            spool.insert_run(&submission("s-persist")).unwrap();
        }
        {
            let spool = Spool::open(&db).unwrap();
            let pending = spool.pending().unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].session, "s-persist");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_failed_quarantines_row_but_keeps_it() {
        let spool = spool();
        spool.insert_run(&submission("s-poison")).unwrap();
        spool.insert_run(&submission("s-good")).unwrap();
        spool
            .mark_failed("s-poison", "http 422: end_reason mismatch")
            .unwrap();
        // The poisoned row leaves pending — it can no longer starve
        // newer runs — but stays on disk for inspection.
        let pending = spool.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].session, "s-good");
        let entry = spool.entry("s-poison").unwrap().unwrap();
        assert!(entry.failed_at.is_some());
        assert_eq!(entry.attempts, 1);
        assert_eq!(
            entry.last_error.as_deref(),
            Some("http 422: end_reason mismatch")
        );
        assert!(entry.submitted_at.is_none());
    }

    #[test]
    fn opens_spool_created_before_failed_at_column() {
        // A spool written by an older supervisor lacks failed_at; opening
        // it must migrate the schema, not fail or mis-read rows.
        let dir = std::env::temp_dir().join(format!("spool-migrate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("spool.sqlite");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE spooled_runs (
                   session      TEXT PRIMARY KEY,
                   payload      TEXT NOT NULL,
                   created_at   TEXT NOT NULL,
                   submitted_at TEXT,
                   attempts     INTEGER NOT NULL DEFAULT 0,
                   last_error   TEXT
                 );
                 INSERT INTO spooled_runs (session, payload, created_at)
                 VALUES ('s-old', '{}', '2026-08-17T12:00:00Z');",
            )
            .unwrap();
        }
        let spool = Spool::open(&db).unwrap();
        let pending = spool.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].session, "s-old");
        let entry = spool.entry("s-old").unwrap().unwrap();
        assert_eq!(entry.failed_at, None);
        // The migrated column is fully functional.
        spool.mark_failed("s-old", "http 422: poison").unwrap();
        assert!(spool.pending().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entry_for_unknown_session_is_none() {
        let spool = spool();
        assert!(spool.entry("nope").unwrap().is_none());
        // Updates against unknown sessions are quiet no-ops.
        spool.mark_submitted("nope").unwrap();
        spool.record_failure("nope", "err").unwrap();
    }
}
