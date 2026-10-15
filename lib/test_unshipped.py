"""Unshipped work, against real git repos built in a temp dir.

The old stranded scan's failures are the cases: a review checkout reported as
your work, a fork PR already upstream under the same title (fork #8 / #1330),
and a cherry-picked commit counted as unshipped because its hash changed."""
import os, subprocess, sys, tempfile, shutil
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import unshipped as U
from testkit import chk, done, issue, ME

tmp = tempfile.mkdtemp()
MINE, OTHER = "me@example.com", "other@example.com"

def git(path, *args, email=MINE):
    env = {**os.environ, "GIT_AUTHOR_NAME": "x", "GIT_AUTHOR_EMAIL": email,
           "GIT_COMMITTER_NAME": "x", "GIT_COMMITTER_EMAIL": email,
           "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull}
    return subprocess.run(["git", "-C", path, *args], env=env, check=True,
                          capture_output=True, text=True).stdout

def commit(path, name, email=MINE, msg=None):
    with open(os.path.join(path, name), "w") as f:
        f.write(name)
    git(path, "add", name, email=email)
    git(path, "commit", "-qm", msg or f"add {name}", email=email)

up = os.path.join(tmp, "upstream")
os.makedirs(up)
git(up, "init", "-q", "-b", "master")
commit(up, "base", OTHER)
clone = os.path.join(tmp, "clone")
subprocess.run(["git", "clone", "-q", "-o", "upstream", up, clone], check=True)
git(clone, "config", "user.email", MINE)

def branch(name, *files, email=MINE, msg=None):
    git(clone, "checkout", "-q", "-b", name, "upstream/master")
    for f in files:
        commit(clone, f, email, msg)
    git(clone, "checkout", "-q", "master")

branch("dedup-config-parse", "cfg1", "cfg2", msg="fix(node): parse config.toml once")
branch("review-of-bobs-pr", "bob1", email=OTHER)          # someone else's commits
branch("pr-1105", "c1")                                   # a PR checkout by name
branch("backup/old-wip", "w1")
branch("already-up", "picked")
# Upstream takes the "already-up" patch by cherry-pick: same change, new hash.
git(up, "fetch", "-q", clone, "already-up")
git(up, "cherry-pick", "FETCH_HEAD", email=OTHER)
git(clone, "fetch", "-q", "upstream")
branch("shipped-as-pr", "s1")

print("1. local branches: only commits you wrote that upstream lacks")
got = {b["branch"]: b for b in U.local_branches(clone, {MINE})}
chk("your unshipped branch is found", "dedup-config-parse" in got, list(got))
chk("with your commit count", got.get("dedup-config-parse", {}).get("commits") == 2)
chk("a review checkout of others' work is not yours", "review-of-bobs-pr" not in got)
chk("a branch named after a PR is a checkout", "pr-1105" not in got)
chk("ignored prefixes stay ignored", "backup/old-wip" not in got)
chk("a cherry-picked patch counts as shipped", "already-up" not in got, list(got))

print("\n2. run(): what an upstream PR already carries is hidden")
real_fork_prs = U.fork_prs
U.fork_prs = lambda fork, me: [
    {"source": "fork-pr", "number": 8, "title": "fix(wire): report block download progress on a time interval",
     "body": "", "url": "https://github.com/lorenzolfm/Floresta/pull/8", "branch": "fix/time-log",
     "head": "a" * 40, "created": "2026-09-12", "last": "2026-09-12", "additions": 1,
     "deletions": 0, "commits": 2, "oids": set()},
    {"source": "fork-pr", "number": 6, "title": "Read and parse config.toml once per boot",
     "body": "", "url": "https://github.com/lorenzolfm/Floresta/pull/6", "branch": "other-name",
     "head": "b" * 40, "created": "2026-09-11", "last": "2026-09-11", "additions": 1,
     "deletions": 0, "commits": 6, "oids": set()}]
upstream_mine = [{"number": 1330, "title": "fix(wire): report block download progress on a time interval",
                  "state": "OPEN", "head_ref": "fix/time-based-ibd-progress-log", "commits": set()},
                 {"number": 1325, "title": "x", "state": "MERGED", "head_ref": "shipped-as-pr", "commits": set()}]
issues = [issue(1308, title="config.toml is read and parsed three times per startup"),
          issue(167, title="[tracking-issue] Audit logging and identify improvements")]
try:
    items = {u["key"]: u for u in U.run(issues, upstream_mine, ME, clones=[clone])}
finally:
    U.fork_prs = real_fork_prs
chk("fork PR already upstream under its title is hidden (#8 / #1330)",
    "lorenzolfm/Floresta#8" not in items, list(items))
chk("a fork PR nobody upstream has seen stays", "lorenzolfm/Floresta#6" in items)
chk("a branch upstream merged as a PR is hidden", "branch:shipped-as-pr" not in items, list(items))
chk("an unpushed branch stays", "branch:dedup-config-parse" in items, list(items))

print("\n3. linking work to the issue it fixes")
link = lambda text, words: (lambda r: (r[0] and r[0]["number"], r[1]))(U.link_issue(text, words, issues))
chk("fork #6 probably fixes #1308",
    link("", U.tokens("Read and parse config.toml once per boot dedup-config-parse")) == (1308, "guess"))
chk("one shared word is not a link (OTLP vs the logging audit)",
    link("", U.tokens("feat(florestad): prototype OTLP/HTTP JSON log export")) == (None, None))
chk("an explicit upstream URL wins",
    link("See https://github.com/getfloresta/Floresta/issues/167", set()) == (167, "refs"))
chk("a small bare #N is the fork's own numbering, not upstream",
    link("follow-up to #6", set()) == (None, None))

shutil.rmtree(tmp)
done()
