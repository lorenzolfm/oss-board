"""Your Floresta work that exists but never reached upstream.

Two sources:
  * open PRs against your own fork. They look reviewed-and-done there, but
    nobody upstream has seen them.
  * local branches carrying commits you wrote that upstream does not have.

The false positives the old stranded scan produced are the design here:
  * a branch counts only for commits *you* authored that `git cherry` says are
    not upstream -- so a checkout of someone else's PR for review never does;
  * anything already carried by an upstream PR of yours, in any state, is
    hidden: matched on title, branch name, or commit. A fork PR you
    later opened upstream (fork #8 -> #1330) is not stranded;
  * branches named after a PR (`pr-1105`, `review-1252`) are review checkouts.
"""
import os, re, subprocess, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import config, gh
from board import key_of

CHECKOUT_RE = re.compile(r"^(?:pr|review)[-_]?\d+(?:-head)?$", re.I)
STOP = set("""a an and are as at be by for from in into is it its of on or per
the to with without when not no via this that fix feat chore refactor test
tests docs make add use""".split())

def _git(path, *args, timeout=30):
    try:
        p = subprocess.run(["git", "-C", path, *args], capture_output=True,
                           text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return ""
    return p.stdout if p.returncode == 0 else ""

def my_emails(clones):
    out = set()
    for c in clones:
        e = _git(c, "config", "user.email").strip().lower()
        if e:
            out.add(e)
    return out

# ---------------------------------------------------------------- matching
def norm_title(t):
    return " ".join(re.sub(r"[^a-z0-9]+", " ", (t or "").lower()).split())

def tokens(text):
    """Comparable words: lowercase, split on punctuation (config.toml is two
    words), stopwords out, crude suffix stemming so parse/parsed/parsing meet."""
    out = set()
    for w in re.split(r"[^a-z0-9]+", (text or "").lower()):
        if len(w) < 3 or w in STOP or w.isdigit():
            continue
        for suf in ("ing", "ed", "es", "s"):
            if w.endswith(suf) and len(w) - len(suf) >= 4:
                w = w[: -len(suf)]
                break
        if len(w) >= 4 and w[-1] == w[-2]:
            w = w[:-1]                      # logg -> log
        out.add(w)
    return out

REF_RE = re.compile(r"(?:github\.com/getfloresta/floresta/(?:issues|pull)/|getfloresta/floresta#|(?<![\w/])#)(\d+)", re.I)

def link_issue(text, words, issues):
    """The open upstream issue this work most likely fixes, as (issue, how).

    An explicit reference wins. Bare `#N` in a fork PR means the fork's own
    numbering unless N is too big to be one, hence the >= 100 floor. Failing
    that, a title match on at least two shared words, which is why the card
    says "probably"."""
    by_num = {i["number"]: i for i in issues}
    for m in REF_RE.finditer(text or ""):
        n = int(m.group(1))
        if n in by_num and (n >= 100 or "floresta" in m.group(0).lower()):
            return by_num[n], "refs"
    best, score = None, 0
    for i in issues:
        s = len(words & tokens(i["title"]))
        if s > score:
            best, score = i, s
    return (best, "guess") if score >= 2 else (None, None)

# ---------------------------------------------------------------- sources
def fork_prs(fork, me):
    owner, name = fork.split("/", 1)
    data = gh.graphql(f'''query {{ repository(owner: "{owner}", name: "{name}") {{
      pullRequests(states: OPEN, first: 50) {{ nodes {{
        number title body url createdAt updatedAt additions deletions
        headRefName headRefOid author {{ login }}
        commits(first: 100) {{ totalCount nodes {{ commit {{ oid }} }} }} }} }} }} }}''')
    out = []
    for n in ((data.get("repository") or {}).get("pullRequests") or {}).get("nodes") or []:
        if ((n.get("author") or {}).get("login") or "").lower() != me.lower():
            continue
        out.append({"source": "fork-pr", "number": n["number"], "title": n["title"],
                    "body": n.get("body") or "", "url": n["url"],
                    "branch": n["headRefName"], "head": n["headRefOid"],
                    "created": n["createdAt"][:10], "last": n["updatedAt"][:10],
                    "additions": n["additions"], "deletions": n["deletions"],
                    "commits": (n.get("commits") or {}).get("totalCount", 0),
                    "oids": {c["commit"]["oid"] for c in (n.get("commits") or {}).get("nodes") or []}})
    return out

def local_branches(clone, emails, upstream_ref=config.UPSTREAM_REF):
    """Branches with commits you wrote that upstream does not carry."""
    out = []
    if not os.path.isdir(os.path.join(clone, ".git")):
        return out
    for br in _git(clone, "for-each-ref", "--format=%(refname:short)", "refs/heads").split():
        if br in ("master", "main") or br.startswith(config.BRANCH_IGNORE) or CHECKOUT_RE.match(br):
            continue
        # `git cherry` compares patches, so a commit cherry-picked or rebased
        # into upstream counts as shipped even though its hash differs.
        fresh = [l[2:].strip() for l in _git(clone, "cherry", upstream_ref, br).splitlines()
                 if l.startswith("+ ")]
        if not fresh:
            continue
        log = _git(clone, "log", "--no-walk", "--format=%H%x09%ae%x09%cs%x09%s", *fresh)
        mine = []
        for line in log.splitlines():
            oid, email, day, subject = (line.split("\t") + ["", "", ""])[:4]
            if email.lower() in emails:
                mine.append((oid, day, subject))
        if not mine:
            continue
        mine.sort(key=lambda m: m[1])
        out.append({"source": "branch", "branch": br, "clone": clone,
                    "title": mine[-1][2], "subjects": [m[2] for m in mine],
                    "commits": len(mine), "last": mine[-1][1],
                    "oids": {m[0] for m in mine}})
    return out

def run(snap_issues, upstream_mine, me, fork=config.FORK, clones=config.CLONES):
    """[unshipped item], each with `key`, and `issue` set when it links one.

    `upstream_mine` is snapshot.mine(): every PR you opened upstream."""
    up_titles = {norm_title(p["title"]) for p in upstream_mine}
    up_refs = {p["head_ref"] for p in upstream_mine}
    up_oids = set().union(*(p["commits"] for p in upstream_mine)) if upstream_mine else set()

    items = []
    prs = fork_prs(fork, me)
    for p in prs:
        if (norm_title(p["title"]) in up_titles or p["branch"] in up_refs
                or p["head"] in up_oids):
            continue
        p["key"] = key_of(p["url"])
        p["words"] = tokens(p["title"] + " " + p["branch"])
        items.append(p)
    fork_refs = {p["branch"] for p in prs}

    emails = my_emails(clones)
    for clone in clones:
        for b in local_branches(clone, emails):
            if b["branch"] in up_refs or b["branch"] in fork_refs or b["oids"] & up_oids:
                continue
            b["key"] = f"branch:{b['branch']}"
            b["words"] = tokens(b["branch"] + " " + " ".join(b["subjects"]))
            b["body"] = "\n".join(b["subjects"])
            items.append(b)

    for it in items:
        issue, how = link_issue(it.get("body", "") + "\n" + it["title"], it["words"], snap_issues)
        it["issue"] = {"number": issue["number"], "title": issue["title"],
                       "url": issue["url"], "how": how} if issue else None
    return items
