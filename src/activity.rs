//! Activity scoring for repos — one number summarising "how lively is this
//! place" across every activity dimension we've wired up. Used to sort the
//! dashboard repo list so balanced-activity repos rise above one-dimension-
//! heavy ones.
//!
//! ## The scoring function
//!
//! For each activity dimension i, with per-repo value `xᵢ` and corpus-wide
//! max `Mᵢ`:
//!
//! ```text
//! score = Σ ln(1 + xᵢ / Mᵢ)
//! ```
//!
//! ### Why this form
//!
//! - **Rewards breadth.** A repo with some activity in every dimension beats
//!   one that's huge on a single dimension. That's exactly the "having both
//!   commits AND sessions should put you at the top" intuition, generalised.
//!   This is the same idea as a geometric mean (which multiplies terms), but
//!   expressed additively so the math stays numerically stable.
//! - **Diminishing returns on magnitude.** The first few commits / sessions
//!   on a dormant repo move it up a lot; going from 500 to 600 barely does.
//!   Matches the product-intuition: the 1000th commit is not 10× as
//!   meaningful as the 100th.
//! - **Zero-safe.** `ln(1 + 0) = 0` — a missing dimension contributes nothing
//!   but doesn't annihilate the score (unlike raw product / geometric mean).
//! - **Scales to N attributes without special casing.** Add an entry to
//!   `ATTRS` and both the score and the sort pick it up.
//! - **Normalisation by max.** Without it, commits (hundreds) would
//!   swamp sessions (single digits) and the "breadth" property degrades.
//!   Dividing by max_i puts every dim in [0, 1], so each contributes at most
//!   `ln(2) ≈ 0.69` to the score when it's at its peak.
//!
//! ### What we're not doing
//!
//! - Hand-tuned per-dimension weights. Too fiddly; the log+normalise combo
//!   does most of the work.
//! - A CES / softmin / other tunable aggregator. Overkill for a dashboard
//!   sort; we'd reach for one of those if the ranking felt wrong in practice.
//! - Recency weighting (e.g. decay by age of latest commit). `commits_30d`
//!   already does this at the data layer — attributes that want recency can
//!   bake it into their own value.

use crate::db::Repo;

/// Function that pulls one activity dimension's value off a repo.
pub type AttrFn = fn(&Repo) -> i64;

/// Three categories of repo signal, each with its own pace and cost:
///
/// - **Historical** — past activity derived from git log + Claude sessions.
///   Cheap to compute (subprocess against a local repo), covers a window
///   (e.g. 30 days).
/// - **LocalState** — what the working tree looks like *right now* on disk
///   (untracked, modified, maybe branch divergence later). Also cheap; also
///   changes between refreshes.
/// - **RemoteState** — what a remote service currently thinks: CI / CD
///   status, open PRs, review requests. Requires a network call (via `gh`
///   or the GitHub REST API), so these attributes are refreshed with a
///   tolerance for latency + failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Historical,
    LocalState,
    RemoteState,
}

/// One activity dimension: a stable key (for logs / future debugging), its
/// category (see [`Category`]), and the function that extracts its per-repo
/// value.
pub struct Attr {
    pub key: &'static str,
    pub category: Category,
    pub get: AttrFn,
}

/// Every activity dimension we've wired up. The design target is 5+ entries.
/// To add a new dimension: land the underlying count on `Repo`, then append
/// an `Attr` here. Everything downstream — score, sort, dormancy fade —
/// picks it up.
pub const ATTRS: &[Attr] = &[
    // --- Historical: past activity, cheap + offline --------------------
    Attr {
        key: "sessions",
        category: Category::Historical,
        get: |r| r.session_count,
    },
    Attr {
        key: "commits_30d",
        category: Category::Historical,
        get: |r| r.commits_30d,
    },
    Attr {
        key: "loc_churn_30d",
        category: Category::Historical,
        get: |r| r.loc_churn_30d,
    },
    Attr {
        key: "authors_30d",
        category: Category::Historical,
        get: |r| r.authors_30d,
    },
    // --- LocalState: current working tree, cheap + offline -------------
    Attr {
        // Combined signal — untracked + modified treated as one "working
        // tree is dirty" dimension. The breakdown still lives on `Repo`
        // (and in the `uncommitted_files` table) for tooltips + the
        // right-column listing, but activity scoring uses the sum.
        key: "uncommitted_files",
        category: Category::LocalState,
        get: |r| r.untracked_files + r.modified_files,
    },
    // --- RemoteState: requires a network call --------------------------
    Attr {
        // Binary: 1 if the latest default-branch CI run failed. Failing CI
        // is a strong "needs attention" signal — we want it to pull repos
        // up the activity ranking even if nothing else is moving.
        key: "ci_failing",
        category: Category::RemoteState,
        get: |r| i64::from(r.ci_status.as_deref() == Some("failure")),
    },
    Attr {
        key: "prs_awaiting_my_review",
        category: Category::RemoteState,
        get: |r| r.prs_awaiting_my_review,
    },
    Attr {
        // Not included in action-required (open PRs are informational), but
        // contributes to activity scoring so repos with active PR flow rank.
        key: "open_prs",
        category: Category::RemoteState,
        get: |r| r.open_prs,
    },
];

