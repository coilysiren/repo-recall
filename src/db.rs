use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

pub fn init(path: &Path) -> Result<()> {
    let conn = open(path)?;
    conn.execute_batch(
        r#"
        DROP TABLE IF EXISTS search_idx;
        DROP TABLE IF EXISTS file_changes;
        DROP TABLE IF EXISTS uncommitted_files;
        DROP TABLE IF EXISTS commits;
        DROP TABLE IF EXISTS session_repos;
        DROP TABLE IF EXISTS sessions;
        DROP TABLE IF EXISTS repos;

        CREATE TABLE repos (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL,
            discovered_at INTEGER NOT NULL,
            -- Normalized browsable base, e.g. `https://github.com/owner/repo`.
            -- Null when the repo has no `origin` or the URL wasn't parseable.
            remote_url TEXT,
            -- Short branch name of `refs/remotes/origin/HEAD`, e.g. `main`.
            -- Null when there's no origin or origin/HEAD is unset.
            default_branch TEXT,
            -- Sum of (additions + deletions) from `git log --numstat` across
            -- commits in the last 30 days. A rough "how much code moved"
            -- signal, surfaced alongside the commit-count pill.
            loc_churn_30d INTEGER NOT NULL DEFAULT 0,
            -- Working-tree state (from `git status --porcelain=v1 -uall` at
            -- refresh time). Untracked = `??`-prefixed lines; modified =
            -- everything else (staged + unstaged, including renames).
            untracked_files INTEGER NOT NULL DEFAULT 0,
            modified_files INTEGER NOT NULL DEFAULT 0,
            -- Latest default-branch CI run outcome from `gh run list`:
            -- 'success' | 'failure' | 'running' | 'pending' | NULL (unknown /
            -- no `gh` / no GitHub remote / not yet checked).
            ci_status TEXT,
            -- Commits the local branch is ahead / behind its upstream.
            -- 0 when no upstream is configured (rather than NULL — simpler).
            commits_ahead INTEGER NOT NULL DEFAULT 0,
            commits_behind INTEGER NOT NULL DEFAULT 0,
            -- `git stash list` count. Stash-heavy repos are a useful signal.
            stash_count INTEGER NOT NULL DEFAULT 0,
            -- Short branch name, or literal "detached" for a detached HEAD.
            -- NULL when HEAD isn't resolvable (brand new empty repo, etc).
            head_ref TEXT,
            -- One of 'rebase' | 'merge' | 'cherry-pick' | 'bisect' | 'revert'
            -- when the repo is mid-operation (detected by `.git/` state
            -- files). NULL when clean. Any non-NULL is action-required.
            in_progress_op TEXT,
            -- GitHub remote-state snapshot (filled by `gh pr list` /
            -- `gh issue list` in the parallel remote pass). Zero when we
            -- can't query (no GH remote / `gh` missing / errored).
            open_prs INTEGER NOT NULL DEFAULT 0,
            draft_prs INTEGER NOT NULL DEFAULT 0,
            open_issues INTEGER NOT NULL DEFAULT 0,
            prs_awaiting_my_review INTEGER NOT NULL DEFAULT 0,
            prs_mine_awaiting_review INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE sessions (
            id INTEGER PRIMARY KEY,
            session_uuid TEXT NOT NULL UNIQUE,
            cwd TEXT,
            started_at INTEGER,
            ended_at INTEGER,
            message_count INTEGER NOT NULL DEFAULT 0,
            summary TEXT,
            source_file TEXT NOT NULL,
            -- Wall-clock span of the session in milliseconds (ended - started).
            -- NULL when we only saw one timestamp.
            duration_ms INTEGER,
            -- Token usage summed across every assistant turn's `message.usage`
            -- in the JSONL. Zero when the usage blocks aren't populated (older
            -- sessions / malformed lines).
            input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens INTEGER NOT NULL DEFAULT 0,
            cache_creation_tokens INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE session_repos (
            session_id INTEGER NOT NULL REFERENCES sessions(id),
            repo_id INTEGER NOT NULL REFERENCES repos(id),
            match_type TEXT NOT NULL,
            PRIMARY KEY (session_id, repo_id, match_type)
        );

        CREATE TABLE commits (
            id INTEGER PRIMARY KEY,
            repo_id INTEGER NOT NULL REFERENCES repos(id),
            sha TEXT NOT NULL,
            author_name TEXT NOT NULL,
            author_email TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            subject TEXT NOT NULL,
            UNIQUE(repo_id, sha)
        );

        -- A capped sample of file paths from `git status --porcelain` per
        -- repo. We store *some* individual paths (for the dashboard's
        -- uncommitted-files column) and rely on the full counts living on
        -- the `repos` row for the score + pill. This is per-refresh state,
        -- not a long-term log.
        CREATE TABLE uncommitted_files (
            id INTEGER PRIMARY KEY,
            repo_id INTEGER NOT NULL REFERENCES repos(id),
            path TEXT NOT NULL,
            -- 'untracked' (`??`) or 'modified' (everything else).
            kind TEXT NOT NULL
        );

        CREATE INDEX idx_sessions_started_at ON sessions(started_at DESC);
        CREATE INDEX idx_session_repos_repo ON session_repos(repo_id);
        CREATE INDEX idx_session_repos_session ON session_repos(session_id);
        CREATE INDEX idx_commits_repo_ts ON commits(repo_id, timestamp DESC);
        CREATE INDEX idx_commits_ts ON commits(timestamp DESC);
        CREATE INDEX idx_uncommitted_repo ON uncommitted_files(repo_id);

        -- One row per (commit, file) pair from `git log --numstat`. Holds
        -- the raw per-file churn so we can derive hotspots, per-author
        -- churn, etc. without re-running git. Replaces the old aggregate-
        -- only `loc_churn_30d` column as the source of truth (that column
        -- is still materialised for quick score lookups).
        CREATE TABLE file_changes (
            id INTEGER PRIMARY KEY,
            repo_id INTEGER NOT NULL REFERENCES repos(id),
            sha TEXT NOT NULL,
            file_path TEXT NOT NULL,
            additions INTEGER NOT NULL,
            deletions INTEGER NOT NULL,
            author_email TEXT NOT NULL,
            timestamp INTEGER NOT NULL
        );
        CREATE INDEX idx_file_changes_repo_ts ON file_changes(repo_id, timestamp DESC);
        CREATE INDEX idx_file_changes_path ON file_changes(repo_id, file_path);

        -- FTS5 virtual table indexing the searchable text across every
        -- domain entity we have. `kind` is 'repo' | 'session' | 'commit'
        -- and `ref_id` is the source row's primary key (UNINDEXED so it's
        -- stored without tokenisation overhead). Populated at the end of
        -- every refresh.
        CREATE VIRTUAL TABLE IF NOT EXISTS search_idx USING fts5(
            kind UNINDEXED,
            ref_id UNINDEXED,
            text,
            tokenize = 'porter unicode61 remove_diacritics 1'
        );
        "#,
    )?;
    Ok(())
}

