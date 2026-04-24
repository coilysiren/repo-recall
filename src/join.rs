use std::path::{Path, PathBuf};

/// Given a session cwd and a slice of known repos, return the index of the
/// longest repo path that is an ancestor of (or equal to) `cwd`. Returns
/// `None` if no repo matches.
pub fn best_repo_for_cwd(cwd: &str, repos: &[(i64, PathBuf)]) -> Option<i64> {
    let cwd_canon = canonicalize(Path::new(cwd));
    let mut best: Option<(usize, i64)> = None;
    for (id, repo_path) in repos {
        if is_ancestor_or_equal(repo_path, &cwd_canon) {
            let depth = repo_path.components().count();
            match best {
                Some((best_depth, _)) if best_depth >= depth => {}
                _ => best = Some((depth, *id)),
            }
        }
    }
    best.map(|(_, id)| id)
}

fn canonicalize(p: &Path) -> PathBuf {
    dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn is_ancestor_or_equal(ancestor: &Path, descendant: &Path) -> bool {
    // Compare on macOS (case-insensitive by default) and Linux (case-sensitive)
    // consistently by lowercasing on macOS-style targets. Simpler: compare
    // components as-is; if that fails, fall back to case-insensitive.
    if descendant.starts_with(ancestor) {
        return true;
    }
    let a = ancestor.to_string_lossy().to_lowercase();
    let d = descendant.to_string_lossy().to_lowercase();
    d == a || d.starts_with(&format!("{a}/")) || d.starts_with(&format!("{a}\\"))
}
