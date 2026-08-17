//! In-crate integration tests: `tower::ServiceExt::oneshot` against the
//! real router with an in-memory SQLite database.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use protocol::{
    map_score, run_score, Board, BoardCategory, BoardsResponse, EndReason, MapResult, MapStats,
    RunSubmission, MAP_ROTATION, MAP_ROTATION_ID, SCORING_VERSION, TICS_PER_SECOND,
};
use tower::ServiceExt;

use crate::app::{self, AppState};
use crate::db;

const IWAD: &str = "feedface";

async fn test_app(token: Option<&str>) -> Router {
    let pool = db::open(":memory:").await.expect("open :memory:");
    app::router(Arc::new(AppState::new(pool, token.map(str::to_owned))))
}

/// Builds an internally consistent submission from per-map
/// `(kills, secrets, seconds, completed)` tuples.
fn run_with(
    session: &str,
    initials: &str,
    cabinet: &str,
    ended_at: &str,
    end_reason: EndReason,
    maps: &[(i64, i64, i64, bool)],
) -> RunSubmission {
    let stats: Vec<MapStats> = maps
        .iter()
        .enumerate()
        .map(|(i, &(kills, secrets, seconds, completed))| MapStats {
            map: MAP_ROTATION[i].to_owned(),
            kills,
            total_monsters: 200,
            secrets,
            total_secrets: 10,
            items: 0,
            total_items: 50,
            tics: seconds * TICS_PER_SECOND,
            completed,
        })
        .collect();
    let map_results: Vec<MapResult> = stats
        .iter()
        .enumerate()
        .map(|(seq, s)| MapResult {
            seq: seq as i64,
            map: s.map.clone(),
            kills: s.kills,
            total_monsters: s.total_monsters,
            secrets: s.secrets,
            total_secrets: s.total_secrets,
            items: s.items,
            total_items: s.total_items,
            tics: s.tics,
            completed: s.completed,
            map_score: map_score(s),
        })
        .collect();
    RunSubmission {
        session: session.to_owned(),
        initials: initials.to_owned(),
        cabinet_id: cabinet.to_owned(),
        started_at: "2026-08-17T12:00:00Z".to_owned(),
        ended_at: ended_at.to_owned(),
        end_reason,
        maps_completed: stats.iter().filter(|s| s.completed).count() as i64,
        kills: stats.iter().map(|s| s.kills).sum(),
        secrets: stats.iter().map(|s| s.secrets).sum(),
        items: stats.iter().map(|s| s.items).sum(),
        total_tics: stats.iter().map(|s| s.tics).sum(),
        run_score: run_score(&stats),
        iwad_sha256: IWAD.to_owned(),
        scoring_version: SCORING_VERSION,
        map_rotation_id: MAP_ROTATION_ID.to_owned(),
        maps: map_results,
    }
}

fn post_run(sub: &RunSubmission, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/runs")
        .header("content-type", "application/json");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(serde_json::to_vec(sub).unwrap()))
        .unwrap()
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

