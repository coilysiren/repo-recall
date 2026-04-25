//! Git-subprocess helpers. `scan()` pulls `git log`; `remote_info()` pulls
//! the default-branch + origin URL. We shell out rather than linking libgit2:
//! system `git` is everywhere Rust already is, one subprocess per repo is
//! cheap at our scale, and the output is plain text we can stream.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// One commit — enough to power a recent-activity list. We pull full SHA,
/// author timestamp (unix), name, email, and subject. Body/stats are
/// intentionally out of scope for MVP.
#[derive(Debug, Clone)]
pub struct CommitRecord {
    pub sha: String,
    pub author_name: String,
    pub author_email: String,
    pub timestamp: i64,
    pub subject: String,
}

/// Run `git log` in `repo_path` and parse the last `limit` commits across all
/// refs. Merges are excluded — they clutter the feed without adding signal.
///
/// Returns an empty vec rather than erroring if `git` can't run the log (e.g.
/// a shallow clone with a corrupted ref). Individual-repo errors shouldn't
/// fail the whole scan.
pub fn scan(repo_path: &Path, limit: usize) -> Result<Vec<CommitRecord>> {
    let path_str = repo_path.to_str().context("repo path is not valid utf-8")?;

    // `--format` uses NUL (\0, `%x00`) as field separator so commit subjects
    // containing tabs/pipes/newlines don't confuse us. Newlines between
    // records are preserved because git log emits LF between entries.
    let output = Command::new("git")
        .args([
            "-C",
            path_str,
            "log",
            "--all",
            "--no-merges",
            "-n",
            &limit.to_string(),
            "--format=%H%x00%at%x00%an%x00%ae%x00%s",
        ])
        .output()
        .with_context(|| format!("failed to invoke git in {}", repo_path.display()))?;

    if !output.status.success() {
        // Log and move on — a broken repo shouldn't kill the whole refresh.
        tracing::debug!(
            "git log failed in {}: {}",
            repo_path.display(),
            String::from_utf8_lossy(&output.stderr).trim(),
        );
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut out = Vec::new();
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(5, '\0').collect();
        if parts.len() != 5 {
            tracing::debug!(
                "skip malformed git log line in {}: {line:?}",
                repo_path.display()
            );
            continue;
        }
        let Ok(ts) = parts[1].parse::<i64>() else {
            continue;
        };
        out.push(CommitRecord {
            sha: parts[0].to_string(),
            timestamp: ts,
            author_name: parts[2].to_string(),
            author_email: parts[3].to_string(),
            subject: parts[4].to_string(),
        });
    }
    Ok(out)
}

/// Origin metadata for a repo — raw normalized base URL (suitable for
/// building `.../tree/<branch>` links) and the short default branch name.
/// Either field can be `None`: the repo may have no `origin`, origin/HEAD
/// may not be set locally (common after a fresh clone before the first
/// fetch), or the URL may be in a form we can't parse (exotic SSH config).
#[derive(Debug, Clone, Default)]
pub struct RemoteInfo {
    pub url: Option<String>,
    pub default_branch: Option<String>,
}

