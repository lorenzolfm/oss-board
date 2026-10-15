"""The obligation rules, through `collect` -- the one entry point run.py uses.

Several cases replay what the live Floresta data showed while this was built:
a quote-reply with no @ (#1336), an answer that predated the assignment
(#167), a mention already answered by an inline review (#1075), and the busy
PR you once commented on that must NOT count (#1161's conversation)."""
import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import obligations as O
from testkit import chk, done, snap, pr, issue, c, review, thread, day, ME, NOW

def kinds(s, merge=None):
    """{number: sorted reason kinds} for everything collect returns."""
    return {ob["number"]: sorted(r["kind"] for r in ob["reasons"])
            for ob in O.collect(s, ME, NOW, merge)}

print("1. conversation: what counts as waiting on you")
cases = [
    ("review requested", pr(1, requested=[ME], requested_at={ME: day(5)}), ["review_requested"]),
    ("@-mention after your last word", pr(2, comments=[c(ME, 10), c("bob", 3, f"@{ME} thoughts?")]),
     ["mention"]),
    ("mention answered by an inline reply since (#1075)",
     pr(3, comments=[c("bob", 9, f"cc @{ME}")], threads=[thread(c(ME, 4, "nit"))]), []),
    ("mention answered by a review with no body",
     pr(4, comments=[c("bob", 9, f"@{ME} ping")], reviews=[review(ME, 4)]), []),
    ("quote-reply without an @ (#1336)",
     pr(5, comments=[c(ME, 5, "Imagine a scenario where we are still in DownloadingHeaders and a peer stalls"),
                     c("davidson", 1, "> Imagine a scenario where we are still in DownloadingHeaders\n\nYes, fixed.")]),
     ["mention"]),
    ("busy PR you once commented on is not an obligation",
     pr(6, comments=[c(ME, 40), c("bob", 30), c("carol", 20), c("bob", 2)]), []),
    ("mention of a longer login is not you",
     pr(7, comments=[c("bob", 2, f"@{ME}-bot please run")]), []),
    ("bots never wait on you",
     pr(8, author=ME, head_at=day(10), comments=[c("ci[bot]", 2, "coverage report", bot=True)]), []),
    ("PR description cc's you", pr(9, body=f"Fixes a stall. cc @{ME} for review"), ["mention"]),
]
for name, item, want in cases:
    got = kinds(snap(prs=[item])).get(item["number"], [])
    chk(name, got == want, got)

ob = O.collect(snap(prs=[pr(9, body=f"cc @{ME} for review")]), ME, NOW)[0]
chk("an informal ask reads as one", "asked you to review" in ob["reasons"][0]["text"],
    ob["reasons"][0]["text"])

print("\n2. assigned issues")
cases = [
    ("an answer to you stays one after a later assignment (#167)",
     issue(167, assignees=[ME], assigned_at={ME: day(8)},
           comments=[c(ME, 40, "What's the scope?"), c("csgui", 29, "Run a node and re-level logs")]),
     ["assigned", "reply"]),
    ("chatter from before you were handed it is not addressed to you",
     issue(11, assignees=[ME], assigned_at={ME: day(5)}, comments=[c("bob", 20), c("carol", 10)]),
     ["assigned"]),
    ("an assigned issue with a fix linked is not idle",
     issue(12, assignees=[ME], assigned_at={ME: day(5)}, linked_prs=[{"number": 1327, "author": ME}]),
     []),
    ("someone else's issue you commented on is not yours",
     issue(13, comments=[c(ME, 10), c("bob", 2)]), []),
]
for name, item, want in cases:
    got = kinds(snap(issues=[item])).get(item["number"], [])
    chk(name, got == want, got)

print("\n3. your own PRs")
cases = [
    ("a comment since your push", pr(20, author=ME, head_at=day(10), comments=[c("bob", 3)]), ["reply"]),
    ("a push answers the comment before it", pr(21, author=ME, head_at=day(2), comments=[c("bob", 3)]), []),
    ("an ACK is good news, not a request",
     pr(22, author=ME, head_at=day(10), comments=[c("bob", 3, "ACK 1a2b3c4d\n\nran the tests")]), []),
    ("an APPROVED review asks nothing",
     pr(23, author=ME, head_at=day(10), reviews=[review("bob", 3, "APPROVED", "ACK ffffff1")]), []),
    ("changes requested since your push",
     pr(24, author=ME, head_at=day(10), reviews=[review("bob", 3, "CHANGES_REQUESTED", "needs a test")]),
     ["changes_requested"]),
    ("changes requested before your latest push is answered",
     pr(25, author=ME, head_at=day(2), reviews=[review("bob", 3, "CHANGES_REQUESTED", "needs a test")]), []),
    ("an unresolved thread blocks merge, whoever spoke last",
     pr(26, author=ME, head_at=day(2), threads=[thread(c("jaoleal", 30, "typo: is_backfiling"))]),
     ["unresolved_thread"]),
    ("a resolved thread does not", pr(27, author=ME, threads=[thread(c("bob", 3), resolved=True)]), []),
]
for name, item, want in cases:
    got = kinds(snap(prs=[item])).get(item["number"], [])
    chk(name, got == want, got)
