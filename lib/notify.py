"""Send the run summary to Telegram, and beat the uptime-kuma heartbeat.

Two transports:

  telegram  -- the Telegram Bot API directly. Fully specified: the bot token
               already exists in the homelab sealed secret as TELEGRAM_BOT_TOKEN
               and the chat id is the openclaw allowlist entry. Works even when
               openclaw is down.
  openclaw  -- POST through the openclaw gateway. Its /api/channels/telegram
               endpoint exists (it answers 401, not 404) but the request body
               shape is undocumented, so this stays opt-in until confirmed.

Notification failure must never fail the run: the board is the deliverable.
"""
import json, os, subprocess, sys, urllib.error, urllib.parse, urllib.request

TIMEOUT = 20

def _read_secret(path):
    if not path:
        return None
    try:
        return open(os.path.expanduser(path)).read().strip() or None
    except OSError:
        return None

def _post(url, data, headers, timeout=TIMEOUT):
    req = urllib.request.Request(url, data=data, headers=headers, method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status, r.read(400).decode("utf-8", "replace")

def send_telegram(text, token, chat_id):
    if not token or not chat_id:
        return False, "missing token or chat_id"
    url = f"https://api.telegram.org/bot{token}/sendMessage"
    body = urllib.parse.urlencode({
        "chat_id": chat_id,
        "text": text[:4000],
        "disable_web_page_preview": "true",
    }).encode()
    try:
        status, resp = _post(url, body,
                             {"Content-Type": "application/x-www-form-urlencoded"})
        return status == 200, f"HTTP {status} {resp[:120]}"
    except urllib.error.HTTPError as e:
        return False, f"HTTP {e.code} {e.read(200).decode('utf-8','replace')}"
    except Exception as e:
        return False, f"{type(e).__name__}: {e}"

def send_openclaw(text, base_url, token, path, timeout=TIMEOUT):
    """Opt-in: confirm the body shape against your openclaw build before use."""
    if not token:
        return False, "missing gateway token"
    url = base_url.rstrip("/") + path
    body = json.dumps({"text": text[:4000]}).encode()
    try:
        status, resp = _post(url, body, {"Content-Type": "application/json",
                                         "Authorization": f"Bearer {token}"}, timeout)
        return 200 <= status < 300, f"HTTP {status} {resp[:120]}"
    except urllib.error.HTTPError as e:
        return False, f"HTTP {e.code} {e.read(200).decode('utf-8','replace')}"
    except Exception as e:
        return False, f"{type(e).__name__}: {e}"

def heartbeat(push_url, status="up", msg="ok", timeout=TIMEOUT):
    """uptime-kuma push monitor, mirroring the borgmatic hooks."""
    if not push_url:
        return False, "no push url"
    sep = "&" if "?" in push_url else "?"
    url = f"{push_url}{sep}" + urllib.parse.urlencode({"status": status, "msg": msg[:200]})
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return r.status == 200, f"HTTP {r.status}"
    except Exception as e:
        return False, f"{type(e).__name__}: {e}"

def chunks(text, limit=3900):
    """Split at line boundaries so no part exceeds Telegram's limit. The DM
    lists every obligation, uncapped, so a long day is several messages
    rather than a truncated one."""
    out, cur = [], ""
    for line in text.split("\n"):
        while len(line) > limit:                 # a single absurd line
            out.append(line[:limit]); line = line[limit:]
        if cur and len(cur) + 1 + len(line) > limit:
            out.append(cur); cur = line
        else:
            cur = f"{cur}\n{line}" if cur else line
    if cur:
        out.append(cur)
    return out

def dispatch(text, env=os.environ):
    """Fire whichever transports are configured, one message per chunk.
    Returns a list of (name, ok, detail)."""
    results = []
    for part in chunks(text):
        results.extend(_dispatch_one(part, env))
    return results

def _dispatch_one(text, env):
    results = []
    tg_token = env.get("TELEGRAM_BOT_TOKEN") or _read_secret(env.get("TELEGRAM_BOT_TOKEN_FILE"))
    chat_id = env.get("TELEGRAM_CHAT_ID")
    if tg_token and chat_id:
        results.append(("telegram",) + send_telegram(text, tg_token, chat_id))
    oc_url = env.get("OPENCLAW_URL")
    if oc_url and env.get("OPENCLAW_ENABLED") == "1":
        oc_token = env.get("OPENCLAW_GATEWAY_TOKEN") or _read_secret(env.get("OPENCLAW_GATEWAY_TOKEN_FILE"))
        results.append(("openclaw",) + send_openclaw(
            text, oc_url, oc_token,
            env.get("OPENCLAW_SEND_PATH", "/api/channels/telegram/send")))
    if not results:
        results.append(("none", False, "no transport configured"))
    return results
