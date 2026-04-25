use std::path::PathBuf;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::Utc;
use rusqlite::params;

use crate::AppState;
use crate::{commits, db, join, scanner, sessions};

pub async fn trigger(State(state): State<AppState>) -> impl IntoResponse {
    tokio::spawn(async move {
        if let Err(e) = run_refresh(state).await {
            tracing::error!("refresh failed: {e:?}");
        }
    });
    (StatusCode::ACCEPTED, "refresh started")
}

pub async fn run_refresh(state: AppState) -> anyhow::Result<()> {
    // Prevent overlapping refreshes.
    let _guard = match state.refresh_lock.try_lock() {
        Ok(g) => g,
        Err(_) => {
            let _ = state
                .progress_tx
                .send(status_fragment("refresh already in progress…"));
            return Ok(());
        }
    };

    let tx = state.progress_tx.clone();
    let _ = tx.send(status_fragment("starting refresh…"));

    let cwd = state.cwd.clone();
    let db_path = state.db_path.clone();
    let scan_depth = state.scan_depth;
    let commits_per_repo = state.commits_per_repo;

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<RefreshStats> {
        let conn = db::open(&db_path)?;
        db::wipe(&conn)?;

        // --- repos ---
        let discovered = scanner::scan(&cwd, scan_depth)?;
        let now = Utc::now().timestamp();
        let mut repo_id_by_path: Vec<(i64, PathBuf)> = Vec::with_capacity(discovered.len());
        {
            let tx_ins = conn.unchecked_transaction()?;
            for r in &discovered {
                let remote = commits::remote_info(&r.path);
                tx_ins.execute(
                    "INSERT OR IGNORE INTO repos
                     (path, name, discovered_at, remote_url, default_branch)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        r.path.to_string_lossy(),
                        r.name,
                        now,
                        remote.url,
                        remote.default_branch,
                    ],
                )?;
                let id: i64 = tx_ins.query_row(
                    "SELECT id FROM repos WHERE path = ?1",
                    params![r.path.to_string_lossy()],
                    |row| row.get(0),
                )?;
                repo_id_by_path.push((id, r.path.clone()));
            }
            tx_ins.commit()?;
        }
        let repos_n = repo_id_by_path.len();

        // --- sessions ---
        let Some(projects_dir) = sessions::default_projects_dir() else {
            // No Claude projects dir — still try commits before bailing.
            let commits_n = ingest_commits(&conn, &repo_id_by_path, commits_per_repo, &tx)?;
            return Ok(RefreshStats {
                repos: repos_n,
                sessions: 0,
                links: 0,
                commits: commits_n,
                skipped: 0,
            });
        };
        let files = sessions::list_session_files(&projects_dir)?;
        let total_files = files.len();

        let mut inserted = 0usize;
        let mut skipped = 0usize;
        let mut links = 0usize;

        let tx_ins = conn.unchecked_transaction()?;
        for (i, path) in files.iter().enumerate() {
            match sessions::parse_session_file(path) {
                Ok(Some(rec)) => {
                    let res = tx_ins.execute(
                        "INSERT OR IGNORE INTO sessions
                         (session_uuid, cwd, started_at, ended_at, message_count, summary,
                          source_file, duration_ms, input_tokens, output_tokens,
                          cache_read_tokens, cache_creation_tokens)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                        params![
                            rec.session_uuid,
                            rec.cwd,
                            rec.started_at,
                            rec.ended_at,
                            rec.message_count,
                            rec.summary,
                            rec.source_file,
                            rec.duration_ms,
                            rec.input_tokens,
                            rec.output_tokens,
                            rec.cache_read_tokens,
                            rec.cache_creation_tokens,
                        ],
                    )?;
                    if res == 0 {
                        // Duplicate UUID across files — skip.
                        skipped += 1;
                        continue;
                    }
                    inserted += 1;
                    let session_id = tx_ins.last_insert_rowid();

                    if let Some(cwd_str) = rec.cwd.as_deref() {
                        if let Some(repo_id) = join::best_repo_for_cwd(cwd_str, &repo_id_by_path) {
                            tx_ins.execute(
                                "INSERT OR IGNORE INTO session_repos (session_id, repo_id, match_type)
                                 VALUES (?1, ?2, 'cwd')",
                                params![session_id, repo_id],
                            )?;
                            links += 1;
                        }
                    }
                }
                Ok(None) => { skipped += 1; }
                Err(e) => {
                    tracing::debug!("parse error {}: {}", path.display(), e);
                    skipped += 1;
                }
            }
            // Emit progress every 50 files.
            if (i + 1) % 50 == 0 || i + 1 == total_files {
                let msg = format!("indexing sessions… {}/{}", i + 1, total_files);
                let _ = tx.send(status_fragment(&msg));
            }
        }
        tx_ins.commit()?;

        // --- commits (git log per repo) ---
        let commits_n = ingest_commits(&conn, &repo_id_by_path, commits_per_repo, &tx)?;

        // --- search index (rebuilt from the populated tables) ---
        db::rebuild_search_index(&conn)?;

        Ok(RefreshStats {
            repos: repos_n,
            sessions: inserted,
            links,
            commits: commits_n,
            skipped,
        })
    })
    .await?;

    let stats = match result {
        Ok(s) => {
            let msg = format!(
                "done — {} repos, {} sessions, {} links, {} commits ({} skipped). \
                 checking CI…",
                s.repos, s.sessions, s.links, s.commits, s.skipped,
            );
            let _ = state.progress_tx.send(status_fragment(&msg));
            s
        }
        Err(e) => {
            let _ = state
                .progress_tx
                .send(status_fragment(&format!("error: {e}")));
            return Ok(());
        }
    };

    // Second pass: CI/CD status + PR + issue counts. Separate from the main
    // blocking refresh so we can run `gh` subprocesses concurrently (tokio
    // spawn + spawn_blocking) rather than serializing N×network-latency
    // into the scan time. Runs after the main refresh has already surfaced
    // its counts, so the UI updates as soon as the offline data is ready
    // and the remote stuff fills in later.
    let ci_updated = ingest_ci_status(state.clone()).await;

    // Third pass: content-mention matching. Walks every session JSONL
    // looking for bare-word hits on known repo names. Separate because:
    // (a) it's heavy — N sessions × M repos of string-scanning; (b) it's
    // best-effort, so overcounting is OK and users see a "fuzzy" admission.
    let content_matches = ingest_content_mentions(state.clone()).await;

    *state.last_scan.lock().await = Some(Utc::now());
    let msg = format!(
        "done — {} repos, {} sessions, {} links, {} commits, {} remote, {} content-matches ({} skipped)",
        stats.repos,
        stats.sessions,
        stats.links,
        stats.commits,
        ci_updated,
        content_matches,
        stats.skipped,
    );
    let _ = state.progress_tx.send(status_fragment(&msg));
    // Sentinel: tells the dashboard's reload observer that fresh data is
    // available. OOB-swap outerHTML of the hidden #dashboard-reload-sentinel
    // span so a MutationObserver in dashboard-reload.js sees the
    // `data-reload-trigger` attribute and fires location.reload(). The span
    // only exists on the dashboard — detail pages have no swap target, so
    // they don't reload mid-read.
    let _ = state.progress_tx.send(
        r#"<span id="dashboard-reload-sentinel" hx-swap-oob="true" data-reload-trigger="1" style="display:none"></span>"#
            .to_string(),
    );
    Ok(())
}

