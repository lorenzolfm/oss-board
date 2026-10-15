"""The board holds the user's own decisions; losing a card is the one
unacceptable failure. These pin the merge contract, the one-time migration
from the multi-repo board, and the card identity rules that mutes, tombstones
and claims all rest on."""
import os, sys, tempfile, shutil
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import board as B, obligations as O, render
from testkit import chk, done, snap, pr, ME, NOW

tmp = tempfile.mkdtemp()
p = os.path.join(tmp, "Tasks.md")
F = "https://github.com/getfloresta/Floresta"

print("1. a fresh board is one obsidian-kanban will parse")
b = B.Board(p)
b.set_agent_lane("Needs me", [f"- [ ] x [#1]({F}/issues/1)"])
b.save()
txt = open(p).read()
chk("kanban frontmatter", "kanban-plugin: board" in txt)
chk("every lane, in order", [l[3:] for l in txt.splitlines() if l.startswith("## ")] == B.LANE_ORDER)
chk("list-collapse matches lane count",
    '"list-collapse":[' + ",".join(["false"] * len(B.LANE_ORDER)) + "]" in txt)

print("\n2. user lanes survive an agent rewrite byte for byte")
b = B.Board(p)
b.lanes["Todo"] = [f"- [ ] my words [#99]({F}/issues/99) ^abc"]
b.lanes["Doing"] = [f"- [ ] reviewing [#5]({F}/pull/5) [[Floresta-5]]"]
b.lanes["Done"] = [f"- [x] shipped [#7]({F}/pull/7)"]
b.lanes["Passed"] = [f"- [ ] nope [#1]({F}/issues/1)"]
b.save()
before = {k: list(v) for k, v in B.Board(p).lanes.items()}
b = B.Board(p)
b.set_agent_lane("Needs me", [f"- [ ] new [#2]({F}/issues/2)"])
b.set_agent_lane("Opportunities", [])
b.save()
after = B.Board(p).lanes
for lane in B.USER_LANES:
    chk(f"{lane} preserved", before[lane] == after[lane], f"{before[lane]} != {after[lane]}")
chk("agent lane replaced", after["Needs me"] == [f"- [ ] new [#2]({F}/issues/2)"])
# run.py skips set_agent_lane("Unshipped") when its source fails; the guard
# rests on an unset lane keeping its cards.
b = B.Board(p)
b.set_agent_lane("Unshipped", ["- [ ] ⚡ `wip` <!--k:branch:wip-->"])
b.save()
b = B.Board(p)
b.set_agent_lane("Needs me", [])
b.save()
chk("a lane the run did not set keeps its cards",
    B.Board(p).lanes["Unshipped"] == ["- [ ] ⚡ `wip` <!--k:branch:wip-->"])
try:
    B.Board(p).set_agent_lane("Todo", [])
    chk("refuses to rewrite a user lane", False, "no exception")
except ValueError:
    chk("refuses to rewrite a user lane", True)

print("\n3. annotations replace rather than stack, and never move a card")
b = B.Board(p)
chk("annotate finds the card", b.annotate("getfloresta/Floresta#99", "first"))
b.save()
b = B.Board(p)
b.annotate("getfloresta/Floresta#99", "second")
b.save()
line = B.Board(p).lanes["Todo"][0]
chk("one annotation, the newest", line.count("<!--a-->") == 1 and "second" in line, line)
chk("blockref and wording survive", "^abc" in line and "my words" in line, line)
b = B.Board(p)
b.annotate("getfloresta/Floresta#99", "")
b.save()
chk("annotation clears", "<!--a-->" not in B.Board(p).lanes["Todo"][0])

print("\n4. identity: a key marker beats any URL in the text")
card = f"- [ ] ⚡ `dedup` probably fixes [#1308]({F}/issues/1308) <!--k:branch:dedup-->"
chk("marker wins", B.card_key(card) == "branch:dedup", B.card_key(card))
chk("reworded card keeps its URL key",
    B.card_key(f"- [ ] TOTALLY rewritten [x]({F}/issues/42) blah") == "getfloresta/Floresta#42")
chk("branch keys are not GitHub keys", not B.is_github_key("branch:dedup")
    and B.is_github_key("lorenzolfm/Floresta#6"))

print("\n5. every rendered card yields its key back through the parser")
s = snap(prs=[pr(1336, requested=[ME], requested_at={ME: "2026-10-08T17:14:49Z"})])
ob = O.collect(s, ME, NOW)[0]
ob_line = render.ob_card(ob, "a gist")
chk("obligation card", B.card_key(ob_line) == "getfloresta/Floresta#1336", ob_line)
opp = {"key": "getfloresta/Floresta#7", "number": 7, "kind": "pr", "url": f"{F}/pull/7",
       "sources": ["unreviewed"], "note": "", "age_days": 3.0, "item": pr(7), "ci": "SUCCESS"}
