"""What nobody is waiting on you for, but you would want to know about.

Four sources, as agreed:
  new_issue     opened in the last NEW_ISSUE_DAYS, unassigned, nothing linked
  unreviewed    non-draft PR with zero approvals that you have not touched
  moved         a PR you engaged with that got commits or comments since
  needs_review  on the org board's Needs Review column (when enabled)

An item that is already an obligation is never an opportunity too, and
anything in Passed stays gone. Ranking belongs to the judge; `fallback_rank`
is only what the board shows when the judge is unavailable.
"""
import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import config, footprint
from board import key_of
from obligations import engaged_at, parse

def _days(iso, now):
    return (now - parse(iso)).total_seconds() / 86400

def _since_you(pr, me, mine_at):
    others = [c for c in pr["comments"] if c["author"] != me and not c["bot"] and c["at"] > mine_at]
    others += [c for t in pr["threads"] for c in t["comments"]
               if c["author"] != me and not c["bot"] and c["at"] > mine_at]
    pushed = (pr.get("head_at") or "") > mine_at
    return pushed, len(others)

def collect(snap, me, now, skip, tombstones, fp, column=frozenset()):
    """`skip` holds the obligation keys; `column` the numbers on Needs Review."""
    out = []
    for pr in snap["prs"]:
        key = key_of(pr["url"])
        if key in skip or key in tombstones or pr["author"] == me or pr["author_bot"]:
            continue
        sources, note = [], ""
        mine_at = engaged_at(pr, me)
        if mine_at:
            pushed, n = _since_you(pr, me, mine_at)
            if pushed or n:
                sources.append("moved")
                bits = (["pushed"] if pushed else []) + ([f"{n} comment{'s' if n != 1 else ''}"] if n else [])
                note = " + ".join(bits) + " since you"
        elif (not pr["draft"] and pr["review_decision"] != "CHANGES_REQUESTED"
              and not any(r["state"] == "APPROVED" for r in pr["reviews"])):
            sources.append("unreviewed")
        if pr["number"] in column:
            sources.append("needs_review")
        if sources:
            out.append(_opp(pr, key, sources, note, now, fp))
    for issue in snap["issues"]:
        key = key_of(issue["url"])
        if key in skip or key in tombstones or issue["author"] == me or issue["author_bot"]:
            continue
        sources = []
        if (_days(issue["created"], now) <= config.NEW_ISSUE_DAYS
                and not issue["assignees"] and not issue["linked_prs"]):
            sources.append("new_issue")
        if issue["number"] in column:
            sources.append("needs_review")
        if sources:
            out.append(_opp(issue, key, sources, "", now, fp))
    return out

def _opp(it, key, sources, note, now, fp):
    return {"key": key, "number": it["number"], "kind": it["kind"], "title": it["title"],
            "url": it["url"], "author": it["author"], "sources": sources, "note": note,
            "age_days": round(_days(it["created"], now), 1),
            "areas": footprint.overlap(it.get("files") or [], fp),
            "item": it}

PRIORITY = {"needs_review": 3, "moved": 2, "unreviewed": 1, "new_issue": 1}

def blocked(o):
    """A PR that cannot merge as it stands: draft, conflicting, or red."""
    return bool(o["item"].get("draft") or o.get("mergeable") == "CONFLICTING"
                or o.get("ci") in ("FAILURE", "ERROR"))

def fallback_rank(opps, fp):
    """Anything mergeable before anything blocked; within that, maintainers'
    column, then threads you're already in, then your areas, then smaller and
    newer. Mechanical on purpose: the DM says so."""
    def weight(o):
        return sum(fp.get(a, 0) for a in o["areas"])
    def size(o):
        it = o["item"]
        return it.get("additions", 0) + it.get("deletions", 0)
    return sorted(opps, key=lambda o: (blocked(o), -max(PRIORITY[s] for s in o["sources"]),
                                       -weight(o), size(o), o["age_days"]))
