use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use tokio::sync::{broadcast, Mutex};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use repo_recall::{commits, db, routes, AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env from cwd (or repo root when launched via cargo run) before
    // reading any env vars. Missing .env is not an error.
    let _ = dotenvy::dotenv();

    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,repo_recall=debug")),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cwd = match std::env::var_os("REPO_RECALL_CWD") {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir()?,
    };
    let cwd = dunce::canonicalize(&cwd).unwrap_or(cwd);

    let db_path = std::env::var("REPO_RECALL_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("repo-recall.sqlite"));

    tracing::info!("cwd: {}", cwd.display());
    tracing::info!("db:  {}", db_path.display());

    // Initialize schema (wiping any prior data).
    db::init(&db_path)?;

    let (progress_tx, _) = broadcast::channel::<String>(128);

    let scan_depth: usize = std::env::var("REPO_RECALL_DEPTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let commits_per_repo: usize = std::env::var("REPO_RECALL_COMMITS_PER_REPO")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);

    let gh_health = commits::gh_health();
    let my_gh_login = if gh_health == commits::GhHealth::Ok {
        commits::my_gh_login()
    } else {
        None
    };
    let my_git_email = detect_my_git_email();

    let refresh_interval_secs: u64 = std::env::var("REPO_RECALL_REFRESH_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(150);
    let remote_target_limit: usize = std::env::var("REPO_RECALL_REMOTE_TARGET_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(25);

    let state = AppState {
        db_path,
        cwd,
        scan_depth,
        commits_per_repo,
        refresh_interval_secs,
        remote_target_limit,
        progress_tx,
        refresh_lock: Arc::new(Mutex::new(())),
        last_scan: Arc::new(Mutex::new(None)),
        gh_health: Arc::new(Mutex::new(gh_health)),
        my_gh_login: Arc::new(Mutex::new(my_gh_login)),
        my_git_email: Arc::new(Mutex::new(my_git_email)),
    };

    // Kick off initial scan in the background so the dashboard has data
    // by the time the user finishes typing the URL.
    {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = routes::refresh::run_refresh(state).await {
                tracing::error!("initial refresh failed: {e:?}");
            }
        });
    }

    // Periodic refresh: fires every REPO_RECALL_REFRESH_INTERVAL_SECS. Set to
    // 0 to disable. Uses the same `refresh_lock` as the manual /refresh, so a
    // tick that overlaps an in-flight scan no-ops cleanly.
    if refresh_interval_secs > 0 {
        tracing::info!("periodic refresh: every {refresh_interval_secs}s");
        let state = state.clone();
        tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(std::time::Duration::from_secs(refresh_interval_secs));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // skip the immediate first tick — initial scan covers it
            loop {
                ticker.tick().await;
                if let Err(e) = routes::refresh::run_refresh(state.clone()).await {
                    tracing::error!("periodic refresh failed: {e:?}");
                }
            }
        });
    } else {
        tracing::info!("periodic refresh disabled (REPO_RECALL_REFRESH_INTERVAL_SECS=0)");
    }

    let app: Router = routes::router(state.clone());

    let port: u16 = std::env::var("REPO_RECALL_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7777);
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on http://{}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

/// Detect the viewer's git identity so "my commits" author-filter works by
/// default. Honors `REPO_RECALL_AUTHOR` env var if set, else falls back to
/// `git config --global user.email`.
fn detect_my_git_email() -> Option<String> {
    if let Ok(email) = std::env::var("REPO_RECALL_AUTHOR") {
        if !email.is_empty() && email != "all" {
            return Some(email);
        }
    }
    let out = std::process::Command::new("git")
        .args(["config", "--global", "user.email"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
