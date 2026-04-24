use std::path::Path;

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use maud::{html, Markup};
use serde::Deserialize;

use crate::routes::templates::{
    absolute_time, compact_count, display_name, page, page_with_banners, relative_time, H2, LI,
    LINK, META, PANEL, PANEL_ALERT, PATH, PILL, PILL_ALERT, PILL_FAINT, ROW,
};
use crate::{activity, db, AppState};

#[derive(Debug, Deserialize, Default)]
pub struct DashboardParams {
    /// `me` → filter to the detected git user's email. `all` → no filter
    /// (the default). Any other string is treated as a literal email.
    #[serde(default)]
    pub author: Option<String>,
}

pub async fn index(
    State(state): State<AppState>,
    Query(params): Query<DashboardParams>,
) -> impl IntoResponse {
    // Resolve `?author=` into a concrete email filter. `me` needs to see the
    // cached git email; anything non-`all` / non-empty is used literally.
    let my_email = state.my_git_email.lock().await.clone();
    let author_filter: Option<String> = match params.author.as_deref() {
        None | Some("") | Some("all") => None,
        Some("me") => my_email.clone(),
        Some(email) => Some(email.to_string()),
    };
    let filter_label = author_filter.clone();

    let state2 = state.clone();
    let af = author_filter.clone();
    let data = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db::open(&state2.db_path)?;
        let (repos_n, sessions_n, links_n, commits_n) = db::counts(&conn)?;
        let earliest_ts = db::earliest_session_ts(&conn)?;
        let mut repos = db::list_repos_with_counts(&conn)?;
        activity::sort(&mut repos);
        let recent_sessions = db::recent_sessions(&conn, 15)?;
        let recent_commits = db::recent_commits(&conn, 15, af.as_deref())?;
        // Capped at 6 repos × 4 files/repo = max 24 rows in the panel
        // (+headers), enough to read at a glance without scrolling forever.
        let uncommitted_groups = db::uncommitted_by_repo(&conn, 6, 4)?;
        let ci_failures = db::failing_ci_repos(&conn)?;
        Ok((
            repos_n,
            sessions_n,
            links_n,
            commits_n,
            earliest_ts,
            repos,
            recent_sessions,
            recent_commits,
            uncommitted_groups,
            ci_failures,
        ))
    })
    .await
    .unwrap();

    let (
        repos_n,
        sessions_n,
        links_n,
        commits_n,
        earliest_ts,
        repos,
        recent_sessions,
        recent_commits,
        uncommitted_groups,
        ci_failures,
    ) = match data {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("dashboard query failed: {e:?}");
            return page("error", html! { p { "Error: " (e.to_string()) } });
        }
    };

    let last_scan = *state.last_scan.lock().await;
    let last_scan_str = last_scan
        .map(|t| absolute_time(Some(t.timestamp())))
        .unwrap_or_else(|| "never".into());
    let gh_health = *state.gh_health.lock().await;

    // Format "back to" line: "2025-11-12 (164d)" or "—" if we have no
    // sessions with timestamps yet (first boot before the initial scan lands).
    let earliest_str = earliest_ts
        .and_then(|ts| chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0))
        .map(|dt| {
            let days = (chrono::Utc::now() - dt).num_days().max(0);
            format!("{} ({}d)", dt.format("%Y-%m-%d"), days)
        })
        .unwrap_or_else(|| "—".into());

    // Aggregate the action-required signals for the top bar. Cheap — we
    // already have every repo in memory.
    let ci_failing_count = repos
        .iter()
        .filter(|r| r.ci_status.as_deref() == Some("failure"))
        .count();
    let dirty_count = repos
        .iter()
        .filter(|r| (r.untracked_files + r.modified_files) > 0)
        .count();
    let in_progress_count = repos.iter().filter(|r| r.in_progress_op.is_some()).count();
    let detached_count = repos
        .iter()
        .filter(|r| r.head_ref.as_deref() == Some("detached"))
        .count();
    let review_requested_count: i64 = repos.iter().map(|r| r.prs_awaiting_my_review).sum();
    let has_action = ci_failing_count
        + dirty_count
        + in_progress_count
        + detached_count
        + (review_requested_count as usize)
        > 0;

    let body = html! {
        @if has_action {
            section class="mb-4 px-3 py-2 rounded-md bg-[#574f7d] text-white text-xs
                           flex items-baseline gap-x-3 gap-y-1 flex-wrap" {
                span class="text-base leading-none" { "⚠" }
                span class="font-bold uppercase tracking-[0.08em]" { "action required" }
                @if ci_failing_count > 0 {
                    span { (ci_failing_count) " failing CI" @if ci_failing_count != 1 { "s" } }
                }
                @if dirty_count > 0 {
                    span { (dirty_count) " dirty " @if dirty_count == 1 { "repo" } @else { "repos" } }
                }
                @if in_progress_count > 0 {
                    span { (in_progress_count) " mid-op" }
                }
                @if detached_count > 0 {
                    span { (detached_count) " detached HEAD" @if detached_count != 1 { "s" } }
                }
                @if review_requested_count > 0 {
                    span { (review_requested_count) " awaiting your review" }
                }
            }
        }

        section class="flex gap-8 items-end mb-2 flex-wrap" {
            (stat("repos", &repos_n.to_string()))
            (stat("sessions", &sessions_n.to_string()))
            (stat("commits", &commits_n.to_string()))
            (stat("links", &links_n.to_string()))
            div {
                div class="text-[11px] uppercase tracking-[0.08em] text-[#9e9fc2] font-bold" { "earliest" }
                div class="text-sm text-[#574f7d] mt-1 font-mono" { (earliest_str) }
            }
            div {
                div class="text-[11px] uppercase tracking-[0.08em] text-[#9e9fc2] font-bold" { "last scan" }
                div class="text-sm text-[#574f7d] mt-1 font-mono" { (last_scan_str) }
            }
            (author_toggle(my_email.as_deref(), filter_label.as_deref()))
            form method="post" action="/refresh" hx-post="/refresh" hx-swap="none" {
                button type="submit"
                    class="bg-[#574f7d] text-white px-4 py-2 rounded-md text-xs font-bold tracking-wide
                           hover:bg-[#3e375d] hover:-translate-y-px hover:shadow-md
                           transition-all duration-150 cursor-pointer
                           shadow-sm" {
                    "↻ refresh"
                }
            }
        }

        p class="text-[11px] text-[#574f7d]/60 italic mb-3 max-w-3xl" {
            "History goes as far back as Claude Code has kept sessions on disk — we read every "
            code class="font-mono not-italic bg-[#9e9fc2]/15 px-1 rounded" { ".jsonl" }
            " under "
            code class="font-mono not-italic bg-[#9e9fc2]/15 px-1 rounded" { "~/.claude/projects/" }
            " and don't cap the range ourselves. If yours stops earlier than expected, Claude Code has rotated or cleaned them up."
        }

        (standup_details(&repos, &recent_commits, &recent_sessions, filter_label.as_deref()))

        div id="scan-status" hx-ext="ws" ws-connect="/ws"
            class="px-3 py-2 bg-[#c9dcd5] border border-[#9e9fc2]/50 rounded text-[#574f7d] text-xs mb-4" {
            "waiting for scan status…"
        }

        @let uncommitted_total: i64 = uncommitted_groups.iter().map(|g| g.total).sum();
        @let uncommitted_panel = if uncommitted_groups.is_empty() { PANEL } else { PANEL_ALERT };
        @let ci_panel = if ci_failures.is_empty() { PANEL } else { PANEL_ALERT };
        div class="grid grid-cols-1 lg:grid-cols-2 gap-4" {
            section class=(PANEL) {
                h2 class=(H2) { "repos" }
                (render_repos(&repos, &state.cwd))
            }
            div class="flex flex-col gap-4 min-w-0" {
                @if !ci_failures.is_empty() {
                    section class=(ci_panel) {
                        h2 class={ (H2) " text-[#3e375d]" } {
                            "CI failing — action required"
                            span class="text-[#574f7d]/70 normal-case tracking-normal font-normal" {
                                " (" (ci_failures.len()) " repo"
                                @if ci_failures.len() != 1 { "s" }
                                ")"
                            }
                        }
                        (render_ci_failures(&ci_failures))
                    }
                }
                section class=(uncommitted_panel) {
                    h2 class={
                        (H2)
                        @if !uncommitted_groups.is_empty() { " text-[#3e375d]" }
                    } {
                        "uncommitted work"
                        @if !uncommitted_groups.is_empty() {
                            " — action required"
                            span class="text-[#574f7d]/70 normal-case tracking-normal font-normal" {
                                " ("
                                (compact_count(uncommitted_total)) " file"
                                @if uncommitted_total != 1 { "s" }
                                " across " (uncommitted_groups.len()) " repo"
                                @if uncommitted_groups.len() != 1 { "s" }
                                ")"
                            }
                        }
                    }
                    (render_uncommitted_groups(&uncommitted_groups))
                }
                section class=(PANEL) {
                    h2 class=(H2) { "recent sessions" }
                    (render_sessions(&recent_sessions))
                }
                section class=(PANEL) {
                    h2 class=(H2) { "recent commits" }
                    (render_commits(&recent_commits, &state.cwd))
                }
            }
        }
    };
    page_with_banners("dashboard", body, Some(gh_health))
}

