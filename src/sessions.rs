use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub session_uuid: String,
    pub cwd: Option<String>,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub message_count: i64,
    pub summary: Option<String>,
    pub source_file: String,
    /// Wall-clock span in milliseconds (end - start). `None` when we saw at
    /// most one timestamp. Uses ms rather than seconds so sub-second sessions
    /// don't collapse to zero.
    pub duration_ms: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
}

/// Returns the default Claude Code projects directory: `~/.claude/projects/`.
pub fn default_projects_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = PathBuf::from(home).join(".claude").join("projects");
    if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
}

/// Enumerate every `.jsonl` file under the Claude projects directory.
pub fn list_session_files(projects_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(projects_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(p.to_path_buf());
        }
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct RawLine {
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    content: Option<serde_json::Value>,
    #[serde(default)]
    message: Option<RawMessage>,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    #[serde(default)]
    content: Option<serde_json::Value>,
    /// Claude's usage block. Real shape (verified against live JSONL):
    ///   `input_tokens`, `output_tokens`, `cache_read_input_tokens`,
    ///   `cache_creation_input_tokens`, plus richer detail we don't use.
    /// `None` for user turns and older sessions.
    #[serde(default)]
    usage: Option<serde_json::Value>,
}

/// Parse one JSONL session file into a `SessionRecord`. Returns `Ok(None)` if
/// the file doesn't yield any recognisable session data (empty, malformed, etc).
pub fn parse_session_file(path: &Path) -> Result<Option<SessionRecord>> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);

    let mut session_uuid: Option<String> = None;
    let mut cwd: Option<String> = None;
    // Milliseconds for duration; seconds for display-ts fields (historical).
    let mut first_ts_ms: Option<i64> = None;
    let mut last_ts_ms: Option<i64> = None;
    let mut first_ts: Option<i64> = None;
    let mut last_ts: Option<i64> = None;
    let mut message_count: i64 = 0;
    let mut summary: Option<String> = None;
    let mut input_tokens: i64 = 0;
    let mut output_tokens: i64 = 0;
    let mut cache_read_tokens: i64 = 0;
    let mut cache_creation_tokens: i64 = 0;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                tracing::debug!("read error in {}: {}", path.display(), e);
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let raw: RawLine = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("skip malformed line in {}: {}", path.display(), e);
                continue;
            }
        };

        if session_uuid.is_none() {
            session_uuid = raw.session_id.clone();
        }

        // Timestamp (ISO8601) → epoch seconds (for display) + epoch ms (for
        // duration, so sub-second sessions aren't rounded away).
        if let Some(ts_str) = raw.timestamp.as_deref() {
            if let Ok(dt) = DateTime::parse_from_rfc3339(ts_str) {
                let utc = dt.with_timezone(&Utc);
                let secs = utc.timestamp();
                let ms = utc.timestamp_millis();
                first_ts = Some(first_ts.map_or(secs, |cur| cur.min(secs)));
                last_ts = Some(last_ts.map_or(secs, |cur| cur.max(secs)));
                first_ts_ms = Some(first_ts_ms.map_or(ms, |cur| cur.min(ms)));
                last_ts_ms = Some(last_ts_ms.map_or(ms, |cur| cur.max(ms)));
            }
        }

        // `message.usage` on assistant turns carries the token counts. Sum
        // across every turn we see — these are per-turn values, not running
        // totals, so addition is the right aggregation.
        if let Some(usage) = raw.message.as_ref().and_then(|m| m.usage.as_ref()) {
            let pull = |k: &str| usage.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
            input_tokens += pull("input_tokens");
            output_tokens += pull("output_tokens");
            cache_read_tokens += pull("cache_read_input_tokens");
            cache_creation_tokens += pull("cache_creation_input_tokens");
        }

        // cwd is typically on user/assistant message lines.
        if cwd.is_none() {
            if let Some(c) = raw.cwd.as_deref() {
                if !c.is_empty() {
                    cwd = Some(c.to_string());
                }
            }
        }

        let line_type = raw.r#type.as_deref().unwrap_or("");

        // Count user + assistant messages only.
        if line_type == "user" || line_type == "assistant" {
            message_count += 1;
        }

        // Take the first user message content as summary, if we don't have one.
        if summary.is_none() && line_type == "user" {
            let text = extract_text(raw.message.as_ref().and_then(|m| m.content.as_ref()))
                .or_else(|| extract_text(raw.content.as_ref()));
            if let Some(t) = text {
                let trimmed = t.trim();
                if !trimmed.is_empty() {
                    summary = Some(truncate(trimmed, 200));
                }
            }
        }

        // queue-operation lines may also carry a plain string `content` with the
        // first user prompt — fall back to that if we never see a "user" line.
        if summary.is_none() && line_type == "queue-operation" {
            if let Some(t) = extract_text(raw.content.as_ref()) {
                let trimmed = t.trim();
                if !trimmed.is_empty() {
                    summary = Some(truncate(trimmed, 200));
                }
            }
        }
    }

    // Derive a session id if none was found (fall back to the filename stem).
    if session_uuid.is_none() {
        session_uuid = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());
    }

    let Some(uuid) = session_uuid else {
        return Ok(None);
    };

    let duration_ms = match (first_ts_ms, last_ts_ms) {
        (Some(a), Some(b)) if b >= a => Some(b - a),
        _ => None,
    };

    Ok(Some(SessionRecord {
        session_uuid: uuid,
        cwd,
        started_at: first_ts,
        ended_at: last_ts,
        message_count,
        summary,
        source_file: path.display().to_string(),
        duration_ms,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_creation_tokens,
    }))
}