/// Best-effort word-boundary content match: for each session file we've
/// indexed, read it once and add `session_repos` rows with
/// `match_type = 'content_mention'` for any repo whose name appears as a
/// bare word. Runs inside a single `spawn_blocking` — IO-heavy rather than
/// CPU-heavy, and serial is fine since a few dozen MB of JSONL parses fast.
async fn ingest_content_mentions(state: AppState) -> usize {
    let db_path = state.db_path.clone();
    let tx = state.progress_tx.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
        let conn = db::open(&db_path)?;
        // Needles: prefer the basename since that's what shows up in prose.
        let mut stmt = conn.prepare("SELECT id, name FROM repos")?;
        let needles: Vec<(i64, String)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let mut stmt = conn.prepare("SELECT id, source_file FROM sessions")?;
        let sessions: Vec<(i64, String)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let total = sessions.len();
        let mut inserted = 0usize;
        let tx_ins = conn.unchecked_transaction()?;
        for (i, (session_id, path)) in sessions.iter().enumerate() {
            let hits = crate::sessions::mentions_in_file(std::path::Path::new(path), &needles);
            for repo_id in hits {
                let n = tx_ins.execute(
                    "INSERT OR IGNORE INTO session_repos
                     (session_id, repo_id, match_type)
                     VALUES (?1, ?2, 'content_mention')",
                    rusqlite::params![session_id, repo_id],
                )?;
                inserted += n;
            }
            if (i + 1) % 25 == 0 || i + 1 == total {
                let _ = tx.send(status_fragment(&format!(
                    "scanning sessions for repo mentions… {}/{}",
                    i + 1,
                    total
                )));
            }
        }
        tx_ins.commit()?;
        Ok(inserted)
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    .unwrap_or(0)
}

