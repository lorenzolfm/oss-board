# Floresta triage

Answers one question about `getfloresta/Floresta`: **who is actually waiting on
you?** And then a second, on the board only: what is worth your evening.

It refreshes an obsidian-kanban board at `~/Documents/Vault/Opensource/Tasks.md`
every hour and sends one short Telegram DM each morning (08:07).

This replaced a 91-repo contribution board. That one failed for two reasons:
every card cost a context load into a codebase you don't carry in your head,
and it answered "what could I pick up?" when the question that matters is
"who is waiting on me?".

The next version listed every obligation twice a day and failed differently: of 11
cards, 3 needed anything from you, and you stopped reading. Your ACKs don't count
toward merge, so a miss usually means someone pings again, while a false alarm means
the message goes unread. The board is the complete ledger, and the DM only interrupts.

## Obligations and opportunities

**Obligations** are found by code alone, are never capped on the board, and are
listed longest-waiting first. The model may add a one-line gist to one; it can never
hide, drop or reorder it there. Six rules (`src/obligations.rs`):

1. **A review was requested from you**, formally or by an informal "@you can
   you review" comment. A comment of yours with nothing pushed since has answered
   it, until someone pushes or asks again.
2. **A conversation is waiting on you**: the newest comment isn't yours, and
   it @-mentions you, quote-replies you, is in a review thread you're part of,
   or is on an item you authored or are assigned. A busy PR you commented on
   once does *not* count -- that is the notifications inbox this replaces.
   Any activity of yours (a comment, a submitted review, an inline reply)
   answers what came before it, in any thread; on your own PR, so does a push.
   A comment left unanswered for 14 days stops counting. If it still matters
   someone will ping again, and that brings the card back.
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

**Opportunities** are ranked by the judge, capped at 8, and only on the board:
new issues (14 days, unassigned, nothing linked), non-draft PRs with
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
  **muted until a new trigger** -- a newer mention, reply or re-request, a push
  on a PR you muted for a conflict, red CI or a stale ACK, or a reason of a kind
  it didn't have. General activity never re-arms it. Anything else dragged to
  Passed is gone for good.
- **Dragging a card into Todo/Doing claims it.** A claimed obligation leaves
  `Needs me` and lives on your card as an annotation (a new ask on it still
  reaches the DM, marked "in your Doing"). Claiming someone else's PR writes a **review
  brief** -- reading order, review so far, unresolved threads, what moved
  since you last looked; claiming an issue writes the fix-oriented brief.
- Card identity is the GitHub URL, or a `<!--k:...-->` key for cards with no
  URL of their own, so you can reword a card freely. Text from GitHub or the
  model can't become a marker or a new line, whatever it says.
- **The board is written on top of the file as it is by then**, so a card you
  drag during a run is kept. The file has no schema and `rename` has no
  condition, so an edit in the instant between that read and the write can still
  be lost; the log says when the file moved during a run.
- **A file that isn't a whole board is refused.** It needs the front matter and
  the settings block Obsidian writes last. The run fails and the heartbeat goes
  down, and the file is left as it was. Anything else in a board passes through.

## The DM

One message each morning, and it only carries what is new:

```
Floresta · 1 new · 2 on the board

#1308 ship fork#6 or hand it to Digvijay
https://github.com/getfloresta/Floresta/issues/1308
```

An obligation reaches it once, on a **new trigger** since the last DM (the same test
a mute uses: a newer event, a new kind of reason, or a new head) and only if it is an
**ask**. A review request, changes requested, a conflict, red CI or an assignment is
an ask by its nature. Anything else needs the model to read someone waiting on you
specifically. Your own reply, a bot's, and more work on your own unshipped fix aren't
new triggers. At most 3 per message; any more stay new for the next one. A quiet
morning is the header line alone, so a missing message means something is broken.

Only clearing forgets what you were told. Dragging a card to Passed doesn't, so what
re-arms the mute is new and what you already knew is not. A source that can't be read
doesn't either, so a conflict isn't news twice because GitHub was computing it one
morning.

Opportunities and Unshipped stay on the board.

If the model gives no verdict on a new item, it goes through and the message says it
wasn't filtered: a false alarm can be seen, a dropped ask can't.

A source that can't be read is not a clear board. `Unshipped` and any card held by an
unread source stay as they were, a claimed card keeps its note, and the heartbeat and
`state/last_run.json` name the source. The DM names it too when it could change what
the DM says. That includes a conflict GitHub is still computing, as it does after
master moves: a card held for a conflict waits for an answer.

The timer is hourly; the DM goes out on the first run at or after 08:07, so a morning
missed while the desktop was off is sent late -- unless it is more than 8 hours stale,
in which case it waits for tomorrow.

## Layout

