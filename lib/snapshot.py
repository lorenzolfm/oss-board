"""One read of everything open in the repo, normalised for the rules.

Everything downstream -- obligations, opportunities, the judge -- is a pure
function of this snapshot, so the rules can be tested against fixtures with no
network at all.

Two queries per item kind, deliberately split:
  * the bulk pass pages through every open PR and issue with the conversation
    attached. It never asks for `mergeable` or the CI rollup: both force GitHub
    to compute state on demand, and that is what 502'd the old 91-repo scan.
  * `merge_state` asks for those two only for the handful of PRs that need
    them (yours, and the opportunity shortlist).
"""
import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import gh

PR_PAGE = 10        # reviewThreads x comments make PR pages heavy
ISSUE_PAGE = 25
MERGE_CHUNK = 10

# The newest N of each stream. A mention older than this on a busier thread is
# missed, which is why the cap is generous and truncation is reported.
COMMENTS = 60
REVIEWS = 60
THREADS = 100
THREAD_COMMENTS = 30

ACTOR = "author { __typename login }"

PR_FIELDS = f"""
  number title url body isDraft createdAt updatedAt
  additions deletions changedFiles headRefName headRefOid
  {ACTOR}
  milestone {{ title dueOn }}
  labels(first: 15) {{ nodes {{ name }} }}
  assignees(first: 5) {{ nodes {{ login }} }}
  reviewDecision
  reviewRequests(first: 15) {{ nodes {{ requestedReviewer {{ ... on User {{ login }} }} }} }}
  commits(last: 1) {{ nodes {{ commit {{ oid committedDate }} }} }}
  reviews(last: {REVIEWS}) {{ totalCount nodes {{
    {ACTOR} state body submittedAt createdAt commit {{ oid }} }} }}
  comments(last: {COMMENTS}) {{ totalCount nodes {{ {ACTOR} body createdAt }} }}
  reviewThreads(first: {THREADS}) {{ totalCount nodes {{
    isResolved isOutdated path
    comments(first: {THREAD_COMMENTS}) {{ totalCount nodes {{ {ACTOR} body createdAt }} }} }} }}
  timelineItems(last: 20, itemTypes: [REVIEW_REQUESTED_EVENT]) {{ nodes {{
    ... on ReviewRequestedEvent {{ createdAt requestedReviewer {{ ... on User {{ login }} }} }} }} }}
  files(first: 100) {{ nodes {{ path }} }}
"""

ISSUE_FIELDS = f"""
  number title url body createdAt updatedAt
  {ACTOR}
  milestone {{ title dueOn }}
  labels(first: 15) {{ nodes {{ name }} }}
  assignees(first: 5) {{ nodes {{ login }} }}
  comments(last: {COMMENTS}) {{ totalCount nodes {{ {ACTOR} body createdAt }} }}
  closedByPullRequestsReferences(first: 5, includeClosedPrs: false) {{
    nodes {{ number state {ACTOR} }} }}
  timelineItems(last: 10, itemTypes: [ASSIGNED_EVENT]) {{ nodes {{
    ... on AssignedEvent {{ createdAt assignee {{ ... on User {{ login }} }} }} }} }}
"""

def _nodes(conn):
    return [n for n in ((conn or {}).get("nodes") or []) if n]

def _actor(node):
    a = (node or {}).get("author") or {}
    login = a.get("login") or "ghost"
    bot = a.get("__typename") == "Bot" or login.endswith("[bot]")
    return login, bot

def _comment(node, kind):
    login, bot = _actor(node)
    return {"author": login, "bot": bot, "at": node.get("createdAt"),
            "body": node.get("body") or "", "kind": kind}

def _page(owner, name, conn, fields, size):
    out, cursor = [], None
    while True:
        after = f', after: "{cursor}"' if cursor else ""
        q = f'''query {{ repository(owner: "{owner}", name: "{name}") {{
          {conn}(states: OPEN, first: {size}{after}, orderBy: {{field: CREATED_AT, direction: DESC}}) {{
            pageInfo {{ hasNextPage endCursor }}
            nodes {{ {fields} }}
          }} }} }}'''
        data = gh.graphql(q)
        page = (data.get("repository") or {}).get(conn) or {}
        out.extend(_nodes(page))
        info = page.get("pageInfo") or {}
        if not info.get("hasNextPage"):
            return out
        cursor = info.get("endCursor")

