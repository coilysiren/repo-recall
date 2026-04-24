use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use maud::html;

use crate::routes::templates::{
    absolute_time, compact_count, page, relative_time, H2, LI, LINK, PANEL, PATH, ROW,
};
use crate::{db, sessions as sess, AppState};

/// Rough cost estimate for a session's token usage in USD. Prices are the
/// public Claude Sonnet 4.x rates (cheaper tier; most of our sessions run
/// here); users on other tiers will see numbers that are roughly-proportional
/// rather than exact. Cache reads are an order of magnitude cheaper than
/// fresh input tokens. This is intentionally best-effort — we don't know the
/// exact model per turn without parsing more of the JSONL.
fn estimate_cost_usd(input: i64, output: i64, cache_read: i64, cache_creation: i64) -> f64 {
    // $ / 1M tokens, Sonnet-tier baseline.
    const INPUT_PER_M: f64 = 3.00;
    const OUTPUT_PER_M: f64 = 15.00;
    const CACHE_READ_PER_M: f64 = 0.30;
    const CACHE_CREATE_PER_M: f64 = 3.75;
    let per = |n: i64, rate: f64| (n as f64 / 1_000_000.0) * rate;
    per(input, INPUT_PER_M)
        + per(output, OUTPUT_PER_M)
        + per(cache_read, CACHE_READ_PER_M)
        + per(cache_creation, CACHE_CREATE_PER_M)
}

fn render_transcript(turns: &[sess::Turn]) -> maud::Markup {
    html! {
        ol class="list-none p-0 m-0 flex flex-col gap-3" {
            @for t in turns {
                (render_turn(t))
            }
        }
    }
}

fn render_turn(t: &sess::Turn) -> maud::Markup {
    let (role_label, role_class) = match t.role {
        sess::TurnRole::User => ("user", "bg-[#c9dcd5] text-[#3e375d]"),
        sess::TurnRole::Assistant => ("assistant", "bg-[#9e9fc2]/25 text-[#3e375d]"),
        sess::TurnRole::System => (
            "system",
            "bg-transparent text-[#574f7d]/70 border border-dashed border-[#9e9fc2]/50",
        ),
    };
    html! {
        li class="flex flex-col gap-1 pl-3 border-l-2 border-[#9e9fc2]/40" {
            div class="flex items-baseline gap-2" {
                span class={ "text-[10px] uppercase tracking-[0.08em] font-bold px-1.5 py-0.5 rounded " (role_class) } {
                    (role_label)
                }
                @if let Some(ts) = t.timestamp {
                    span class="text-[11px] text-[#574f7d]/60" { (relative_time(Some(ts))) }
                }
            }
            @for text in &t.texts {
                (render_prose_block(text))
            }
            @for (name, args) in &t.tool_uses {
                details class="text-[11px]" {
                    summary class="cursor-pointer text-[#574f7d] hover:text-[#3e375d]" {
                        "🔧 " span class="font-mono font-semibold" { (name) }
                        " — " span class="italic text-[#574f7d]/70" { "tool call" }
                    }
                    pre class="mt-1 p-2 bg-[#9e9fc2]/15 rounded font-mono text-[11px] overflow-x-auto whitespace-pre-wrap break-all" {
                        (args)
                    }
                }
            }
            @for result in &t.tool_results {
                details class="text-[11px]" {
                    summary class="cursor-pointer text-[#574f7d] hover:text-[#3e375d]" {
                        "↩ " span class="italic text-[#574f7d]/70" { "tool result (truncated to 2k chars)" }
                    }
                    pre class="mt-1 p-2 bg-[#9e9fc2]/15 rounded font-mono text-[11px] overflow-x-auto whitespace-pre-wrap" {
                        (result)
                    }
                }
            }
            @for think in &t.thinking {
                details class="text-[11px]" {
                    summary class="cursor-pointer text-[#574f7d]/60 italic hover:text-[#3e375d]" {
                        "💭 thinking"
                    }
                    div class="mt-1 p-2 bg-[#9e9fc2]/10 rounded italic text-[#574f7d] whitespace-pre-wrap" {
                        (think)
                    }
                }
            }
        }
    }
}

/// A prose block from a user or assistant message. Preserves line breaks and
/// doesn't try to do any markdown rendering — that's MVP-out-of-scope and
/// risks mis-rendering pasted code. `whitespace-pre-wrap` gives us newlines
/// without XSS risk since maud escapes by default.
fn render_prose_block(text: &str) -> maud::Markup {
    html! {
        div class="text-[13px] leading-relaxed whitespace-pre-wrap break-words" {
            (text)
        }
    }
}

fn format_duration_ms(ms: Option<i64>) -> String {
    let Some(ms) = ms else {
        return "—".into();
    };
    let s = ms / 1_000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3_600 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{}h {}m", s / 3_600, (s % 3_600) / 60)
    }
}

