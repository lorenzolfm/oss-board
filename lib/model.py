"""The two model passes, and the only places a model touches the output.

  gists  one line per obligation. Decorative by contract: an obligation with
         no gist is still listed, in the same place, with its computed reasons.
  rank   the opportunity judge -- filters, orders, estimates, pitches. It may
         only point at candidates it was given; anything else is dropped.

Both are cached in state/ so the hourly refresh pays for the model only when
something it would look at has changed. Neither subprocess gets a GitHub
token: the data goes in inline, and nothing here needs to fetch.
"""
import hashlib, json, os, re, subprocess, sys, time

def log(msg):
    print(f"[model] {msg}", file=sys.stderr, flush=True)

def claude(prompt, timeout=300):
    """Parsed JSON object from `claude -p`, or None on any failure."""
    env = {k: v for k, v in os.environ.items() if k not in ("GH_TOKEN", "GITHUB_TOKEN")}
    try:
        p = subprocess.run(["claude", "-p", prompt], capture_output=True,
                           text=True, timeout=timeout, env=env)
    except (subprocess.TimeoutExpired, FileNotFoundError) as e:
        log(f"unavailable ({type(e).__name__})")
        return None
    if p.returncode != 0:
        log(f"failed rc={p.returncode}")
        return None
    m = re.search(r"\{.*\}", p.stdout, re.S)
    try:
        return json.loads(m.group(0)) if m else None
    except json.JSONDecodeError:
        log("unparseable JSON")
        return None

def load(path):
    try:
        with open(path) as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}

def save(path, data):
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(data, f, indent=1)
    os.replace(tmp, path)

def _clip(text, n):
    text = " ".join((text or "").split())
    return text if len(text) <= n else text[: n - 1] + "…"

# ---------------------------------------------------------------- gists
GIST_PROMPT = """Each item below is waiting on Lorenzo, a senior Rust/Bitcoin \
engineer, in the Floresta repo. The `reasons` already say WHY it is waiting; do \
not repeat them. For each item write `gist`: at most 22 words saying WHAT is \
being asked of him or what changed, concretely enough that he can tell from \
his phone whether it is a two-minute reply or an evening ("asks whether the \
empty-headers case can still stall; wants a yes/no and a test"). If the \
excerpts do not say, write what he would have to look at first. Never invent \
facts not in the excerpts.

Reply with ONLY a JSON object, no prose, no code fence:
{"gists":[{"key":"owner/repo#1","gist":"..."}]}

ITEMS:
%s
"""

def gist_fingerprint(ob):
    last = max((r["last"] or "" for r in ob["reasons"]), default="")
    kinds = ",".join(sorted(r["kind"] for r in ob["reasons"]))
    return f"{last}|{kinds}|{ob['item'].get('head') or ''}"

def _excerpt(ob, me):
    it = ob["item"]
    since = max((c["at"] for c in it["comments"] if c["author"] == me), default="")
    waiting = [c for c in it["comments"] if c["author"] != me and not c["bot"] and c["at"] > since]
    # The comment that started the wait is the one the gist is about; on a busy
    # thread it would fall out of a plain "last three".
    recent = waiting if len(waiting) <= 3 else waiting[:1] + waiting[-2:]
    out = {"key": ob["key"], "kind": it["kind"], "title": it["title"],
           "reasons": [r["text"] for r in ob["reasons"]],
           "description": _clip(it["body"], 500),
           "waiting_comments": [{"by": c["author"], "text": _clip(c["body"], 700)} for c in recent]}
    if it["kind"] == "pr":
        out["size"] = f"+{it['additions']}/-{it['deletions']} in {it['changed_files']} files"
        out["files"] = it["files"][:12]
        open_threads = [t for t in it["threads"] if not t["resolved"]
                        and (it["author"] == me or any(c["author"] == me for c in t["comments"]))]
        out["threads"] = [{"path": t["path"], "last": [{"by": c["author"], "text": _clip(c["body"], 400)}
                                                        for c in t["comments"][-2:]]}
                          for t in open_threads[:4]]
    if ob.get("interdiff"):
        out["since_your_ack"] = ob["interdiff"]
    return out

