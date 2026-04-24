use std::path::Path;

use axum::response::Html;
use chrono::{DateTime, Utc};
use maud::{html, Markup, DOCTYPE};

use crate::commits::GhHealth;
use crate::db;

pub fn page(title: &str, body: Markup) -> Html<String> {
    page_with_banners(title, body, None)
}

/// Renders a page with an optional warning banner between the header and
/// the main content. Used for "`gh` not available" today; any other global
/// "you should know" signals would ride here too.
pub fn page_with_banners(title: &str, body: Markup, gh_health: Option<GhHealth>) -> Html<String> {
    Html(layout_with_banners(title, body, gh_health).into_string())
}

pub fn layout(title: &str, body: Markup) -> Markup {
    layout_with_banners(title, body, None)
}

fn layout_with_banners(title: &str, body: Markup, gh_health: Option<GhHealth>) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" class="h-full" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width,initial-scale=1";
                title { "repo-recall — " (title) }
                link rel="icon" type="image/svg+xml" href="/static/favicon.svg";
                link rel="preconnect" href="https://fonts.googleapis.com";
                link rel="preconnect" href="https://fonts.gstatic.com" crossorigin;
                link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Roboto:wght@400;700&display=swap";
                meta name="description" content="Local Claude Code session index — joins sessions to git repos on disk.";
                meta name="color-scheme" content="light";
                script src="https://cdn.jsdelivr.net/npm/@tailwindcss/browser@4" {}
                script src="https://unpkg.com/htmx.org@2.0.3" {}
                script src="https://unpkg.com/htmx-ext-ws@2.0.1" {}
                link rel="stylesheet" href="/static/style.css";
                script src="/static/livereload.js" defer {}
            }
            body class="h-full bg-[#e0dde5] text-[#3e375d] text-sm leading-6 antialiased" {
                header class="flex items-baseline gap-4 px-6 py-4 border-b border-[#9e9fc2]/50 bg-[#c9dcd5]" {
                    a class="font-bold text-base text-[#3e375d] hover:no-underline" href="/" { "repo-recall" }
                    span class="text-[#574f7d]/70 text-xs" { "local claude code session index" }
                    form method="get" action="/search" class="ml-auto flex" {
                        input name="q" placeholder="search…"
                              class="px-2 py-1 text-xs rounded-md border border-[#9e9fc2]/60
                                     bg-white/70 text-[#3e375d] placeholder:text-[#574f7d]/50
                                     focus:outline-none focus:border-[#574f7d] focus:bg-white";
                    }
                }
                (gh_health_banner(gh_health))
                main class="max-w-7xl mx-auto p-6" { (body) }
            }
        }
    }
}

pub fn relative_time(ts: Option<i64>) -> String {
    let Some(ts) = ts else {
        return "—".to_string();
    };
    let Some(dt) = DateTime::<Utc>::from_timestamp(ts, 0) else {
        return "—".to_string();
    };
    let now = Utc::now();
    let dur = now.signed_duration_since(dt);
    let secs = dur.num_seconds();
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 86_400 * 30 {
        format!("{}d ago", secs / 86_400)
    } else {
        dt.format("%Y-%m-%d").to_string()
    }
}

