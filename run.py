#!/usr/bin/env python3
"""Floresta triage: what is blocked on you, then what is worth your evening.

Order of operations matters:
  1. Read the board FIRST -- mutes, tombstones and claims are user state and
     must be honoured before anything is decided.
  2. Gather deterministically: one snapshot of the repo, then obligations,
     unshipped work and opportunities as pure functions of it.
  3. Ask the model for gists and an opportunity ranking. Either can fail; the
     board and the DM are still written, from computed reasons and ranking
     order, and say so.
  4. Write the board deterministically, then DM if a slot is due.
"""
import argparse, datetime as dt, json, os, sys

ROOT = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(ROOT, "lib"))
import board as B, brief, claims, config, footprint, gh, model, notify
import obligations as O, opportunities, project, render, snapshot, unshipped

BOARD = os.path.expanduser("~/Documents/Vault/Opensource/Tasks.md")
STATE = os.path.join(ROOT, "state")
MAX_BRIEFINGS = 2       # bound the cost of a claim spree
SLOT_GRACE_HOURS = 8    # a slot missed by more than this is skipped, not sent late

def log(msg):
    print(f"[{dt.datetime.now().strftime('%H:%M:%S')}] {msg}", file=sys.stderr, flush=True)

def state_path(name):
    return os.path.join(STATE, name)

def is_floresta(key):
    k = key.lower()
    return k.startswith(config.REPO.lower() + "#") or k.startswith(config.FORK.lower() + "#")

# ---------------------------------------------------------------- schedule
def due_slot(now, slots, last_sent):
    """The DM slot this run should answer for, or None.

    The timer is hourly; the DM is twice a day. A run sends when the most
    recent slot at or before `now` has not been answered yet. A missed slot
    (the desktop was off) is sent by the first run after it -- unless it is
    so old that the next one is the better message."""
    cands = []
    for s in slots:
        h, m = map(int, s.split(":"))
        for back in (0, 1):
            d = now.date() - dt.timedelta(days=back)
            t = dt.datetime.combine(d, dt.time(h, m), tzinfo=now.tzinfo)
            if t <= now:
                cands.append(t)
    slot = max(cands)
    if now - slot > dt.timedelta(hours=SLOT_GRACE_HOURS):
        return None
    if last_sent and dt.datetime.fromisoformat(last_sent) >= slot:
        return None
    return slot

# ---------------------------------------------------------------- gathering
def promote_unshipped(obs, items, snap, me):
    """An unshipped fix for an issue that is yours -- assigned to you, or one
    you were @-mentioned on -- is an obligation on that issue, not a card in
    the Unshipped lane. Returns the keys promoted."""
    rx = O.mention_re(me)
    by_num = {i["number"]: i for i in snap["issues"]}
    promoted = set()
    for u in items:
        issue = by_num.get((u.get("issue") or {}).get("number"))
        if not issue:
            continue
        mentioned = any(rx.search(c["body"]) for c in [{"body": issue["body"]}] + issue["comments"])
        if me not in issue["assignees"] and not mentioned:
            continue
        what = f"fork#{u['number']}" if u["source"] == "fork-pr" else f"branch `{u['branch']}`"
        at = f"{u['last']}T00:00:00Z"
        O.add_reason(obs, issue, O.reason("unshipped", at,
                                           f"your {what} would fix this and never went upstream"))
        promoted.add(u["key"])
    return promoted

