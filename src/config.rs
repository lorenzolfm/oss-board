//! Everything that is a choice rather than a fact. A couple of repos, so constants.

use std::fmt;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Repo {
    pub owner: &'static str,
    pub name: &'static str,
}

impl Repo {
    /// What a card calls this repo, spelled as a card key spells it.
    pub fn label(self) -> String {
        self.name.to_lowercase()
    }
}

impl fmt::Display for Repo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// A repo you actually work in. Issue numbers repeat between these, so a bare number
/// means nothing, and an item is its `CardKey`.
pub struct Project {
    pub repo: Repo,
    pub fork: Repo,
    /// The branch PRs land on.
    pub base: &'static str,
    /// The local clone, under `$HOME`. Read-only.
    clone: &'static str,
    /// What that clone calls `repo`.
    remote: &'static str,
    /// The review column on its org board.
    pub column: ReviewColumn,
}

impl Project {
    pub fn local(&self) -> LocalClone {
        LocalClone {
            repo: self.repo,
            path: home().join(self.clone),
            upstream_ref: format!("{}/{}", self.remote, self.base),
            base: self.base,
        }
    }
}

/// A project's clone and what it calls things, scanned for unshipped branches and for the
/// footprint. These go together: a ref means something only in the clone that has it.
pub struct LocalClone {
    /// What it is a clone of.
    pub repo: Repo,
    pub path: PathBuf,
    /// `repo`'s base as the clone knows it, as fresh as your last fetch: the unit can only
    /// read the clones.
    pub upstream_ref: String,
    /// The branch PRs land on, which is never work in its own right.
    pub base: &'static str,
}

pub static PROJECTS: [Project; 2] = [
    Project {
        repo: Repo {
            owner: "getfloresta",
            name: "Floresta",
        },
        fork: Repo {
            owner: "lorenzolfm",
            name: "floresta",
        },
        base: "master",
        clone: "Projects/misc/floresta",
        remote: "upstream",
        // Checked 2026-10-14 before enabling: 23 open PRs, 13 of them moved in the
        // previous two weeks -- a curated queue, not a column nobody uses.
        column: ReviewColumn::Curated(ProjectColumn {
            org: "getfloresta",
            number: 2,
            column: "Needs review",
        }),
    },
    Project {
        repo: Repo {
            owner: "cashubtc",
            name: "cdk",
        },
        fork: Repo {
            owner: "lorenzolfm",
            name: "cdk",
        },
        base: "main",
        clone: "Projects/misc/cdk",
        remote: "cashubtc",
        column: ReviewColumn::Unproven(ProjectColumn {
            org: "cashubtc",
            number: 2,
            column: "Needs Review",
        }),
    },
];

/// Branches that are never work in their own right.
pub const BRANCH_IGNORE: [&str; 2] = ["backup/", "worktree-agent-"];

/// Board lane, across every project.
pub const OPPORTUNITY_CAP: usize = 8;
/// New asks in one DM, across every project. Any beyond this wait for the next one.
pub const DM_ASKS: usize = 3;
/// A mention or reply nobody has followed up in this long has stopped being an ask.
/// If it still matters someone will ping again, and that brings it back.
pub const CONVERSATION_TTL_DAYS: f64 = 14.0;
/// The "new" issue window.
pub const NEW_ISSUE_DAYS: f64 = 14.0;
/// Re-rank at least this often even if nothing moved.
pub const JUDGE_TTL_HOURS: f64 = 12.0;

/// Bound the cost of a claim spree.
pub const MAX_BRIEFINGS: usize = 2;

/// The DM goes out on the first run at or after this, local time. The board itself
/// refreshes on every (hourly) run.
pub const DM_SLOTS: [(u32, u32); 1] = [(8, 7)];
/// A slot missed by more than this is skipped, not sent late.
pub const SLOT_GRACE_HOURS: i64 = 8;

/// A status column on an org's project board. Needs a token with read:project.
pub struct ProjectColumn {
    pub org: &'static str,
    pub number: u64,
    pub column: &'static str,
}

/// Whether a column has earned a run's attention. A name nobody uses would only add noise
/// to the ranking, so that takes checking first, and the check needs the board's address.
pub enum ReviewColumn {
    /// Not shown to be curated yet. `survey` reads it, and a run does not.
    Unproven(ProjectColumn),
    /// Checked before enabling. A run reads it too.
    Curated(ProjectColumn),
}

fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn default_board() -> PathBuf {
    home().join("Documents/Vault/Opensource/Tasks.md")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Short names, branch keys and note names all go by the label.
    #[test]
    fn no_two_projects_share_a_label() {
        let labels: HashSet<String> = PROJECTS.iter().map(|p| p.repo.label()).collect();
        assert_eq!(labels.len(), PROJECTS.len());
    }
}
