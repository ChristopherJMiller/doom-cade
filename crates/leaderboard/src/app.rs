//! HTTP surface of the leaderboard service (SPEC §7.3).
//!
//! Routes:
//! - `POST /v1/runs` — idempotent run submission (bearer auth, validation,
//!   per-cabinet rate limit).
//! - `GET /v1/boards` — all five boards as [`protocol::BoardsResponse`].
//! - `GET /v1/boards/{category}` — a single [`protocol::Board`].
//! - `GET /healthz` — liveness.
//! - `GET /` — the HTML leaderboard view.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use protocol::{
    validate_initials, BoardCategory, BoardsResponse, EndReason, RunSubmission, Season,
};
use serde::Deserialize;
use sqlx::SqlitePool;
use std::sync::Arc;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::db::{self, InsertOutcome};
use crate::html;

/// Default number of entries per board.
const DEFAULT_LIMIT: i64 = 10;
/// Hard cap on `?limit`.
const MAX_LIMIT: i64 = 100;
/// Maximum per-map rows in a submission (rotation is 5; leave headroom).
const MAX_MAPS: usize = 16;
/// Cap on free-text fields in a submission.
const MAX_STRING: usize = 64;
/// Per-cabinet POST rate limit: this many requests per window.
const RATE_LIMIT_MAX: usize = 10;
/// Global POST rate limit across all cabinets per window. The cabinet_id
/// key comes from the request body, so on its own the per-cabinet limit is
/// bypassable by minting fresh ids; this cap bounds the total POST volume
/// regardless of the claimed cabinet.
const GLOBAL_RATE_LIMIT_MAX: usize = 60;
/// Rate-limit window.
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
/// Hard cap on distinct cabinet keys retained by the rate limiter, so a
/// client rotating cabinet_id values cannot grow the map without bound.
const RATE_KEYS_MAX: usize = 1024;
/// Tolerated forward clock skew on a submission's `ended_at`.
const MAX_ENDED_AT_SKEW: time::Duration = time::Duration::minutes(5);

/// Sliding-window request logs behind [`AppState::rate_check`].
#[derive(Default)]
struct RateState {
    /// Per-cabinet request log.
    per_cabinet: HashMap<String, Vec<Instant>>,
    /// All-cabinets request log for the global cap.
    global: Vec<Instant>,
}

/// Shared application state.
pub struct AppState {
    /// Database handle.
    pub pool: SqlitePool,
    /// Bearer token required on `POST /v1/runs`; `None` = open (dev mode).
    pub token: Option<String>,
    /// Sliding-window request logs for rate limiting.
    rate: Mutex<RateState>,
    /// [`GLOBAL_RATE_LIMIT_MAX`], overridable in tests.
    global_rate_max: usize,
    /// [`RATE_KEYS_MAX`], overridable in tests.
    rate_keys_max: usize,
}

impl AppState {
    /// Creates state around an opened pool.
    pub fn new(pool: SqlitePool, token: Option<String>) -> Self {
        AppState {
            pool,
            token,
            rate: Mutex::new(RateState::default()),
            global_rate_max: GLOBAL_RATE_LIMIT_MAX,
            rate_keys_max: RATE_KEYS_MAX,
        }
    }

    /// Overrides the rate-limiter bounds so tests can exercise them
    /// without thousands of requests.
    #[cfg(test)]
    fn with_rate_bounds(mut self, global_max: usize, keys_max: usize) -> Self {
        self.global_rate_max = global_max;
        self.rate_keys_max = keys_max;
        self
    }

    /// Records a POST from `cabinet_id`; returns `false` when over budget.
    ///
    /// Three bounds, in order: the global request budget (cabinet_id is
    /// client-supplied, so per-key budgets alone are mintable), the cap on
    /// distinct keys (evicting keys idle for a full window first, so the
    /// map cannot grow without bound), and the per-cabinet budget.
    fn rate_check(&self, cabinet_id: &str) -> bool {
        let now = Instant::now();
        let mut rate = self.rate.lock().expect("rate limiter poisoned");
        let rate = &mut *rate;
        rate.global
            .retain(|t| now.duration_since(*t) < RATE_LIMIT_WINDOW);
        if rate.global.len() >= self.global_rate_max {
            return false;
        }
        if !rate.per_cabinet.contains_key(cabinet_id)
            && rate.per_cabinet.len() >= self.rate_keys_max
        {
            rate.per_cabinet.retain(|_, log| {
                log.last()
                    .is_some_and(|t| now.duration_since(*t) < RATE_LIMIT_WINDOW)
            });
            if rate.per_cabinet.len() >= self.rate_keys_max {
                return false;
            }
        }
        let log = rate.per_cabinet.entry(cabinet_id.to_owned()).or_default();
        log.retain(|t| now.duration_since(*t) < RATE_LIMIT_WINDOW);
        if log.len() >= RATE_LIMIT_MAX {
            return false;
        }
        log.push(now);
        rate.global.push(now);
        true
    }
}

