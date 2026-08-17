//! SQLite storage for the leaderboard service (SPEC §7.2).
//!
//! The schema is applied on startup via `CREATE TABLE IF NOT EXISTS`
//! statements that mirror SPEC §7.2 exactly — no separate migration files,
//! since the schema is frozen by the spec and versioned by this crate. WAL
//! mode is enabled for file-backed databases (`:memory:` databases have no
//! journal to configure and run on a single pooled connection so every
//! query sees the same database).

use std::time::Duration;

use anyhow::{Context, Result};
use protocol::{
    format_tics_clock, Board, BoardCategory, BoardEntry, EndReason, MapResult, RunSubmission,
    Season,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

/// SPEC §7.2 schema, with `IF NOT EXISTS` so startup is idempotent.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runs (
  id              INTEGER PRIMARY KEY,
  session         TEXT NOT NULL UNIQUE,
  initials        TEXT NOT NULL,
  cabinet_id      TEXT NOT NULL,
  started_at      TEXT NOT NULL,
  ended_at        TEXT NOT NULL,
  end_reason      TEXT NOT NULL,
  maps_completed  INTEGER NOT NULL,
  kills           INTEGER NOT NULL,
  secrets         INTEGER NOT NULL,
  items           INTEGER NOT NULL,
  total_tics      INTEGER NOT NULL,
  run_score       INTEGER NOT NULL,
  iwad_sha256     TEXT NOT NULL,
  scoring_version INTEGER NOT NULL,
  map_rotation_id TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS run_maps (
  run_id     INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  seq        INTEGER NOT NULL,
  map        TEXT NOT NULL,
  kills      INTEGER NOT NULL, total_monsters INTEGER NOT NULL,
  secrets    INTEGER NOT NULL, total_secrets  INTEGER NOT NULL,
  items      INTEGER NOT NULL, total_items    INTEGER NOT NULL,
  tics       INTEGER NOT NULL,
  completed  INTEGER NOT NULL,
  map_score  INTEGER NOT NULL,
  PRIMARY KEY (run_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_season_score
  ON runs(iwad_sha256, scoring_version, map_rotation_id, run_score DESC);
";

/// Opens (creating if missing) the database at `path` and applies the
/// schema. `":memory:"` yields an ephemeral in-memory database.
pub async fn open(path: &str) -> Result<SqlitePool> {
    let memory = path == ":memory:";
    let mut opts = if memory {
        // SqliteConnectOptions::new() defaults to an in-memory database.
        SqliteConnectOptions::new()
    } else {
        SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
    };
    opts = opts.foreign_keys(true).busy_timeout(Duration::from_secs(5));

    let mut pool_opts = SqlitePoolOptions::new();
    if memory {
        // Each pooled connection would otherwise get its own private
        // in-memory database; pin a single long-lived connection.
        pool_opts = pool_opts
            .min_connections(1)
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None);
    } else {
        pool_opts = pool_opts.max_connections(8);
    }

    let pool = pool_opts
        .connect_with(opts)
        .await
        .with_context(|| format!("opening sqlite database at {path:?}"))?;

    sqlx::raw_sql(SCHEMA)
        .execute(&pool)
        .await
        .context("applying schema")?;
    Ok(pool)
}

/// Whether [`insert_run`] stored a new row or found an existing session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    /// First submission of this session — a new row was written.
    Inserted,
    /// The session already existed — nothing was written.
    Duplicate,
}

/// Idempotently inserts a run keyed on `session` (SPEC §7.3).
///
/// Returns the outcome plus the stored submission: the input on first
/// insert, the previously stored record on a duplicate.
pub async fn insert_run(
    pool: &SqlitePool,
    sub: &RunSubmission,
) -> Result<(InsertOutcome, RunSubmission)> {
    let mut tx = pool.begin().await?;

    let res = sqlx::query(
        "INSERT INTO runs (session, initials, cabinet_id, started_at, ended_at, end_reason,
                           maps_completed, kills, secrets, items, total_tics, run_score,
                           iwad_sha256, scoring_version, map_rotation_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(session) DO NOTHING",
    )
    .bind(&sub.session)
    .bind(&sub.initials)
    .bind(&sub.cabinet_id)
    .bind(&sub.started_at)
    .bind(&sub.ended_at)
    .bind(sub.end_reason.as_str())
    .bind(sub.maps_completed)
    .bind(sub.kills)
    .bind(sub.secrets)
    .bind(sub.items)
    .bind(sub.total_tics)
    .bind(sub.run_score)
    .bind(&sub.iwad_sha256)
    .bind(sub.scoring_version)
    .bind(&sub.map_rotation_id)
    .execute(&mut *tx)
    .await?;

    if res.rows_affected() == 0 {
        // Session already stored; return the existing record untouched.
        tx.rollback().await?;
        let existing = fetch_run(pool, &sub.session)
            .await?
            .context("conflicting session vanished mid-transaction")?;
        return Ok((InsertOutcome::Duplicate, existing));
    }

    let run_id = res.last_insert_rowid();
    for m in &sub.maps {
        sqlx::query(
            "INSERT INTO run_maps (run_id, seq, map, kills, total_monsters, secrets,
                                   total_secrets, items, total_items, tics, completed, map_score)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )
        .bind(run_id)
        .bind(m.seq)
        .bind(&m.map)
        .bind(m.kills)
        .bind(m.total_monsters)
        .bind(m.secrets)
        .bind(m.total_secrets)
        .bind(m.items)
        .bind(m.total_items)
        .bind(m.tics)
        .bind(m.completed as i64)
        .bind(m.map_score)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok((InsertOutcome::Inserted, sub.clone()))
}

/// Fetches a stored run (and its per-map rows) by session.
pub async fn fetch_run(pool: &SqlitePool, session: &str) -> Result<Option<RunSubmission>> {
    let Some(row) = sqlx::query("SELECT * FROM runs WHERE session = ?1")
        .bind(session)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };

    let run_id: i64 = row.try_get("id")?;
    let end_reason: String = row.try_get("end_reason")?;
    let end_reason: EndReason = end_reason
        .parse()
        .map_err(|e| anyhow::anyhow!("corrupt end_reason in db: {e}"))?;

    let map_rows = sqlx::query("SELECT * FROM run_maps WHERE run_id = ?1 ORDER BY seq ASC")
        .bind(run_id)
        .fetch_all(pool)
        .await?;
    let maps = map_rows
        .iter()
        .map(|r| -> Result<MapResult> {
            Ok(MapResult {
                seq: r.try_get("seq")?,
                map: r.try_get("map")?,
                kills: r.try_get("kills")?,
                total_monsters: r.try_get("total_monsters")?,
                secrets: r.try_get("secrets")?,
                total_secrets: r.try_get("total_secrets")?,
                items: r.try_get("items")?,
                total_items: r.try_get("total_items")?,
                tics: r.try_get("tics")?,
                completed: r.try_get::<i64, _>("completed")? != 0,
                map_score: r.try_get("map_score")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Some(RunSubmission {
        session: row.try_get("session")?,
        initials: row.try_get("initials")?,
        cabinet_id: row.try_get("cabinet_id")?,
        started_at: row.try_get("started_at")?,
        ended_at: row.try_get("ended_at")?,
        end_reason,
        maps_completed: row.try_get("maps_completed")?,
        kills: row.try_get("kills")?,
        secrets: row.try_get("secrets")?,
        items: row.try_get("items")?,
        total_tics: row.try_get("total_tics")?,
        run_score: row.try_get("run_score")?,
        iwad_sha256: row.try_get("iwad_sha256")?,
        scoring_version: row.try_get("scoring_version")?,
        map_rotation_id: row.try_get("map_rotation_id")?,
        maps,
    }))
}

/// The current season: the season triple of the most recent run (by
/// `ended_at`). With no runs at all, falls back to an empty IWAD hash and
/// this build's scoring/rotation constants so the response shape is stable.
pub async fn current_season(pool: &SqlitePool) -> Result<Season> {
    let row = sqlx::query(
        "SELECT iwad_sha256, scoring_version, map_rotation_id
         FROM runs ORDER BY ended_at DESC, id DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        Some(r) => Season {
            iwad_sha256: r.try_get("iwad_sha256")?,
            scoring_version: r.try_get("scoring_version")?,
            map_rotation_id: r.try_get("map_rotation_id")?,
        },
        None => Season {
            iwad_sha256: String::new(),
            scoring_version: protocol::SCORING_VERSION,
            map_rotation_id: protocol::MAP_ROTATION_ID.to_owned(),
        },
    })
}

/// Per-category ranked value and ordering (SPEC §6).
fn board_ranking(category: BoardCategory) -> (&'static str, &'static str, &'static str) {
    match category {
        BoardCategory::HighScore => ("run_score", "", "run_score DESC, ended_at ASC, id ASC"),
        BoardCategory::Deepest => (
            "maps_completed",
            "",
            "maps_completed DESC, run_score DESC, ended_at ASC, id ASC",
        ),
        BoardCategory::FastestClear => (
            "total_tics",
            "AND end_reason = 'complete'",
            "total_tics ASC, ended_at ASC, id ASC",
        ),
        BoardCategory::MostKills => (
            "kills",
            "",
            "kills DESC, run_score DESC, ended_at ASC, id ASC",
        ),
        BoardCategory::SecretHunter => (
            "secrets",
            "",
            "secrets DESC, run_score DESC, ended_at ASC, id ASC",
        ),
    }
}

/// Queries one ranked board for a season.
pub async fn board(
    pool: &SqlitePool,
    season: &Season,
    category: BoardCategory,
    limit: i64,
) -> Result<Board> {
    let (value, extra_where, order) = board_ranking(category);
    let sql = format!(
        "SELECT initials, {value} AS value, run_score, maps_completed, ended_at
         FROM runs
         WHERE iwad_sha256 = ?1 AND scoring_version = ?2 AND map_rotation_id = ?3 {extra_where}
         ORDER BY {order} LIMIT ?4"
    );
    let rows = sqlx::query(&sql)
        .bind(&season.iwad_sha256)
        .bind(season.scoring_version)
        .bind(&season.map_rotation_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;

    let entries = rows
        .iter()
        .enumerate()
        .map(|(i, r)| -> Result<BoardEntry> {
            let value: i64 = r.try_get("value")?;
            let value_display = match category {
                BoardCategory::FastestClear => format_tics_clock(value),
                _ => value.to_string(),
            };
            Ok(BoardEntry {
                rank: i as i64 + 1,
                initials: r.try_get("initials")?,
                value,
                value_display,
                run_score: r.try_get("run_score")?,
                maps_completed: r.try_get("maps_completed")?,
                ended_at: r.try_get("ended_at")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Board {
        category,
        title: category.title().to_owned(),
        entries,
    })
}

/// All five boards, in [`BoardCategory::ALL`] order.
pub async fn all_boards(pool: &SqlitePool, season: &Season, limit: i64) -> Result<Vec<Board>> {
    let mut boards = Vec::with_capacity(BoardCategory::ALL.len());
    for category in BoardCategory::ALL {
        boards.push(board(pool, season, category, limit).await?);
    }
    Ok(boards)
}
