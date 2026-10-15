"""Which parts of Floresta are yours, computed rather than declared.

There is no CODEOWNERS. Instead: every file your commits touched, on upstream
master and on your unshipped branches, rolled up to its crate. The judge gets
this as context for ranking; nothing is ever excluded on it.
"""
import collections, os, subprocess, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import config

def area(path):
    parts = path.split("/")
    if parts[0] in ("crates", "bin") and len(parts) > 2:
        return "/".join(parts[:2])
    return parts[0] if len(parts) > 1 else "(root)"

def compute(emails, clones=config.CLONES, refs=(config.UPSTREAM_REF,), extra=()):
    """{area: commits touching it}, most-touched first. `extra` adds refs, e.g.
    your unshipped branches."""
    count = collections.Counter()
    for clone in clones:
        for ref in list(refs) + list(extra):
            for email in emails:
                try:
                    p = subprocess.run(
                        ["git", "-C", clone, "log", ref, f"--author={email}",
                         "--no-merges", "--name-only", "--format=%x00"],
                        capture_output=True, text=True, timeout=60)
                except subprocess.TimeoutExpired:
                    continue
                if p.returncode != 0:
                    continue
                for commit in p.stdout.split("\x00"):
                    for a in {area(l) for l in commit.split("\n") if l.strip()}:
                        count[a] += 1
    return dict(count.most_common())

def overlap(files, fp):
    """The areas of `files` you have worked in, best first."""
    hit = collections.Counter(area(f) for f in files if area(f) in fp)
    return sorted(hit, key=lambda a: -fp[a])