/// Builds the router with all routes attached.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/v1/runs", post(submit_run))
        .route("/v1/boards", get(boards))
        .route("/v1/boards/{category}", get(board_one))
        .with_state(state)
}

/// Current time as an RFC 3339 string.
pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// Uniform JSON error: `{"error": "..."}` with a status code.
struct ApiError {
    status: StatusCode,
    msg: String,
}

impl ApiError {
    fn new(status: StatusCode, msg: impl Into<String>) -> Self {
        ApiError {
            status,
            msg: msg.into(),
        }
    }

    fn unprocessable(msg: impl Into<String>) -> Self {
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, msg)
    }

    fn internal(err: anyhow::Error) -> Self {
        tracing::error!(error = %err, "internal error");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({ "error": self.msg }));
        let mut resp = (self.status, body).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            resp.headers_mut()
                .insert(header::WWW_AUTHENTICATE, "Bearer".parse().unwrap());
        }
        resp
    }
}

/// Constant-time byte comparison. Length mismatch returns early (the token
/// length is not a secret); equal-length inputs are compared without
/// data-dependent branching.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Checks `Authorization: Bearer <token>` against the configured token.
fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = state.token.as_deref() else {
        return Ok(()); // open mode
    };
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "missing bearer token"))?;
    if ct_eq(supplied.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "invalid bearer token",
        ))
    }
}

fn check_str(field: &str, value: &str, max: usize, required: bool) -> Result<(), ApiError> {
    if required && value.is_empty() {
        return Err(ApiError::unprocessable(format!(
            "{field} must not be empty"
        )));
    }
    if value.len() > max {
        return Err(ApiError::unprocessable(format!(
            "{field} exceeds {max} bytes"
        )));
    }
    Ok(())
}

fn check_rfc3339(field: &str, value: &str) -> Result<OffsetDateTime, ApiError> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| ApiError::unprocessable(format!("{field} is not an RFC 3339 timestamp")))
}