/// Expandable standup summary — collapsed by default so it doesn't crowd the
/// main dashboard, but when you click it opens to a tight digest of the last
/// 24h: commits per repo, sessions today, action-required counts rolled up.
/// Intentionally lives on the main page (user spec: "keep people on the main
/// page 95% of the time").
fn standup_details(
    repos: &[db::Repo],
    recent_commits: &[db::CommitWithRepo],
    recent_sessions: &[db::SessionWithRepos],
    author_filter: Option<&str>,
) -> Markup {
    let now = chrono::Utc::now().timestamp();
    let cutoff = now - 86_400; // 24h

    // Commits in the last 24h, grouped by repo.
    use std::collections::BTreeMap;
    let mut by_repo: BTreeMap<String, Vec<&db::CommitWithRepo>> = BTreeMap::new();
    for c in recent_commits
        .iter()
        .filter(|c| c.commit.timestamp >= cutoff)
    {
        by_repo.entry(c.repo_name.clone()).or_default().push(c);
    }

    let sessions_today: Vec<_> = recent_sessions
        .iter()
        .filter(|sr| sr.session.started_at.map(|t| t >= cutoff).unwrap_or(false))
        .collect();

    let dirty: Vec<_> = repos
        .iter()
        .filter(|r| (r.untracked_files + r.modified_files) > 0)
        .collect();
    let failing_ci: Vec<_> = repos
        .iter()
        .filter(|r| r.ci_status.as_deref() == Some("failure"))
        .collect();

    html! {
        details class="mb-4 rounded-md border border-[#9e9fc2]/45 bg-[#f5f3f9] overflow-hidden" {
            summary class="cursor-pointer px-4 py-2 text-xs font-bold uppercase
                           tracking-[0.08em] text-[#574f7d] hover:bg-[#9e9fc2]/15 select-none
                           flex items-center gap-2" {
                span { "📝 today's standup" }
                span class="normal-case tracking-normal font-normal text-[#574f7d]/70" {
                    "("
                    (by_repo.len()) " repo"
                    @if by_repo.len() != 1 { "s" }
                    " committed to · "
                    (sessions_today.len()) " session"
                    @if sessions_today.len() != 1 { "s" }
                    @if let Some(email) = author_filter {
                        " · author=" (email)
                    }
                    ")"
                }
            }
            div class="px-4 pb-4 pt-2 text-xs leading-relaxed flex flex-col gap-3" {
                @if by_repo.is_empty() && sessions_today.is_empty()
                    && dirty.is_empty() && failing_ci.is_empty()
                {
                    p class="text-[#574f7d]/70" { "nothing to report — clean slate." }
                }

                @if !by_repo.is_empty() {
                    div {
                        div class="font-bold text-[#3e375d] mb-1" { "commits (last 24h)" }
                        ul class="list-none p-0 m-0 flex flex-col gap-1.5" {
                            @for (repo_name, cs) in &by_repo {
                                li {
                                    span class="font-semibold" { (repo_name) }
                                    span class="text-[#574f7d]/70" {
                                        " — " (cs.len()) " commit"
                                        @if cs.len() != 1 { "s" }
                                    }
                                    ul class="list-none pl-3 mt-0.5 border-l border-[#9e9fc2]/40
                                              flex flex-col gap-0.5" {
                                        @for c in cs.iter().take(4) {
                                            li class="truncate" { (c.commit.subject) }
                                        }
                                        @if cs.len() > 4 {
                                            li class="italic text-[#574f7d]/60" {
                                                "…and " (cs.len() - 4) " more"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                @if !sessions_today.is_empty() {
                    div {
                        div class="font-bold text-[#3e375d] mb-1" { "Claude sessions (last 24h)" }
                        ul class="list-none p-0 m-0 flex flex-col gap-0.5" {
                            @for sr in sessions_today.iter().take(6) {
                                li class="truncate" {
                                    @if let Some(s) = &sr.session.summary { (s) }
                                    @else { "(no summary)" }
                                }
                            }
                            @if sessions_today.len() > 6 {
                                li class="italic text-[#574f7d]/60" {
                                    "…and " (sessions_today.len() - 6) " more"
                                }
                            }
                        }
                    }
                }

                @if !dirty.is_empty() || !failing_ci.is_empty() {
                    div {
                        div class="font-bold text-[#3e375d] mb-1" { "open loops" }
                        ul class="list-none p-0 m-0 flex flex-col gap-0.5" {
                            @for r in &failing_ci {
                                li { "CI failing: " span class="font-semibold" { (r.name) } }
                            }
                            @for r in &dirty {
                                li {
                                    "uncommitted work: "
                                    span class="font-semibold" { (r.name) }
                                    span class="text-[#574f7d]/70" {
                                        " (" (r.untracked_files + r.modified_files) " file"
                                        @if (r.untracked_files + r.modified_files) != 1 { "s" }
                                        ")"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Author-filter toggle. Three states: "all" (default, no query param),
/// "me" (uses detected git email), and per-pill the currently-active one is
/// bolded + underlined. Links not buttons so it's bookmarkable + history-
/// navigable. Only rendered when we actually know the viewer's email.
fn author_toggle(my_email: Option<&str>, active: Option<&str>) -> Markup {
    let Some(me) = my_email else {
        return html! {};
    };
    let is_me = active.map(|e| e == me).unwrap_or(false);
    let is_all = active.is_none();
    let base = "text-[11px] px-2 py-1 rounded-md border transition-colors";
    let active_cls = "bg-[#574f7d] text-white border-[#3e375d] font-semibold";
    let inactive_cls = "bg-transparent text-[#574f7d] border-[#9e9fc2]/50 hover:bg-[#9e9fc2]/15";
    html! {
        div class="flex items-center gap-1 ml-auto mr-2"
            title={ "author filter — currently " (active.unwrap_or("all")) } {
            span class="text-[10px] uppercase tracking-[0.08em] text-[#9e9fc2] font-bold mr-1" {
                "author"
            }
            a href="?author=all"
              class={ (base) " " @if is_all { (active_cls) } @else { (inactive_cls) } } {
                "all"
            }
            a href="?author=me"
              class={ (base) " " @if is_me { (active_cls) } @else { (inactive_cls) } }
              title=(me) {
                "me"
            }
        }
    }
}

fn stat(label: &str, value: &str) -> Markup {
    html! {
        div {
            div class="text-[11px] uppercase tracking-[0.08em] text-[#9e9fc2] font-bold" { (label) }
            div class="text-2xl font-bold text-[#3e375d] leading-none mt-1" { (value) }
        }
    }
}

fn render_repos(repos: &[db::Repo], scan_cwd: &Path) -> Markup {
    html! {
        @if repos.is_empty() {
            p class="text-[#574f7d]/70" { "no repos discovered in cwd + configured depth" }
        } @else {
            ul class="list-none p-0 m-0" {
                @for r in repos {
                    li class={
                        (LI)
                        @if activity::is_dormant(r) { " opacity-40" }
                    } {
                        div class=(ROW) {
                            span class="font-semibold" {
                                a class=(LINK) href={ "/repos/" (r.id) } {
                                    (display_name(r, scan_cwd))
                                }
                            }
                            @if r.session_count > 0 {
                                span class=(PILL) { (r.session_count) " sessions" }
                            }
                            @if r.commits_30d > 0 {
                                span class=(PILL) title="commits in the last 30 days" {
                                    (r.commits_30d) " commits (30d)"
                                }
                            }
                            @if r.loc_churn_30d > 0 {
                                span class=(PILL)
                                     title="lines added + deleted in the last 30 days (30-day churn)" {
                                    (compact_count(r.loc_churn_30d)) " churn (30d)"
                                }
                            }
                            @if r.authors_30d > 0 {
                                span class=(PILL)
                                     title="unique commit authors in the last 30 days" {
                                    (r.authors_30d) " authors (30d)"
                                }
                            }
                            @let uncommitted = r.untracked_files + r.modified_files;
                            @if uncommitted > 0 {
                                span class=(PILL_ALERT)
                                     title={
                                        "working-tree files right now — "
                                        (r.modified_files) " modified + "
                                        (r.untracked_files) " untracked"
                                     } {
                                    (compact_count(uncommitted)) " uncommitted"
                                }
                            }
                            @if let Some(op) = r.in_progress_op.as_deref() {
                                span class=(PILL_ALERT) title="a git operation is mid-flight — finish or abort it" {
                                    (op) " in progress"
                                }
                            }
                            @if r.head_ref.as_deref() == Some("detached") {
                                span class=(PILL_ALERT) title="HEAD is detached — not on any branch" {
                                    "detached HEAD"
                                }
                            }
                            @if r.stash_count > 0 {
                                span class=(PILL) title="`git stash list` entries" {
                                    (r.stash_count) " stashed"
                                }
                            }
                            @if r.commits_ahead > 0 {
                                span class=(PILL) title="local commits not on origin yet" {
                                    "↑ " (r.commits_ahead) " unpushed"
                                }
                            }
                            @if r.commits_behind > 0 {
                                span class=(PILL) title="upstream has commits you don't — pull to catch up" {
                                    "↓ " (r.commits_behind) " behind"
                                }
                            }
                            (ci_pill(r))
                            @if r.prs_awaiting_my_review > 0 {
                                span class=(PILL_ALERT)
                                     title="PRs where you're a requested reviewer" {
                                    (r.prs_awaiting_my_review) " awaiting your review"
                                }
                            }
                            @if r.prs_mine_awaiting_review > 0 {
                                span class=(PILL)
                                     title="your open PRs waiting on someone else" {
                                    "↗ " (r.prs_mine_awaiting_review) " yours open"
                                }
                            }
                            @if r.open_prs > 0 {
                                span class=(PILL) title="total open PRs (including drafts)" {
                                    (r.open_prs) " PRs"
                                    @if r.draft_prs > 0 {
                                        " (" (r.draft_prs) " draft)"
                                    }
                                }
                            }
                            @if r.open_issues > 0 {
                                span class=(PILL) title="total open issues" {
                                    (r.open_issues) " issues"
                                }
                            }
                        }
                        @if let Some(url) = remote_link(r) {
                            div class="text-xs mt-0.5" {
                                a class=(LINK) href=(url.0) target="_blank" rel="noopener" {
                                    (url.1)
                                }
                            }
                        }
                        div class=(PATH) { (r.path) }
                    }
                }
            }
        }
    }
}

fn render_uncommitted_groups(groups: &[db::UncommittedGroup]) -> Markup {
    html! {
        @if groups.is_empty() {
            p class="text-[#574f7d]/70 text-xs" {
                "nothing dirty — every working tree is clean"
            }
        } @else {
            ul class="list-none p-0 m-0 flex flex-col gap-3" {
                @for g in groups {
                    li {
                        // Per-repo header row: repo name + total count pill.
                        div class="flex items-baseline gap-2 flex-wrap" {
                            a class={ (LINK) " font-semibold" } href={ "/repos/" (g.repo_id) } {
                                (g.repo_name)
                            }
                            span class=(PILL) {
                                (g.total) " file"
                                @if g.total != 1 { "s" }
                            }
                        }
                        // Sampled file paths (mod first, then untracked).
                        ul class="list-none pl-3 mt-1 border-l border-[#9e9fc2]/40 \
                                  flex flex-col gap-0.5" {
                            @for (path, kind) in &g.sample {
                                li class="flex gap-2 items-baseline" {
                                    span class={
                                        "text-[10px] uppercase tracking-[0.04em] font-bold \
                                         shrink-0 w-7 "
                                        @if kind == "untracked" { "text-[#9192bb]" }
                                        @else { "text-[#574f7d]" }
                                    } {
                                        @if kind == "untracked" { "new" } @else { "mod" }
                                    }
                                    span class="font-mono text-[11px] break-all" { (path) }
                                }
                            }
                            // "…and 3 more" — the DB query capped the sample, but
                            // `total` is the true count, so we can show what's
                            // hidden without refetching.
                            @let shown = g.sample.len() as i64;
                            @if g.total > shown {
                                li class="text-[11px] text-[#574f7d]/60 italic" {
                                    "…and " (g.total - shown) " more"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn render_ci_failures(failures: &[db::CiFailure]) -> Markup {
    html! {
        ul class="list-none p-0 m-0" {
            @for f in failures {
                li class=(LI) {
                    div class="flex items-baseline gap-2 flex-wrap" {
                        a class={ (LINK) " font-semibold" } href={ "/repos/" (f.repo_id) } {
                            (f.repo_name)
                        }
                        @if let (Some(url), Some(branch)) =
                            (f.remote_url.as_deref(), f.default_branch.as_deref())
                        {
                            a class=(PILL)
                              href={ (url) "/actions?query=branch%3A" (branch) }
                              target="_blank" rel="noopener"
                              title="open the failing branch's Actions history on GitHub" {
                                "view CI ↗"
                            }
                        }
                    }
                }
            }
        }
    }
}

fn render_commits(commits: &[db::CommitWithRepo], scan_cwd: &Path) -> Markup {
    html! {
        @if commits.is_empty() {
            p class="text-[#574f7d]/70" { "no commits indexed yet" }
        } @else {
            ul class="list-none p-0 m-0" {
                @for c in commits {
                    li class=(LI) {
                        div class=(ROW) {
                            (short_sha(&c.commit.sha, c.repo_remote_url.as_deref()))
                            span class="font-semibold truncate" { (c.commit.subject) }
                        }
                        div class={ (ROW) " " (META) } {
                            span { (relative_time(Some(c.commit.timestamp))) }
                            a class=(PILL) href={ "/repos/" (c.repo_id) } {
                                (display_repo_label(&c.repo_name, &c.repo_path, scan_cwd))
                            }
                            span { (c.commit.author_name) }
                        }
                    }
                }
            }
        }
    }
}

/// Short-SHA chip. Renders as an external link to `{remote}/commit/<sha>` if
/// we know the remote; plain `<code>` otherwise. GitHub / GitLab / Bitbucket
/// all use the same `/commit/<sha>` path, so the link works without
/// host-specific branching.
fn short_sha(sha: &str, remote_url: Option<&str>) -> Markup {
    let short: &str = &sha[..sha.len().min(7)];
    let chip_class = "font-mono text-[11px] text-[#574f7d] bg-[#9e9fc2]/15 \
                      px-1.5 py-0.5 rounded hover:bg-[#9e9fc2]/30 transition-colors";
    html! {
        @match remote_url {
            Some(url) if !url.is_empty() => {
                a class=(chip_class) href={ (url) "/commit/" (sha) }
                  target="_blank" rel="noopener" title=(sha) {
                    (short)
                }
            }
            _ => {
                code class="font-mono text-[11px] text-[#9e9fc2] bg-[#9e9fc2]/15 px-1.5 py-0.5 rounded"
                     title=(sha) {
                    (short)
                }
            }
        }
    }
}

/// CI status pill — only rendered for actionable states. "success" and
/// "unknown" / missing stay silent to keep the row chrome-free. "failure"
/// uses `PILL_ALERT` so a broken default branch jumps out of the list;
/// "running" / "pending" use the dashed faint variant so they read as
/// transient.
fn ci_pill(r: &db::Repo) -> Markup {
    let Some(status) = r.ci_status.as_deref() else {
        return html! {};
    };
    let default_branch = r.default_branch.as_deref().unwrap_or("");
    let href = r
        .remote_url
        .as_deref()
        .filter(|_| !default_branch.is_empty())
        .map(|u| format!("{u}/actions?query=branch%3A{default_branch}"));
    let (class, text, title) = match status {
        "failure" => (
            PILL_ALERT,
            "CI failing",
            "latest default-branch CI run failed",
        ),
        "running" => (
            PILL_FAINT,
            "CI running",
            "default-branch CI currently running",
        ),
        "pending" => (
            PILL_FAINT,
            "CI pending",
            "default-branch CI is queued / waiting",
        ),
        _ => return html! {}, // success / unknown — stay silent
    };
    html! {
        @match href {
            Some(h) => {
                a class=(class) href=(h) target="_blank" rel="noopener" title=(title) { (text) }
            }
            None => {
                span class=(class) title=(title) { (text) }
            }
        }
    }
}

/// `(href, text)` for a repo's default-branch remote link. Returns `None` if
/// we don't know the origin URL. When the default branch is known, the href
/// points at `/tree/<branch>` and the text strips `https://` + appends
/// ` @ <branch>` so the link reads like
/// `github.com/coilysiren/backend @ main`. When we have a URL but no branch,
/// the href is the bare URL and the text is the bare host + path.
fn remote_link(r: &db::Repo) -> Option<(String, String)> {
    let base = r.remote_url.as_ref()?;
    let display = base
        .strip_prefix("https://")
        .or_else(|| base.strip_prefix("http://"))
        .unwrap_or(base);
    match r.default_branch.as_deref() {
        Some(branch) if !branch.is_empty() => Some((
            format!("{base}/tree/{branch}"),
            format!("{display} @ {branch}"),
        )),
        _ => Some((base.clone(), display.to_string())),
    }
}

/// Compute a cwd-relative label from a (name, path) pair. Mirrors
/// `display_name(&Repo, ...)` but takes the primitives already returned by our
/// join queries, so callers don't need to reconstruct a `db::Repo`.
fn display_repo_label(name: &str, path: &str, scan_cwd: &Path) -> String {
    let p = Path::new(path);
    match p.strip_prefix(scan_cwd) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => name.to_string(),
    }
}

fn render_sessions(sessions: &[db::SessionWithRepos]) -> Markup {
    html! {
        @if sessions.is_empty() {
            p class="text-[#574f7d]/70" { "no sessions indexed yet" }
        } @else {
            ul class="list-none p-0 m-0" {
                @for sr in sessions {
                    li class=(LI) {
                        div class=(ROW) {
                            a class={ (LINK) " font-semibold" } href={ "/sessions/" (sr.session.id) } {
                                @if let Some(s) = &sr.session.summary { (s) }
                                @else { "(no summary)" }
                            }
                        }
                        div class={ (ROW) " " (META) } {
                            span { (relative_time(sr.session.started_at)) }
                            span { (sr.session.message_count) " msgs" }
                            @for (rid, name, _path) in &sr.repos {
                                a class=(PILL) href={ "/repos/" (rid) } { (name) }
                            }
                        }
                    }
                }
            }
        }
    }
}
