//! `arcade-leaderboard` — score API + web view (SPEC §7).
//!
//! Single axum + sqlx (SQLite) binary. CLI:
//!
//! ```text
//! arcade-leaderboard [--listen ADDR] [--db PATH] [--seed N] [--token-file PATH]
//! ```
//!
//! Bearer-token auth for `POST /v1/runs` comes from the `ARCADE_API_TOKEN`
//! environment variable, falling back to `--token-file`; with neither set
//! the POST endpoint is open and a warning is logged at startup (dev mode).

mod app;
mod db;
mod html;
mod seed;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;

const USAGE: &str = "\
arcade-leaderboard — DOOM arcade cabinet score API + web view

USAGE:
  arcade-leaderboard [OPTIONS]

OPTIONS:
  --listen ADDR       Listen address (default 127.0.0.1:8080)
  --db PATH           SQLite database path (default ./leaderboard.sqlite,
                      \":memory:\" for ephemeral)
  --seed N            Insert N plausible fake runs, then serve
  --token-file PATH   File containing the POST bearer token
  --help              Show this help

ENVIRONMENT:
  ARCADE_API_TOKEN    POST bearer token (takes precedence over --token-file)
";

struct Cli {
    listen: SocketAddr,
    db: String,
    seed: u64,
    token_file: Option<String>,
}

fn parse_cli(args: &[String]) -> Result<Cli, String> {
    let mut cli = Cli {
        listen: "127.0.0.1:8080".parse().expect("default listen addr"),
        db: "./leaderboard.sqlite".to_owned(),
        seed: 0,
        token_file: None,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match arg.as_str() {
            "--listen" => {
                let v = value("--listen")?;
                cli.listen = v
                    .parse()
                    .map_err(|_| format!("invalid --listen address {v:?}"))?;
            }
            "--db" => cli.db = value("--db")?,
            "--seed" => {
                let v = value("--seed")?;
                cli.seed = v
                    .parse()
                    .map_err(|_| format!("invalid --seed count {v:?}"))?;
            }
            "--token-file" => cli.token_file = Some(value("--token-file")?),
            "--help" | "-h" => return Err(String::new()),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(cli)
}

/// Resolves the bearer token: `ARCADE_API_TOKEN` wins, then `--token-file`.
fn resolve_token(cli: &Cli) -> anyhow::Result<Option<String>> {
    if let Ok(t) = std::env::var("ARCADE_API_TOKEN") {
        let t = t.trim().to_owned();
        if !t.is_empty() {
            return Ok(Some(t));
        }
    }
    if let Some(path) = &cli.token_file {
        let t = std::fs::read_to_string(path)
            .with_context(|| format!("reading token file {path:?}"))?
            .trim()
            .to_owned();
        if t.is_empty() {
            anyhow::bail!("token file {path:?} is empty");
        }
        return Ok(Some(t));
    }
    Ok(None)
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let token = resolve_token(&cli)?;
    let auth_mode = if token.is_some() { "bearer" } else { "open" };

    let pool = db::open(&cli.db).await?;

    if cli.seed > 0 {
        let mut inserted = 0u64;
        for sub in seed::fake_runs(cli.seed) {
            let (outcome, _) = db::insert_run(&pool, &sub).await?;
            if outcome == db::InsertOutcome::Inserted {
                inserted += 1;
            }
        }
        tracing::info!(requested = cli.seed, inserted, "seeded fake runs");
    }

    if token.is_none() {
        tracing::warn!(
            "no bearer token configured (ARCADE_API_TOKEN / --token-file): \
             POST /v1/runs is OPEN — dev mode only"
        );
    }
    tracing::info!(
        listen = %cli.listen,
        db = %cli.db,
        auth = auth_mode,
        "arcade-leaderboard starting"
    );

    let state = Arc::new(app::AppState::new(pool, token));
    let listener = tokio::net::TcpListener::bind(cli.listen)
        .await
        .with_context(|| format!("binding {}", cli.listen))?;
    axum::serve(listener, app::router(state))
        .await
        .context("serving")?;
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse_cli(&args) {
        Ok(cli) => cli,
        Err(msg) => {
            if msg.is_empty() {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            eprintln!("error: {msg}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %format!("{err:#}"), "fatal");
            ExitCode::FAILURE
        }
    }
}