def enrich_stale_acks(obs, me):
    """Say whether the push past your ACK was a rebase or new code. Computed,
    not guessed: the PR's diff at the ACKed commit against its diff now."""
    for ob in obs:
        for r in ob["reasons"]:
            if r["kind"] != "stale_ack":
                continue
            ack = O.latest_ack(ob["item"], me)
            d = snapshot.ack_interdiff(config.REPO, ack[0], ob["item"]["head"]) if ack else None
            if d is None:
                continue
            ob["interdiff"] = d
            if d["rebase_only"]:
                r["text"] += " · rebase only, re-ACK is quick"
            else:
                files = ", ".join(f.rsplit("/", 1)[-1] for f in d["changed"][:3])
                more = f" +{len(d['changed']) - 3}" if len(d["changed"]) > 3 else ""
                r["text"] += f" · code changed in {files}{more}"

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--board", default=BOARD)
    ap.add_argument("--dry-run", action="store_true",
                    help="render to stdout, do not touch the vault or send anything")
    ap.add_argument("--no-model", action="store_true", help="skip gists and the judge")
    ap.add_argument("--no-briefings", action="store_true",
                    help="skip writing briefing notes for newly claimed cards")
    ap.add_argument("--max-briefings", type=int, default=MAX_BRIEFINGS)
    ap.add_argument("--notify", action="store_true",
                    help="beat the heartbeat, and DM when a slot is due")
    ap.add_argument("--dm-now", action="store_true", help="DM regardless of the schedule")
    args = ap.parse_args()

    os.makedirs(STATE, exist_ok=True)
    now = dt.datetime.now(dt.timezone.utc)
    local = now.astimezone()
    vault_dir = os.path.dirname(args.board)
    warnings, degraded = [], []

    # 1. user state first
    bd = B.Board(args.board)
    dropped = bd.migrate(is_floresta)
    if dropped:
        log(f"board: migrated from the multi-repo layout, {dropped} non-Floresta cards dropped")
    tombstones, mutes, claimed = bd.tombstones(), bd.mutes(), bd.claimed()
    log(f"board: {'existing' if bd.existed else 'new'} · {len(tombstones)} passed "
        f"({len(mutes)} muted) · {len(claimed)} claimed")

    # 2. gather. The snapshot is the one source that may fail the run: without
    #    it every lane would be computed from nothing, so the board is left
    #    exactly as it was and the heartbeat goes down.
    me = gh.viewer_login()
    log(f"snapshot of {config.REPO} as {me}...")
    snap = snapshot.fetch(config.REPO)
    log(f"  {len(snap['prs'])} open PRs, {len(snap['issues'])} open issues")
    my_prs = [p["number"] for p in snap["prs"] if p["author"] == me]
    merge = snapshot.merge_state(config.REPO, my_prs)
    obs = O.collect(snap, me, now, merge)
    enrich_stale_acks(obs, me)

    # Neither of these may cost the board. A failed source leaves its lane as
    # it was: stale cards are recoverable, deleted ones are not.
    try:
        items, unshipped_ok = unshipped.run(snap["issues"], snapshot.mine(config.REPO, me), me), True
    except Exception as exc:                       # noqa: BLE001 - degrade the lane
        log(f"  unshipped failed ({type(exc).__name__}: {exc})")
        items, unshipped_ok = [], False
        degraded.append("unshipped")
    promoted = promote_unshipped(obs, items, snap, me)
    # Work you have already done for an issue that is waiting on you is worth
    # saying on that card, even when it isn't yours enough to promote.
    for u in items:
        num = (u.get("issue") or {}).get("number")
        for ob in obs:
            if ob["number"] == num and u["key"] not in promoted:
                what = f"fork#{u['number']}" if u["source"] == "fork-pr" else f"`{u['branch']}`"
                ob.setdefault("notes", []).append(f"⚡ your {what} may fix this")
    shelf = [u for u in items if u["key"] not in promoted
             and u["key"] not in tombstones and u["key"] not in claimed]

    visible = [ob for ob in obs if not O.is_muted(ob, mutes.get(ob["key"]))]
    n_muted = len(obs) - len(visible)
    log(f"  {len(visible)} obligations ({n_muted} muted) · {len(shelf)} unshipped"
        + (f" · {len(promoted)} promoted" if promoted else ""))

    fp = footprint.compute(unshipped.my_emails(config.CLONES),
                           extra=[f"{config.UPSTREAM_REF}..{u['branch']}"
                                  for u in items if u["source"] == "branch"])
    column = frozenset()
    if config.PROJECT["enabled"]:
        try:
            column = frozenset(project.needs_review())
        except gh.GhError as exc:
            log(f"  project board unreadable ({exc})")
            warnings.append("project board unreadable — does the token have read:project?")
            degraded.append("project")
    skip = {ob["key"] for ob in obs} | set(claimed)
    opps = opportunities.collect(snap, me, now, skip, tombstones, fp, column)
    ms = snapshot.merge_state(config.REPO, [o["number"] for o in opps if o["kind"] == "pr"])
    for o in opps:
        o.update(ms.get(o["number"]) or {})
    opps = opportunities.fallback_rank(opps, fp)
    log(f"  {len(opps)} opportunity candidates")

    # 3. model. Gists are decoration; the ranking falls back to fallback_rank.
    gists = {} if args.no_model else model.gists(visible, me, state_path("gists.json"))
    if args.no_model:
        picks, judged = [(o, "", "", "") for o in opps[:config.OPPORTUNITY_CAP]], False
    else:
        picks, judged = model.rank(opps, fp, config.OPPORTUNITY_CAP, state_path("rank.json"),
                                   config.JUDGE_TTL_HOURS)
    log(f"  {len(gists)} gists · ranking {'judged' if judged else 'mechanical'}")

    # 4. write. An obligation you have dragged into Todo/Doing is yours already:
    #    it stays on that card, as an annotation, instead of appearing twice.
    active = {k: lane for k, (lane, _) in claimed.items() if lane in ("Todo", "Doing")}
    bd.set_agent_lane("Needs me", [render.ob_card(ob, gists.get(ob["key"]))
                                   for ob in visible if ob["key"] not in active])
    bd.set_agent_lane("Opportunities", [render.opp_card(o, w) for o, w, _, _ in picks])
    if unshipped_ok:
        bd.set_agent_lane("Unshipped", [render.unshipped_card(u) for u in shelf])
    else:
        log("  Unshipped left untouched: its source failed")

    status = claims.fetch([k for k in claimed if B.is_github_key(k)], me) if claimed else {}
    by_key = {ob["key"]: ob for ob in visible}
    claim_alerts = []
    for key in claimed:
        notes = []
        if key in by_key:
            notes.append(render.reasons_text(by_key[key]))
        st = status.get(key) or {}
        notes += st.get("notes") or []
        bd.annotate(key, " · ".join(notes))
        if st.get("urgent"):
            claim_alerts.append(f"#{key.rsplit('#', 1)[1]} — {' · '.join(st['notes'])}")

    briefed = [] if args.no_briefings or args.dry_run else write_briefings(
        bd, claimed, snap, me, vault_dir, args.max_briefings, local)

    if args.dry_run:
        print(bd.render())
    else:
        bd.save()
        log(f"wrote {args.board}")

    # 5. DM
    last_dm = model.load(state_path("last_dm.json"))
    seen = last_dm.get("seen") or {}
    marks = {ob["key"]: O.fingerprint(ob)[0] for ob in visible}
    marks.update({o["key"]: o["item"].get("head") or o["item"]["created"] for o, *_ in picks})
    new_keys = {k for k, v in marks.items() if seen.get(k) != v}
    text = render.dm(local.strftime("%a %d %b %H:%M"), visible, gists, active, picks, judged,
                     len(picks), len(shelf), new_keys, warnings=warnings,
                     claim_alerts=claim_alerts, muted=n_muted, briefed=briefed,
                     cap=config.DM_OPPORTUNITIES)
    with open(state_path("summary.txt"), "w") as f:
        f.write(text)
    slot = due_slot(local, config.DM_SLOTS, last_dm.get("at"))
    if args.dry_run:
        print(text)
    elif args.notify and (slot or args.dm_now):
        results = notify.dispatch(text)
        for name, ok, detail in results:
            log(f"notify {name}: {'ok' if ok else 'FAILED'} — {detail}")
        if any(ok for _, ok, _ in results):
            model.save(state_path("last_dm.json"), {"at": local.isoformat(), "seen": marks})
    elif args.notify:
        log(f"DM not due; last sent {last_dm.get('at', 'never')}")

    json.dump({"generated": now.isoformat(), "degraded": degraded,
               "counts": {"obligations": len(visible), "muted": n_muted,
                          "opportunities": len(picks), "candidates": len(opps),
                          "unshipped": len(shelf), "judged": judged, "gists": len(gists)}},
              open(state_path("last_run.json"), "w"), indent=2)

    if args.notify:
        ok, detail = notify.heartbeat(
            os.environ.get("UPTIME_KUMA_PUSH_URL"), status="up",
            msg=f"{len(visible)} need you · {len(picks)} opp"
                # A lane that silently goes stale every run is the exact failure
                # this monitor exists to catch, so say so in the beat.
                + (f" · degraded: {','.join(degraded)}" if degraded else ""))
        log(f"heartbeat: {'ok' if ok else 'skipped'} — {detail}")
    return 0

