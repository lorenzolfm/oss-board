"""What is blocked on you: the six obligation rules, as pure functions.

An obligation is found by code alone. The model may add a gist to one but can
never hide, drop or reorder it -- that is the whole difference from an
opportunity. Each item carries a list of reasons; each reason has

  kind  one of KINDS -- also what a mute remembers
  at    when the other side started waiting (ordering), or None for a pure
        state like a merge conflict that has no start time we can see
  last  the newest event behind it (re-arming a mute)
  text  the phone-sized explanation
"""
import datetime as dt, os, re, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from board import key_of

KINDS = ("review_requested", "mention", "reply", "thread", "changes_requested",
         "unresolved_thread", "conflict", "ci_failed", "stale_ack", "assigned",
         "pending_review", "unshipped")

# Code-review ACKs name the commit they reviewed. "Concept ACK" names none and
# is not a code review, so it cannot go stale. `\bACK` keeps NACK out.
ACK_RE = re.compile(r"\b(?:ut|t|cr|re-?)?ACK\s+([0-9a-f]{7,40})\b")
# Someone else's approval on your PR is good news, not a request for a reply.
APPROVAL_RE = re.compile(r"^\s*(?:(?:re-?|ut|t|cr|concept|approach)\s*)?ACK\b|^\s*LGTM\b", re.I)

def parse(iso):
    return dt.datetime.fromisoformat(iso.replace("Z", "+00:00")) if iso else None

def ago(iso, now):
    d = now - parse(iso)
    if d.days >= 1:
        return f"{d.days}d"
    return f"{max(1, d.seconds // 3600)}h"

def mention_re(me):
    return re.compile(rf"(?<![\w/-])@{re.escape(me)}(?![\w-])", re.I)

def _human(c):
    return not c["bot"]

def _my_last(comments, me):
    return max((c["at"] for c in comments if c["author"] == me), default="")

def engaged_at(item, me):
    """Your newest activity of any kind on the item: a comment, a submitted
    review (even one with no body), an inline reply. Any of them is an answer
    to whatever came before it."""
    ats = [c["at"] for c in _stream(item) if c["author"] == me]
    if item["kind"] == "pr":
        ats += [r["at"] for r in item["reviews"] if r["author"] == me and r["state"] != "PENDING"]
        ats += [c["at"] for t in item["threads"] for c in t["comments"] if c["author"] == me]
    return max((a for a in ats if a), default="")

def _norm(text):
    return " ".join(text.split()).lower()

def quotes(comment, mine):
    """Whether `comment` quote-replies one of your comments. GitHub's "Quote
    reply" pastes your text behind `>` and mentions nobody -- it is how a
    maintainer answers you without an @, so it counts as one."""
    quoted = _norm(" ".join(l[1:] for l in comment["body"].splitlines() if l.startswith(">")))
    if len(quoted) < 20:
        return False
    probe = quoted[:60]
    return any(probe in _norm(m["body"]) for m in mine)

def _stream(item):
    """The main conversation, with the item's own body as its first comment so
    a "cc @you" in a PR description counts like any other mention."""
    head = {"author": item["author"], "bot": item["author_bot"], "at": item["created"],
            "body": item["body"], "kind": "body"}
    return [head] + item["comments"]

def reason(kind, at, text, last=None):
    return {"kind": kind, "at": at, "last": last or at, "text": text}

