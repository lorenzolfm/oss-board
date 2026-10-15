"""GitHub access via the `gh` CLI.

Deliberately avoids the Search API: during design it returned materially wrong
counts (382 open PRs for bitcoin/bitcoin reported as 0), so everything here uses
GraphQL repository() queries or REST, which are authoritative.
"""
import json, subprocess, time

class GhError(RuntimeError):
    pass

def graphql(query, retries=3, timeout=90):
    """Run one GraphQL request and return its `data` object.

    Keep queries small: a large multi-alias query is rejected with an HTML error
    page rather than JSON, which is why callers chunk.
    """
    last = None
    for attempt in range(retries):
        try:
            p = subprocess.run(["gh", "api", "graphql", "-f", "query=" + query],
                               capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            last = f"timed out after {timeout}s"
            time.sleep(2 * (attempt + 1))
            continue
        if p.returncode == 0:
            try:
                payload = json.loads(p.stdout)
            except json.JSONDecodeError:
                last = "non-JSON response: " + p.stdout[:200].replace("\n", " ")
            else:
                if payload.get("errors") and not payload.get("data"):
                    last = json.dumps(payload["errors"])[:300]
                else:
                    return payload.get("data") or {}
        else:
            last = (p.stderr or p.stdout)[:300].replace("\n", " ")
        time.sleep(2 * (attempt + 1))
    raise GhError(f"graphql failed after {retries} attempts: {last}")

def rest(path, paginate=False, jq=None, timeout=90):
    cmd = ["gh", "api", path]
    if paginate:
        cmd.append("--paginate")
    if jq:
        cmd += ["--jq", jq]
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        raise GhError(f"rest {path} timed out")
    if p.returncode != 0:
        raise GhError(f"rest {path} failed: {(p.stderr or p.stdout)[:300]}")
    return p.stdout

def viewer_login():
    return graphql("query { viewer { login } }")["viewer"]["login"]