/// Full server-side validation of a submission (SPEC §7.3): shape and size
/// caps, per-map plausibility, aggregate consistency, end-reason sanity,
/// and score recomputation.
fn validate(sub: &RunSubmission) -> Result<(), ApiError> {
    if !validate_initials(&sub.initials) {
        return Err(ApiError::unprocessable(
            "initials must be exactly 3 chars, A-Z or 0-9",
        ));
    }
    check_str("session", &sub.session, MAX_STRING, true)?;
    check_str("cabinet_id", &sub.cabinet_id, MAX_STRING, true)?;
    check_str("iwad_sha256", &sub.iwad_sha256, MAX_STRING, false)?;
    check_str("map_rotation_id", &sub.map_rotation_id, MAX_STRING, true)?;
    check_str("started_at", &sub.started_at, MAX_STRING, true)?;
    check_str("ended_at", &sub.ended_at, MAX_STRING, true)?;
    let started_at = check_rfc3339("started_at", &sub.started_at)?;
    let ended_at = check_rfc3339("ended_at", &sub.ended_at)?;
    // Timestamps are client-supplied: bound them so a far-future ended_at
    // cannot poison anything ordered or filtered by it.
    if started_at > ended_at {
        return Err(ApiError::unprocessable("started_at is after ended_at"));
    }
    if ended_at > OffsetDateTime::now_utc() + MAX_ENDED_AT_SKEW {
        return Err(ApiError::unprocessable("ended_at is in the future"));
    }
    // The scoring formula below is this binary's only formula: a
    // submission scored under any other version cannot be validated (or
    // ranked) here, so reject it distinctly instead of failing the score
    // recomputation with a misleading mismatch. This is a permanent
    // (non-retryable) rejection for the submitting cabinet.
    if sub.scoring_version != protocol::SCORING_VERSION {
        return Err(ApiError::unprocessable(format!(
            "unsupported scoring_version {} (this server validates only version {})",
            sub.scoring_version,
            protocol::SCORING_VERSION
        )));
    }

    if sub.maps.len() > MAX_MAPS {
        return Err(ApiError::unprocessable(format!(
            "maps exceeds {MAX_MAPS} entries"
        )));
    }
    for (name, v) in [
        ("maps_completed", sub.maps_completed),
        ("kills", sub.kills),
        ("secrets", sub.secrets),
        ("items", sub.items),
        ("total_tics", sub.total_tics),
        ("run_score", sub.run_score),
        ("scoring_version", sub.scoring_version),
    ] {
        if v < 0 {
            return Err(ApiError::unprocessable(format!("{name} is negative")));
        }
    }

    for (i, m) in sub.maps.iter().enumerate() {
        if m.seq != i as i64 {
            return Err(ApiError::unprocessable(format!(
                "maps[{i}].seq is {} (expected {i} — no gaps, in play order)",
                m.seq
            )));
        }
        check_str(&format!("maps[{i}].map"), &m.map, 16, true)?;
        for (name, v) in [
            ("kills", m.kills),
            ("total_monsters", m.total_monsters),
            ("secrets", m.secrets),
            ("total_secrets", m.total_secrets),
            ("items", m.items),
            ("total_items", m.total_items),
            ("tics", m.tics),
            ("map_score", m.map_score),
        ] {
            if v < 0 {
                return Err(ApiError::unprocessable(format!(
                    "maps[{i}].{name} is negative"
                )));
            }
        }
        for (name, got, total) in [
            ("kills", m.kills, m.total_monsters),
            ("secrets", m.secrets, m.total_secrets),
            ("items", m.items, m.total_items),
        ] {
            if total > 0 && got > total {
                return Err(ApiError::unprocessable(format!(
                    "maps[{i}].{name} exceeds the map total"
                )));
            }
        }
    }

    let completed = sub.maps.iter().filter(|m| m.completed).count() as i64;
    if sub.maps_completed != completed {
        return Err(ApiError::unprocessable(
            "maps_completed does not match the per-map completed flags",
        ));
    }
    for (name, agg, sum) in [
        ("kills", sub.kills, sub.maps.iter().map(|m| m.kills).sum()),
        (
            "secrets",
            sub.secrets,
            sub.maps.iter().map(|m| m.secrets).sum(),
        ),
        ("items", sub.items, sub.maps.iter().map(|m| m.items).sum()),
        (
            "total_tics",
            sub.total_tics,
            sub.maps.iter().map(|m| m.tics).sum::<i64>(),
        ),
    ] {
        if agg != sum {
            return Err(ApiError::unprocessable(format!(
                "{name} does not equal the sum of the per-map values"
            )));
        }
    }

    // The supervisor's state machine deliberately emits end_reason
    // "complete" with a not-completed map row when a level_complete line
    // was lost in telemetry (its documented lossy-FIFO leniency), so that
    // shape must be accepted — rejecting it would make the client's own
    // tolerated states unsubmittable and lose the run forever. Only the
    // fully-empty shape (complete with no map rows at all) is implausible
    // enough to reject.
    if sub.end_reason == EndReason::Complete && sub.maps.is_empty() {
        return Err(ApiError::unprocessable(
            "end_reason is \"complete\" but no maps were recorded",
        ));
    }

    let recomputed = sub.recompute_score();
    if recomputed != sub.run_score {
        return Err(ApiError::unprocessable(format!(
            "run_score mismatch: submitted {}, recomputed {recomputed}",
            sub.run_score
        )));
    }
    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn submit_run(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(sub): Json<RunSubmission>,
) -> Result<Response, ApiError> {
    check_auth(&state, &headers)?;
    // Cap the cabinet id before it becomes a rate-limiter map key.
    check_str("cabinet_id", &sub.cabinet_id, MAX_STRING, true)?;
    if !state.rate_check(&sub.cabinet_id) {
        let mut resp =
            ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
        resp.headers_mut()
            .insert(header::RETRY_AFTER, "60".parse().unwrap());
        return Ok(resp);
    }
    validate(&sub)?;

    let (outcome, stored) = db::insert_run(&state.pool, &sub)
        .await
        .map_err(ApiError::internal)?;
    let status = match outcome {
        InsertOutcome::Inserted => StatusCode::CREATED,
        InsertOutcome::Duplicate => StatusCode::OK,
    };
    tracing::info!(
        session = %stored.session,
        initials = %stored.initials,
        cabinet = %stored.cabinet_id,
        run_score = stored.run_score,
        outcome = ?outcome,
        "run submitted"
    );
    Ok((status, Json(stored)).into_response())
}

/// Query parameters shared by the board endpoints.
#[derive(Debug, Deserialize)]
struct BoardsQuery {
    limit: Option<i64>,
    season: Option<String>,
}

fn effective_limit(q: &BoardsQuery) -> i64 {
    q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// Parses `?season=<iwad_sha>:<scoring_version>:<rotation_id>`.
fn parse_season(s: &str) -> Result<Season, ApiError> {
    let mut parts = s.splitn(3, ':');
    let (Some(iwad), Some(ver), Some(rotation)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "season must be <iwad_sha>:<scoring_version>:<rotation_id>",
        ));
    };
    let scoring_version: i64 = ver.parse().map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "season scoring_version must be an integer",
        )
    })?;
    Ok(Season {
        iwad_sha256: iwad.to_owned(),
        scoring_version,
        map_rotation_id: rotation.to_owned(),
    })
}