/// Compact count formatting for dashboard pills — keeps small numbers exact
/// and collapses large ones to `1.2k` / `3.4M`. Negative inputs are clamped
/// to zero since these are always activity counts.
pub fn compact_count(n: i64) -> String {
    let n = n.max(0);
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

pub fn absolute_time(ts: Option<i64>) -> String {
    let Some(ts) = ts else {
        return "—".to_string();
    };
    let Some(dt) = DateTime::<Utc>::from_timestamp(ts, 0) else {
        return "—".to_string();
    };
    dt.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// A repo's display name, relative to the scan root when possible.
/// For a scan root of `~/projects` and a repo at `~/projects/coilysiren/backend`,
/// returns `coilysiren/backend`. Falls back to the repo's basename if the path
/// isn't under the scan root (shouldn't happen in normal use, but don't panic).
pub fn display_name(repo: &db::Repo, scan_cwd: &Path) -> String {
    let p = Path::new(&repo.path);
    match p.strip_prefix(scan_cwd) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => repo.name.clone(),
    }
}

// Class bundles reused across templates. Define once, keep markup shorter.
// Palette is borrowed from coilysiren.me: light-blue #dff0ea, faint-purple
// #9e9fc2, light-purple #9192bb, mid-purple #574f7d, dark-purple #3e375d.
// Type hierarchy + hover-lift conventions borrowed from eco-mcp.coilysiren.me:
// uppercase label + big value, card hover = subtle border shift (no translate
// on panels themselves — only on pressable CTAs / LI rows).
pub const PANEL: &str =
    "bg-[#f5f3f9] border border-[#9e9fc2]/45 rounded-md p-4 shadow-sm transition-shadow \
     hover:shadow-md";
/// Action-required variant — panel-level equivalent of `PILL_ALERT`. Stays
/// inside the palette (mid-purple / dark-purple) and uses a thicker
/// left-border accent + header tint so "something needs doing here" reads
/// at a glance without adopting a separate red theme.
pub const PANEL_ALERT: &str =
    "bg-[#f5f3f9] border border-[#9e9fc2]/45 border-l-4 border-l-[#574f7d] \
     rounded-md p-4 shadow-sm transition-shadow hover:shadow-md";
pub const H2: &str = "text-[11px] text-[#574f7d] font-bold uppercase tracking-[0.08em] mb-3";
pub const LI: &str = "py-2 px-2 -mx-2 border-b border-[#9e9fc2]/25 last:border-0 rounded \
     transition-colors hover:bg-[#9e9fc2]/10";
pub const ROW: &str = "flex gap-3 items-baseline flex-wrap";
pub const META: &str = "text-[#574f7d]/70 text-xs";
pub const PILL: &str = "bg-[#9e9fc2]/25 text-[#574f7d] text-[11px] px-2 py-0.5 rounded-full \
     border border-[#9e9fc2]/30 hover:bg-[#9192bb]/40 transition-colors";
/// Attention-grabbing variant for "needs your eyes" signals (failing CI,
/// stale PRs, etc.). Stays within the coilysiren.me palette (mid-purple +
/// stronger contrast) rather than importing a new red — consistent with the
/// "don't overfit on color" direction while still reading as urgent next to
/// the default PILL.
pub const PILL_ALERT: &str =
    "bg-[#574f7d] text-white text-[11px] px-2 py-0.5 rounded-full font-semibold \
     border border-[#3e375d] hover:bg-[#3e375d] transition-colors";
/// Faint variant for "in progress" / informational signals.
pub const PILL_FAINT: &str =
    "bg-transparent text-[#574f7d]/60 text-[11px] px-2 py-0.5 rounded-full \
     border border-dashed border-[#9e9fc2]/50";
pub const LINK: &str = "text-[#574f7d] hover:text-[#3e375d] hover:underline";
pub const PATH: &str = "text-[#574f7d]/60 text-xs break-all font-mono";

/// Warning banner for missing / unauthenticated `gh`. Full-width strip
/// between the site header and the main content. Empty markup when `gh` is
/// healthy or when the caller didn't pass a health value (internal pages
/// that don't care).
fn gh_health_banner(health: Option<GhHealth>) -> Markup {
    let (message, hint) = match health {
        Some(GhHealth::Missing) => (
            "gh CLI not found — CI/CD status is disabled.",
            "install from cli.github.com and run `gh auth login`.",
        ),
        Some(GhHealth::NotAuthenticated) => (
            "gh CLI not authenticated — CI/CD status is disabled.",
            "run `gh auth login` to enable it.",
        ),
        _ => return html! {},
    };
    html! {
        div class="px-6 py-2 border-b border-[#9e9fc2]/50 bg-[#574f7d] text-white text-xs
                   flex items-baseline gap-2 flex-wrap" {
            span class="text-base leading-none" { "⚠" }
            span class="font-semibold" { (message) }
            span class="opacity-80 font-mono" { (hint) }
        }
    }
}
