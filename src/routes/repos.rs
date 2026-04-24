use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use maud::html;

use crate::db;
use crate::routes::templates::{page, relative_time, H2, LI, LINK, META, PANEL, PATH, ROW};
use crate::AppState;

pub async fn detail(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let data = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db::open(&state.db_path)?;
        let repo = db::get_repo(&conn, id)?;
        let sessions = if repo.is_some() {
            db::sessions_for_repo(&conn, id)?
        } else {
            Vec::new()
        };
        let commits = if repo.is_some() {
            db::commits_for_repo(&conn, id, 50)?
        } else {
            Vec::new()
        };
        let cutoff_30d = chrono::Utc::now().timestamp() - 30 * 86_400;
        let hotspots = if repo.is_some() {
            db::file_hotspots(&conn, id, cutoff_30d, 10)?
        } else {
            Vec::new()
        };
        Ok((repo, sessions, commits, hotspots))
    })
    .await
    .unwrap();

    let (repo, sessions, commits, hotspots) = match data {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                page("error", html! { p { (e.to_string()) } }),
            )
                .into_response();
        }
    };

    let Some(repo) = repo else {
        return (
            StatusCode::NOT_FOUND,
            page("not found", html! { p { "repo not found" } }),
        )
            .into_response();
    };

    let body = html! {
        h1 class="text-lg font-semibold mb-1" { (repo.name) }
        p class=(PATH) { (repo.path) }
        @if !hotspots.is_empty() {
            section class={ (PANEL) " mt-4" } {
                h2 class=(H2) { "hotspots — most-churned files (last 30d, top " (hotspots.len()) ")" }
                ul class="list-none p-0 m-0" {
                    @for h in &hotspots {
                        li class=(LI) {
                            div class=(ROW) {
                                span class="font-mono text-[11px] break-all" { (h.file_path) }
                            }
                            div class={ (ROW) " " (META) } {
                                span { (h.churn) " LOC churn" }
                                span { (h.commits) " commit"
                                    @if h.commits != 1 { "s" }
                                }
                                span { (h.authors) " author"
                                    @if h.authors != 1 { "s" }
                                }
                            }
                        }
                    }
                }
            }
        }
        section class={ (PANEL) " mt-4" } {
            h2 class=(H2) { "sessions (" (sessions.len()) ")" }
            @if sessions.is_empty() {
                p class="text-[#574f7d]/70" { "no sessions joined to this repo yet" }
            } @else {
                ul class="list-none p-0 m-0" {
                    @for s in &sessions {
                        li class=(LI) {
                            div class=(ROW) {
                                a class={ (LINK) " font-semibold" } href={ "/sessions/" (s.id) } {
                                    @if let Some(sum) = &s.summary { (sum) }
                                    @else { "(no summary)" }
                                }
                            }
                            div class={ (ROW) " " (META) } {
                                span { (relative_time(s.started_at)) }
                                span { (s.message_count) " msgs" }
                            }
                        }
                    }
                }
            }
        }
        section class={ (PANEL) " mt-4" } {
            h2 class=(H2) { "commits (" (commits.len()) ")" }
            @if commits.is_empty() {
                p class="text-[#574f7d]/70" { "no commits indexed yet" }
            } @else {
                ul class="list-none p-0 m-0" {
                    @for c in &commits {
                        li class=(LI) {
                            div class=(ROW) {
                                @match repo.remote_url.as_deref() {
                                    Some(url) if !url.is_empty() => {
                                        a class="font-mono text-[11px] text-[#574f7d] bg-[#9e9fc2]/15 px-1.5 py-0.5 rounded hover:bg-[#9e9fc2]/30 transition-colors"
                                            href={ (url) "/commit/" (c.sha) }
                                            target="_blank" rel="noopener"
                                            title=(c.sha) {
                                            (&c.sha[..c.sha.len().min(7)])
                                        }
                                    }
                                    _ => {
                                        code class="font-mono text-[11px] text-[#9e9fc2] bg-[#9e9fc2]/15 px-1.5 py-0.5 rounded"
                                             title=(c.sha) {
                                            (&c.sha[..c.sha.len().min(7)])
                                        }
                                    }
                                }
                                span class="font-semibold" { (c.subject) }
                            }
                            div class={ (ROW) " " (META) } {
                                span { (relative_time(Some(c.timestamp))) }
                                span { (c.author_name) }
                            }
                        }
                    }
                }
            }
        }
    };
    page(&repo.name, body).into_response()
}
