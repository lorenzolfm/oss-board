"""The org project board's "Needs Review" column.

Needs a token with `read:project`; without it the query fails and the source
is simply absent from that run. `survey` exists for the one-off check that the
column is curated -- items moving in and out recently, not a name nobody uses.
"""
import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import config, gh

def _items(org, number):
    out, cursor = [], None
    while True:
        after = f', after: "{cursor}"' if cursor else ""
        data = gh.graphql(f'''query {{ organization(login: "{org}") {{
          projectV2(number: {number}) {{ items(first: 100{after}) {{
            pageInfo {{ hasNextPage endCursor }}
            nodes {{
              updatedAt
              fieldValueByName(name: "Status") {{
                ... on ProjectV2ItemFieldSingleSelectValue {{ name updatedAt }} }}
              content {{ __typename
                ... on PullRequest {{ number state repository {{ nameWithOwner }} }}
                ... on Issue {{ number state repository {{ nameWithOwner }} }} }}
            }} }} }} }} }}''')
        page = (((data.get("organization") or {}).get("projectV2") or {}).get("items")) or {}
        for n in page.get("nodes") or []:
            c = n.get("content") or {}
            status = n.get("fieldValueByName") or {}
            out.append({"repo": (c.get("repository") or {}).get("nameWithOwner"),
                        "number": c.get("number"), "state": c.get("state"),
                        "type": c.get("__typename"), "status": status.get("name"),
                        "moved": status.get("updatedAt") or n.get("updatedAt")})
        info = page.get("pageInfo") or {}
        if not info.get("hasNextPage"):
            return out
        cursor = info.get("endCursor")

def needs_review(repo=config.REPO, cfg=config.PROJECT):
    """Open item numbers of `repo` sitting in the column."""
    want = cfg["column"].lower()
    return {i["number"] for i in _items(cfg["org"], cfg["number"])
            if (i["repo"] or "").lower() == repo.lower() and i["state"] == "OPEN"
            and (i["status"] or "").lower() == want}

def survey(cfg=config.PROJECT):
    import collections
    items = _items(cfg["org"], cfg["number"])
    by = collections.defaultdict(list)
    for i in items:
        by[i["status"]].append(i)
    for status, its in sorted(by.items(), key=lambda kv: -len(kv[1])):
        opened = sum(1 for i in its if i["state"] == "OPEN")
        newest = max((i["moved"] or "" for i in its), default="")
        print(f"{status!s:20} {len(its):4} items  {opened:4} open  last moved {newest[:10]}")

if __name__ == "__main__":
    survey()
