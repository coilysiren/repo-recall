# Agent instructions

## Project overview

`repo-recall` is a local dev dashboard that indexes Claude Code session history and joins sessions to git repos discovered on disk. It answers two questions:

- *What Claude Code sessions have I had about this repo?*
- *What repos has this session touched?*

Everything runs locally and bound to `127.0.0.1` only. No telemetry, no auth. The only outbound call is `gh run list` for CI status, best-effort.

- **Language**: Rust (edition 2021, stable toolchain)
- **Stack**: [axum](https://docs.rs/axum) 0.8 + [tokio](https://tokio.rs) (HTTP + WebSocket), [rusqlite](https://docs.rs/rusqlite) (bundled SQLite), [maud](https://maud.lambda.xyz) (compile-time HTML), [htmx](https://htmx.org) + `htmx-ext-ws` (UI reactivity, loaded from CDN)
- **Runtime deps**: none beyond the bundled SQLite. No config file. Discovery is lazy — the server scans from whatever directory it was launched in.

## Repository structure

```
src/
  main.rs           # entry point; reads env, bootstraps state, runs initial scan
  lib.rs            # AppState + shared types (keeps main.rs thin and tests importable)
  db.rs             # SQLite schema + queries (wipe-and-rebuild on every refresh)
  scanner.rs        # repo discovery: walk cwd + REPO_RECALL_DEPTH levels for .git entries
  sessions.rs       # data source #1: parse Claude Code JSONL session files
  commits.rs        # data source #2: shell out to `git log`, NUL-separated
  join.rs           # cwd -> repo matching (longest-prefix wins)
  activity.rs       # activity scoring + attribute categories (Historical / LocalState / RemoteState)
  routes/
    mod.rs          # router wiring + ServeDir for /static/*
    dashboard.rs    # GET /
    repos.rs        # GET /repos/{id}
    sessions.rs     # GET /sessions/{id}
    search.rs       # GET /search
    refresh.rs      # POST /refresh (kicks off async scan+index)
    ws.rs           # GET /ws (progress broadcast), GET /livereload (dev reload)
    fallback.rs     # 404 handler
    templates.rs    # maud layout + reusable Tailwind class bundles (PANEL, PILL, ...)
static/
  style.css         # small overrides on top of Tailwind (scrollbar, body font)
  livereload.js     # browser reconnect-and-reload loop
  favicon.svg       # 32×32 monochrome magnifying-glass
tests/
  smoke.rs          # integration tests: boot the router on port 0, hit every endpoint
Cargo.toml
Makefile            # `make help` for the full target list
.pre-commit-config.yaml
.github/workflows/ci.yml
```

## Dev loop

```sh
make install   # cargo-watch + pre-commit hooks
make run       # one-off run against the current directory
make watch     # rebuild + browser livereload on every save (~1s incremental rebuild)
make test      # integration smoke tests against the real router
make ci        # fmt-check + clippy + check + test (what GitHub Actions runs)
make help      # full target list
```

The `make` targets are thin wrappers over `cargo` commands — use raw cargo if you prefer. `cargo run` and `cargo watch` work too; `REPO_RECALL_CWD` and friends can go in a `.env` file at the repo root, which is loaded automatically at startup via `dotenvy`.

Environment variables:

| Var                 | Default                                    | Purpose                                                                      |
|---------------------|--------------------------------------------|------------------------------------------------------------------------------|
| `REPO_RECALL_PORT`  | `7777`                                     | HTTP port (always bound to `127.0.0.1`)                                      |
| `REPO_RECALL_CWD`   | process cwd                                | Directory to scan for repos. Useful under `cargo watch`, where the process cwd is the Cargo project root, not the directory you actually want indexed. |
| `REPO_RECALL_DEPTH` | `4`                                        | How many directory levels below cwd to walk before giving up. Raise cautiously — a wide tree can blow up both scan time and the repo count. |
| `REPO_RECALL_COMMITS_PER_REPO` | `500`                           | How many commits to pull per repo via `git log --all --no-merges`. Higher = longer history at the cost of scan time and DB size. |
| `REPO_RECALL_DB`    | `$TMPDIR/repo-recall.sqlite`               | SQLite file. Schema is dropped and recreated on every startup.               |
| `RUST_LOG`          | `info,repo_recall=debug`                   | `tracing-subscriber` filter.                                                 |

Browser auto-reload: every page includes a small script that opens a WebSocket to `/livereload`. When `cargo watch` restarts the process, the socket drops; on reconnect the page calls `location.reload()`. This is always-on — it's cheap, invisible when the server is stable, and unnecessary to gate behind a dev flag.

## Conventions

- **SQLite is a cache, not a database.** The schema is wiped and recreated on every process start. No migrations, no `INSERT OR REPLACE` heroics, no stale-state bugs. If you need to change the schema, change it in [`src/db.rs`](./src/db.rs) and restart. On refresh, the tables are truncated and rebuilt from scratch.
- **Discovery is lazy.** No config file, no root-dir setting. The server walks its cwd + `REPO_RECALL_DEPTH` levels deep (default 4). If you want it to index a different tree, run it there (or set `REPO_RECALL_CWD`).
- **`session_repos.match_type` is the extension point.** MVP writes only `'cwd'`. Additional signals (file paths touched in a session, branch-name matches, etc.) go in as new rows with new `match_type` values — don't replace the `cwd` row, add to it.
- **DB access uses `spawn_blocking` + a fresh `rusqlite::Connection` per task.** rusqlite is sync; SQLite handles concurrent readers via WAL. Don't introduce an `Arc<Mutex<Connection>>` — it serializes request handling during long scans.
- **Integration tests boot the real router on port 0.** See [`tests/smoke.rs`](./tests/smoke.rs). Each test gets its own SQLite file under `$TMPDIR` (nanos + PID + an atomic counter) so parallel `cargo test` invocations don't collide. Prefer adding tests here over writing manual-curl README snippets.
- **Session parsing tolerates malformed lines.** Individual JSONL lines can be skipped with a `tracing::debug!` log; don't fail a whole file because one line is bad. The parser already handles the mix of `queue-operation` / `user` / `assistant` record shapes we've seen.
- **Data sources are independent tables, not a single unified "events" table.** Sessions live in `sessions` + `session_repos`, commits live in `commits`. Both reference `repos.id` but don't join through each other. When future data sources arrive (GitHub PRs, CI runs, etc.) they each get their own table + refresh step. A cross-source "activity feed" is a query-time concern, not a schema-time one — don't pre-unify.
- **Activity attributes fall into three categories**, declared via [`activity::Category`](./src/activity.rs): **Historical** (past activity, local, cheap), **LocalState** (working tree right now, local, cheap), **RemoteState** (requires a network call to a remote service — GitHub, CI, etc.). Each new attribute picks a category; the category drives *how* it's refreshed (main blocking pass vs. parallel async post-pass) and *how* it's rendered (alert-style pill vs. standard vs. silent-when-healthy).
- **Activity score is `Σ ln(1 + xᵢ / Mᵢ)`** where `Mᵢ` is the corpus-wide max for each attribute. See the docstring at the top of [`src/activity.rs`](./src/activity.rs) for the full reasoning (breadth-rewarding, diminishing-returns, zero-safe). A repo at peak on every dimension scores `N · ln(2)`. Action-required repos (failing CI, dirty tree, in-progress git op, detached HEAD) hard-sort to the top as a separate bucket, regardless of score.
- **`is_action_required` is a curated subset of signals, not every local/remote attr.** Only the ones that ought to pull attention: failing CI, dirty working tree, in-progress rebase/merge/cherry-pick/revert/bisect, detached HEAD. Common states (commits ahead/behind, stash present) are shown as informational pills, not urgent ones.
- **Remote-state refresh runs in a second pass.** The main refresh stays fully local + blocking (runs inside one `spawn_blocking`). Remote-state checks run after, using tokio tasks with a bounded semaphore (8 concurrent) so N network-latency `gh` calls overlap instead of serialising. The UI shows offline data immediately and CI fills in once it lands. Failures are swallowed at `debug!` — `gh` not installed / not authenticated / rate-limited shouldn't break the dashboard.
- **Git log is shelled out, not linked.** `src/commits.rs` runs `git log --all --no-merges` as a subprocess per repo and parses NUL-separated fields. Reasons: system `git` is everywhere, no libgit2 build pain, one subprocess per repo is cheap. Individual-repo errors are swallowed (logged at `debug!`) rather than aborting the whole scan.
- **Templates are maud macros; CSS/JS are files.** The HTML lives in Rust (compile-time-checked), but Tailwind handles nearly all styling as utility classes on the markup. Anything awkward as a utility goes in [`static/style.css`](./static/style.css). Client JS lives under [`static/`](./static/) too — no inline `<script>` blocks. Served via `tower_http::services::ServeDir` mounted at `/static/*`.
- **Tailwind loads via the v4 browser CDN.** No build step, no `tailwind.config.js`, no PostCSS pipeline. For reused class bundles (panel, pill, list-row) define a `pub const` in [`src/routes/templates.rs`](./src/routes/templates.rs) rather than repeating the same 6-class string across files.
- **WebSocket fragments use HTMX out-of-band swaps.** The server sends `<div id="scan-status" hx-swap-oob="true">…</div>` fragments; HTMX pulls them out by id and swaps them in. Don't invent a JSON progress protocol — HTML fragments over the socket is the whole point of `hx-ext="ws"`.

## Privacy

Claude Code session files can contain code, pasted credentials, and internal discussion. This project:

- Stores **only metadata and a truncated 200-char summary** — not full transcripts.
- Binds the web server to **loopback only** (`127.0.0.1`). Never `0.0.0.0`, never a socket on a shared box.
- Writes the SQLite cache to `$TMPDIR` by default, which most OSes wipe on reboot.
- **Outbound network calls only for the `RemoteState` category.** `gh run list` (for CI status) is the only outbound call today. It reuses the user's existing `gh` auth — we never store tokens. If `gh` isn't installed or authenticated, the remote-state column stays blank; nothing else breaks. Add new remote calls only when a new `RemoteState` attribute genuinely needs them, and keep them best-effort.

The 200-char summary can still leak sensitive content. Redaction is future work.

## Key references

- [Claude Code session file format](https://docs.claude.com/en/docs/claude-code/settings) — sessions live in `~/.claude/projects/<encoded-project-dir>/*.jsonl`. Each line is an independent JSON record. Record shapes vary: `queue-operation` lines, `user`/`assistant` message lines, etc. `sessions.rs` ignores unknown shapes rather than failing.
- [htmx WebSocket extension](https://htmx.org/extensions/ws/) — how the server's OOB HTML fragments make it into the DOM without any client JS of our own.
- [axum 0.8 migration notes](https://github.com/tokio-rs/axum/blob/main/axum/CHANGELOG.md) — path params use `{id}` syntax, not `:id`. This is the most common thing that breaks when copying axum snippets from the internet.
