"""Status of things the user has claimed, so their cards can be annotated.

The agent never moves a card out of Todo/Doing/Done. It only appends a note
saying what changed upstream, because the decision stays the user's.
"""
import datetime as dt, os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import gh

NOW = dt.datetime.now(dt.timezone.utc)

def _age(iso):
    return (NOW - dt.datetime.fromisoformat(iso.replace("Z", "+00:00"))).days

def fetch(keys, me):
    """keys: iterable of 'owner/repo#number'. Returns {key: status dict}."""
    keys = list(keys)
    out = {}
    STEP = 5
    for i in range(0, len(keys), STEP):
        batch = keys[i:i + STEP]
        body = ""
        for j, k in enumerate(batch):
            repo, num = k.rsplit("#", 1)
            owner, name = repo.split("/", 1)
            body += f'''
  n{j}: repository(owner: "{owner}", name: "{name}") {{
    issueOrPullRequest(number: {num}) {{
      __typename
      ... on Issue {{
        state closedAt updatedAt
        assignees(first: 5) {{ nodes {{ login }} }}
        timelineItems(last: 20, itemTypes: [CROSS_REFERENCED_EVENT]) {{
          nodes {{ ... on CrossReferencedEvent {{ createdAt source {{
            __typename
            ... on PullRequest {{ number url state author {{ login }} }}
          }} }} }}
        }}
      }}
      ... on PullRequest {{
        state merged mergedAt updatedAt author {{ login }}
      }}
    }}
  }}'''
        try:
            data = gh.graphql("query {" + body + "\n}")
        except gh.GhError:
            continue
        for j, k in enumerate(batch):
            node = (data.get(f"n{j}") or {}).get("issueOrPullRequest")
            if node:
                out[k] = _interpret(k, node, me)
    return out

def _interpret(key, n, me):
    kind = n.get("__typename")
    st = {"key": key, "kind": kind, "notes": [], "urgent": False}
    if kind == "Issue":
        if n.get("state") == "CLOSED":
            st["notes"].append(f"closed upstream {(n.get('closedAt') or '')[:10]}")
            st["urgent"] = True
        others = [a["login"] for a in (n.get("assignees") or {}).get("nodes") or []
                  if a["login"] != me]
        if others:
            st["notes"].append(f"assigned to {others[0]}")
            st["urgent"] = True
        for ev in (n.get("timelineItems") or {}).get("nodes") or []:
            src = (ev or {}).get("source") or {}
            if src.get("__typename") != "PullRequest":
                continue
            author = (src.get("author") or {}).get("login")
            if author and author != me and src.get("state") == "OPEN":
                st["notes"].append(f"PR #{src['number']} opened by {author} — stop")
                st["urgent"] = True
                break
    elif kind == "PullRequest":
        if n.get("merged"):
            mine = ((n.get("author") or {}).get("login") or "") == me
            st["notes"].append(f"{'your PR ' if mine else ''}merged "
                               f"{(n.get('mergedAt') or '')[:10]} — drag to Done")
        elif n.get("state") == "CLOSED":
            st["notes"].append("closed without merge")
            st["urgent"] = True
        # Review feedback on an open PR is an obligation now, with rules of
        # its own; repeating it here would say the same thing twice.
    return st
