"""Cards for the board and the DM text. Formatting only -- every decision about
what appears, and in what order, has been made before anything gets here."""
import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from board import key_marker, mute_marker
from obligations import fingerprint

# First match wins, so the order is which reason names the card.
ICONS = [("pending_review", "📝"), ("review_requested", "👀"), ("stale_ack", "♻️"),
         ("changes_requested", "🔧"), ("unresolved_thread", "🔧"), ("conflict", "🔧"),
         ("ci_failed", "🔧"), ("mention", "💬"), ("thread", "💬"), ("reply", "💬"),
         ("assigned", "📌"), ("unshipped", "⚡")]
OPP_ICONS = [("needs_review", "📋"), ("moved", "🔁"), ("unreviewed", "🔍"), ("new_issue", "📥")]

def ob_icon(ob):
    kinds = {r["kind"] for r in ob["reasons"]}
    return next((i for k, i in ICONS if k in kinds), "•")

def opp_icon(o):
    return next((i for k, i in OPP_ICONS if k in o["sources"]), "•")

def _ci(v):
    return {"SUCCESS": "green", "FAILURE": "red", "ERROR": "red",
            "PENDING": "running", "EXPECTED": "running"}.get(v or "", "none")

def _size(it):
    return f"+{it['additions']}/-{it['deletions']} {it['changed_files']}f"

def reasons_text(ob):
    """The reasons, then any context notes -- notes inform, but are not part
    of what a mute remembers."""
    return " · ".join([r["text"] for r in ob["reasons"]] + ob.get("notes", []))

# ---------------------------------------------------------------- board
def ob_card(ob, gist=None):
    it = ob["item"]
    size = f" · {_size(it)}" if it["kind"] == "pr" and any(
        r["kind"] in ("review_requested", "stale_ack") for r in ob["reasons"]) else ""
    tail = f" *({gist})*" if gist else ""
    last, kinds = fingerprint(ob)
    return (f"- [ ] {ob_icon(ob)} [#{ob['number']}]({ob['url']}) {it['title'][:80]}"
            f" · {reasons_text(ob)}{size}{tail} {mute_marker(last, kinds)}")

def opp_card(o, why=None):
    it = o["item"]
    bits = []
    if o["kind"] == "pr":
        bits.append(_size(it))
        bits.append(f"CI {_ci(o.get('ci'))}")
        if o.get("mergeable") == "CONFLICTING":
            bits.append("conflicts")
    elif it["labels"]:
        bits.append(it["labels"][0])
    bits.append(f"{o['age_days']:.0f}d")
    if o["note"]:
        bits.append(o["note"])
    tail = f" *({why})*" if why else ""
    return (f"- [ ] {opp_icon(o)} [#{o['number']}]({o['url']}) {it['title'][:80]} · "
            + " · ".join(bits) + tail)

def unshipped_card(u):
    if u["source"] == "fork-pr":
        head = (f"[fork#{u['number']}]({u['url']}) {u['title'][:80]} · "
                f"+{u['additions']}/-{u['deletions']} · {u['commits']} commit"
                f"{'s' if u['commits'] != 1 else ''} · last {u['last']}")
    else:
        head = (f"`{u['branch']}` {u['title'][:70]} · {u['commits']} commit"
                f"{'s' if u['commits'] != 1 else ''} · last {u['last']} · no PR anywhere")
    link = ""
    if u.get("issue"):
        verb = "fixes" if u["issue"]["how"] == "refs" else "probably fixes"
        link = f" · {verb} [#{u['issue']['number']}]({u['issue']['url']})"
    return f"- [ ] ⚡ {head}{link} {key_marker(u['key'])}"

# ---------------------------------------------------------------- DM
def dm(today, obs, gists, claimed_lane, picks, judged, n_opps, unshipped,
       new_keys, warnings=(), claim_alerts=(), muted=0, briefed=(), cap=3):
    """The phone message. Every obligation, then the top picks, then counts.
    Plain text: Telegram autolinks bare URLs, and markdown would have to
    survive whatever transport carries it."""
    L = [f"Floresta · {today}", ""]
    for a in claim_alerts:
        L.append(f"⚠ {a}")
    if claim_alerts:
        L.append("")
    if obs:
        L.append(f"🔴 Needs you · {len(obs)}")
        for i, ob in enumerate(obs, 1):
            new = "🆕 " if ob["key"] in new_keys else ""
            where = f" (in your {claimed_lane[ob['key']]})" if ob["key"] in claimed_lane else ""
            L.append(f"{i}. {new}{ob_icon(ob)} #{ob['number']} {ob['item']['title'][:70]}{where}")
            L.append(f"   {reasons_text(ob)}")
            if gists.get(ob["key"]):
                L.append(f"   {gists[ob['key']]}")
            L.append(f"   {ob['url']}")
    else:
        L.append("✅ Nothing is blocked on you.")
    L.append("")
    top = picks[:cap]
    if top:
        L.append("🎯 Worth a look" + ("" if judged else " (ranking order — judge unavailable)"))
        for i, (o, why, pitch, est) in enumerate(top, 1):
            it = o["item"]
            new = "🆕 " if o["key"] in new_keys else ""
            L.append(f"{i}. {new}{opp_icon(o)} #{o['number']} {it['title'][:70]}")
            size = (f"{_size(it)} · CI {_ci(o.get('ci'))}" if o["kind"] == "pr"
                    else f"{o['age_days']:.0f}d old")
            if o["note"]:
                size += f" · {o['note']}"
            L.append(f"   {('~' + est + ' · ') if est else ''}{size}")
            if pitch or why:
                L.append(f"   {pitch or why}")
            L.append(f"   {o['url']}")
        L.append("")
    rest = n_opps - len(top)
    counts = ([f"{rest} more opportunit{'ies' if rest != 1 else 'y'}"] if rest > 0 else []) + \
             ([f"⚡ {unshipped} unshipped"] if unshipped else []) + \
             ([f"🔕 {muted} muted"] if muted else [])
    L.append("📋 board: " + (" · ".join(counts) if counts else "nothing else"))
    if briefed:
        L.append("📝 briefed: " + ", ".join(briefed))
    for w in warnings:
        L.append(f"⚠ {w}")
    return "\n".join(L)