/// Content in Claude JSONL can be a plain string or an array of blocks like
/// `[{"type":"text","text":"..."}]`. Extract the first text fragment we find.
fn extract_text(v: Option<&serde_json::Value>) -> Option<String> {
    let v = v?;
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(arr) => {
            for block in arr {
                if let Some(obj) = block.as_object() {
                    let ty = obj.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    if ty == "text" {
                        if let Some(t) = obj.get("text").and_then(|x| x.as_str()) {
                            return Some(t.to_string());
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Renderable turn for the session-detail transcript.
#[derive(Debug, Clone)]
pub struct Turn {
    pub role: TurnRole,
    pub timestamp: Option<i64>,
    /// Plain text blocks (user content, assistant prose).
    pub texts: Vec<String>,
    /// Tool uses on this turn: (tool name, serialized args JSON).
    pub tool_uses: Vec<(String, String)>,
    /// Tool results attached to this turn: truncated previews.
    pub tool_results: Vec<String>,
    /// Chain-of-thought blocks (collapsed by default on the page).
    pub thinking: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    User,
    Assistant,
    System,
}

/// Parse every user/assistant line of a JSONL and return a flat list of
/// turns for rendering. Silent on malformed lines (same tolerance as the
/// metadata pass — one bad line doesn't kill the whole view).
pub fn parse_transcript(path: &Path) -> Result<Vec<Turn>> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(raw): std::result::Result<RawLine, _> = serde_json::from_str(&line) else {
            continue;
        };
        let role = match raw.r#type.as_deref().unwrap_or("") {
            "user" => TurnRole::User,
            "assistant" => TurnRole::Assistant,
            "system" => TurnRole::System,
            _ => continue,
        };
        let ts = raw
            .timestamp
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc).timestamp());

        let mut turn = Turn {
            role,
            timestamp: ts,
            texts: Vec::new(),
            tool_uses: Vec::new(),
            tool_results: Vec::new(),
            thinking: Vec::new(),
        };

        // Walk whichever content shape we have. A `user` turn often uses
        // top-level `content` (string); assistant turns nest under
        // `message.content` as an array of typed blocks.
        let content = raw
            .message
            .as_ref()
            .and_then(|m| m.content.as_ref())
            .or(raw.content.as_ref());
        if let Some(v) = content {
            walk_content(v, &mut turn);
        }

        // Skip turns that produced nothing visible (usually tool-result
        // only bookkeeping turns from the user role).
        if !turn.texts.is_empty()
            || !turn.tool_uses.is_empty()
            || !turn.tool_results.is_empty()
            || !turn.thinking.is_empty()
        {
            out.push(turn);
        }
    }
    Ok(out)
}

/// Scan a single JSONL file for bare-word mentions of each repo name.
/// `needles` is a list of `(repo_id, name)` pairs; the name is case-folded
/// and matched with word boundaries (ASCII only — names with weird chars
/// will still match, just less cleanly).
///
/// Returns the set of matching `repo_id`s. Purely best-effort: common
/// English-word names ("backend", "website") will over-match, which is why
/// the UI labels these as fuzzy content-mentions separate from cwd matches.
pub fn mentions_in_file(path: &Path, needles: &[(i64, String)]) -> Vec<i64> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let haystack = content.to_ascii_lowercase();
    let mut hits = Vec::new();
    for (id, name) in needles {
        let n = name.to_ascii_lowercase();
        if n.is_empty() || n.len() < 3 {
            // Shorter than 3 chars is pure noise — everything matches "io",
            // "ai", etc. Skip.
            continue;
        }
        if has_word_match(&haystack, &n) {
            hits.push(*id);
        }
    }
    hits
}

