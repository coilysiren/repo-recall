# Local Dev Dashboard — MVP Spec

## Goal

A local web app that indexes Claude Code session history and joins sessions to git repos discovered on disk. Answers the question: *"what Claude Code sessions have I had about this repo?"* — and, inversely, *"what repos has this session touched?"*

Everything else on the original feature list (git status, GitHub issues/PRs, CI, healthchecks, menu bar, MCP server) is out of scope for MVP but should not be architecturally precluded.

## MVP Scope

**In:**
- Repo discovery (lazy: scan cwd up to N levels deep for `.git` directories)
- Multiple data sources keyed to the same repo set (session history is the first, git log is the second; 3+ more are planned)
- Claude Code session indexing into SQLite
- Git log ingestion per repo (subprocess to `git log --all --no-merges`, capped per-repo)
- Join sessions to repos via session `cwd`
- Web UI: list repos, list sessions, list commits, show session↔repo relationships
- WebSocket-based updates (see rationale below)
- Page-load refresh (no background workers, no file watchers, no polling)

**Out (deferred, keep room for):**
- Git working-tree state (untracked, diffs, branches, ahead/behind, stashes)
- GitHub API integration (issues, PRs, CI status)
- Ops/healthcheck monitoring
- Menu bar companion app
- MCP server exposing the same data
- Richer session↔repo join signals (file path resolution, branch name matching)
- Background refresh, file watchers, scheduled scans
- Multi-user, auth, deployment — this is strictly local

## Architecture

### Data flow

```
Claude Code sessions (~/.claude/projects/**/*.jsonl)
                │
                ▼
          [Session parser]
                │
                ▼
┌─────────────────────────────────┐
│  SQLite (throwaway cache)       │
│  - repos                        │
│  - sessions                     │
│  - session_repos (join)         │
└─────────────────────────────────┘
                ▲
                │
         [Repo scanner]
                │
                ▼
    cwd + N levels deep for .git dirs
                │
                ▼
          [Web server]
                │
                ▼
      HTMX UI + WebSocket
```

### Key principles

1. **SQLite is a cache, not a database.** On refresh, wipe and rebuild. No migrations, no upserts, no stale-state bugs. If the schema needs to change, delete the file.
2. **Discovery is lazy.** The server scans from its own cwd, max depth N (default 4, via `REPO_RECALL_DEPTH`). No config file, no root directory setting. Run it where you want it to look.
3. **Sessions are the primary entity.** Repos are cheap to rediscover; session history is what we're actually indexing.
4. **Join logic is pluggable.** MVP uses one signal (session cwd matches repo path). More signals get added as additional rows in `session_repos` with a `match_type` column.

### WebSocket rationale

Use case for MVP: push progress updates during initial scan/index (which may take a few seconds for users with lots of session history), and push updates when a manual refresh completes. HTMX has first-class WebSocket support via the `hx-ext="ws"` extension — the server sends HTML fragments over the socket and HTMX swaps them into the DOM. No JSON API, no client-side state.

For MVP, one WebSocket endpoint is enough: `/ws` broadcasts scan/index progress and completion events. Future features (live CI status, live git status) can reuse the same socket or add dedicated ones. A second dedicated `/livereload` socket is used purely for dev ergonomics (browser auto-refresh on rebuild).

## Data Model

```sql
CREATE TABLE repos (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,          -- absolute path
    name TEXT NOT NULL,                 -- basename of path
    discovered_at INTEGER NOT NULL      -- unix timestamp
);

CREATE TABLE sessions (
    id INTEGER PRIMARY KEY,
    session_uuid TEXT NOT NULL UNIQUE,  -- from Claude Code session file
    cwd TEXT,                           -- working directory recorded in session
    started_at INTEGER,                 -- unix timestamp, first message
    ended_at INTEGER,                   -- unix timestamp, last message
    message_count INTEGER NOT NULL DEFAULT 0,
    summary TEXT,                       -- first user message, truncated, for display
    source_file TEXT NOT NULL           -- path to source JSONL, for debugging
);

CREATE TABLE session_repos (
    session_id INTEGER NOT NULL REFERENCES sessions(id),
    repo_id INTEGER NOT NULL REFERENCES repos(id),
    match_type TEXT NOT NULL,           -- 'cwd' for MVP; future: 'file_path', 'branch_name', etc.
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

CREATE INDEX idx_sessions_started_at ON sessions(started_at DESC);
CREATE INDEX idx_session_repos_repo ON session_repos(repo_id);
CREATE INDEX idx_session_repos_session ON session_repos(session_id);
```

Notes:
- `session_repos.match_type` is the extension point. MVP only writes `'cwd'`. Adding new join signals = new rows with new match_type values. Queries can filter or union across types.
- `summary` is for display only. Truncate to ~200 chars. First user message is usually descriptive enough.
- No foreign key cascades — we wipe and rebuild on refresh anyway.

## Repo Discovery