async fn resolve_season(pool: &SqlitePool, q: &BoardsQuery) -> Result<Season, ApiError> {
    match &q.season {
        Some(s) => parse_season(s),
        None => db::current_season(pool).await.map_err(ApiError::internal),
    }
}

async fn boards(
    State(state): State<Arc<AppState>>,
    Query(q): Query<BoardsQuery>,
) -> Result<Json<BoardsResponse>, ApiError> {
    let season = resolve_season(&state.pool, &q).await?;
    let boards = db::all_boards(&state.pool, &season, effective_limit(&q))
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(BoardsResponse {
        season,
        boards,
        generated_at: now_rfc3339(),
    }))
}

async fn board_one(
    State(state): State<Arc<AppState>>,
    Path(category): Path<String>,
    Query(q): Query<BoardsQuery>,
) -> Result<Response, ApiError> {
    let category: BoardCategory = category
        .parse()
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "unknown board category"))?;
    let season = resolve_season(&state.pool, &q).await?;
    let board = db::board(&state.pool, &season, category, effective_limit(&q))
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(board).into_response())
}

async fn index(State(state): State<Arc<AppState>>) -> Result<Html<String>, ApiError> {
    let season = db::current_season(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let boards = db::all_boards(&state.pool, &season, DEFAULT_LIMIT)
        .await
        .map_err(ApiError::internal)?;
    Ok(Html(html::render(&season, &boards)))
}

#[cfg(test)]
mod rate_tests {
    use super::*;
    use crate::db;

    async fn state(global_max: usize, keys_max: usize) -> AppState {
        let pool = db::open(":memory:").await.expect("open :memory:");
        AppState::new(pool, None).with_rate_bounds(global_max, keys_max)
    }

    #[tokio::test]
    async fn rate_limiter_key_count_is_bounded() {
        // Distinct cabinet_ids are attacker-mintable; the key map must
        // stay bounded instead of growing until OOM.
        let s = state(10_000, 4).await;
        for key in ["a", "b", "c", "d"] {
            assert!(s.rate_check(key), "key {key} within the key cap");
        }
        // A fifth fresh key while all four are live inside the window:
        // rejected rather than inserted.
        assert!(
            !s.rate_check("e"),
            "fresh key past the cap must be rejected"
        );
        // Existing keys keep their budget, and the map stayed bounded.
        assert!(s.rate_check("a"));
        let rate = s.rate.lock().unwrap();
        assert!(rate.per_cabinet.len() <= 4);
    }

    #[tokio::test]
    async fn global_budget_bounds_rotating_cabinet_ids() {
        // Rotating cabinet_id used to mint a fresh 10/min budget per
        // request; the global cap bounds the flood regardless of key.
        let s = state(5, 1_000).await;
        for i in 0..5 {
            assert!(
                s.rate_check(&format!("cab-{i}")),
                "request {i} within the global budget"
            );
        }
        assert!(
            !s.rate_check("cab-fresh"),
            "a fresh cabinet_id must not bypass the global budget"
        );
    }
}
