"""Everything that is a choice rather than a fact. One repo, so constants."""
import os

REPO = "getfloresta/Floresta"
FORK = "lorenzolfm/floresta"

# Local clones scanned for unshipped branches and for the footprint. Read-only:
# the unit cannot fetch into them, so `upstream/master` is as fresh as your
# last fetch.
CLONES = [os.path.expanduser("~/Projects/misc/floresta")]
UPSTREAM_REF = "upstream/master"
# Branches that are never work in their own right.
BRANCH_IGNORE = ("backup/", "worktree-agent-")

OPPORTUNITY_CAP = 8     # board lane
DM_OPPORTUNITIES = 3    # top of that lane, in the DM
NEW_ISSUE_DAYS = 14     # "new" issue window; 🆕 in the DM is "since the last DM"
JUDGE_TTL_HOURS = 12    # re-rank at least this often even if nothing moved

# The DM goes out on the first run at or after each of these, local time. The
# board itself refreshes on every (hourly) run.
DM_SLOTS = ("08:07", "19:23")

# The org board's "Needs review" column. Checked 2026-10-14 before enabling:
# 23 open PRs, 13 of them moved in the previous two weeks -- a curated queue,
# not a column nobody uses. Needs a token with read:project.
PROJECT = {"org": "getfloresta", "number": 2, "column": "Needs review",
           "enabled": True}