def _pr(n):
    login, bot = _actor(n)
    head = (_nodes(n.get("commits")) or [{}])[0].get("commit") or {}
    stream = [_comment(c, "comment") for c in _nodes(n.get("comments"))]
    reviews = []
    for r in _nodes(n.get("reviews")):
        rl, rb = _actor(r)
        rev = {"author": rl, "bot": rb, "state": r.get("state"),
               "body": r.get("body") or "",
               "at": r.get("submittedAt") or r.get("createdAt"),
               "commit": (r.get("commit") or {}).get("oid")}
        reviews.append(rev)
        # A review's body is part of the conversation exactly like a comment;
        # an empty APPROVED click is not.
        if rev["body"].strip() and rev["state"] != "PENDING":
            stream.append({"author": rl, "bot": rb, "at": rev["at"],
                           "body": rev["body"], "kind": "review:" + rev["state"]})
    threads = []
    for t in _nodes(n.get("reviewThreads")):
        cs = [_comment(c, "thread") for c in _nodes(t.get("comments"))]
        threads.append({"resolved": bool(t.get("isResolved")),
                        "outdated": bool(t.get("isOutdated")),
                        "path": t.get("path"), "comments": cs})
    requested = {((r.get("requestedReviewer") or {}).get("login") or "")
                 for r in _nodes(n.get("reviewRequests"))} - {""}
    requested_at = {}
    for ev in _nodes(n.get("timelineItems")):
        who = (ev.get("requestedReviewer") or {}).get("login")
        if who:
            requested_at[who] = max(requested_at.get(who, ""), ev.get("createdAt") or "")
    return {
        "kind": "pr", "number": n["number"], "title": n["title"], "url": n["url"],
        "author": login, "author_bot": bot, "draft": bool(n.get("isDraft")),
        "created": n["createdAt"], "updated": n["updatedAt"],
        "body": n.get("body") or "",
        "additions": n.get("additions") or 0, "deletions": n.get("deletions") or 0,
        "changed_files": n.get("changedFiles") or 0,
        "head_ref": n.get("headRefName"), "head": n.get("headRefOid"),
        "head_at": head.get("committedDate"),
        "milestone": n.get("milestone"),
        "labels": [l["name"] for l in _nodes(n.get("labels"))],
        "assignees": [a["login"] for a in _nodes(n.get("assignees"))],
        "review_decision": n.get("reviewDecision"),
        "requested": sorted(requested), "requested_at": requested_at,
        "reviews": reviews, "comments": sorted(stream, key=lambda c: c["at"] or ""),
        "threads": threads,
        "files": [f["path"] for f in _nodes(n.get("files"))],
        "truncated": [k for k, conn, cap in (
            ("comments", n.get("comments"), COMMENTS),
            ("reviews", n.get("reviews"), REVIEWS),
            ("threads", n.get("reviewThreads"), THREADS))
            if (conn or {}).get("totalCount", 0) > cap],
    }

def _issue(n):
    login, bot = _actor(n)
    assigned_at = {}
    for ev in _nodes(n.get("timelineItems")):
        who = (ev.get("assignee") or {}).get("login")
        if who:
            assigned_at[who] = max(assigned_at.get(who, ""), ev.get("createdAt") or "")
    return {
        "kind": "issue", "number": n["number"], "title": n["title"], "url": n["url"],
        "author": login, "author_bot": bot,
        "created": n["createdAt"], "updated": n["updatedAt"],
        "body": n.get("body") or "",
        "milestone": n.get("milestone"),
        "labels": [l["name"] for l in _nodes(n.get("labels"))],
        "assignees": [a["login"] for a in _nodes(n.get("assignees"))],
        "assigned_at": assigned_at,
        "comments": sorted((_comment(c, "comment") for c in _nodes(n.get("comments"))),
                           key=lambda c: c["at"] or ""),
        "linked_prs": [{"number": p["number"], "author": _actor(p)[0]}
                       for p in _nodes(n.get("closedByPullRequestsReferences"))],
        "truncated": ["comments"] if (n.get("comments") or {}).get("totalCount", 0) > COMMENTS else [],
    }