/// Word-boundary substring check. A match counts when `needle` appears with
/// non-alphanumeric (or start/end) on both sides. Prevents "backend" from
/// matching "backends" or "bugbackend". Fast path uses raw substring scan
/// then validates the boundary — avoids pulling in a regex engine.
fn has_word_match(hay: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = hay[start..].find(needle) {
        let abs = start + pos;
        let before_ok = abs == 0
            || !hay
                .as_bytes()
                .get(abs - 1)
                .is_some_and(|b| b.is_ascii_alphanumeric());
        let after = abs + needle.len();
        let after_ok = after >= hay.len()
            || !hay
                .as_bytes()
                .get(after)
                .is_some_and(|b| b.is_ascii_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        start = abs + needle.len();
        if start >= hay.len() {
            break;
        }
    }
    false
}

fn walk_content(v: &serde_json::Value, turn: &mut Turn) {
    match v {
        serde_json::Value::String(s) if !s.trim().is_empty() => {
            turn.texts.push(s.clone());
        }
        serde_json::Value::Array(arr) => {
            for block in arr {
                let Some(obj) = block.as_object() else {
                    continue;
                };
                let ty = obj.get("type").and_then(|x| x.as_str()).unwrap_or("");
                match ty {
                    "text" => {
                        if let Some(t) = obj.get("text").and_then(|x| x.as_str()) {
                            turn.texts.push(t.to_string());
                        }
                    }
                    "thinking" => {
                        if let Some(t) = obj.get("thinking").and_then(|x| x.as_str()) {
                            turn.thinking.push(t.to_string());
                        }
                    }
                    "tool_use" => {
                        let name = obj
                            .get("name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("(tool)")
                            .to_string();
                        let args = obj
                            .get("input")
                            .map(|i| serde_json::to_string(i).unwrap_or_default())
                            .unwrap_or_default();
                        turn.tool_uses.push((name, args));
                    }
                    "tool_result" => {
                        // `content` here is either a string or an array of
                        // text blocks. Truncate to a preview — the raw file
                        // is there for deep inspection.
                        let text = obj
                            .get("content")
                            .map(|c| match c {
                                serde_json::Value::String(s) => s.clone(),
                                serde_json::Value::Array(arr) => arr
                                    .iter()
                                    .filter_map(|b| {
                                        b.get("text").and_then(|x| x.as_str()).map(String::from)
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                _ => String::new(),
                            })
                            .unwrap_or_default();
                        if !text.trim().is_empty() {
                            turn.tool_results.push(truncate(&text, 2_000));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}
