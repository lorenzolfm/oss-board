"""Write a briefing note when the user claims a card.

This is the feature that converts a scarce evening from "orient myself" into
"write the fix" -- or, for a review, into "read the diff in the right order".
It runs only for items he actually chose, never for candidates, and is capped
per run to bound cost.

Two prompts: an issue gets the fix-oriented brief, someone else's PR gets a
review brief. Your own PR gets none -- you wrote it.

The note is authored by a `claude -p` subprocess because it needs judgement
(which files matter, what the thread actually concluded) rather than a template.
"""
import os, re, subprocess, sys

PROMPT = """Write an Obsidian briefing note for an open-source contribution the \
user has just claimed. He is a senior Rust/Bitcoin engineer with limited evening \
time; the note exists so he can start writing code immediately instead of \
orienting himself.

Target: %(kind)s %(repo)s#%(number)s
URL: %(url)s

Research it yourself with the `gh` CLI (read-only: `gh issue view`, `gh pr view`,
`gh api`, `gh search`) and by reasoning about the project. Do NOT clone anything,
do NOT post anything, do NOT modify any repo.

Write the file to exactly this path, and nothing else anywhere:
  %(path)s

Structure it as:

---
tags: [opensource]
---

# %(repo)s#%(number)s — <title>

**Repo:** %(repo)s · **Link:** %(url)s · **Claimed:** %(today)s

## What's asked
Two or three sentences. The actual requirement, not the title restated.

## Thread so far
What was concluded, disputed, or decided. Name who said what when it matters.
Say "No discussion." if the thread is empty -- do not pad.

## Maintainer guidance
Any steer a maintainer gave (approach, file, test, acceptance bar). Quote it.
Say "None given." if there is none.

## Likely files
Concrete paths in the repo, with one line each on why. Mark uncertainty
explicitly rather than guessing confidently.

## Build & test
The real commands for THIS project -- how to build, how to run the relevant
tests, and the narrowest test invocation that would exercise this change.

## Prior attempts
Earlier PRs or issues that tried this and what happened. Say "None found."
if you looked and found none.

## Open questions
Anything he must decide or ask before starting. Omit the section if none.

Be concrete and short. Prefer "don't know" over a confident guess: a wrong file
path costs him more time than a missing one. Write the file, then reply with
only the word DONE.
"""

REVIEW_PROMPT = """Write an Obsidian review briefing for a pull request the \
user has just claimed to review. He is a senior Rust/Bitcoin engineer and a \
regular Floresta reviewer with limited evening time; the note exists so he can \
open the diff and start reviewing in the right place instead of orienting.

Target: %(repo)s#%(number)s
URL: %(url)s
Reviewer: %(me)s

Research it yourself with the `gh` CLI (read-only: `gh pr view`, `gh pr diff`,
`gh api`, including `repos/%(repo)s/pulls/%(number)s/comments` for inline
threads). Do NOT clone anything, do NOT post anything, do NOT submit a review.

Write the file to exactly this path, and nothing else anywhere:
  %(path)s

Structure it as:

---
tags: [opensource, review]
---

# %(repo)s#%(number)s — <title>

**Author:** <login> · **Link:** %(url)s · **Claimed:** %(today)s
**Size:** +<additions>/-<deletions> in <n> files · <n> commits

## What it changes
Two or three sentences: the behaviour change, not the title restated.

## Reading order
The files (or commits, if the PR is meant to be reviewed commit by commit) in
the order that makes the diff make sense, one line each on what to look for.
Put the core logic first and the mechanical fallout (renames, test plumbing)
last, and say which is which.

## Review so far
Who reviewed, what they raised, what was resolved. List every UNRESOLVED
inline thread with its file and one line on the open question. Note any ACKs
and the commit each one named. Say "No reviews yet." if so.

## Since %(me)s last looked
If %(me)s has commented or ACKed before: the commits pushed since, and whether
they are a rebase or new code. Omit the section if he has never engaged.

## Where to push
Two to four concrete things worth checking in this diff -- invariants it
could break, a missing test case, a consensus or concurrency edge. Be specific
to the code; no generic review checklists.

## Test it
The narrowest commands that exercise this change (cargo test filters, the
functional test file), if you can tell.

Prefer "don't know" over a confident guess. Write the file, then reply with
only the word DONE.
"""

SAFE = re.compile(r"[^A-Za-z0-9._-]+")

def note_path(vault_dir, repo, number):
    slug = SAFE.sub("-", f"{repo.split('/')[-1]}-{number}").strip("-")
    return os.path.join(vault_dir, f"{slug}.md")

def write_briefing(vault_dir, repo, number, url, kind, today, me=None, timeout=600):
    """`kind` is "issue" or "review"; your own PRs are never briefed."""
    path = note_path(vault_dir, repo, number)
    if os.path.exists(path):
        return path, "exists"
    os.makedirs(vault_dir, exist_ok=True)
    fields = {"kind": kind, "repo": repo, "number": number, "url": url,
              "path": path, "today": today, "me": me}
    prompt = (REVIEW_PROMPT if kind == "review" else PROMPT) % fields
    # This subprocess is a full agent with Bash and the `gh` CLI, so the prompt's
    # "do NOT post anything" is guidance, not a boundary -- the token is the
    # boundary. Pin it to the unit's read-only GH_TOKEN explicitly: with the var
    # merely inherited, a manual run leaves it unset and `gh` falls back to the
    # user's own keyring auth, which carries `repo` (write). Refuse instead.
    token = os.environ.get("GH_TOKEN")
    if not token:
        return None, "no read-only GH_TOKEN; refusing to fall back to user auth"
    env = {**os.environ, "GH_TOKEN": token, "GITHUB_TOKEN": token}
    try:
        p = subprocess.run(["claude", "-p", prompt], capture_output=True,
                           text=True, timeout=timeout, env=env)
    except (subprocess.TimeoutExpired, FileNotFoundError) as e:
        return None, type(e).__name__
    if p.returncode != 0:
        return None, f"rc={p.returncode}"
    if not os.path.exists(path):
        return None, "no file written"
    # A briefing that is mostly empty is worse than none -- it looks answered.
    if len(open(path, encoding="utf-8").read().strip()) < 300:
        os.remove(path)
        return None, "too thin, discarded"
    return path, "written"

def wikilink(path):
    return "[[" + os.path.splitext(os.path.basename(path))[0] + "]]"