# ---------------------------------------------------------------- the rules
def conversation(item, me, now):
    """Rule 2: the newest comment isn't yours, and it mentions you, or the item
    is yours (author or assignee). Busy threads you once commented on do not
    count -- that is the notifications inbox this exists to replace."""
    out, rx = [], mention_re(me)
    stream = _stream(item)
    mine = [c for c in stream if c["author"] == me]
    mine_at = engaged_at(item, me)
    if item["kind"] == "pr" and item["author"] == me:
        mine_at = max(mine_at, item.get("head_at") or "")     # a push answers too
    if not mine_at and me in item["assignees"]:
        # Chatter from before you were handed it was never addressed to you.
        # Once you have spoken, though, an answer to you stays an answer even
        # if the assignment came later.
        mine_at = (item.get("assigned_at") or {}).get(me, "")
    after = [c for c in stream if c["author"] != me and _human(c) and (c["at"] or "") > mine_at]

    pinged = [c for c in after if rx.search(c["body"]) or quotes(c, mine)]
    if pinged:
        first = pinged[0]
        if not rx.search(first["body"]):
            verb = "replied to you"
        elif "review" in first["body"].lower():
            verb = "asked you to review"
        else:
            verb = "mentioned you"
        out.append(reason("mention", first["at"],
                           f"{first['author']} {verb} {ago(first['at'], now)} ago",
                           pinged[-1]["at"]))

    owns = item["author"] == me or me in item["assignees"]
    if owns:
        # Verdicts have rules of their own (changes_requested) or ask nothing.
        replies = [c for c in after if c not in pinged and c["kind"] != "body"
                   and c["kind"] not in ("review:APPROVED", "review:CHANGES_REQUESTED",
                                         "review:DISMISSED")
                   and not APPROVAL_RE.match(c["body"])]
        if replies:
            n = len(replies)
            out.append(reason("reply", replies[0]["at"],
                               f"{n} comment{'s' if n != 1 else ''} since yours, "
                               f"last by {replies[-1]['author']} {ago(replies[-1]['at'], now)} ago",
                               replies[-1]["at"]))
    return out

def threads(pr, me, now):
    """Rule 2, inline half: someone answered in a review thread you're in."""
    if pr["author"] == me:
        return []                # your own PR's threads are rule 3's business
    waiting = []
    for t in pr["threads"]:
        cs = [c for c in t["comments"] if _human(c)]
        if t["resolved"] or not cs or not any(c["author"] == me for c in cs):
            continue
        if cs[-1]["author"] == me:
            continue
        mine_at = _my_last(cs, me)
        first = next(c for c in cs if c["at"] > mine_at and c["author"] != me)
        waiting.append((first, cs[-1], t["path"]))
    if not waiting:
        return []
    waiting.sort(key=lambda w: w[0]["at"])
    first, _, path = waiting[0]
    n = len(waiting)
    where = f" ({path.rsplit('/', 1)[-1]})" if n == 1 and path else ""
    return [reason("thread", first["at"],
                    f"{first['author']} replied in {n} of your review thread"
                    f"{'s' if n != 1 else ''}{where} {ago(first['at'], now)} ago",
                    max(w[1]["at"] for w in waiting))]

def review_request(pr, me, now):
    """Rule 1. Informal asks ("@you can you review") arrive via `conversation`."""
    if me not in pr["requested"]:
        return []
    at = pr["requested_at"].get(me) or pr["created"]
    return [reason("review_requested", at, f"review requested {ago(at, now)} ago")]

def own_pr(pr, me, now, merge=None):
    """Rule 3: your PR, when there is something for you to do on it."""
    if pr["author"] != me:
        return []
    out = []
    open_threads = [t for t in pr["threads"] if not t["resolved"] and t["comments"]]
    if open_threads:
        lasts = [max(c["at"] for c in t["comments"]) for t in open_threads]
        n = len(open_threads)
        # The ruleset refuses to merge with an open thread, even a typo nit.
        out.append(reason("unresolved_thread", min(lasts),
                           f"{n} unresolved thread{'s' if n != 1 else ''} "
                           f"block{'s' if n == 1 else ''} merge",
                           max(lasts)))
    latest = {}
    for r in pr["reviews"]:
        if r["author"] != me and r["state"] in ("APPROVED", "CHANGES_REQUESTED", "DISMISSED"):
            latest[r["author"]] = r
    blocking = [r for r in latest.values() if r["state"] == "CHANGES_REQUESTED"
                and (r["at"] or "") > (pr.get("head_at") or "")]
    if blocking:
        r = min(blocking, key=lambda r: r["at"])
        out.append(reason("changes_requested", r["at"],
                           f"{r['author']} requested changes {ago(r['at'], now)} ago",
                           max(b["at"] for b in blocking)))
    m = merge or {}
    if m.get("mergeable") == "CONFLICTING":
        out.append(reason("conflict", None, "conflicts with master, needs a rebase"))
    if m.get("ci") in ("FAILURE", "ERROR"):
        out.append(reason("ci_failed", None, "CI is red"))
    return out

def latest_ack(pr, me):
    """(hash, at) of your newest code-review ACK on this PR, or None."""
    best = None
    for c in pr["comments"]:
        if c["author"] != me:
            continue
        for h in ACK_RE.findall(c["body"]):
            if best is None or c["at"] >= best[1]:
                best = (h.lower(), c["at"])
    for t in pr["threads"]:
        for c in t["comments"]:
            if c["author"] == me:
                for h in ACK_RE.findall(c["body"]):
                    if best is None or c["at"] >= best[1]:
                        best = (h.lower(), c["at"])
    return best