Algorithm:
1. Start at the server's current working directory.
2. Walk up to N levels deep.
3. For each directory found, check if it contains a `.git` subdirectory (directory OR file — git worktrees use a file).
4. If yes, record as a repo. Stop descending into that tree (don't look for repos inside repos, e.g. vendored submodules).
5. Skip hidden directories (names starting with `.`) except don't skip the starting directory itself if it's hidden.
6. Skip `node_modules`, `target`, `dist`, `build`, `.venv`, `venv` — common heavy directories that won't contain repos you care about.

Depth semantics: depth 0 = cwd itself, depth 1 = immediate children, depth 2 = grandchildren, etc. So at depth 4, `~/code/client-work/projectA/.git` is found and `~/code/client-work/a/b/c/d/.git` is the deepest layer we'd still reach.

Record absolute paths. Canonicalize (resolve symlinks) so the same repo reached via different paths doesn't get double-indexed.

## Session Indexing

Claude Code stores sessions on disk locally at `~/.claude/projects/<encoded-project-dir>/*.jsonl`. Each line is an independent JSON record. Record shapes include:

- `queue-operation` lines (enqueue/dequeue book-keeping)
- `user` / `assistant` message lines, with `sessionId`, `timestamp`, `cwd`, and a `message.content` that may be a string or an array of typed blocks
- Occasional unknown shapes — the parser skips these with a debug log rather than failing

For each session file, extract:

- A stable session identifier (`sessionId`, or fall back to the filename stem)
- Working directory (`cwd`) at session start, if recorded
- First and last timestamps (seen across all lines)
- Message count (`user` + `assistant` records only)
- A short summary (first user message, truncated to 200 chars)

Insert into `sessions`. If a `cwd` is present and matches (or is inside) a known repo path, insert a `session_repos` row with `match_type = 'cwd'`.

**Matching logic for cwd→repo:**

A session's cwd matches a repo if the cwd is equal to the repo's path OR is a descendant of it. Canonicalize both sides before comparing. If a cwd matches multiple repos (nested repos — rare but possible), pick the most specific (longest) match.

**Error handling:** Individual session files may be malformed, truncated, or in unexpected formats. Skip with a logged warning; don't fail the whole index.

## Web UI

Three views, all server-rendered HTML with HTMX for interactivity.

### `/` — Dashboard home

Header: repo count, session count, last scan time, manual refresh button.

Two columns:

- **Repos** — list of discovered repos, each showing: name, path (muted), session count (link). Repos with 0 linked sessions fade to 0.4 opacity so the ones you've actually worked in stand out.
- **Recent sessions** — last 20 sessions by `started_at DESC`, each showing: summary, started_at (relative: "2h ago"), linked repos as pill-shaped links.

Clicking a repo → `/repos/{id}`.
Clicking a session → `/sessions/{id}`.

Refresh button: `hx-post="/refresh"`. Server kicks off scan + index, pushes progress over WebSocket, updates the `#scan-status` line via out-of-band swaps.

### `/repos/{id}` — Repo detail

Header: repo name, path.

List of sessions joined to this repo, ordered by `started_at DESC`. Each row: summary, relative timestamp, message count, link to session detail.

### `/sessions/{id}` — Session detail

Header: session summary, timestamps, message count, cwd.

Linked repos (usually one, could be more in future when more match types are added).

Source file path (for debugging / opening the raw JSONL).

*Not in scope:* rendering session transcripts. That's a future feature. MVP just shows metadata.

### WebSocket endpoints

- `/ws` — progress broadcast. HTML fragments using `hx-swap-oob="true"` targeting `#scan-status`.
- `/livereload` — dev ergonomic. A browser script opens this socket; when the server restarts (via `cargo watch`), the socket drops and the browser reloads on reconnect.

## Refresh Flow

1. User clicks refresh (or loads page for the first time with empty DB).
2. Server drops all rows from `session_repos`, `sessions`, `repos`.
3. Scan for repos. Emit progress over WS.
4. Index sessions. Emit progress over WS (batch updates, not per-session — every 50 sessions).
5. Compute joins during the indexing pass. Emit completion over WS.

The `POST /refresh` handler returns `202 Accepted` immediately and the actual work runs on a background tokio task.

## Privacy Considerations

Session files may contain sensitive content (code, credentials pasted into prompts, internal discussions). The MVP:

- Stores only metadata and a truncated summary — not full transcripts.
- Keeps all data local. No network requests. No telemetry.
- The truncated summary could still contain sensitive info. Accept this for MVP; a "redact summaries" toggle is a candidate for v2.
- Binds the web server to `127.0.0.1` only. Never `0.0.0.0`.

## What "Done" Looks Like for MVP

- [x] Run the server in a directory containing git repos within the configured depth.
- [x] Open `http://127.0.0.1:7777/` in a browser.
- [x] See list of repos and recent sessions.
- [x] Click a repo → see sessions associated with it.
- [x] Click refresh → see live progress, then updated data.
- [x] Indexing 500+ sessions completes in well under 10 seconds on a modern laptop.

## Deferred Ideas

Preserved here so they don't get lost. Do not build these without a separate conversation.

- Git working-tree state: untracked files, diffs, staged/unstaged, stashes, unpushed commits
- Open branches with staleness (last commit date, merged-to-main detection)
- GitHub integration: open issues, PRs (yours / awaiting your review / drafts), CI status
- Security/dependency alerts from GitHub
- Ops/healthcheck monitoring for deployed services
- Recent activity feed across repos (standup prep / "what did I do yesterday")
- Menu bar companion showing urgent signals (failed CI, review requests, stale uncommitted work)
- MCP server exposing the dashboard's data so Claude Code can query it
- Additional session↔repo match signals:
  - File paths edited in a session resolved to repos
  - Branch names mentioned in session text (noisy — needs filtering)
- Session transcript rendering in the UI
- "Resume abandoned work" view: branch has uncommitted changes + last Claude session was N days ago + here's what you were doing
- File watchers / background polling for automatic refresh
- Per-project summary stats, time-in-session metrics
- Search across sessions (full-text)
- Export / share a session's summary
- Redacted-summary toggle for sensitive prompts

## Open Questions

1. How to canonicalize paths on case-insensitive filesystems (macOS default) without breaking on case-sensitive ones (Linux). Current implementation does prefix match on raw strings, then falls back to lowercase comparison — good enough in practice but can false-match in pathological mixed-case cases.
2. How to handle sessions with no recorded cwd (currently: kept but unlinked).
3. Port collision — MVP uses a fixed port (7777, overridable via `REPO_RECALL_PORT`) and errors out if it's in use.