def fetch(repo):
    """{'prs': [...], 'issues': [...]} for every open item in `repo`."""
    owner, name = repo.split("/", 1)
    prs = [_pr(n) for n in _page(owner, name, "pullRequests", PR_FIELDS, PR_PAGE)]
    issues = [_issue(n) for n in _page(owner, name, "issues", ISSUE_FIELDS, ISSUE_PAGE)]
    return {"prs": prs, "issues": issues}

def merge_state(repo, numbers):
    """{number: {'mergeable': ..., 'ci': ...}} for just these PRs."""
    owner, name = repo.split("/", 1)
    numbers = sorted(set(numbers))
    out = {}
    for i in range(0, len(numbers), MERGE_CHUNK):
        chunk = numbers[i:i + MERGE_CHUNK]
        body = "".join(f'''
          p{n}: pullRequest(number: {n}) {{ mergeable
            commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state }} }} }} }} }}'''
                       for n in chunk)
        try:
            data = gh.graphql(f'query {{ repository(owner: "{owner}", name: "{name}") {{{body}\n}} }}')
        except gh.GhError:
            continue          # unknown merge state degrades a card, never the run
        repo_node = data.get("repository") or {}
        for n in chunk:
            node = repo_node.get(f"p{n}") or {}
            commit = ((_nodes(node.get("commits")) or [{}])[0].get("commit") or {})
            out[n] = {"mergeable": node.get("mergeable"),
                      "ci": ((commit.get("statusCheckRollup") or {}).get("state")) or "NONE"}
    return out

def mine(repo, me):
    """Every PR you ever opened upstream, any state: what unshipped work is
    checked against. REST's issue list takes a creator filter and returns PRs
    too, so this is one request instead of paging 1000+ PRs."""
    import json
    raw = gh.rest(f"repos/{repo}/issues?creator={me}&state=all&per_page=100",
                  paginate=True,
                  jq='.[] | select(.pull_request) | {number, title, state}')
    numbers = [json.loads(l) for l in raw.splitlines() if l.strip()]
    if not numbers:
        return []
    owner, name = repo.split("/", 1)
    out = []
    for i in range(0, len(numbers), 20):
        chunk = numbers[i:i + 20]
        body = "".join(f'''
          p{p["number"]}: pullRequest(number: {p["number"]}) {{
            number title state headRefName headRefOid
            commits(first: 100) {{ nodes {{ commit {{ oid }} }} }} }}''' for p in chunk)
        data = gh.graphql(f'query {{ repository(owner: "{owner}", name: "{name}") {{{body}\n}} }}')
        for p in chunk:
            node = (data.get("repository") or {}).get(f"p{p['number']}")
            if node:
                out.append({"number": node["number"], "title": node["title"],
                            "state": node["state"], "head_ref": node["headRefName"],
                            "commits": {c["commit"]["oid"] for c in _nodes(node.get("commits"))}})
    return out

def _pr_diff(repo, sha):
    """{file: patch-without-hunk-headers} for the PR as it stood at `sha`.
    Three-dot compare diffs from the merge base, which is the PR's own diff at
    that commit no matter how far master has moved since."""
    import json
    raw = gh.rest(f"repos/{repo}/compare/master...{sha}",
                  jq='[.files[] | {filename, patch: (.patch // "")}]')
    return {f["filename"]: "\n".join(l for l in f["patch"].splitlines() if not l.startswith("@@"))
            for f in json.loads(raw or "[]")}

def ack_interdiff(repo, acked, head):
    """What changed between the commit you ACKed and the head, ignoring where
    in master each sits: {'rebase_only': bool, 'changed': [files]} or None if
    either side cannot be compared (an ACKed commit GitHub has since dropped)."""
    try:
        before, after = _pr_diff(repo, acked), _pr_diff(repo, head)
    except gh.GhError:
        return None
    changed = sorted(f for f in set(before) | set(after) if before.get(f) != after.get(f))
    return {"rebase_only": not changed, "changed": changed}