got = kinds(snap(prs=[pr(28, author=ME, draft=True)]), {28: {"mergeable": "CONFLICTING", "ci": "FAILURE"}})
chk("conflicts and red CI come from merge state", got.get(28) == ["ci_failed", "conflict"], got)

print("\n4. review threads on others' PRs")
cases = [
    ("someone answered in your thread", pr(30, threads=[thread(c(ME, 9, "why?"), c("bob", 2, "because"))]),
     ["thread"]),
    ("you had the last word", pr(31, threads=[thread(c("bob", 9), c(ME, 2))]), []),
    ("a resolved thread is done",
     pr(32, threads=[thread(c(ME, 9), c("bob", 2), resolved=True)]), []),
    ("a thread you are not in is not yours", pr(33, threads=[thread(c("bob", 9), c("carol", 2))]), []),
]
for name, item, want in cases:
    got = kinds(snap(prs=[item])).get(item["number"], [])
    chk(name, got == want, got)

print("\n5. stale ACKs and unsubmitted reviews")
head = "abcdef0123456789abcdef0123456789abcdef01"
cases = [
    ("an ACK the PR moved past", pr(40, head=head, head_at=day(2), comments=[c(ME, 5, "ACK 1234567")]),
     ["stale_ack"]),
    ("an ACK of the current head", pr(41, head=head, comments=[c(ME, 5, "utACK abcdef0")]), []),
    ("Concept ACK names no commit", pr(42, head=head, comments=[c(ME, 5, "Concept ACK")]), []),
    ("NACK is not an ACK", pr(43, head=head, comments=[c(ME, 5, "NACK 1234567, see above")]), []),
    ("only your newest ACK counts",
     pr(44, head=head, comments=[c(ME, 9, "ACK 1234567"), c(ME, 2, "re-ACK abcdef0")]), []),
    ("a review you never submitted", pr(45, reviews=[review(ME, 6, "PENDING")]), ["pending_review"]),
]
for name, item, want in cases:
    got = kinds(snap(prs=[item])).get(item["number"], [])
    chk(name, got == want, got)

print("\n6. longest wait first; a bare state sorts by when the item last moved")
s = snap(prs=[pr(50, requested=[ME], requested_at={ME: day(2)}),
              pr(51, requested=[ME], requested_at={ME: day(40)}),
              pr(52, author=ME, updated=day(10))],
         issues=[issue(53, comments=[c("bob", 20, f"@{ME}")])])
order = [ob["number"] for ob in O.collect(s, ME, NOW, {52: {"mergeable": "CONFLICTING"}})]
chk("ordered by how long they have waited", order == [51, 53, 52, 50], order)

print("\n7. a mute holds until a new trigger, not general activity")
base = pr(60, comments=[c(ME, 10), c("bob", 5, f"@{ME} can you look?")])
ob = O.collect(snap(prs=[base]), ME, NOW)[0]
mute = (O.fingerprint(ob)[0], set(O.fingerprint(ob)[1]))
busier = dict(base, updated=day(0), comments=base["comments"] + [c("carol", 1, "+1 from me")])
ob2 = O.collect(snap(prs=[busier]), ME, NOW)[0]
chk("unrelated comments keep it muted", O.is_muted(ob2, mute))
pinged = dict(base, comments=base["comments"] + [c("carol", 1, f"@{ME} also this")])
chk("a newer mention re-arms it", not O.is_muted(O.collect(snap(prs=[pinged]), ME, NOW)[0], mute))
requested = dict(base, requested=[ME], requested_at={ME: day(1)})
chk("a new kind of reason re-arms it",
    not O.is_muted(O.collect(snap(prs=[requested]), ME, NOW)[0], mute))
own = pr(61, author=ME, head_at=day(2))
cob = O.collect(snap(prs=[own]), ME, NOW, {61: {"mergeable": "CONFLICTING"}})[0]
cmute = (O.fingerprint(cob)[0], set(O.fingerprint(cob)[1]))
chk("a muted conflict stays muted while it persists",
    O.is_muted(O.collect(snap(prs=[own]), ME, NOW, {61: {"mergeable": "CONFLICTING"}})[0], cmute))

done()
