use std::path::{Path, PathBuf};

use anyhow::Result;

const SKIP_DIRS: &[&str] = &["node_modules", "target", "dist", "build", ".venv", "venv"];

#[derive(Debug, Clone)]
pub struct DiscoveredRepo {
    pub path: PathBuf,
    pub name: String,
}

/// Scan `root` and up to `max_depth` levels down for directories containing a
/// `.git` entry (file or directory — worktrees use a file).
///
/// Depth 0 = root itself; depth 1 = immediate children; etc.
pub fn scan(root: &Path, max_depth: usize) -> Result<Vec<DiscoveredRepo>> {
    let root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    walk(&root, 0, max_depth, &mut out, &mut seen, true)?;
    Ok(out)
}

fn walk(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<DiscoveredRepo>,
    seen: &mut std::collections::HashSet<PathBuf>,
    is_root: bool,
) -> Result<()> {
    // Is this directory itself a repo?
    let git_entry = dir.join(".git");
    if git_entry.exists() {
        let canon = dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        if seen.insert(canon.clone()) {
            let name = canon
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| canon.display().to_string());
            out.push(DiscoveredRepo { path: canon, name });
        }
        // Don't descend into a repo — skip nested submodules / vendor git dirs.
        return Ok(());
    }

    if depth >= max_depth {
        return Ok(());
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!("read_dir({}) failed: {}", dir.display(), e);
            return Ok(());
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if SKIP_DIRS.contains(&name) {
            continue;
        }
        // Skip hidden directories (except we still started at root even if hidden).
        if name.starts_with('.') && !(is_root && depth == 0) {
            continue;
        }
        walk(&path, depth + 1, max_depth, out, seen, false)?;
    }
    Ok(())
}