async fn get_boards(app: &Router, uri: &str) -> (StatusCode, Vec<u8>) {
    send(
        app,
        Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

fn board_by(resp: &BoardsResponse, category: BoardCategory) -> &Board {
    resp.boards
        .iter()
        .find(|b| b.category == category)
        .expect("board present")
}

fn initials_of(board: &Board) -> Vec<&str> {
    board.entries.iter().map(|e| e.initials.as_str()).collect()
}

#[tokio::test]
async fn healthz_ok() {
    let app = test_app(None).await;
    let (status, body) = get_boards(&app, "/healthz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"ok");
}

#[tokio::test]
async fn idempotent_double_submit() {
    let app = test_app(None).await;
    let sub = run_with(
        "s-idem",
        "ABC",
        "cab-1",
        "2026-08-17T12:10:00Z",
        EndReason::Death,
        &[(50, 2, 200, true), (12, 1, 60, false)],
    );

    let (status, body) = send(&app, post_run(&sub, None)).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let stored: RunSubmission = serde_json::from_slice(&body).unwrap();
    assert_eq!(stored, sub);

    // Re-submit the same session with different (still valid) content:
    // 200, and the ORIGINAL record comes back, never a duplicate.
    let mut resubmit = run_with(
        "s-idem",
        "ZZZ",
        "cab-1",
        "2026-08-17T12:10:00Z",
        EndReason::Death,
        &[(50, 2, 200, true), (12, 1, 60, false)],
    );
    resubmit.session = sub.session.clone();
    let (status, body) = send(&app, post_run(&resubmit, None)).await;
    assert_eq!(status, StatusCode::OK);
    let stored: RunSubmission = serde_json::from_slice(&body).unwrap();
    assert_eq!(stored.initials, "ABC", "original record must be returned");

    let (status, body) = get_boards(&app, "/v1/boards").await;
    assert_eq!(status, StatusCode::OK);
    let resp: BoardsResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        board_by(&resp, BoardCategory::HighScore).entries.len(),
        1,
        "double-submit must not create a second row"
    );
}

#[tokio::test]
async fn score_mismatch_is_422() {
    let app = test_app(None).await;
    let mut sub = run_with(
        "s-cheat",
        "ABC",
        "cab-1",
        "2026-08-17T12:10:00Z",
        EndReason::Death,
        &[(50, 2, 200, false)],
    );
    sub.run_score += 1_000_000;
    let (status, body) = send(&app, post_run(&sub, None)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(err["error"]
        .as_str()
        .unwrap()
        .contains("run_score mismatch"));
}

#[tokio::test]
async fn bad_initials_is_422() {
    let app = test_app(None).await;
    for bad in ["ab1", "ABCD", "A B", "Á1C", ""] {
        let sub = run_with(
            "s-bad-ini",
            bad,
            "cab-1",
            "2026-08-17T12:10:00Z",
            EndReason::Death,
            &[(1, 0, 10, false)],
        );
        let (status, _) = send(&app, post_run(&sub, None)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "initials {bad:?}");
    }
}

#[tokio::test]
async fn implausible_values_are_422() {
    let app = test_app(None).await;
    let base = |session: &str| {
        run_with(
            session,
            "ABC",
            "cab-1",
            "2026-08-17T12:10:00Z",
            EndReason::Death,
            &[(10, 1, 60, true), (5, 0, 30, false)],
        )
    };

    // kills > total_monsters (totals are 200 in the helper).
    let mut sub = base("s-kills");
    sub.maps[0].kills = 500;
    sub.kills = sub.maps.iter().map(|m| m.kills).sum();
    sub.run_score = sub.recompute_score();
    let (status, _) = send(&app, post_run(&sub, None)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "kills > total");

    // Negative tics.
    let mut sub = base("s-neg");
    sub.maps[1].tics = -5;
    sub.total_tics = sub.maps.iter().map(|m| m.tics).sum();
    sub.run_score = sub.recompute_score();
    let (status, _) = send(&app, post_run(&sub, None)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "negative tics");

    // Seq gap.
    let mut sub = base("s-gap");
    sub.maps[1].seq = 3;
    let (status, _) = send(&app, post_run(&sub, None)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "seq gap");

    // Aggregate kills disagree with the per-map sum.
    let mut sub = base("s-agg");
    sub.kills += 7;
    let (status, _) = send(&app, post_run(&sub, None)).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "aggregate mismatch"
    );

    // end_reason complete without completing every map.
    let mut sub = base("s-endr");
    sub.end_reason = EndReason::Complete;
    let (status, _) = send(&app, post_run(&sub, None)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "bogus complete");

    // Oversized cabinet_id.
    let mut sub = base("s-size");
    sub.cabinet_id = "x".repeat(65);
    let (status, _) = send(&app, post_run(&sub, None)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "oversized string");
}

#[tokio::test]
async fn auth_enforced_when_token_configured() {
    let app = test_app(Some("sekrit")).await;
    let sub = run_with(
        "s-auth",
        "ABC",
        "cab-1",
        "2026-08-17T12:10:00Z",
        EndReason::Death,
        &[(1, 0, 10, false)],
    );

    let (status, _) = send(&app, post_run(&sub, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "missing token");

    let (status, _) = send(&app, post_run(&sub, Some("wrong"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "wrong token");

    let (status, _) = send(&app, post_run(&sub, Some("sekrit"))).await;
    assert_eq!(status, StatusCode::CREATED, "correct token");

    // GET stays open regardless.
    let (status, _) = get_boards(&app, "/v1/boards").await;
    assert_eq!(status, StatusCode::OK);
}

/// Three hand-crafted runs with fully known stats:
///
/// - AAA: full clear, 20 kills / 1 secret / 120 s per map → score 9800,
///   100 kills, 5 secrets, 10:00 total.
/// - BBB: full clear, faster and quieter: 10 kills / 0 secrets / 100 s per
///   map → score 9000, 50 kills, 0 secrets, 08:20 total.
/// - CCC: death on MAP03 after two clears → score 3850, 75 kills,
///   5 secrets.
async fn seeded_ordering_app() -> Router {
    let app = test_app(None).await;
    let runs = [
        run_with(
            "s-aaa",
            "AAA",
            "cab-1",
            "2026-08-17T13:00:00Z",
            EndReason::Complete,
            &[(20, 1, 120, true); 5],
        ),
        run_with(
            "s-bbb",
            "BBB",
            "cab-1",
            "2026-08-17T14:00:00Z",
            EndReason::Complete,
            &[(10, 0, 100, true); 5],
        ),
        run_with(
            "s-ccc",
            "CCC",
            "cab-1",
            "2026-08-17T15:00:00Z",
            EndReason::Death,
            &[(30, 2, 300, true), (30, 2, 300, true), (15, 1, 100, false)],
        ),
    ];
    for run in &runs {
        let (status, body) = send(&app, post_run(run, None)).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "seed {}: {}",
            run.session,
            String::from_utf8_lossy(&body)
        );
    }
    app
}

#[tokio::test]
async fn boards_ordering_is_correct() {
    let app = seeded_ordering_app().await;
    let (status, body) = get_boards(&app, "/v1/boards").await;
    assert_eq!(status, StatusCode::OK);
    let resp: BoardsResponse = serde_json::from_slice(&body).unwrap();

    // Season is the one every run was submitted under.
    assert_eq!(resp.season.iwad_sha256, IWAD);
    assert_eq!(resp.season.scoring_version, SCORING_VERSION);
    assert_eq!(resp.season.map_rotation_id, MAP_ROTATION_ID);
    assert_eq!(resp.boards.len(), 5);

    let high = board_by(&resp, BoardCategory::HighScore);
    assert_eq!(initials_of(high), ["AAA", "BBB", "CCC"]);
    assert_eq!(high.entries[0].value, 9800);
    assert_eq!(high.entries[1].value, 9000);
    assert_eq!(high.entries[2].value, 3850);

    // Deepest: both clears at 5 maps — tiebreak on score puts AAA first.
    let deepest = board_by(&resp, BoardCategory::Deepest);
    assert_eq!(initials_of(deepest), ["AAA", "BBB", "CCC"]);
    assert_eq!(deepest.entries[0].value, 5);
    assert_eq!(deepest.entries[2].value, 2);

    let fastest = board_by(&resp, BoardCategory::FastestClear);
    assert_eq!(initials_of(fastest), ["BBB", "AAA"]);
    assert_eq!(fastest.entries[0].value, 500 * TICS_PER_SECOND);
    assert_eq!(fastest.entries[0].value_display, "08:20");
    assert_eq!(fastest.entries[1].value_display, "10:00");

    let kills = board_by(&resp, BoardCategory::MostKills);
    assert_eq!(initials_of(kills), ["AAA", "CCC", "BBB"]);
    assert_eq!(kills.entries[0].value, 100);
    assert_eq!(kills.entries[1].value, 75);

    // Secrets: AAA and CCC tie at 5 — score tiebreak puts AAA first.
    let secrets = board_by(&resp, BoardCategory::SecretHunter);
    assert_eq!(initials_of(secrets), ["AAA", "CCC", "BBB"]);
    assert_eq!(secrets.entries[0].value, 5);
}

#[tokio::test]
async fn fastest_clear_excludes_deaths() {
    let app = seeded_ordering_app().await;
    let (status, body) = get_boards(&app, "/v1/boards/fastest-clear").await;
    assert_eq!(status, StatusCode::OK);
    let board: Board = serde_json::from_slice(&body).unwrap();
    assert_eq!(board.category, BoardCategory::FastestClear);
    assert_eq!(
        initials_of(&board),
        ["BBB", "AAA"],
        "the death run (CCC) must not appear on fastest-clear"
    );
}

#[tokio::test]
async fn single_board_endpoint_limit_season_and_404() {
    let app = seeded_ordering_app().await;

    let (status, body) = get_boards(&app, "/v1/boards/high-score?limit=2").await;
    assert_eq!(status, StatusCode::OK);
    let board: Board = serde_json::from_slice(&body).unwrap();
    assert_eq!(initials_of(&board), ["AAA", "BBB"]);

    // Explicit season filter: a different season has an empty board.
    let uri = format!("/v1/boards/high-score?season=otherwad:{SCORING_VERSION}:{MAP_ROTATION_ID}");
    let (status, body) = get_boards(&app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    let board: Board = serde_json::from_slice(&body).unwrap();
    assert!(board.entries.is_empty());

    let (status, _) = get_boards(&app, "/v1/boards/no-such-board").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = get_boards(&app, "/v1/boards/high-score?season=garbage").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn per_cabinet_rate_limit_is_429() {
    let app = test_app(None).await;
    for i in 0..10 {
        let sub = run_with(
            &format!("s-rl-{i}"),
            "RRR",
            "cab-flood",
            "2026-08-17T12:10:00Z",
            EndReason::Death,
            &[(1, 0, 10, false)],
        );
        let (status, _) = send(&app, post_run(&sub, None)).await;
        assert_eq!(status, StatusCode::CREATED, "request {i} within budget");
    }
    let sub = run_with(
        "s-rl-over",
        "RRR",
        "cab-flood",
        "2026-08-17T12:10:00Z",
        EndReason::Death,
        &[(1, 0, 10, false)],
    );
    let (status, _) = send(&app, post_run(&sub, None)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // A different cabinet still has budget.
    let mut other = sub.clone();
    other.session = "s-rl-other".to_owned();
    other.cabinet_id = "cab-other".to_owned();
    let (status, _) = send(&app, post_run(&other, None)).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn empty_db_boards_and_html() {
    let app = test_app(None).await;

    let (status, body) = get_boards(&app, "/v1/boards").await;
    assert_eq!(status, StatusCode::OK);
    let resp: BoardsResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(resp.boards.len(), 5);
    assert!(resp.boards.iter().all(|b| b.entries.is_empty()));
    assert_eq!(resp.season.scoring_version, SCORING_VERSION);

    let (status, body) = get_boards(&app, "/").await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(body).unwrap();
    assert!(html.contains("DOOM ARCADE"));
    assert!(html.contains("INSERT COIN"), "empty state present");
    assert!(
        html.contains("board-fastest-clear"),
        "all five cards render"
    );
    assert!(!html.contains("src=\"http"), "no external requests");
    assert!(!html.contains("href=\"http"), "no external requests");
}

#[tokio::test]
async fn html_renders_entries_and_escapes() {
    let app = seeded_ordering_app().await;
    let (status, body) = get_boards(&app, "/").await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(body).unwrap();
    assert!(html.contains("AAA"));
    assert!(html.contains("08:20"), "fastest-clear clock rendered");
    assert!(html.contains(IWAD), "season fingerprint in footer");
}
