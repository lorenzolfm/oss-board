//! The model prompts, byte for byte what each pass sends. `%(name)s` and `%s` are
//! filled by the caller.

/// One line per obligation.
pub const GIST: &str = "Each item below is waiting on Lorenzo, a senior Rust/Bitcoin \
engineer, in the Floresta repo. The `reasons` already say WHY it is waiting; do \
not repeat them. For each item write `gist`: at most 22 words saying WHAT is \
being asked of him or what changed, concretely enough that he can tell from \
his phone whether it is a two-minute reply or an evening (\"asks whether the \
empty-headers case can still stall; wants a yes/no and a test\"). If the \
excerpts do not say, write what he would have to look at first. Never invent \
facts not in the excerpts.

Reply with ONLY a JSON object, no prose, no code fence:
{\"gists\":[{\"key\":\"owner/repo#1\",\"gist\":\"...\"}]}

ITEMS:
%s
";

/// The opportunity judge.
pub const RANK: &str = "You are picking open-source work for Lorenzo: a senior \
Rust/Bitcoin engineer, a regular reviewer on Floresta (a Utreexo-based Bitcoin \
node in Rust), whose binding constraint is TIME, not skill. These are \
opportunities -- nobody is waiting on him for them.

`footprint` is where his past commits landed, most-touched first; `areas` on a \
candidate is its overlap with that. Use it to judge where his review is worth \
most, but never drop an item for being outside it.

Rank the candidates, best use of an evening first, and keep at most %(cap)d. \
Drop anything that is not worth his time: a stale or abandoned PR, an issue \
that is an open design argument, a PR another reviewer is already deep in. \
Prefer: a bounded review in his areas that unblocks a merge over a huge \
refactor; a fresh issue with a clear spec over a vague one; a PR he already \
engaged with that moved, when his re-review is what it needs.

For each pick:
  `why`:   at most 12 words, concrete (\"chain reorg path you wrote, 40 lines\").
  `pitch`: 1-2 sentences, at most 35 words: why this one, and the first step.
  `est`:   rough wall-clock, one token like \"30m\", \"1h\", \"2-3h\".

Reply with ONLY a JSON object, no prose, no code fence:
{\"picks\":[{\"number\":1,\"why\":\"...\",\"pitch\":\"...\",\"est\":\"1h\"}]}

FOOTPRINT: %(footprint)s

CANDIDATES:
%(payload)s
";

/// A claimed issue: the fix-oriented brief.
pub const BRIEF_ISSUE: &str =
    "Write an Obsidian briefing note for an open-source contribution the \
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
Say \"No discussion.\" if the thread is empty -- do not pad.

## Maintainer guidance
Any steer a maintainer gave (approach, file, test, acceptance bar). Quote it.
Say \"None given.\" if there is none.

## Likely files
Concrete paths in the repo, with one line each on why. Mark uncertainty
explicitly rather than guessing confidently.

## Build & test
The real commands for THIS project -- how to build, how to run the relevant
tests, and the narrowest test invocation that would exercise this change.

## Prior attempts
Earlier PRs or issues that tried this and what happened. Say \"None found.\"
if you looked and found none.

## Open questions
Anything he must decide or ask before starting. Omit the section if none.

Be concrete and short. Prefer \"don't know\" over a confident guess: a wrong file
path costs him more time than a missing one. Write the file, then reply with
only the word DONE.
";

/// Someone else's claimed PR: the review brief.
pub const BRIEF_REVIEW: &str = "Write an Obsidian review briefing for a pull request the \
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
and the commit each one named. Say \"No reviews yet.\" if so.

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

Prefer \"don't know\" over a confident guess. Write the file, then reply with
only the word DONE.
";