/// Per-attribute normaliser across a slice of repos. Uses the **median of
/// non-zero values** rather than the max. Rationale: one super-active repo
/// (a monorepo with 10× the commits of anything else) used to compress
/// everybody else's contribution toward zero; the median pins the "typical
/// active repo" at `ln(2) ≈ 0.69` per dimension, so differences between
/// smaller repos actually register.
///
/// Returns 0.0 for a dimension where no repo has any signal — `score()` then
/// contributes nothing for it rather than dividing by zero.
pub fn normalisers(repos: &[Repo]) -> Vec<f64> {
    ATTRS
        .iter()
        .map(|a| {
            let mut vs: Vec<f64> = repos
                .iter()
                .map(|r| (a.get)(r) as f64)
                .filter(|v| *v > 0.0)
                .collect();
            if vs.is_empty() {
                return 0.0;
            }
            vs.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
            let n = vs.len();
            if n % 2 == 1 {
                vs[n / 2]
            } else {
                (vs[n / 2 - 1] + vs[n / 2]) / 2.0
            }
        })
        .collect()
}

/// Sum of `ln(1 + xᵢ / norm_i)` across every activity dimension. See the
/// module docs for the "why this shape". `norms` comes from [`normalisers`].
pub fn score(repo: &Repo, norms: &[f64]) -> f64 {
    ATTRS
        .iter()
        .zip(norms)
        .map(|(attr, &norm)| {
            if norm <= 0.0 {
                return 0.0;
            }
            let v = (attr.get)(repo) as f64;
            (1.0 + v / norm).ln()
        })
        .sum()
}

/// "Action required" repos — something tangible is waiting for you. These
/// hard-sort above the activity-score ranking.
///
/// The trigger set is intentionally a subset of all `RemoteState` /
/// `LocalState` attrs — not *every* local/remote signal is actionable, only
/// the ones that ought to pull attention. Add a check here when a new
/// attribute carries the same "needs doing" weight.
///
/// Current triggers:
/// - Failing default-branch CI
/// - Dirty working tree (untracked + modified)
/// - In-progress git operation (rebase / merge / cherry-pick / revert / bisect)
/// - Detached HEAD
pub fn is_action_required(r: &Repo) -> bool {
    r.ci_status.as_deref() == Some("failure")
        || (r.untracked_files + r.modified_files) > 0
        || r.in_progress_op.is_some()
        || r.head_ref.as_deref() == Some("detached")
        || r.prs_awaiting_my_review > 0
}