/// Parallel `gh run list` across every repo with a GitHub remote + known
/// default branch. Returns how many rows we successfully updated. Each
/// subprocess runs in its own `spawn_blocking` so network latency overlaps;
/// a bounded `JoinSet` caps in-flight `gh` calls to avoid fork-bombing.
async fn ingest_ci_status(state: AppState) -> usize {
    // Re-probe `gh` on every refresh — the user may have installed it or
    // logged in since startup, and the banner should update.
    let health = tokio::task::spawn_blocking(commits::gh_health)
        .await
        .unwrap_or(commits::GhHealth::Missing);
    *state.gh_health.lock().await = health;
    if health != commits::GhHealth::Ok {
        return 0;
    }
    // Re-probe viewer login so it updates if the user switched accounts.
    let my_login = tokio::task::spawn_blocking(commits::my_gh_login)
        .await
        .ok()
        .flatten();
    *state.my_gh_login.lock().await = my_login.clone();
    let my_login = my_login.unwrap_or_default();

    // Pull the candidate list off one blocking DB read. Order by most-recent
    // commit (LEFT JOIN — repos with no commits sort to the bottom) so the
    // optional `LIMIT` keeps the activity-rich repos and drops dormant ones.
    // Repos beyond the cap get NULL remote-state fields this cycle; once
    // they see a fresh commit they bubble back into the window. Acceptable
    // because the schema is wiped on every refresh anyway, so "no remote
    // data" is the natural quiet state.
    let target_limit = state.remote_target_limit;
    let targets = {
        let db_path = state.db_path.clone();
        match tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<(i64, String, String)>> {
            let conn = db::open(&db_path)?;
            let sql = if target_limit == 0 {
                "SELECT r.id, r.remote_url, r.default_branch
                 FROM repos r
                 LEFT JOIN (
                     SELECT repo_id, MAX(timestamp) AS latest_ts
                     FROM commits GROUP BY repo_id
                 ) c ON c.repo_id = r.id
                 WHERE r.remote_url IS NOT NULL AND r.default_branch IS NOT NULL
                 ORDER BY COALESCE(c.latest_ts, 0) DESC"
                    .to_string()
            } else {
                format!(
                    "SELECT r.id, r.remote_url, r.default_branch
                     FROM repos r
                     LEFT JOIN (
                         SELECT repo_id, MAX(timestamp) AS latest_ts
                         FROM commits GROUP BY repo_id
                     ) c ON c.repo_id = r.id
                     WHERE r.remote_url IS NOT NULL AND r.default_branch IS NOT NULL
                     ORDER BY COALESCE(c.latest_ts, 0) DESC
                     LIMIT {target_limit}"
                )
            };
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
        {
            Ok(Ok(v)) => v,
            _ => return 0,
        }
    };

    // Filter to repos we actually know how to query (GitHub-hosted only).
    let jobs: Vec<_> = targets
        .into_iter()
        .filter_map(|(id, url, branch)| {
            commits::github_owner_repo(&url).map(|slug| (id, slug, branch))
        })
        .collect();
    let total = jobs.len();
    if total == 0 {
        return 0;
    }

    // Bounded concurrency: 8 concurrent `gh` processes is plenty without
    // hammering the rate limit or fork-bombing the laptop.
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(8));
    let mut set = tokio::task::JoinSet::new();
    for (id, slug, branch) in jobs {
        let sem = semaphore.clone();
        let login = my_login.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await.ok()?;
            // Fan out CI + PRs + issues in one blocking block so the
            // subprocess cost is sequential-per-repo but overlapping across
            // repos (bounded by the semaphore). PRs and issues share one
            // GraphQL call to halve the per-repo API spend.
            tokio::task::spawn_blocking(move || {
                let ci = commits::ci_status(&slug, &branch);
                let (prs, issues) = match commits::fetch_pr_and_issue_counts(&slug, &login) {
                    Some((p, i)) => (Some(p), Some(i)),
                    None => (None, None),
                };
                RemoteSnapshot {
                    id,
                    ci,
                    prs,
                    issues,
                }
            })
            .await
            .ok()
        });
    }

    // Collect + write in one sweep. Keeps the SQLite write lock short.
    let mut results: Vec<RemoteSnapshot> = Vec::with_capacity(total);
    let tx = state.progress_tx.clone();
    let mut done = 0usize;
    while let Some(res) = set.join_next().await {
        done += 1;
        if let Ok(Some(snap)) = res {
            results.push(snap);
        }
        if done.is_multiple_of(10) || done == total {
            let _ = tx.send(status_fragment(&format!("remote state… {done}/{total}")));
        }
    }

    let db_path = state.db_path.clone();
    let updated = tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
        let conn = db::open(&db_path)?;
        let tx_ins = conn.unchecked_transaction()?;
        let mut n = 0usize;
        for snap in results {
            let prs = snap.prs.unwrap_or_default();
            tx_ins.execute(
                "UPDATE repos
                 SET ci_status = COALESCE(?1, ci_status),
                     open_prs = ?2,
                     draft_prs = ?3,
                     prs_awaiting_my_review = ?4,
                     prs_mine_awaiting_review = ?5,
                     open_issues = COALESCE(?6, open_issues)
                 WHERE id = ?7",
                rusqlite::params![
                    snap.ci,
                    prs.open,
                    prs.draft,
                    prs.awaiting_my_review,
                    prs.mine_awaiting_review,
                    snap.issues,
                    snap.id,
                ],
            )?;
            n += 1;
        }
        tx_ins.commit()?;
        Ok(n)
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    .unwrap_or(0);
    updated
}

