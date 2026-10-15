"""Snapshot-shaped fixtures for the tests: the same dicts snapshot.fetch
produces, so the rules are exercised exactly as production calls them."""
import datetime as dt

ME = "lorenzolfm"
NOW = dt.datetime(2026, 10, 14, 12, 0, tzinfo=dt.timezone.utc)
REPO_URL = "https://github.com/getfloresta/Floresta"

FAIL = []
def chk(name, cond, detail=""):
    print(("  ok   " if cond else "  FAIL ") + name + ("" if cond else f"  <- {detail}"))
    if not cond:
        FAIL.append(name)

def done():
    import sys
    print("\n" + ("ALL PASS" if not FAIL else f"{len(FAIL)} FAILURES: {FAIL}"))
    sys.exit(1 if FAIL else 0)

def day(n, hour=12):
    """ISO timestamp n days before NOW."""
    return (NOW - dt.timedelta(days=n, hours=12 - hour)).strftime("%Y-%m-%dT%H:%M:%SZ")

def c(author, days_ago, body="a comment", kind="comment", bot=False):
    return {"author": author, "bot": bot, "at": day(days_ago), "body": body, "kind": kind}

def review(author, days_ago, state="COMMENTED", body="", commit=None):
    return {"author": author, "bot": False, "state": state, "body": body,
            "at": day(days_ago), "commit": commit}

def thread(*comments, resolved=False, path="crates/floresta-wire/src/node.rs"):
    return {"resolved": resolved, "outdated": False, "path": path, "comments": list(comments)}

def pr(number, author="alice", **kw):
    d = {"kind": "pr", "number": number, "title": f"pr {number}",
         "url": f"{REPO_URL}/pull/{number}", "author": author, "author_bot": False,
         "draft": False, "created": day(30), "updated": day(1), "body": "",
         "additions": 10, "deletions": 2, "changed_files": 1,
         "head_ref": f"branch-{number}", "head": "f" * 40, "head_at": day(20),
         "milestone": None, "labels": [], "assignees": [], "review_decision": None,
         "requested": [], "requested_at": {}, "reviews": [], "comments": [],
         "threads": [], "files": ["crates/floresta-wire/src/node.rs"], "truncated": []}
    d.update(kw)
    # As snapshot._pr does: a review with a body is part of the conversation.
    for r in d["reviews"]:
        if r["body"].strip() and r["state"] != "PENDING":
            d["comments"] = d["comments"] + [{"author": r["author"], "bot": False, "at": r["at"],
                                              "body": r["body"], "kind": "review:" + r["state"]}]
    d["comments"] = sorted(d["comments"], key=lambda x: x["at"])
    return d

def issue(number, author="alice", **kw):
    d = {"kind": "issue", "number": number, "title": f"issue {number}",
         "url": f"{REPO_URL}/issues/{number}", "author": author, "author_bot": False,
         "created": day(30), "updated": day(1), "body": "", "milestone": None,
         "labels": [], "assignees": [], "assigned_at": {}, "comments": [],
         "linked_prs": [], "truncated": []}
    d.update(kw)
    d["comments"] = sorted(d["comments"], key=lambda x: x["at"])
    return d

def snap(prs=(), issues=()):
    return {"prs": list(prs), "issues": list(issues)}
