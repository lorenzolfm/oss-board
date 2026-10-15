# Floresta triage

Answers one question about `getfloresta/Floresta`: **what is blocked on you?**
And then a second, strictly below it: what is worth your evening.

It refreshes an obsidian-kanban board at `~/Documents/Vault/Opensource/Tasks.md`
every hour and sends a Telegram DM twice a day (08:07, 19:23).

This replaced a 91-repo contribution board. That one failed for two reasons:
every card cost a context load into a codebase you don't carry in your head,
and it answered "what could I pick up?" when the question that matters is
"who is waiting on me?".

## Obligations and opportunities

**Obligations** are found by code alone, are never capped, and are listed
longest-waiting first. The model may add a one-line gist to one; it can never
hide, drop or reorder it. Six rules (`lib/obligations.py`):

1. **A review was requested from you**, formally or by an informal "@you can
   you review" comment.
2. **A conversation is waiting on you**: the newest comment isn't yours, and
   it @-mentions you, quote-replies you, is in a review thread you're part of,
   or is on an item you authored or are assigned. A busy PR you commented on
   once does *not* count -- that is the notifications inbox this replaces.
   Any activity of yours (a comment, a submitted review, an inline reply)
   answers what came before it; on your own PR, so does a push.
3. **Your own PRs when there's something for you to do**: an unresolved
   thread (the ruleset won't merge with one), changes requested since your
   push, a conflict, red CI, or a comment since your last push. ACKs and
   approvals are good news, not requests.
4. **A stale ACK**: you posted `ACK <hash>` and the head moved. The card says
   whether it was a rebase (computed: the PR's diff at your ACK against its
   diff now) or new code, and in which files.
5. **An assigned issue with nothing linked**, with how long it's been quiet and
   the milestone due date.
6. **A review you started and never submitted.**

**Opportunities** are ranked by the judge, capped at 8 on the board and 3 in
the DM: new issues (14 days, unassigned, nothing linked), non-draft PRs with
zero approvals you haven't touched, PRs you engaged with that moved since, and
the org board's Needs review column. Your **footprint** -- the
crates your commits touched, computed from your local clone -- is ranking
context for the judge, never a filter. There is no CODEOWNERS to use instead.

**Unshipped** work is your fork PRs and local branches carrying commits you
wrote that upstream lacks. Anything an upstream PR of yours already carries
(by title, branch or commit) is hidden, as are review checkouts (`pr-1105`)
and branches of only others' commits. Each is linked to the issue it likely
fixes. One linked to an issue you're assigned to or were @-mentioned on is
promoted to an obligation on that issue.

## The lane contract

| Lane | Owner | |
|---|---|---|
| `Needs me` | agent, rewritten every run | uncapped |
| `Opportunities` | agent | 8 |
| `Unshipped` | agent | uncapped |
| `Todo` / `Doing` / `Done` | **you** -- agent only appends annotations | |
| `Passed` | you drag here | |

- **Passed means two things, told apart by the card.** An obligation card
  carries a hidden fingerprint (`<!--t:...-->`); dragged to Passed, it is
  **muted until a new trigger** -- a newer mention, reply, re-request or push
  past your ACK, or a reason of a kind it didn't have. General activity never
  re-arms it. Anything else dragged to Passed is gone for good.
- **Dragging a card into Todo/Doing claims it.** A claimed obligation leaves
  `Needs me` and lives on your card as an annotation (the DM still lists it,
  marked "in your Doing"). Claiming someone else's PR writes a **review
  brief** -- reading order, review so far, unresolved threads, what moved
  since you last looked; claiming an issue writes the fix-oriented brief.
- Card identity is the GitHub URL, or a `<!--k:...-->` key for cards with no
  URL of their own, so you can reword a card freely.

## The DM

Every obligation, longest wait first, each with its computed reasons, the
model's gist and a tappable URL; then the judge's top 3 opportunities with an
estimate and a first step; then a count line. 🆕 marks anything new or changed
since the last DM. A long day is split into several messages, never truncated.
If the judge is unavailable the opportunities are in mechanical order and the
message says so.

The timer is hourly; the DM goes out on the first run at or after each slot, so
a slot missed while the desktop was off is sent late -- unless it is more than
8 hours stale, in which case the next slot's message is the better one.

## Layout

```
run.py                 orchestrator; board always written, DM when a slot is due
oss-board.nix          systemd timer (hourly) + service; copy to /etc/nixos
lib/config.py          the repo, the clone, caps, DM slots, the project column
lib/snapshot.py        one GraphQL read of every open PR and issue; merge state;
                       your upstream PRs; the stale-ACK interdiff
lib/obligations.py     the six rules and the mute logic -- pure functions
lib/opportunities.py   the four opportunity sources and the fallback ranking
lib/unshipped.py       fork PRs and local branches, and issue linking
lib/footprint.py       which crates are yours, from git history
lib/project.py         the org board's Needs Review column (+ a survey tool)
lib/model.py           gists and the judge, both cached in state/
lib/render.py          cards and the DM text
lib/board.py           kanban parse/merge/write, migration -- owns the state contract
lib/claims.py          upstream status of claimed cards
lib/brief.py           issue and review briefs for claimed cards
lib/notify.py          Telegram (chunked) + uptime-kuma heartbeat
lib/testkit.py         snapshot-shaped fixtures
lib/test_*.py          board, obligations, unshipped, DM
state/                 gists.json, rank.json, last_dm.json, last_run.json, summary.txt
```

## Running by hand

```sh
python3 run.py --dry-run --no-model      # board + DM to stdout, no writes, no model
python3 run.py --dry-run                 # ...with gists and the judge
python3 run.py                           # update the board
python3 run.py --notify --dm-now         # ...and DM now regardless of the slot
python3 lib/project.py                   # survey the org board's columns
for t in lib/test_*.py; do python3 "$t" || break; done
```

A run takes under a minute without the model, a few with it; the hourly runs
reuse cached gists and rankings unless something they look at moved.

## Deliberate design choices

- **One snapshot, pure rules.** Everything is a function of one read of the
  open items, so the rules are tested against fixtures with no network, and a
  failed snapshot fails the whole run *before* the board is touched -- a lane
  computed from nothing would delete every card.
- **No GitHub notifications.** You don't use them, and a read-only fine-grained
  token can't read them anyway. The rules reconstruct "waiting on you" from the
  conversation itself.
- **The GitHub Search API is never used.** It reported bitcoin/bitcoin as
  having 0 open PRs when REST showed 382, and a search for `author:me` is
  evaluated across every org the account belongs to, employer orgs included --
  one org's token policy failed a whole run. Your upstream PRs come from REST's
  issue list with a `creator` filter; your fork PRs from your own fork.
- **`mergeable` and the CI rollup are fetched only where needed** (your PRs,
  the opportunity shortlist). They force GitHub to compute state on demand and
  502'd the old bulk scan.
- **The board write is deterministic Python, not model output.** The model
  writes gists and ranks opportunities; it cannot invent an item, hide an
  obligation, or touch your lanes. Any model failure degrades to computed
  reasons and mechanical order, and says so.
- **Read-only, enforced by the token.** The scanner sends only GETs and
  GraphQL queries. The one exception is the briefing note, authored by a
  `claude -p` subprocess holding the `gh` CLI -- there the prompt's "do NOT
  post anything" is guidance, and the token is what enforces it. `brief.py`
  refuses to run without `GH_TOKEN` rather than fall back to your own keyring
  auth, which carries `repo`.
- **Migration was a one-off.** The first run on the old board dropped its agent
  lanes and every non-Floresta card (you chose overwrite over archive). A board
  without the old lanes is never migrated again, so a card you add later stays.

## Setup

1. **The token** is a *classic* PAT with only `read:project` -- no `repo`, no
   `public_repo` -- in the sops secret `oss-board-github-token`. sops-nix
   only refreshes `/run/secrets` on a rebuild:
   ```sh
   sops /etc/nixos/hosts/desktop/secrets.yaml
   sudo nixos-rebuild switch
   ```
2. **The unit.** `oss-board.nix` is the deployed copy; the timer runs `run.py`
   from this working tree, so whatever is checked out here is what runs.
   Deployed copies have drifted before -- diff before syncing:
   ```sh
   diff oss-board.nix /etc/nixos/hosts/desktop/oss-board.nix
   cp oss-board.nix /etc/nixos/hosts/desktop/oss-board.nix
   sudo nixos-rebuild switch
   ```
3. **The Needs review column** is on. It was checked before enabling
   (2026-10-14: 23 open PRs, 13 moved in the previous two weeks).
   `python3 lib/project.py` re-runs that survey; if the column ever goes
   stale, set `config.PROJECT["enabled"]` back to `False`.