def gists(obs, me, cache_path, timeout=300):
    """{key: gist}. Cached per fingerprint; only changed items reach the model."""
    cache = load(cache_path)
    want = {ob["key"]: gist_fingerprint(ob) for ob in obs}
    todo = [ob for ob in obs if (cache.get(ob["key"]) or {}).get("fp") != want[ob["key"]]]
    if todo:
        payload = json.dumps([_excerpt(ob, me) for ob in todo], indent=1)
        got = claude(GIST_PROMPT % payload, timeout=timeout)
        for g in (got or {}).get("gists") or []:
            k = g.get("key") if isinstance(g, dict) else None
            if k in want and g.get("gist"):
                cache[k] = {"fp": want[k], "gist": _clip(str(g["gist"]), 200)}
    cache = {k: v for k, v in cache.items() if k in want}     # forget what cleared
    save(cache_path, cache)
    return {k: v["gist"] for k, v in cache.items() if v.get("fp") == want[k]}

# ---------------------------------------------------------------- rank
RANK_PROMPT = """You are picking open-source work for Lorenzo: a senior \
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
  `why`:   at most 12 words, concrete ("chain reorg path you wrote, 40 lines").
  `pitch`: 1-2 sentences, at most 35 words: why this one, and the first step.
  `est`:   rough wall-clock, one token like "30m", "1h", "2-3h".

Reply with ONLY a JSON object, no prose, no code fence:
{"picks":[{"number":1,"why":"...","pitch":"...","est":"1h"}]}

FOOTPRINT: %(footprint)s

CANDIDATES:
%(payload)s
"""

def _candidate(o):
    it = o["item"]
    c = {"number": o["number"], "kind": o["kind"], "title": it["title"],
         "author": it["author"], "sources": o["sources"], "age_days": o["age_days"],
         "labels": it["labels"], "milestone": (it.get("milestone") or {}).get("title"),
         "comments": len(it["comments"]), "areas": o["areas"],
         "description": _clip(it["body"], 300)}
    if o["note"]:
        c["since_you"] = o["note"]
    if it["kind"] == "pr":
        c.update({"size": f"+{it['additions']}/-{it['deletions']}",
                  "files": it["changed_files"], "draft": it["draft"],
                  "ci": o.get("ci"), "mergeable": o.get("mergeable"),
                  "reviews": len({r["author"] for r in it["reviews"] if r["state"] != "PENDING"})})
    return c

def rank_fingerprint(opps):
    """Re-rank when the candidate set or a PR's head moves, not on every
    comment -- comments are most of the churn and rarely change the order."""
    sig = sorted(f"{o['key']}@{o['item'].get('head') or ''}" for o in opps)
    return hashlib.sha256("\n".join(sig).encode()).hexdigest()[:16]

def rank(opps, fp, cap, cache_path, ttl_hours, timeout=300):
    """(ordered [(opp, why, pitch, est)], judged). Falls back to the order
    given, untouched, with judged=False."""
    by_num = {o["number"]: o for o in opps}
    cache, sig = load(cache_path), rank_fingerprint(opps)
    fresh = cache.get("sig") == sig and time.time() - cache.get("at", 0) < ttl_hours * 3600
    picks = cache.get("picks") if fresh else None
    if picks is None and opps:
        payload = json.dumps([_candidate(o) for o in opps], indent=1)
        got = claude(RANK_PROMPT % {"cap": cap, "payload": payload,
                                    "footprint": json.dumps(list(fp)[:10])}, timeout=timeout)
        picks = (got or {}).get("picks") if isinstance((got or {}).get("picks"), list) else None
        if picks:                        # an empty verdict is retried, not kept
            save(cache_path, {"sig": sig, "at": time.time(), "picks": picks})
    out, seen = [], set()
    for p in picks or []:
        try:
            n = int(p["number"])
        except (KeyError, TypeError, ValueError):
            continue
        if n in seen or n not in by_num:
            continue                     # invented, or a duplicate
        seen.add(n)
        out.append((by_num[n], _clip(str(p.get("why") or ""), 80),
                    _clip(str(p.get("pitch") or ""), 240), _clip(str(p.get("est") or ""), 8)))
        if len(out) >= cap:
            break
    if out:
        return out, True
    # No usable answer -- or the judge kept nothing, which is distrusted the
    # same way the old board distrusted it.
    return [(o, "", "", "") for o in opps[:cap]], False