struct RemoteSnapshot {
    id: i64,
    ci: Option<String>,
    prs: Option<commits::PrCounts>,
    issues: Option<i64>,
}

struct RefreshStats {
    repos: usize,
    sessions: usize,
    links: usize,
    commits: usize,
    skipped: usize,
}

/// Run `git log` in every discovered repo and bulk-insert the results.
/// Progress events go out every 5 repos so the user sees movement on large
/// workspaces without drowning the socket in HTML fragments.
/// Also computes 30-day LOC churn in the same sweep (second git subprocess
/// per repo) and updates the `repos` row.
fn ingest_commits(
    conn: &rusqlite::Connection,
    repos: &[(i64, PathBuf)],
    limit_per_repo: usize,
    tx: &tokio::sync::broadcast::Sender<String>,
) -> anyhow::Result<usize> {
    let total_repos = repos.len();
    let mut total_commits = 0usize;
    let churn_cutoff = chrono::Utc::now().timestamp() - 30 * 86_400;
    let tx_ins = conn.unchecked_transaction()?;
    for (i, (repo_id, repo_path)) in repos.iter().enumerate() {
        match commits::scan(repo_path, limit_per_repo) {
            Ok(records) => {
                for rec in &records {
                    tx_ins.execute(
                        "INSERT OR IGNORE INTO commits
                         (repo_id, sha, author_name, author_email, timestamp, subject)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        rusqlite::params![
                            repo_id,
                            rec.sha,
                            rec.author_name,
                            rec.author_email,
                            rec.timestamp,
                            rec.subject,
                        ],
                    )?;
                }
                total_commits += records.len();
            }
            Err(e) => {
                tracing::debug!("commits scan failed in {}: {e}", repo_path.display());
            }
        }
        // Per-file change records for the last 30d — source of truth for
        // both the scalar churn total and the hotspot query on repo detail.
        let file_changes = commits::file_changes_since(repo_path, churn_cutoff);
        let churn: i64 = file_changes
            .iter()
            .map(|fc| fc.additions + fc.deletions)
            .sum();
        for fc in &file_changes {
            tx_ins.execute(
                "INSERT INTO file_changes
                 (repo_id, sha, file_path, additions, deletions, author_email, timestamp)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    repo_id,
                    fc.sha,
                    fc.file_path,
                    fc.additions,
                    fc.deletions,
                    fc.author_email,
                    fc.timestamp,
                ],
            )?;
        }
        // Cap per-repo at 50 paths: enough for the dashboard sample, small
        // enough that a pathological refactor can't blow up the DB.
        let snap = commits::worktree_snapshot(repo_path, 50);
        let local = commits::local_state(repo_path);
        tx_ins.execute(
            "UPDATE repos
             SET loc_churn_30d = ?1, untracked_files = ?2, modified_files = ?3,
                 commits_ahead = ?4, commits_behind = ?5, stash_count = ?6,
                 head_ref = ?7, in_progress_op = ?8
             WHERE id = ?9",
            rusqlite::params![
                churn,
                snap.total_untracked,
                snap.total_modified,
                local.commits_ahead,
                local.commits_behind,
                local.stash_count,
                local.head_ref,
                local.in_progress_op,
                repo_id,
            ],
        )?;
        for f in &snap.files {
            tx_ins.execute(
                "INSERT INTO uncommitted_files (repo_id, path, kind) VALUES (?1, ?2, ?3)",
                rusqlite::params![repo_id, f.path, f.kind.as_str()],
            )?;
        }
        if (i + 1) % 5 == 0 || i + 1 == total_repos {
            let _ = tx.send(status_fragment(&format!(
                "indexing commits + churn… {}/{}",
                i + 1,
                total_repos
            )));
        }
    }
    tx_ins.commit()?;
    Ok(total_commits)
}

/// HTML fragment that HTMX will swap into #scan-status via out-of-band swap.
/// Carries the same class string as the initial template render — without
/// it, every status update strips the banner's styling.
fn status_fragment(text: &str) -> String {
    // Escape angle-brackets minimally — inputs are our own templated strings.
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        r#"<div id="scan-status" hx-swap-oob="true" class="{}">{escaped}</div>"#,
        crate::routes::templates::SCAN_STATUS
    )
}