pub fn remote_info(repo_path: &Path) -> RemoteInfo {
    let Some(path_str) = repo_path.to_str() else {
        return RemoteInfo::default();
    };

    let url = Command::new("git")
        .args(["-C", path_str, "remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .and_then(|raw| normalize_remote_url(&raw));

    // `symbolic-ref refs/remotes/origin/HEAD` prints e.g. `refs/remotes/origin/main`.
    // It's purely local — no network hit — and fails cleanly when unset.
    let default_branch = Command::new("git")
        .args(["-C", path_str, "symbolic-ref", "refs/remotes/origin/HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .and_then(|s| s.strip_prefix("refs/remotes/origin/").map(str::to_string));

    RemoteInfo {
        url,
        default_branch,
    }
}

#[derive(Debug, Clone)]
pub struct FileChange {
    pub sha: String,
    pub author_email: String,
    pub timestamp: i64,
    pub file_path: String,
    pub additions: i64,
    pub deletions: i64,
}

/// Walk `git log --numstat` in a single subprocess per repo and return one
/// `FileChange` per (commit, file) pair. Merges excluded; binary rows
/// (`-\t-\t<path>`) skipped. This replaces the old `churn_since`
/// aggregate — callers can sum for total churn, group-by for per-file
/// hotspots, filter by author for "my churn", etc.
///
/// Format string: `%H|%at|%ae` commit headers followed by numstat rows.
/// A pipe separator is safe here — emails and SHAs don't contain them —
/// and keeps the output trivially parseable as "header or numstat row".
pub fn file_changes_since(repo_path: &Path, since_ts: i64) -> Vec<FileChange> {
    let Some(path_str) = repo_path.to_str() else {
        return Vec::new();
    };
    let output = match Command::new("git")
        .args([
            "-C",
            path_str,
            "log",
            &format!("--since=@{since_ts}"),
            "--no-merges",
            "--pretty=format:H|%H|%at|%ae",
            "--numstat",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::debug!(
                "git log --numstat failed in {}: {}",
                repo_path.display(),
                String::from_utf8_lossy(&o.stderr).trim(),
            );
            return Vec::new();
        }
        Err(e) => {
            tracing::debug!("git subprocess failed in {}: {e}", repo_path.display());
            return Vec::new();
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut out = Vec::new();
    let mut cur_sha = String::new();
    let mut cur_ts: i64 = 0;
    let mut cur_email = String::new();
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("H|") {
            // Commit header row: H|sha|timestamp|email.
            let mut parts = rest.splitn(3, '|');
            cur_sha = parts.next().unwrap_or("").to_string();
            cur_ts = parts
                .next()
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            cur_email = parts.next().unwrap_or("").to_string();
            continue;
        }
        // Numstat row: `<adds>\t<dels>\t<path>`. Binary files = `-\t-\t…`.
        let mut parts = line.splitn(3, '\t');
        let Some(add_s) = parts.next() else { continue };
        let Some(del_s) = parts.next() else { continue };
        let Some(path) = parts.next() else { continue };
        let Ok(add) = add_s.parse::<i64>() else {
            continue;
        };
        let Ok(del) = del_s.parse::<i64>() else {
            continue;
        };
        // Git reports renames as `old => new` or `{dir => other}/file`;
        // keep it simple — record the raw path string as git emits it.
        out.push(FileChange {
            sha: cur_sha.clone(),
            author_email: cur_email.clone(),
            timestamp: cur_ts,
            file_path: path.to_string(),
            additions: add,
            deletions: del,
        });
    }
    out
}

/// Legacy helper kept for backward compatibility with callers that just
/// want the total. Internally sums the per-file rows.
pub fn churn_since(repo_path: &Path, since_ts: i64) -> i64 {
    let Some(path_str) = repo_path.to_str() else {
        return 0;
    };
    // `--pretty=format:` suppresses the per-commit header so stdout is pure
    // numstat rows. `--since=@<unix>` is git's epoch-time form.
    let output = match Command::new("git")
        .args([
            "-C",
            path_str,
            "log",
            &format!("--since=@{since_ts}"),
            "--no-merges",
            "--pretty=format:",
            "--numstat",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::debug!(
                "git log --numstat failed in {}: {}",
                repo_path.display(),
                String::from_utf8_lossy(&o.stderr).trim(),
            );
            return 0;
        }
        Err(e) => {
            tracing::debug!("git subprocess failed in {}: {e}", repo_path.display());
            return 0;
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut total: i64 = 0;
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split('\t');
        let add = parts
            .next()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let del = parts
            .next()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        total += add + del;
    }
    total
}

/// Local-state snapshot of a repo — everything we can learn from plain `git`
/// subprocess calls that changes between refreshes. One struct, one refresh
/// pass, all `git` calls share the same cwd for cache friendliness.
#[derive(Debug, Clone, Default)]
pub struct LocalState {
    pub commits_ahead: i64,
    pub commits_behind: i64,
    pub stash_count: i64,
    /// Short ref name (e.g. "main") or the literal string "detached".
    pub head_ref: Option<String>,
    /// `rebase` / `merge` / `cherry-pick` / `bisect` / `revert` when there's
    /// an interrupted operation in `.git/`. `None` when clean.
    pub in_progress_op: Option<String>,
}

pub fn local_state(repo_path: &Path) -> LocalState {
    let Some(path_str) = repo_path.to_str() else {
        return LocalState::default();
    };
    let git = |args: &[&str]| -> Option<String> {
        let mut full = vec!["-C", path_str];
        full.extend_from_slice(args);
        let out = Command::new("git").args(full).output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    // HEAD: symbolic ref gives branch name; failure means detached.
    let head_ref = git(&["symbolic-ref", "--quiet", "--short", "HEAD"]).or_else(|| {
        // Distinguish "detached" from "unborn HEAD" (brand-new empty repo):
        // the latter fails both symbolic-ref and rev-parse HEAD.
        git(&["rev-parse", "--verify", "HEAD"]).map(|_| "detached".to_string())
    });

    // ahead/behind upstream via `rev-list --left-right --count @{u}...HEAD`.
    // That prints `<behind>\t<ahead>` — count of commits upstream has that
    // HEAD doesn't, then vice versa. Requires an upstream; if none, default 0.
    let (behind, ahead) = git(&["rev-list", "--left-right", "--count", "@{u}...HEAD"])
        .and_then(|s| {
            let mut parts = s.split_whitespace();
            let b: i64 = parts.next()?.parse().ok()?;
            let a: i64 = parts.next()?.parse().ok()?;
            Some((b, a))
        })
        .unwrap_or((0, 0));

    let stash_count = git(&["stash", "list"])
        .map(|s| s.lines().filter(|l| !l.is_empty()).count() as i64)
        .unwrap_or(0);

    // `.git/` state files indicate an interrupted operation. Check in order
    // of how common they are. `git_dir` handles worktrees.
    let in_progress_op = git(&["rev-parse", "--git-dir"]).and_then(|git_dir| {
        let g = std::path::Path::new(&git_dir);
        let checks: &[(&str, &str)] = &[
            ("rebase", "rebase-merge"),
            ("rebase", "rebase-apply"),
            ("merge", "MERGE_HEAD"),
            ("cherry-pick", "CHERRY_PICK_HEAD"),
            ("revert", "REVERT_HEAD"),
            ("bisect", "BISECT_LOG"),
        ];
        for (op, marker) in checks {
            if g.join(marker).exists() {
                return Some((*op).to_string());
            }
        }
        None
    });

    LocalState {
        commits_ahead: ahead,
        commits_behind: behind,
        stash_count,
        head_ref,
        in_progress_op,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Untracked,
    Modified,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::Untracked => "untracked",
            FileKind::Modified => "modified",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorktreeFile {
    pub path: String,
    pub kind: FileKind,
}

/// Working-tree snapshot. Full counts for every dirty file in the tree, plus
/// a capped sample of the individual paths (so the dashboard can show a few
/// without exploding on a monorepo's thousand-file refactor).
#[derive(Debug, Clone, Default)]
pub struct WorktreeSnapshot {
    pub files: Vec<WorktreeFile>,
    pub total_untracked: i64,
    pub total_modified: i64,
}

impl WorktreeSnapshot {
    pub fn total(&self) -> i64 {
        self.total_untracked + self.total_modified
    }
}

/// Run `git status --porcelain=v1 -uall` and return counts + the first
/// `paths_cap` file paths. Format (from git docs): each line is `XY <path>`
/// where X/Y are status codes. `??` = untracked; anything else = tracked
/// and dirty (staged, unstaged, renamed, etc.). `-uall` expands untracked
/// directories to individual files so the count matches what `git status`
/// shows a human.
///
/// Returns an empty snapshot on any failure — one rough repo shouldn't
/// abort the whole refresh.
pub fn worktree_snapshot(repo_path: &Path, paths_cap: usize) -> WorktreeSnapshot {
    let Some(path_str) = repo_path.to_str() else {
        return WorktreeSnapshot::default();
    };
    let output = match Command::new("git")
        .args(["-C", path_str, "status", "--porcelain=v1", "-uall"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::debug!(
                "git status failed in {}: {}",
                repo_path.display(),
                String::from_utf8_lossy(&o.stderr).trim(),
            );
            return WorktreeSnapshot::default();
        }
        Err(e) => {
            tracing::debug!("git subprocess failed in {}: {e}", repo_path.display());
            return WorktreeSnapshot::default();
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut snap = WorktreeSnapshot::default();
    for line in stdout.lines() {
        if line.len() < 4 {
            continue;
        }
        // Porcelain v1: "XY path" — two status chars, a space, then the path.
        // Rename lines look like `R  old -> new`; take the final path.
        let kind = if line.starts_with("??") {
            FileKind::Untracked
        } else {
            FileKind::Modified
        };
        let rest = &line[3..];
        let path = rest.rsplit(" -> ").next().unwrap_or(rest).trim();
        match kind {
            FileKind::Untracked => snap.total_untracked += 1,
            FileKind::Modified => snap.total_modified += 1,
        }
        if snap.files.len() < paths_cap {
            snap.files.push(WorktreeFile {
                path: path.to_string(),
                kind,
            });
        }
    }
    snap
}

/// State of the local `gh` CLI install — drives a startup banner so the
/// user knows why the CI column might be empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GhHealth {
    /// `gh` runs and reports an authenticated account.
    #[default]
    Ok,
    /// `gh` is installed but not logged in. `gh auth login` fixes it.
    NotAuthenticated,
    /// Couldn't invoke `gh` at all (not installed / not on PATH).
    Missing,
}

/// Probe the `gh` install. Cheap — two subprocesses that finish in ms — so
/// safe to call at startup and on every refresh. Never returns an error:
/// any unexpected subprocess failure collapses to `Missing`.
pub fn gh_health() -> GhHealth {
    let Ok(output) = Command::new("gh").arg("--version").output() else {
        return GhHealth::Missing;
    };
    if !output.status.success() {
        return GhHealth::Missing;
    }
    match Command::new("gh").args(["auth", "status"]).output() {
        Ok(o) if o.status.success() => GhHealth::Ok,
        _ => GhHealth::NotAuthenticated,
    }
}

/// Latest default-branch CI run outcome via `gh run list`. Returns one of
/// `"success"`, `"failure"`, `"running"`, `"pending"`, or `None` if we
/// couldn't determine it (no `gh`, not authenticated, not a GitHub repo, no
/// workflow runs yet, etc.). This is the only function in this module that
/// makes a network call; callers should assume it can be slow + failure-
/// prone and run it off the main refresh path.
///
/// `owner_repo` is in GitHub's `OWNER/NAME` form (e.g. `coilysiren/repo-recall`).
pub fn ci_status(owner_repo: &str, default_branch: &str) -> Option<String> {
    // `gh run list --json status,conclusion -L 1 --branch <b> -R <owner/repo>`
    // — returns a JSON array with at most one object. We map the combo of
    // `status` and `conclusion` to a single status string.
    let output = Command::new("gh")
        .args([
            "run",
            "list",
            "--json",
            "status,conclusion",
            "-L",
            "1",
            "--branch",
            default_branch,
            "-R",
            owner_repo,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        tracing::debug!(
            "gh run list failed for {owner_repo}@{default_branch}: {}",
            String::from_utf8_lossy(&output.stderr).trim(),
        );
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let runs: Vec<serde_json::Value> = serde_json::from_str(stdout.trim()).ok()?;
    let run = runs.into_iter().next()?;
    let status = run.get("status").and_then(|v| v.as_str()).unwrap_or("");
    let conclusion = run.get("conclusion").and_then(|v| v.as_str()).unwrap_or("");
    // `status` covers the run state; `conclusion` is only populated once
    // the run finishes. Normalise into a small, stable vocabulary.
    let out = match (status, conclusion) {
        ("completed", "success") => "success",
        ("completed", "failure" | "startup_failure" | "timed_out") => "failure",
        ("completed", _) => "success", // cancelled / skipped / neutral: treat as non-urgent
        ("in_progress", _) => "running",
        ("queued" | "pending" | "requested" | "waiting", _) => "pending",
        _ => return None,
    };
    Some(out.to_string())
}

/// Aggregated open-PR counts for one repo. Derived client-side from a
/// single `gh pr list --json` call so we only pay one subprocess per repo
/// for the PR view.
#[derive(Debug, Clone, Copy, Default)]
pub struct PrCounts {
    pub open: i64,
    pub draft: i64,
    pub awaiting_my_review: i64,
    pub mine_awaiting_review: i64,
}

/// Fetch PR counts + open-issue total for a GitHub repo in one GraphQL
/// call. Replaces the previous pair (`gh pr list` + `gh issue list`),
/// halving the per-repo gh subprocess + API cost on the remote pass.
///
/// `my_login` is the viewer's GitHub handle. Empty string is fine — the
/// reviewer-split fields just stay zero. Returns `None` on any gh failure
/// (network, auth, parse) so one repo can't break the refresh.
///
/// PRs are capped at 100 (GraphQL's hard `first` limit on connection
/// fields). Plenty of headroom for our usage; if some future repo opens
/// over 100 simultaneous PRs, the counts saturate but the dashboard
/// still functions. Issues use GraphQL `totalCount`, which is exact
/// regardless of how many open issues a repo has — no client-side cap.
pub fn fetch_pr_and_issue_counts(owner_repo: &str, my_login: &str) -> Option<(PrCounts, i64)> {
    let (owner, name) = owner_repo.split_once('/')?;
    let query = r#"
        query($owner: String!, $name: String!) {
          repository(owner: $owner, name: $name) {
            issues(states: OPEN) { totalCount }
            pullRequests(first: 100, states: OPEN) {
              nodes {
                isDraft
                author { login }
                reviewRequests(first: 50) {
                  nodes {
                    requestedReviewer {
                      ... on User { login }
                    }
                  }
                }
              }
            }
          }
        }
    "#;
    let output = Command::new("gh")
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={query}"),
            "-F",
            &format!("owner={owner}"),
            "-F",
            &format!("name={name}"),
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        tracing::debug!(
            "gh api graphql failed for {owner_repo}: {}",
            String::from_utf8_lossy(&output.stderr).trim(),
        );
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let body: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let repo = body.get("data")?.get("repository")?;

    let issues = repo
        .get("issues")
        .and_then(|i| i.get("totalCount"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let prs = repo
        .get("pullRequests")
        .and_then(|p| p.get("nodes"))
        .and_then(|n| n.as_array())
        .cloned()
        .unwrap_or_default();

    let mut counts = PrCounts::default();
    for pr in &prs {
        counts.open += 1;
        let is_draft = pr.get("isDraft").and_then(|v| v.as_bool()).unwrap_or(false);
        if is_draft {
            counts.draft += 1;
        }
        let author_login = pr
            .get("author")
            .and_then(|a| a.get("login"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let reviewers: Vec<&str> = pr
            .get("reviewRequests")
            .and_then(|v| v.get("nodes"))
            .and_then(|n| n.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        r.get("requestedReviewer")
                            .and_then(|rr| rr.get("login"))
                            .and_then(|l| l.as_str())
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !my_login.is_empty() && reviewers.contains(&my_login) {
            counts.awaiting_my_review += 1;
        }
        if !my_login.is_empty() && author_login == my_login && !is_draft {
            counts.mine_awaiting_review += 1;
        }
    }
    Some((counts, issues))
}

/// Current viewer's GitHub login via `gh api user --json login`. Called once
/// at startup + on each refresh; cheap, and lets us flag PRs involving "me".
pub fn my_gh_login() -> Option<String> {
    let output = Command::new("gh")
        .args(["api", "user", "--jq", ".login"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Pull `OWNER/NAME` out of a normalised remote URL like
/// `https://github.com/coilysiren/repo-recall`. Returns `None` for non-
/// GitHub remotes (we can only drive `gh` against GitHub). We don't try to
/// be clever with enterprise GHE hosts yet — if that becomes real we'll
/// match on the host whitelist `gh` itself reads.
pub fn github_owner_repo(remote_url: &str) -> Option<String> {
    let rest = remote_url.strip_prefix("https://github.com/")?;
    let trimmed = rest.trim_end_matches('/');
    // Expect exactly `owner/repo`; reject nested paths like `/tree/main/...`.
    let mut parts = trimmed.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// Turn a raw git remote URL (`git@github.com:owner/repo.git`,
/// `https://github.com/owner/repo.git`, `ssh://git@host:22/owner/repo`, …)
/// into a browsable HTTPS base — no trailing `.git`, no trailing slash.
/// Returns `None` if we can't confidently produce one.
fn normalize_remote_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    // SSH shorthand: `git@host:path`. Split on the first ':' *after* the `@`.
    let normalized = if let Some(rest) = raw.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        format!("https://{host}/{path}")
    } else if let Some(rest) = raw.strip_prefix("ssh://") {
        // `ssh://[user@]host[:port]/path` → `https://host/path`.
        let after_user = rest.split_once('@').map(|(_, h)| h).unwrap_or(rest);
        let (host_with_port, path) = after_user.split_once('/')?;
        let host = host_with_port.split(':').next().unwrap_or(host_with_port);
        format!("https://{host}/{path}")
    } else if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        return None;
    };

    let trimmed = normalized.trim_end_matches('/').trim_end_matches(".git");
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_remote_url;

    #[test]
    fn normalizes_ssh_shorthand() {
        assert_eq!(
            normalize_remote_url("git@github.com:coilysiren/repo-recall.git").as_deref(),
            Some("https://github.com/coilysiren/repo-recall"),
        );
    }

    #[test]
    fn normalizes_https() {
        assert_eq!(
            normalize_remote_url("https://gitlab.com/org/proj.git/").as_deref(),
            Some("https://gitlab.com/org/proj"),
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(normalize_remote_url("not-a-url").is_none());
        assert!(normalize_remote_url("").is_none());
    }

    #[test]
    fn extracts_github_owner_repo() {
        use super::github_owner_repo;
        assert_eq!(
            github_owner_repo("https://github.com/coilysiren/repo-recall").as_deref(),
            Some("coilysiren/repo-recall"),
        );
        assert_eq!(
            github_owner_repo("https://github.com/coilysiren/repo-recall/").as_deref(),
            Some("coilysiren/repo-recall"),
        );
        assert!(github_owner_repo("https://gitlab.com/a/b").is_none());
        assert!(github_owner_repo("https://github.com/only-one").is_none());
        assert!(github_owner_repo("https://github.com/a/b/tree/main").is_none());
    }
}