chk("opportunity card", B.card_key(render.opp_card(opp, "why")) == "getfloresta/Floresta#7")
fork = {"source": "fork-pr", "key": "lorenzolfm/Floresta#6", "number": 6, "title": "t",
        "url": "https://github.com/lorenzolfm/Floresta/pull/6", "additions": 1, "deletions": 0,
        "commits": 1, "last": "2026-09-11",
        "issue": {"number": 1308, "url": f"{F}/issues/1308", "how": "guess"}}
chk("fork PR card linking an issue keeps the fork key",
    B.card_key(render.unshipped_card(fork)) == "lorenzolfm/Floresta#6")

print("\n6. Passed: an obligation card is a mute, anything else a tombstone")
b = B.Board(p)
b.lanes["Passed"] = [ob_line, f"- [ ] nope [#1]({F}/issues/1)",
                     f"- [ ] older mute [#1336]({F}/pull/1336) <!--t:2026-01-01T00:00:00Z|mention-->"]
b.save()
b = B.Board(p)
chk("both are tombstones", b.tombstones() >= {"getfloresta/Floresta#1336", "getfloresta/Floresta#1"})
m = b.mutes()
chk("only the fingerprinted card mutes", set(m) == {"getfloresta/Floresta#1336"}, m)
chk("the newer fingerprint wins",
    m["getfloresta/Floresta#1336"] == ("2026-10-08T17:14:49Z", {"review_requested"}), m)
chk("the dragged card is muted as rendered", O.is_muted(ob, m["getfloresta/Floresta#1336"]))

print("\n7. migration from the multi-repo board, once")
old = os.path.join(tmp, "Old.md")
with open(old, "w") as f:
    f.write(f"""---\n\nkanban-plugin: board\n\n---\n
## Issues\n\n- [ ] [a/b#1](https://github.com/a/b/issues/1)\n
## To Review\n\n- [ ] [c/d#2](https://github.com/c/d/pull/2)\n
## Stranded Work\n\n- [ ] ⚡ bitcoin/bips `pr2152` — 4 commit(s) ahead\n
## Todo\n\n## Doing\n\n- [ ] [getfloresta/Floresta#1298]({F}/issues/1298) mine [[Floresta-1298]]
- [ ] [bitcoindevkit/bdk-ffi#1109](https://github.com/bitcoindevkit/bdk-ffi/issues/1109)\n
## Someday\n\n- [ ] custom lane card\n
## Done\n\n## Passed\n\n- [ ] [lorenzolfm/Floresta#4](https://github.com/lorenzolfm/Floresta/pull/4) x
- [ ] [renovatebot/renovate#5](https://github.com/renovatebot/renovate/issues/5)\n""")
is_fl = lambda k: k.lower().startswith(("getfloresta/floresta#", "lorenzolfm/floresta#"))
b = B.Board(old)
n = b.migrate(is_fl)
b.save()
b = B.Board(old)
chk("old agent lanes gone", not any(l in b.lanes for l in B.OLD_AGENT_LANES), b.lane_order)
chk("new lanes first, custom lane kept after", b.lane_order == B.LANE_ORDER + ["Someday"], b.lane_order)
chk("Floresta claim kept verbatim",
    b.lanes["Doing"] == [f"- [ ] [getfloresta/Floresta#1298]({F}/issues/1298) mine [[Floresta-1298]]"],
    b.lanes["Doing"])
chk("fork tombstone kept, other repos dropped",
    b.tombstones() == {"lorenzolfm/Floresta#4"}, b.tombstones())
chk("drop count reported", n == 5, n)
b.lanes["Todo"] = ["- [ ] [bitcoindevkit/bdk-ffi#1](https://github.com/bitcoindevkit/bdk-ffi/issues/1) added later"]
chk("a migrated board is never migrated again", b.migrate(is_fl) == 0)
chk("so a card you add later survives", len(b.lanes["Todo"]) == 1)

print("\n8. the real vault board round-trips without losing a card")
src = os.path.expanduser("~/Documents/Vault/Bitcoin/Tasks.md")
if os.path.exists(src):
    cp = os.path.join(tmp, "Real.md")
    shutil.copy(src, cp)
    orig = B.Board(cp)
    orig.save()
    rt = B.Board(cp)
    chk("every card identical", all(orig.lanes[k] == rt.lanes[k] for k in orig.lanes))
    chk("source untouched", os.path.getmtime(src) < os.path.getmtime(cp))
else:
    print("  skip (no sample board)")

shutil.rmtree(tmp)
done()