pub fn wipe(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM search_idx; \
         DELETE FROM file_changes; \
         DELETE FROM uncommitted_files; \
         DELETE FROM commits; \
         DELETE FROM session_repos; \
         DELETE FROM sessions; \
         DELETE FROM repos;",
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct FileHotspot {
    pub file_path: String,
    pub churn: i64,
    pub commits: i64,
    pub authors: i64,
}

/// Top-N most-edited files in a repo over the given time window. `churn` is
/// `SUM(additions + deletions)`, `commits` is how many commits touched the
/// file, `authors` is distinct authors who touched it — three signals from
/// one query so the repo detail page can show all of them at once.
pub fn file_hotspots(
    conn: &Connection,
    repo_id: i64,
    since_ts: i64,
    limit: i64,
) -> Result<Vec<FileHotspot>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT file_path,
               SUM(additions + deletions) AS churn,
               COUNT(*) AS commits,
               COUNT(DISTINCT author_email) AS authors
        FROM file_changes
        WHERE repo_id = ?1 AND timestamp >= ?2
        GROUP BY file_path
        ORDER BY churn DESC, commits DESC
        LIMIT ?3
        "#,
    )?;
    let rows = stmt.query_map(rusqlite::params![repo_id, since_ts, limit], |row| {
        Ok(FileHotspot {
            file_path: row.get(0)?,
            churn: row.get(1)?,
            commits: row.get(2)?,
            authors: row.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Populate the full-text index from every entity table in one sweep. Call
/// this as the final step of a refresh, after every insert has landed.
pub fn rebuild_search_index(conn: &Connection) -> Result<()> {
    // Separate INSERT per source so the text expression stays readable.
    conn.execute_batch(
        r#"
        INSERT INTO search_idx (kind, ref_id, text)
          SELECT 'repo', id, COALESCE(name, '') || ' ' || COALESCE(path, '') FROM repos;
        INSERT INTO search_idx (kind, ref_id, text)
          SELECT 'session', id, COALESCE(summary, '') FROM sessions WHERE summary IS NOT NULL;
        INSERT INTO search_idx (kind, ref_id, text)
          SELECT 'commit', id, COALESCE(subject, '') FROM commits;
        "#,
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub kind: String,
    pub ref_id: i64,
    pub text: String,
    pub extra: Option<String>,
}

/// Run a user query against the FTS index. Groups results by kind on the
/// caller side. Uses `snippet()` to produce a highlighted excerpt.
pub fn search(conn: &Connection, query: &str, limit: i64) -> Result<Vec<SearchHit>> {
    // FTS5 treats some characters specially ('"', ':', etc). For a
    // user-facing search bar, quote the whole query as a phrase. Users who
    // want AND/OR can still drop the quotes by embedding them themselves.
    let q = format!("\"{}\"", query.replace('"', "\"\""));
    let mut stmt = conn.prepare(
        r#"
        SELECT kind, ref_id, text, rank
        FROM search_idx
        WHERE search_idx MATCH ?1
        ORDER BY rank
        LIMIT ?2
        "#,
    )?;
    let rows = stmt.query_map(rusqlite::params![q, limit], |row| {
        Ok(SearchHit {
            kind: row.get(0)?,
            ref_id: row.get(1)?,
            text: row.get(2)?,
            extra: None,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct Repo {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub session_count: i64,
    /// Commits in the last 30 days. Surfaced as a repo-row pill so quiet
    /// repos (no sessions, no recent commits) fade; active repos surface.
    pub commits_30d: i64,
    pub loc_churn_30d: i64,
    pub untracked_files: i64,
    pub modified_files: i64,
    /// Distinct commit author emails in the last 30 days. Computed via a
    /// subquery from the `commits` table rather than stored — the data is
    /// already there.
    pub authors_30d: i64,
    pub ci_status: Option<String>,
    pub commits_ahead: i64,
    pub commits_behind: i64,
    pub stash_count: i64,
    pub head_ref: Option<String>,
    pub in_progress_op: Option<String>,
    pub open_prs: i64,
    pub draft_prs: i64,
    pub open_issues: i64,
    pub prs_awaiting_my_review: i64,
    pub prs_mine_awaiting_review: i64,
    pub remote_url: Option<String>,
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: i64,
    pub session_uuid: String,
    pub cwd: Option<String>,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub message_count: i64,
    pub summary: Option<String>,
    pub source_file: String,
    pub duration_ms: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
}

#[derive(Debug, Clone)]
pub struct SessionWithRepos {
    pub session: Session,
    pub repos: Vec<(i64, String, String)>, // id, name, path
}

#[derive(Debug, Clone)]
pub struct Commit {
    pub id: i64,
    pub repo_id: i64,
    pub sha: String,
    pub author_name: String,
    pub author_email: String,
    pub timestamp: i64,
    pub subject: String,
}

#[derive(Debug, Clone)]
pub struct CommitWithRepo {
    pub commit: Commit,
    pub repo_id: i64,
    pub repo_name: String,
    pub repo_path: String,
    /// Normalized remote base (`https://github.com/owner/repo`). Present only
    /// when the repo has a recognisable origin. Lets callers build
    /// `/commit/<sha>` links without another DB round-trip.
    pub repo_remote_url: Option<String>,
}

pub fn list_repos_with_counts(conn: &Connection) -> Result<Vec<Repo>> {
    // 30 days ago as unix seconds. Subqueries (vs. joins) keep the two counts
    // independent so neither multiplies the other's row count.
    let cutoff_30d = chrono::Utc::now().timestamp() - 30 * 86_400;
    let mut stmt = conn.prepare(
        r#"
        SELECT r.id, r.path, r.name, r.remote_url, r.default_branch,
               r.loc_churn_30d, r.untracked_files, r.modified_files, r.ci_status,
               r.commits_ahead, r.commits_behind, r.stash_count,
               r.head_ref, r.in_progress_op,
               r.open_prs, r.draft_prs, r.open_issues,
               r.prs_awaiting_my_review, r.prs_mine_awaiting_review,
               (SELECT COUNT(*) FROM session_repos sr WHERE sr.repo_id = r.id) AS session_count,
               (SELECT COUNT(*) FROM commits c
                WHERE c.repo_id = r.id AND c.timestamp >= ?1) AS commits_30d,
               (SELECT COUNT(DISTINCT c.author_email) FROM commits c
                WHERE c.repo_id = r.id AND c.timestamp >= ?1) AS authors_30d
        FROM repos r
        -- Intentionally no ORDER BY: callers re-sort via
        -- `activity::sort` so the ranking stays consistent across however
        -- many activity dimensions we currently have wired up.
        "#,
    )?;
    let rows = stmt.query_map([cutoff_30d], |row| {
        Ok(Repo {
            id: row.get(0)?,
            path: row.get(1)?,
            name: row.get(2)?,
            remote_url: row.get(3)?,
            default_branch: row.get(4)?,
            loc_churn_30d: row.get(5)?,
            untracked_files: row.get(6)?,
            modified_files: row.get(7)?,
            ci_status: row.get(8)?,
            commits_ahead: row.get(9)?,
            commits_behind: row.get(10)?,
            stash_count: row.get(11)?,
            head_ref: row.get(12)?,
            in_progress_op: row.get(13)?,
            open_prs: row.get(14)?,
            draft_prs: row.get(15)?,
            open_issues: row.get(16)?,
            prs_awaiting_my_review: row.get(17)?,
            prs_mine_awaiting_review: row.get(18)?,
            session_count: row.get(19)?,
            commits_30d: row.get(20)?,
            authors_30d: row.get(21)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn get_repo(conn: &Connection, id: i64) -> Result<Option<Repo>> {
    let cutoff_30d = chrono::Utc::now().timestamp() - 30 * 86_400;
    let mut stmt = conn.prepare(
        r#"
        SELECT r.id, r.path, r.name, r.remote_url, r.default_branch,
               r.loc_churn_30d, r.untracked_files, r.modified_files, r.ci_status,
               r.commits_ahead, r.commits_behind, r.stash_count,
               r.head_ref, r.in_progress_op,
               r.open_prs, r.draft_prs, r.open_issues,
               r.prs_awaiting_my_review, r.prs_mine_awaiting_review,
               (SELECT COUNT(*) FROM session_repos sr WHERE sr.repo_id = r.id) AS session_count,
               (SELECT COUNT(*) FROM commits c
                WHERE c.repo_id = r.id AND c.timestamp >= ?2) AS commits_30d,
               (SELECT COUNT(DISTINCT c.author_email) FROM commits c
                WHERE c.repo_id = r.id AND c.timestamp >= ?2) AS authors_30d
        FROM repos r
        WHERE r.id = ?1
        "#,
    )?;
    let mut rows = stmt.query_map(rusqlite::params![id, cutoff_30d], |row| {
        Ok(Repo {
            id: row.get(0)?,
            path: row.get(1)?,
            name: row.get(2)?,
            remote_url: row.get(3)?,
            default_branch: row.get(4)?,
            loc_churn_30d: row.get(5)?,
            untracked_files: row.get(6)?,
            modified_files: row.get(7)?,
            ci_status: row.get(8)?,
            commits_ahead: row.get(9)?,
            commits_behind: row.get(10)?,
            stash_count: row.get(11)?,
            head_ref: row.get(12)?,
            in_progress_op: row.get(13)?,
            open_prs: row.get(14)?,
            draft_prs: row.get(15)?,
            open_issues: row.get(16)?,
            prs_awaiting_my_review: row.get(17)?,
            prs_mine_awaiting_review: row.get(18)?,
            session_count: row.get(19)?,
            commits_30d: row.get(20)?,
            authors_30d: row.get(21)?,
        })
    })?;
    Ok(rows.next().transpose()?)
}

pub fn sessions_for_repo(conn: &Connection, repo_id: i64) -> Result<Vec<Session>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT DISTINCT s.id, s.session_uuid, s.cwd, s.started_at, s.ended_at,
                        s.message_count, s.summary, s.source_file,
                        s.duration_ms, s.input_tokens, s.output_tokens,
                        s.cache_read_tokens, s.cache_creation_tokens
        FROM sessions s
        JOIN session_repos sr ON sr.session_id = s.id
        WHERE sr.repo_id = ?1
        ORDER BY s.started_at DESC NULLS LAST
        "#,
    )?;
    let rows = stmt.query_map([repo_id], map_session)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn recent_sessions(conn: &Connection, limit: i64) -> Result<Vec<SessionWithRepos>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT s.id, s.session_uuid, s.cwd, s.started_at, s.ended_at,
               s.message_count, s.summary, s.source_file,
               s.duration_ms, s.input_tokens, s.output_tokens,
               s.cache_read_tokens, s.cache_creation_tokens
        FROM sessions s
        ORDER BY s.started_at DESC NULLS LAST
        LIMIT ?1
        "#,
    )?;
    let rows = stmt.query_map([limit], map_session)?;
    let mut sessions = Vec::new();
    for r in rows {
        sessions.push(r?);
    }
    let mut out = Vec::with_capacity(sessions.len());
    for s in sessions {
        let repos = repos_for_session(conn, s.id)?;
        out.push(SessionWithRepos { session: s, repos });
    }
    Ok(out)
}

pub fn get_session(conn: &Connection, id: i64) -> Result<Option<SessionWithRepos>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT id, session_uuid, cwd, started_at, ended_at,
               message_count, summary, source_file,
               duration_ms, input_tokens, output_tokens,
               cache_read_tokens, cache_creation_tokens
        FROM sessions
        WHERE id = ?1
        "#,
    )?;
    let mut rows = stmt.query_map([id], map_session)?;
    match rows.next() {
        None => Ok(None),
        Some(r) => {
            let s = r?;
            let repos = repos_for_session(conn, s.id)?;
            Ok(Some(SessionWithRepos { session: s, repos }))
        }
    }
}

pub fn repos_for_session(conn: &Connection, session_id: i64) -> Result<Vec<(i64, String, String)>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT DISTINCT r.id, r.name, r.path
        FROM repos r
        JOIN session_repos sr ON sr.repo_id = r.id
        WHERE sr.session_id = ?1
        ORDER BY r.name COLLATE NOCASE ASC
        "#,
    )?;
    let rows = stmt.query_map([session_id], |row| {
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
}

/// Partitioned version: returns `(cwd_matches, content_matches)` for the
/// session detail page so it can render each with its own tone + admission.
#[allow(clippy::type_complexity)]
pub fn repos_for_session_by_match(
    conn: &Connection,
    session_id: i64,
) -> Result<(Vec<(i64, String, String)>, Vec<(i64, String, String)>)> {
    let mut stmt = conn.prepare(
        r#"
        SELECT r.id, r.name, r.path, sr.match_type
        FROM repos r
        JOIN session_repos sr ON sr.repo_id = r.id
        WHERE sr.session_id = ?1
        ORDER BY sr.match_type ASC, r.name COLLATE NOCASE ASC
        "#,
    )?;
    let rows = stmt.query_map([session_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut cwd = Vec::new();
    let mut content = Vec::new();
    for r in rows {
        let (id, name, path, match_type) = r?;
        match match_type.as_str() {
            "content_mention" => content.push((id, name, path)),
            _ => cwd.push((id, name, path)),
        }
    }
    Ok((cwd, content))
}

/// Earliest `started_at` across all indexed sessions. `None` if no sessions
/// have a timestamp. Used to surface "how far back does history go" on the
/// dashboard — the answer is whatever Claude Code happens to have kept on
/// disk, since we don't cap our own scan.
pub fn earliest_session_ts(conn: &Connection) -> Result<Option<i64>> {
    let ts: Option<i64> = conn.query_row(
        "SELECT MIN(started_at) FROM sessions WHERE started_at IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    Ok(ts)
}

/// One repo's uncommitted-work summary — total counts + a capped sample of
/// individual paths. Used by the dashboard's "action required" column so we
/// group dirt *by repo* instead of emitting a flat list that scrolls past.
#[derive(Debug, Clone)]
pub struct UncommittedGroup {
    pub repo_id: i64,
    pub repo_name: String,
    pub repo_path: String,
    pub repo_remote_url: Option<String>,
    /// Total untracked + modified for this repo (full count, not sample).
    pub total: i64,
    /// A bounded sample of `(path, kind)` pairs for display.
    pub sample: Vec<(String, String)>,
}

pub fn uncommitted_by_repo(
    conn: &Connection,
    max_repos: i64,
    files_per_repo: usize,
) -> Result<Vec<UncommittedGroup>> {
    // Pull every captured file row for repos that actually have uncommitted
    // work, joined to repo metadata. Order within a repo: modified first
    // (tracked, usually the more interesting kind), then untracked.
    let mut stmt = conn.prepare(
        r#"
        SELECT r.id, r.name, r.path, r.remote_url,
               r.untracked_files + r.modified_files AS total,
               u.path, u.kind
        FROM repos r
        JOIN uncommitted_files u ON u.repo_id = r.id
        WHERE (r.untracked_files + r.modified_files) > 0
        ORDER BY total DESC, r.name COLLATE NOCASE ASC,
                 u.kind DESC, u.path ASC
        "#,
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;

    let mut groups: Vec<UncommittedGroup> = Vec::new();
    for row in rows {
        let (repo_id, name, path, url, total, file_path, kind) = row?;
        if let Some(last) = groups.last_mut() {
            if last.repo_id == repo_id {
                if last.sample.len() < files_per_repo {
                    last.sample.push((file_path, kind));
                }
                continue;
            }
        }
        if groups.len() as i64 >= max_repos {
            break;
        }
        groups.push(UncommittedGroup {
            repo_id,
            repo_name: name,
            repo_path: path,
            repo_remote_url: url,
            total,
            sample: vec![(file_path, kind)],
        });
    }
    Ok(groups)
}

#[derive(Debug, Clone)]
pub struct CiFailure {
    pub repo_id: i64,
    pub repo_name: String,
    pub repo_path: String,
    pub remote_url: Option<String>,
    pub default_branch: Option<String>,
}

pub fn failing_ci_repos(conn: &Connection) -> Result<Vec<CiFailure>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT id, name, path, remote_url, default_branch
        FROM repos
        WHERE ci_status = 'failure'
        ORDER BY name COLLATE NOCASE ASC
        "#,
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(CiFailure {
            repo_id: row.get(0)?,
            repo_name: row.get(1)?,
            repo_path: row.get(2)?,
            remote_url: row.get(3)?,
            default_branch: row.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn counts(conn: &Connection) -> Result<(i64, i64, i64, i64)> {
    let repos: i64 = conn.query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0))?;
    let sessions: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
    let links: i64 = conn.query_row("SELECT COUNT(*) FROM session_repos", [], |r| r.get(0))?;
    let commits: i64 = conn.query_row("SELECT COUNT(*) FROM commits", [], |r| r.get(0))?;
    Ok((repos, sessions, links, commits))
}

pub fn recent_commits(
    conn: &Connection,
    limit: i64,
    author_filter: Option<&str>,
) -> Result<Vec<CommitWithRepo>> {
    // Single SQL shape with an always-matches fallback for the email, so we
    // don't have to juggle two parameter lists with different lifetimes.
    let mut stmt = conn.prepare(
        r#"
        SELECT c.id, c.repo_id, c.sha, c.author_name, c.author_email,
               c.timestamp, c.subject,
               r.name, r.path, r.remote_url
        FROM commits c
        JOIN repos r ON r.id = c.repo_id
        WHERE (?2 IS NULL OR c.author_email = ?2)
        ORDER BY c.timestamp DESC
        LIMIT ?1
        "#,
    )?;
    let rows = stmt.query_map(rusqlite::params![limit, author_filter], |row| {
        Ok(CommitWithRepo {
            commit: Commit {
                id: row.get(0)?,
                repo_id: row.get(1)?,
                sha: row.get(2)?,
                author_name: row.get(3)?,
                author_email: row.get(4)?,
                timestamp: row.get(5)?,
                subject: row.get(6)?,
            },
            repo_id: row.get(1)?,
            repo_name: row.get(7)?,
            repo_path: row.get(8)?,
            repo_remote_url: row.get(9)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn commits_for_repo(conn: &Connection, repo_id: i64, limit: i64) -> Result<Vec<Commit>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT id, repo_id, sha, author_name, author_email, timestamp, subject
        FROM commits
        WHERE repo_id = ?1
        ORDER BY timestamp DESC
        LIMIT ?2
        "#,
    )?;
    let rows = stmt.query_map(rusqlite::params![repo_id, limit], |row| {
        Ok(Commit {
            id: row.get(0)?,
            repo_id: row.get(1)?,
            sha: row.get(2)?,
            author_name: row.get(3)?,
            author_email: row.get(4)?,
            timestamp: row.get(5)?,
            subject: row.get(6)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn map_session(row: &rusqlite::Row) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        session_uuid: row.get(1)?,
        cwd: row.get(2)?,
        started_at: row.get(3)?,
        ended_at: row.get(4)?,
        message_count: row.get(5)?,
        summary: row.get(6)?,
        source_file: row.get(7)?,
        duration_ms: row.get(8)?,
        input_tokens: row.get(9)?,
        output_tokens: row.get(10)?,
        cache_read_tokens: row.get(11)?,
        cache_creation_tokens: row.get(12)?,
    })
}