```
flake.nix              the package, and the gates `nix flake check` runs
oss-board.nix          systemd timer (hourly) + service; copy to /etc/nixos
src/main.rs            orchestrator; board always written, DM when a slot is due
src/config.rs          the repo, the clone, caps, the DM slot, the project column
src/snapshot.rs        one GraphQL read of every open PR and issue; merge state;
                       your upstream PRs; the stale-ACK interdiff
src/obligations.rs     the six rules, the mute logic and what the DM may carry --
                       pure functions
src/opportunities.rs   the four opportunity sources and the fallback ranking
src/unshipped.rs       fork PRs and local branches, and issue linking
src/footprint.rs       which crates are yours, from git history
src/project.rs         the org board's Needs Review column (+ a survey tool)
src/model.rs           gists, ask verdicts and the judge, cached in state/
src/prompts.rs         what the model is told, byte for byte
src/render.rs          cards and the DM's words
src/board.rs           kanban parse/merge/write -- owns the state contract
src/claims.rs          upstream status of claimed cards
src/brief.rs           issue and review briefs for claimed cards
src/notify.rs          Telegram (chunked) + uptime-kuma heartbeat
src/gh.rs, src/proc.rs the `gh` CLI, and subprocesses with a deadline
src/testkit.rs         snapshot-shaped fixtures for the unit tests
tests/run.rs           the whole binary against fake gh, git and claude on PATH
state/                 gists.json, rank.json, last_dm.json, last_run.json, summary.txt
```

## Running by hand

```sh
cargo run -- refresh --state state --dry-run --no-model  # board + DM to stdout, no vault writes, no model
cargo run -- refresh --state state --dry-run             # ...with gists and the judge
cargo run -- refresh --state state                       # update the board
cargo run -- refresh --state state --notify --dm-now     # ...and DM now regardless of the slot
cargo run -- survey                                      # survey the org board's columns
cargo test                                               # or: nix flake check
```

`--dry-run` and `--notify` exclude each other, and `--dm-now` needs `--notify`. Only
one run at a time holds `--state`; a run that finds another in progress leaves the
work to it.

A dry run still refreshes `state/summary.txt` and `state/last_run.json`, and the
model caches when the model runs, so point `--state` elsewhere to leave them alone.

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
- **The board write is deterministic code, not model output.** The model
  writes gists and ranks opportunities; it cannot invent an item, hide an
  obligation from the board, or touch your lanes. Any model failure degrades to
  computed reasons and mechanical order.
- **The model does filter the DM.** Rules can't tell "@you already pointed this
  out" from "@you can you check this". A comment can talk the model out of an ask,
  which costs you that interruption and nothing else: the card stays on the board.
- **A push is dated as well as GitHub allows, which isn't exactly.** GitHub keeps no
  push time, and `Commit.pushedDate` is gone. A force-push has its event, and a
  fast-forward has only the date of the commit it adds. So a commit made before your
  comment and fast-forwarded after it reads as nothing pushed, and that review request
  stays answered until someone pushes again or re-requests. The head moving at all is
  still a new trigger, because state tied to a head compares the head, not its date.
- **Read-only, enforced by the token.** The scanner sends only GETs and
  GraphQL queries. The gist and judge passes get no tools at all, because their
  prompts carry text from anyone who can open an issue. The one exception is the
  briefing note, authored by a `claude -p` subprocess holding the `gh` CLI --
  there the prompt's "do NOT post anything" is guidance, and the token is what
  enforces it. `brief.rs` refuses to run without `GH_TOKEN` rather than fall back
  to your own login, which carries `repo`, and the unit hides that login
  (`~/.config/gh`) so the agent can't reach it either. The rest of `$HOME` stays
  readable to that agent, so this covers `gh`, not every credential you have.
- **Responses are parsed once, against GitHub's schema.** A field the schema
  promises is required, so a response of another shape is a failed read rather
  than an empty list. `labels` and `files` are the only lists allowed to be
  missing, because they only decorate a card.

## Setup

1. **The token** is a *classic* PAT with only `read:project` -- no `repo`, no
   `public_repo` -- in the sops secret `oss-board-github-token`. sops-nix
   only refreshes `/run/secrets` on a rebuild:
   ```sh
   sops /etc/nixos/hosts/desktop/secrets.yaml
   sudo nixos-rebuild switch
   ```
2. **The unit.** `oss-board.nix` is the deployed copy, and the binary comes from
   this repo as a flake input, like `wt` and `claude-ps`: what runs is what the
   lock pins, not whatever is checked out here. So a change reaches the timer only
   when it is on `main` on GitHub and you update the input:
   ```sh
   cd /etc/nixos
   nix flake update oss-board
   sudo nixos-rebuild switch
   ```
   The input has to be one a GitHub runner can fetch, because the workflows in
   `/etc/nixos` evaluate and re-lock it there. A local `git+file:` input fails both:
   ```nix
   oss-board = {
     url = "github:lorenzolfm/oss-board";
     inputs.nixpkgs.follows = "nixpkgs";
     inputs.rust-overlay.follows = "rust-overlay";
   };
   ```
   Deployed copies of the module have drifted before -- diff before syncing:
   ```sh
   diff oss-board.nix /etc/nixos/hosts/desktop/oss-board.nix
   cp oss-board.nix /etc/nixos/hosts/desktop/oss-board.nix
   ```
3. **The Needs review column** is on. It was checked before enabling
   (2026-10-14: 23 open PRs, 13 moved in the previous two weeks).
   `oss-board survey` re-runs that survey; if the column ever goes stale, set
   `enabled` in `config::PROJECT` back to `false`.
