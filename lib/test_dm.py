"""The DM: when it goes out, that it lists every obligation, how the judge's
answer is trusted, and that the hourly refresh does not pay for the model
when nothing moved."""
import datetime as dt, os, sys, tempfile
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import model, notify, obligations as O, render
import run as R
from testkit import chk, done, snap, pr, c, day, ME, NOW

tmp = tempfile.mkdtemp()
TZ = dt.timezone(dt.timedelta(hours=-3))
SLOTS = ("08:07", "19:23")
at = lambda d, h, m: dt.datetime(2026, 10, d, h, m, tzinfo=TZ)

print("1. twice a day from an hourly timer")
cases = [
    ("the first run after a slot sends", at(15, 8, 7), at(14, 19, 23), at(15, 8, 7)),
    ("the next hourly run does not", at(15, 9, 7), at(15, 8, 7), None),
    ("a run before the evening slot does not", at(15, 19, 7), at(15, 8, 7), None),
    ("the evening slot sends", at(15, 19, 23), at(15, 8, 7), at(15, 19, 23)),
    ("overnight runs stay quiet", at(16, 3, 7), at(15, 19, 23), None),
    ("a slot missed while the desktop was off is sent late", at(15, 11, 30), at(14, 19, 23), at(15, 8, 7)),
    ("one missed by more than the grace waits for the next", at(16, 6, 0), at(15, 8, 7), None),
    ("a first run ever sends", at(15, 8, 7), None, at(15, 8, 7)),
]
for name, now, last, want in cases:
    got = R.due_slot(now, SLOTS, last.isoformat() if last else None)
    chk(name, got == want, got)

print("\n2. the message lists every obligation, uncapped")
prs = [pr(100 + i, requested=[ME], requested_at={ME: day(i + 1)}, title="t" * 120) for i in range(25)]
obs = O.collect(snap(prs=prs), ME, NOW)
msg = render.dm("Wed 15 Oct 08:07", obs, {}, {}, [], False, 0, 0, set())
chk("all 25 listed", all(f"#{p['number']} " in msg for p in prs))
parts = notify.chunks(msg)
chk("split, not truncated, for Telegram", len(parts) > 1 and all(len(p) <= 3900 for p in parts)
    and "\n".join(parts) == msg, [len(p) for p in parts])
chk("an empty day says so",
    "Nothing is blocked on you" in render.dm("d", [], {}, {}, [], False, 0, 0, set()))
claimed = render.dm("d", obs[:1], {}, {obs[0]["key"]: "Doing"}, [], False, 0, 0, set())
chk("an obligation you already took says where", "(in your Doing)" in claimed, claimed)
chk("plain text, no markdown links", "](" not in msg)

print("\n3. the judge may only reorder what it was given")
opps = [{"key": f"getfloresta/Floresta#{n}", "number": n, "kind": "pr", "title": f"pr {n}",
         "url": f"https://github.com/getfloresta/Floresta/pull/{n}", "sources": ["unreviewed"],
         "note": "", "age_days": 2.0, "areas": [], "item": pr(n)} for n in (1, 2, 3)]
calls = []
def fake(answer):
    def claude(prompt, timeout=300):
        calls.append(prompt)
        return answer
    return claude
real = model.claude
try:
    model.claude = fake({"picks": [{"number": 3, "why": "w", "pitch": "p", "est": "1h"},
                                   {"number": 99, "why": "invented"}, {"number": 3}, {"bogus": 1},
                                   {"number": "1", "why": "x"}]})
    picks, judged = model.rank(opps, {}, 8, os.path.join(tmp, "r1.json"), 12)
    chk("judged order, invented and duplicate dropped",
        judged and [o["number"] for o, *_ in picks] == [3, 1], [o["number"] for o, *_ in picks])
    model.claude = fake({"picks": []})
    picks, judged = model.rank(opps, {}, 8, os.path.join(tmp, "r2.json"), 12)
    chk("an empty verdict falls back to ranking order, flagged",
        not judged and [o["number"] for o, *_ in picks] == [1, 2, 3])
    model.claude = fake(None)
    msg = render.dm("d", [], {}, {}, model.rank(opps, {}, 8, os.path.join(tmp, "r3.json"), 12)[0],
                    False, 3, 0, set())
    chk("a failed judge is said out loud", "judge unavailable" in msg, msg)

    print("\n4. the hourly refresh reuses what has not moved")
    calls.clear()
    model.claude = fake({"picks": [{"number": 2}]})
    model.rank(opps, {}, 8, os.path.join(tmp, "r4.json"), 12)
    model.rank(opps, {}, 8, os.path.join(tmp, "r4.json"), 12)
    chk("same candidates, one judge call", len(calls) == 1, len(calls))
    moved = [dict(o, item=dict(o["item"], head="e" * 40)) if o["number"] == 2 else o for o in opps]
    model.rank(moved, {}, 8, os.path.join(tmp, "r4.json"), 12)
    chk("a push re-ranks", len(calls) == 2, len(calls))

    calls.clear()
    model.claude = fake({"gists": [{"key": obs[0]["key"], "gist": "asks for a yes/no"}]})
    g1 = model.gists(obs[:1], ME, os.path.join(tmp, "g.json"))
    g2 = model.gists(obs[:1], ME, os.path.join(tmp, "g.json"))
    chk("a gist is written once per fingerprint", len(calls) == 1 and g1 == g2 == {obs[0]["key"]: "asks for a yes/no"},
        (len(calls), g1, g2))
    newer = O.collect(snap(prs=[dict(prs[0], comments=[c("bob", 0, f"@{ME} and now?")])]), ME, NOW)
    model.gists(newer, ME, os.path.join(tmp, "g.json"))
    chk("a new trigger asks again", len(calls) == 2, len(calls))
    model.claude = fake(None)
    chk("a failed gist pass leaves no stale gist on a changed item",
        model.gists(O.collect(snap(prs=[dict(prs[0], requested_at={ME: day(0)})]), ME, NOW),
                    ME, os.path.join(tmp, "g.json")) == {})
finally:
    model.claude = real

done()
