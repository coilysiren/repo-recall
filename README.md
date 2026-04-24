# repo-recall

A local dev dashboard that indexes your [Claude Code](https://claude.com/claude-code) session history and joins it to the git repos on your disk. Answers two questions:

- *What Claude Code sessions have I had about this repo?*
- *What repos has this session touched?*

Everything runs on `127.0.0.1`. No network fetches, no auth, no background workers. The server walks its own working directory + two levels deep for `.git` entries, parses `~/.claude/projects/**/*.jsonl`, and matches sessions to repos by their recorded `cwd`.

## Quick start

```sh
# from the directory you want indexed:
cargo run

# then open:
open http://127.0.0.1:7777
```

### Dev loop with auto-reload

```sh
make install   # one-time: installs cargo-watch and pre-commit hooks
make watch     # rebuild on source change; browser auto-refreshes via /livereload
make test      # integration tests (boot the router on a random port and hit it)
make ci        # fmt --check + clippy + check + test, in order
make help      # see all targets
```

Or the raw commands without `make`:

```sh
cargo install cargo-watch
REPO_RECALL_CWD=/path/you/want/indexed cargo watch -w src -w Cargo.toml -w static -x run
```

`REPO_RECALL_CWD` exists because `cargo watch` runs the binary with the Cargo project as its cwd. When you're not using `cargo watch`, just launch from the directory you want scanned.

A `.env` file in the repo root is loaded automatically on startup — drop your `REPO_RECALL_*` overrides there if you don't want to retype them.

### Env vars

| Var                | Default                           | Purpose                                                                         |
|--------------------|-----------------------------------|---------------------------------------------------------------------------------|
| `REPO_RECALL_PORT` | `7777`                            | HTTP port (always bound to `127.0.0.1`).                                        |
| `REPO_RECALL_CWD`  | process cwd                       | Directory to scan for repos.                                                    |
| `REPO_RECALL_DEPTH`| `4`                               | How many directory levels below cwd to walk.                                    |
| `REPO_RECALL_COMMITS_PER_REPO` | `500`                 | Max commits pulled per repo via `git log --all --no-merges`.                    |
| `REPO_RECALL_DB`   | `$TMPDIR/repo-recall.sqlite`      | SQLite cache. Dropped and rebuilt every time the server starts.                 |
| `RUST_LOG`         | `info,repo_recall=debug`          | `tracing-subscriber` filter.                                                    |

## How it works

- **Repo discovery** — walks cwd up to `REPO_RECALL_DEPTH` levels deep (default 4), stops descending inside repos, skips `node_modules` / `target` / `dist` / `build` / `.venv` / `venv` and hidden dirs. Git worktrees (`.git` as a file) are detected.
- **Data sources** — repo-recall is designed around several independent data sources, all keyed to the same repo set. Each attribute belongs to one of three categories, which dictates how it's refreshed and how prominently it displays:
    - **Historical** (past activity, offline, cheap): sessions, commits in the last 30 days, LOC churn, unique authors.
    - **Current local state** (working tree right now, offline, cheap): untracked + modified file counts, and a sampled list of individual paths for the right column.
    - **Current remote state** (requires a network call, runs in a parallel post-pass, best-effort): GitHub Actions status on the default branch via `gh run list`. Failing CI surfaces as a prominent pill.
- **Session history** — each `*.jsonl` under `~/.claude/projects/` is parsed for `sessionId`, first/last timestamps, first user message (as a 200-char summary), message count, and `cwd`. Malformed lines are skipped with a debug log. Sessions are joined to repos when their `cwd` is inside one, via `session_repos.match_type = 'cwd'` — the extension point for richer signals (file paths touched, branch names) as new match types.
- **Git log** — for each discovered repo, `git log --all --no-merges` is run as a subprocess (NUL-separated format) and up to `REPO_RECALL_COMMITS_PER_REPO` commits are ingested with SHA, author, timestamp, and subject. Individual-repo errors are swallowed at `debug!` rather than aborting the whole scan.
- **CI status** — a second, async refresh pass runs `gh run list -L 1 --branch <default>` against every GitHub-hosted repo with bounded concurrency (8 in flight). Requires `gh` installed + authenticated; without it, the column quietly stays empty.
- **Storage** — SQLite is a throwaway cache. On every startup the schema is dropped and recreated; on refresh the rows are wiped and rebuilt. No migrations, no stale-state bugs. Each data source gets its own table (`sessions`, `commits`, …) — a cross-source activity feed is a query-time concern, not a schema-time one.
- **UI** — server-rendered HTML via [maud](https://maud.lambda.xyz), styled with [Tailwind](https://tailwindcss.com) v4 (loaded via the browser CDN — no build step). Interactivity via [htmx](https://htmx.org) + `htmx-ext-ws`. Scan progress streams as out-of-band HTML fragments over a WebSocket. Static assets live under `static/` and are served by `tower_http::services::ServeDir`.

## Privacy

- Stores metadata and a truncated 200-char summary only — not full transcripts.
- Loopback only; the web server never listens on anything but `127.0.0.1`.
- htmx + Tailwind load from CDNs *in the browser*, not from the server process.
- The only outbound call from the server itself is `gh run list` for CI status (category: `RemoteState`). It reuses your existing `gh` auth — repo-recall never stores a token, never reads one from env, and degrades silently when `gh` is missing.

The truncated summary can still contain pasted credentials or private text. Treat the cache file as sensitive (it defaults to `$TMPDIR/repo-recall.sqlite`).

## Scope

MVP deliberately doesn't do git working-tree state, GitHub integration, session transcript rendering, background polling, or a menu-bar companion. See [`SPEC.md`](./SPEC.md) for the full scope, deferred-feature list, and the reasons each one was cut.

## Similar tools

The MVP is narrow — session ↔ repo joining — but [`SPEC.md`](./SPEC.md) scopes a much wider dashboard, and each actually-built or deferred feature already has prior art worth studying.

### Claude Code session browsing
- **[claude-code-history-viewer](https://github.com/jhlee0409/claude-code-history-viewer)** — desktop app, chat-style transcript rendering, covers Codex / Cursor / Aider / OpenCode in one UI.
    - *Relevant overlap:* the transcript viewer repo-recall intentionally skipped for MVP.
- **[claude-devtools](https://github.com/matt1398/claude-devtools)** — "missing DevTools" for Claude Code: visual inspector for tool calls, subagents, token usage, and context window.
    - *Relevant overlap:* per-session tool-call counts and token footprint columns.
- **[Claudoscope](https://github.com/cordwainersmith/Claudoscope)** — native macOS menu bar with session analytics and cost estimation.
    - *Relevant overlap:* the menu-bar shape for the deferred companion app.

### Local state + git-aware dashboards (already built: working-tree + CI, deferred: menu bar)
- **[mgitstatus](https://github.com/fboender/multi-git-status)** — scans N levels deep and prints uncommitted / untracked / unpushed / stashes across every repo.
    - *Relevant overlap:* the signals we already surface as the "uncommitted work" panel; also validates the depth-limited walk that our scanner does.
- **[RepoBar](https://github.com/steipete/RepoBar)** — macOS menu bar showing CI, issues, PRs, releases, plus local branch + sync state per pinned repo.
    - *Relevant overlap:* closest analog to repo-recall's deferred menu bar direction — and it already combines the local + remote signals we're building separately.

### Git log analytics (already built: commits / LOC churn / authors)
- **[Code Maat](https://github.com/adamtornhill/code-maat)** — CLI that mines git logs for churn, author contribution, coupling, and temporal hotspots. Pairs with Tornhill's *Your Code as a Crime Scene*.
    - *Relevant overlap:* our `loc_churn_30d` + `authors_30d` are primitive forms of what Code Maat does exhaustively; hotspot + file-coupling queries are a natural evolution once we store more per-commit data.
- **[RepoSense](https://github.com/reposense/RepoSense)** — contribution analysis across a set of repos, with a chronological per-author code breakdown. Originally built for grading student projects.
    - *Relevant overlap:* the explicit multi-repo framing matches our activity-scored ranking; their per-author timeline is a direction the commits panel could grow into.
- **[git-quick-stats](https://github.com/arzzen/git-quick-stats)** — interactive Bash CLI that prints ownership / churn / hotspots / branch health for one repo at a time.
    - *Relevant overlap:* good catalogue of what individual-repo analytics a repo-detail page could grow into without pulling in a heavy dependency.

### Standup / "what did I do" (deferred: recent activity feed)
- **[git-standup](https://github.com/kamranahmedse/git-standup)** — walks `git log` across nested repos to recall yesterday's work.
    - *Relevant overlap:* pair its commit scraping with repo-recall's session scraping for a richer recap.
## Contributing

See [`AGENTS.md`](./AGENTS.md) for conventions and architecture notes — what's a cache vs. a database, how to add new session↔repo match types, why DB access uses `spawn_blocking`, and so on.