def stale_ack(pr, me, now):
    """Rule 4: you ACKed a commit the PR has since moved past."""
    if pr["author"] == me:
        return []
    ack = latest_ack(pr, me)
    if not ack or not pr.get("head"):
        return []
    acked, at = ack
    if pr["head"].lower().startswith(acked):
        return []
    pushed = pr.get("head_at") or ""
    if pushed > at:
        return [reason("stale_ack", pushed,
                        f"pushed {ago(pushed, now)} ago, past your ACK {acked[:7]}")]
    # A head older than the ACK means the ACK named some other commit.
    return [reason("stale_ack", at, f"your ACK {acked[:7]} is not the PR head")]

def assigned(issue, me, now):
    """Rule 5: assigned to you and nothing links a fix yet."""
    if me not in issue["assignees"] or issue["linked_prs"]:
        return []
    at = issue["assigned_at"].get(me) or issue["created"]
    quiet = max([issue["created"]] + [c["at"] for c in issue["comments"]])
    bits = [f"assigned {ago(at, now)} ago", f"quiet {ago(quiet, now)}"]
    ms = issue.get("milestone") or {}
    if ms.get("dueOn"):
        bits.append(f"{ms['title']} due {ms['dueOn'][:10]}")
    return [reason("assigned", at, " · ".join(bits))]

def pending_review(pr, me, now):
    """Rule 6: a review you started and never submitted. Only its author can
    see a PENDING review, which is why this works at all."""
    mine = [r for r in pr["reviews"] if r["author"] == me and r["state"] == "PENDING"]
    if not mine:
        return []
    at = min(r["at"] for r in mine)
    return [reason("pending_review", at, f"you started a review {ago(at, now)} ago and never submitted it")]

# ---------------------------------------------------------------- assembly
def collect(snap, me, now, merge=None):
    """[obligation], oldest wait first. `merge` is {number: merge_state} for the
    PRs whose mergeability was asked for -- your own."""
    merge = merge or {}
    out = []
    for pr in snap["prs"]:
        reasons = (review_request(pr, me, now) + conversation(pr, me, now)
                   + threads(pr, me, now) + own_pr(pr, me, now, merge.get(pr["number"]))
                   + stale_ack(pr, me, now) + pending_review(pr, me, now))
        if reasons:
            out.append(_item(pr, reasons))
    for issue in snap["issues"]:
        reasons = conversation(issue, me, now) + assigned(issue, me, now)
        if reasons:
            out.append(_item(issue, reasons))
    out.sort(key=sort_key)
    return out

def _item(it, reasons):
    return {"key": key_of(it["url"]),
            "number": it["number"], "kind": it["kind"], "title": it["title"],
            "url": it["url"], "author": it["author"], "item": it,
            "reasons": reasons}

def sort_key(ob):
    """Longest-waiting first. A state with no visible start (a conflict) sorts
    by when the item last moved, so it neither jumps the queue nor sinks."""
    starts = [r["at"] for r in ob["reasons"] if r["at"]]
    return min(starts) if starts else ob["item"]["updated"]

def add_reason(obs, item, r):
    """Attach a reason to the item's obligation, creating it if needed."""
    for ob in obs:
        if ob["number"] == item["number"]:
            ob["reasons"].append(r)
            return
    obs.append(_item(item, [r]))
    obs.sort(key=sort_key)

# ---------------------------------------------------------------- mutes
def fingerprint(ob):
    """What a mute remembers: the newest event behind the card, and which
    kinds of reason it had. Kept in the card itself so dragging it to Passed
    carries it along."""
    last = max((r["last"] for r in ob["reasons"] if r["last"]), default="")
    kinds = sorted({r["kind"] for r in ob["reasons"]})
    return last, kinds

def is_muted(ob, mute):
    """A mute holds until a NEW trigger: a reason kind that wasn't there when
    you muted it, or a timed event newer than the newest one you saw. General
    activity on the item never re-arms it -- only what would have made it an
    obligation in the first place."""
    if not mute:
        return False
    muted_last, muted_kinds = mute
    for r in ob["reasons"]:
        if r["kind"] not in muted_kinds:
            return False
        if r["last"] and r["last"] > muted_last:
            return False
    return True