def write_briefings(bd, claimed, snap, me, vault_dir, limit, local):
    """Briefing notes for newly claimed cards. After the lanes are set, so a
    briefing failure cannot cost the board write."""
    items = {B.key_of(i["url"]): i for i in snap["prs"] + snap["issues"]}
    out = []
    for key, (lane, line) in claimed.items():
        if len(out) >= limit:
            break
        it = items.get(key)
        if lane == "Done" or not it:
            continue                          # finished, closed, or not upstream
        if it["kind"] == "pr" and it["author"] == me:
            continue                          # you wrote it
        repo, num = key.rsplit("#", 1)
        path = brief.note_path(vault_dir, repo, int(num))
        if os.path.exists(path) or brief.wikilink(path) in line:
            continue
        kind = "review" if it["kind"] == "pr" else "issue"
        log(f"briefing {key} ({kind})...")
        written, why = brief.write_briefing(vault_dir, repo, int(num), it["url"], kind,
                                            local.date().isoformat(), me=me)
        if not written:
            log(f"  skipped ({why})")
            continue
        log(f"  {why}: {written}")
        link = brief.wikilink(written)
        for ln in ("Todo", "Doing"):
            for i, l in enumerate(bd.lanes.get(ln, [])):
                if B.card_key(l) == key and link not in l:
                    bd.lanes[ln][i] = f"{l} {link}"
        out.append(f"#{num}")
    return out

if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as exc:                       # noqa: BLE001 - top level
        log(f"FAILED: {type(exc).__name__}: {exc}")
        if "--notify" in sys.argv:
            notify.heartbeat(os.environ.get("UPTIME_KUMA_PUSH_URL"),
                             status="down", msg=f"{type(exc).__name__}: {exc}")
        raise