pub async fn detail(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let data = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db::open(&state.db_path)?;
        let sr = db::get_session(&conn, id)?;
        let partitioned = match sr.as_ref() {
            Some(_) => db::repos_for_session_by_match(&conn, id)?,
            None => (Vec::new(), Vec::new()),
        };
        // Parse the JSONL on demand — cheap enough (~tens of ms for a big
        // session) and avoids caching transcript content in the DB, where
        // we'd have to worry about freshness + sensitive data storage.
        let turns = match sr.as_ref() {
            Some(s) => sess::parse_transcript(std::path::Path::new(&s.session.source_file))
                .unwrap_or_default(),
            None => Vec::new(),
        };
        Ok::<_, anyhow::Error>((sr, turns, partitioned))
    })
    .await
    .unwrap();

    let (session, turns, (cwd_matches, content_matches)) = match data {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                page("error", html! { p { (e.to_string()) } }),
            )
                .into_response();
        }
    };

    let Some(sr) = session else {
        return (
            StatusCode::NOT_FOUND,
            page("not found", html! { p { "session not found" } }),
        )
            .into_response();
    };

    let s = &sr.session;
    let body = html! {
        h1 class="text-lg font-semibold mb-2" {
            @if let Some(sum) = &s.summary { (sum) } @else { "(no summary)" }
        }
        section class={ (PANEL) " mt-4" } {
            dl class="grid grid-cols-[140px_1fr] gap-x-4 gap-y-1" {
                dt class="text-[#574f7d]/70" { "session uuid" }
                dd class="m-0 break-all" { (s.session_uuid) }
                dt class="text-[#574f7d]/70" { "started" }
                dd class="m-0" { (absolute_time(s.started_at)) }
                dt class="text-[#574f7d]/70" { "ended" }
                dd class="m-0" { (absolute_time(s.ended_at)) }
                dt class="text-[#574f7d]/70" { "messages" }
                dd class="m-0" { (s.message_count) }
                dt class="text-[#574f7d]/70" { "duration" }
                dd class="m-0" { (format_duration_ms(s.duration_ms)) }
                dt class="text-[#574f7d]/70" { "tokens" }
                dd class="m-0" {
                    (compact_count(s.input_tokens)) " in · "
                    (compact_count(s.output_tokens)) " out · "
                    (compact_count(s.cache_read_tokens)) " cache-read · "
                    (compact_count(s.cache_creation_tokens)) " cache-create"
                }
                dt class="text-[#574f7d]/70" { "est. cost" }
                dd class="m-0" title="rough estimate at Claude Sonnet 4.x rates — not exact" {
                    "$" (format!(
                        "{:.4}",
                        estimate_cost_usd(
                            s.input_tokens,
                            s.output_tokens,
                            s.cache_read_tokens,
                            s.cache_creation_tokens,
                        )
                    ))
                }
                dt class="text-[#574f7d]/70" { "cwd" }
                dd class="m-0 break-all" {
                    @if let Some(c) = &s.cwd { (c) } @else { span class="text-[#9e9fc2]" { "—" } }
                }
                dt class="text-[#574f7d]/70" { "source" }
                dd class={ "m-0 " (PATH) } { (s.source_file) }
            }
        }
        section class={ (PANEL) " mt-4" } {
            h2 class=(H2) { "transcript (" (turns.len()) " turns)" }
            @if turns.is_empty() {
                p class="text-[#574f7d]/70" { "no turns recorded" }
            } @else {
                (render_transcript(&turns))
            }
        }
        section class={ (PANEL) " mt-4" } {
            h2 class=(H2) { "linked repos — cwd match (" (cwd_matches.len()) ")" }
            @if cwd_matches.is_empty() {
                p class="text-[#574f7d]/70" { "no repos linked by cwd" }
            } @else {
                ul class="list-none p-0 m-0" {
                    @for (rid, name, path) in &cwd_matches {
                        li class=(LI) {
                            div class=(ROW) {
                                a class={ (LINK) " font-semibold" } href={ "/repos/" (rid) } { (name) }
                            }
                            div class=(PATH) { (path) }
                        }
                    }
                }
            }
        }
        @if !content_matches.is_empty() {
            section class={ (PANEL) " mt-4" } {
                h2 class=(H2) { "linked repos — content mention (" (content_matches.len()) ")" }
                p class="text-[11px] text-[#574f7d]/70 italic mb-2 max-w-3xl" {
                    "⚠ fuzzy: the repo name appeared as a bare word somewhere in this session's transcript. "
                    "Best-effort and likely over-counts — short names like "
                    code class="font-mono not-italic bg-[#9e9fc2]/15 px-1 rounded" { "backend" }
                    " or "
                    code class="font-mono not-italic bg-[#9e9fc2]/15 px-1 rounded" { "website" }
                    " collide with generic prose. Use these as hints, not as ground truth."
                }
                ul class="list-none p-0 m-0" {
                    @for (rid, name, path) in &content_matches {
                        li class=(LI) {
                            div class=(ROW) {
                                a class={ (LINK) " font-semibold text-[#574f7d]/80" }
                                  href={ "/repos/" (rid) } { (name) }
                            }
                            div class=(PATH) { (path) }
                        }
                    }
                }
            }
        }
    };
    page("session", body).into_response()
}
