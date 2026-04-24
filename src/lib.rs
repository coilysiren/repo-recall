use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, Mutex};

pub mod activity;
pub mod commits;
pub mod db;
pub mod join;
pub mod routes;
pub mod scanner;
pub mod sessions;

#[derive(Clone)]
pub struct AppState {
    pub db_path: PathBuf,
    pub cwd: PathBuf,
    pub scan_depth: usize,
    pub commits_per_repo: usize,
    /// Seconds between periodic background refreshes. `0` disables the
    /// periodic task; the dashboard hides the countdown in that case.
    pub refresh_interval_secs: u64,
    pub progress_tx: broadcast::Sender<String>,
    pub refresh_lock: Arc<Mutex<()>>,
    pub last_scan: Arc<Mutex<Option<chrono::DateTime<chrono::Utc>>>>,
    /// State of the local `gh` CLI. Updated at startup and re-checked at the
    /// start of each refresh so the banner disappears as soon as the user
    /// installs / logs in.
    pub gh_health: Arc<Mutex<commits::GhHealth>>,
    /// GitHub login of the authenticated user, cached from `gh api user`.
    /// `None` when `gh_health != Ok`. Drives the "awaiting my review" split.
    pub my_gh_login: Arc<Mutex<Option<String>>>,
    /// Viewer's git email (from `git config --global user.email`), used as
    /// the default author for `?author=me` filtering. Fallback when
    /// `REPO_RECALL_AUTHOR` isn't set.
    pub my_git_email: Arc<Mutex<Option<String>>>,
}
