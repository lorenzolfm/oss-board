//! Everything that is a choice rather than a fact. One repo, so constants.

use std::fmt;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Repo {
    pub owner: &'static str,
    pub name: &'static str,
}

impl fmt::Display for Repo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

pub const REPO: Repo = Repo {
    owner: "getfloresta",
    name: "Floresta",
};
pub const FORK: Repo = Repo {
    owner: "lorenzolfm",
    name: "floresta",
};

/// `upstream/master` in the clones is as fresh as your last fetch: the unit can
/// only read them.
pub const UPSTREAM_REF: &str = "upstream/master";
/// Branches that are never work in their own right.
pub const BRANCH_IGNORE: [&str; 2] = ["backup/", "worktree-agent-"];

/// Board lane.
pub const OPPORTUNITY_CAP: usize = 8;
/// New asks in one DM. Any beyond this wait for the next one.
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

pub struct ProjectColumn {
    pub org: &'static str,
    pub number: u64,
    pub column: &'static str,
    pub enabled: bool,
}

/// The org board's "Needs review" column. Checked 2026-10-14 before enabling: 23
/// open PRs, 13 of them moved in the previous two weeks -- a curated queue, not a
/// column nobody uses. Needs a token with read:project.
pub const PROJECT: ProjectColumn = ProjectColumn {
    org: "getfloresta",
    number: 2,
    column: "Needs review",
    enabled: true,
};

fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn default_board() -> PathBuf {
    home().join("Documents/Vault/Opensource/Tasks.md")
}

/// Local clones scanned for unshipped branches and for the footprint. Read-only.
pub fn clones() -> Vec<PathBuf> {
    vec![home().join("Projects/misc/floresta")]
}