/// In-place sort. First by action-required (true before false), then by
/// activity score desc, with name as the final tiebreak.
pub fn sort(repos: &mut [Repo]) {
    let ns = normalisers(repos);
    repos.sort_by(|a, b| {
        is_action_required(b)
            .cmp(&is_action_required(a))
            .then_with(|| {
                let sa = score(a, &ns);
                let sb = score(b, &ns);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// A repo is "dormant" when every activity dimension reads zero. Used for the
/// visual fade on the dashboard. Equivalent to `score(r) == 0` but doesn't
/// require the per-corpus max vector, so callers don't need to pass it in.
pub fn is_dormant(repo: &Repo) -> bool {
    ATTRS.iter().all(|a| (a.get)(repo) == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(id: i64, name: &str, sessions: i64, commits: i64) -> Repo {
        Repo {
            id,
            name: name.into(),
            path: format!("/tmp/{name}"),
            session_count: sessions,
            commits_30d: commits,
            loc_churn_30d: 0,
            untracked_files: 0,
            modified_files: 0,
            authors_30d: 0,
            ci_status: None,
            commits_ahead: 0,
            commits_behind: 0,
            stash_count: 0,
            head_ref: None,
            in_progress_op: None,
            open_prs: 0,
            draft_prs: 0,
            open_issues: 0,
            prs_awaiting_my_review: 0,
            prs_mine_awaiting_review: 0,
            remote_url: None,
            default_branch: None,
        }
    }

    #[test]
    fn balanced_beats_lopsided() {
        // A repo with some activity in every dimension should rank above one
        // that's larger in a single dimension but zero in others.
        let mut repos = vec![
            repo(1, "lopsided", 0, 20),
            repo(2, "balanced", 5, 5),
            repo(3, "dormant", 0, 0),
        ];
        sort(&mut repos);
        assert_eq!(repos[0].name, "balanced");
        assert_eq!(repos[1].name, "lopsided");
        assert_eq!(repos[2].name, "dormant");
    }

    #[test]
    fn equal_scores_break_alphabetically() {
        let mut repos = vec![repo(1, "zulu", 0, 0), repo(2, "alpha", 0, 0)];
        sort(&mut repos);
        assert_eq!(repos[0].name, "alpha");
        assert_eq!(repos[1].name, "zulu");
    }

    #[test]
    fn zero_normaliser_does_not_panic() {
        // If every repo is zero on every dim, scores are all 0 and we fall
        // back to alpha ordering without div-by-zero panics.
        let ns = normalisers(&[repo(1, "a", 0, 0)]);
        assert!(score(&repo(1, "a", 0, 0), &ns).abs() < 1e-12);
    }

    #[test]
    fn median_gives_typical_repo_a_meaningful_score() {
        // Under max-normalisation, a solo outlier would squash everyone else
        // near zero. With median-normalisation, a "typical" repo (at the
        // median) contributes exactly ln(2) per dimension it hits.
        let repos = vec![
            repo(1, "solo-giant", 0, 1000),
            repo(2, "typical-a", 0, 10),
            repo(3, "typical-b", 0, 10),
            repo(4, "typical-c", 0, 10),
        ];
        let ns = normalisers(&repos);
        // Median of {1000, 10, 10, 10} = 10; score(typical) = ln(2).
        let typical = score(&repos[1], &ns);
        assert!((typical - 2f64.ln()).abs() < 1e-9, "got {typical}");
        // Outlier still scores higher than a typical repo, just not 100×.
        let outlier = score(&repos[0], &ns);
        assert!(outlier > typical);
    }

    #[test]
    fn in_progress_and_detached_head_trigger_action_required() {
        let mut rebasing = repo(1, "rebasing", 0, 0);
        rebasing.in_progress_op = Some("rebase".into());
        assert!(is_action_required(&rebasing));

        let mut detached = repo(2, "detached", 0, 0);
        detached.head_ref = Some("detached".into());
        assert!(is_action_required(&detached));

        let mut on_main = repo(3, "on-main", 0, 0);
        on_main.head_ref = Some("main".into());
        assert!(!is_action_required(&on_main));
    }

    #[test]
    fn dormant_detection() {
        assert!(is_dormant(&repo(1, "x", 0, 0)));
        assert!(!is_dormant(&repo(1, "x", 1, 0)));
        assert!(!is_dormant(&repo(1, "x", 0, 1)));
    }

    #[test]
    fn action_required_hard_sorts_to_top() {
        // A quiet repo with failing CI should outrank a very active repo
        // whose tree is clean and CI is green.
        let mut noisy_clean = repo(1, "noisy-clean", 20, 500);
        noisy_clean.authors_30d = 15;
        noisy_clean.ci_status = Some("success".into());

        let mut dormant_broken = repo(2, "dormant-broken", 0, 0);
        dormant_broken.ci_status = Some("failure".into());

        let mut quiet_dirty = repo(3, "quiet-dirty", 0, 0);
        quiet_dirty.modified_files = 3;

        let mut quiet_clean = repo(4, "quiet-clean", 0, 0);
        quiet_clean.ci_status = Some("success".into());

        let mut repos = vec![
            noisy_clean.clone(),
            quiet_clean,
            dormant_broken.clone(),
            quiet_dirty.clone(),
        ];
        sort(&mut repos);

        // Both action-required repos come first (alpha within the bucket).
        assert!(is_action_required(&repos[0]));
        assert!(is_action_required(&repos[1]));
        assert_eq!(repos[0].name, "dormant-broken");
        assert_eq!(repos[1].name, "quiet-dirty");
        // Then the highest-activity repo, then the dormant-but-clean one.
        assert_eq!(repos[2].name, "noisy-clean");
        assert_eq!(repos[3].name, "quiet-clean");
    }
}
