"""Read, merge and write the obsidian-kanban board.

Contract, and the reason this is deterministic Python rather than model output:

  * The agent owns three lanes and rewrites them wholesale.
  * The user's lanes (Todo / Doing / Done / Passed) are preserved EXACTLY,
    except that annotations may be appended to a card's text.
  * Passed means two things, told apart by the card itself:
      - an obligation card carries a `<!--t:...-->` fingerprint; dragged to
        Passed it is MUTED until a new trigger re-arms it;
      - anything else dragged to Passed is a permanent tombstone.
  * Anything unrecognised in the file is passed through untouched.

Card identity is the GitHub URL, not the card text, so the user can rewrite a
card's wording freely without the agent losing track of it. A card with no URL
of its own (a local branch) carries a `<!--k:...-->` key instead, which also
wins over any URL in the text -- an unshipped card links the issue it fixes,
and must not be mistaken for that issue.
"""
import os, re

AGENT_LANES = ["Needs me", "Opportunities", "Unshipped"]
USER_LANES = ["Todo", "Doing", "Done", "Passed"]
LANE_ORDER = AGENT_LANES + USER_LANES
# The 91-repo board's lanes. Their presence is what marks a board to migrate.
OLD_AGENT_LANES = ["Issues", "To Review", "Stranded Work"]

HEADER = "---\n\nkanban-plugin: board\n\n---\n"
URL_RE = re.compile(r"https://github\.com/([\w.-]+/[\w.-]+)/(?:issues|pull)/(\d+)")
KEY_RE = re.compile(r"<!--k:(.*?)-->")
MUTE_RE = re.compile(r"<!--t:([^|>]*)\|([^>]*)-->")
LANE_RE = re.compile(r"^##\s+(.*?)\s*$")
SETTINGS_RE = re.compile(r"^%%\s*kanban:settings", re.M)
# An annotation the agent previously appended, so it can be replaced not stacked.
ANNOT_RE = re.compile(r"\s*<!--a-->.*?<!--/a-->")

def key_of(url):
    """'owner/repo#N' for an issue or PR URL, or None."""
    m = URL_RE.search(url or "")
    return f"{m.group(1)}#{m.group(2)}" if m else None

def is_github_key(key):
    """True for 'owner/repo#N' -- the keys GitHub can be asked about."""
    return bool(re.fullmatch(r"[\w.-]+/[\w.-]+#\d+", key or ""))

def card_key(line):
    m = KEY_RE.search(line)
    return m.group(1) if m else key_of(line)

def key_marker(key):
    return f"<!--k:{key}-->"

def mute_marker(last, kinds):
    return f"<!--t:{last}|{','.join(kinds)}-->"

class Board:
    def __init__(self, path):
        self.path = path
        self.lanes = {}          # name -> list of raw card lines
        self.lane_order = []
        self.settings_block = ""
        self.existed = os.path.exists(path)
        if self.existed:
            self._parse(open(path, encoding="utf-8").read())
        else:
            self.lane_order = list(LANE_ORDER)
            for n in self.lane_order:
                self.lanes[n] = []

    def _parse(self, text):
        m = SETTINGS_RE.search(text)
        if m:
            self.settings_block = text[m.start():]
            text = text[:m.start()]
        current = None
        for line in text.splitlines():
            lm = LANE_RE.match(line)
            if lm:
                current = lm.group(1)
                if current not in self.lanes:
                    self.lanes[current] = []
                    self.lane_order.append(current)
                continue
            if current is None:
                continue                      # frontmatter / preamble
            if line.strip():
                self.lanes[current].append(line)
        for n in LANE_ORDER:                  # ensure every expected lane exists
            if n not in self.lanes:
                self.lanes[n] = []
                self.lane_order.append(n)

    # ---- migration ---------------------------------------------------------
    def migrate(self, keep):
        """One-time conversion of the old multi-repo board, in place.

        The old agent lanes are dropped and the user lanes keep only the cards
        `keep(key)` accepts -- you chose to overwrite rather than archive. A
        board with none of the old lanes is never touched, so after the first
        run your lanes are preserved exactly again. Returns cards dropped."""
        if not any(n in self.lanes for n in OLD_AGENT_LANES):
            return 0
        dropped = 0
        for n in OLD_AGENT_LANES:
            dropped += len(self.lanes.pop(n, []))
        for n in USER_LANES:
            kept = [l for l in self.lanes.get(n, []) if card_key(l) and keep(card_key(l))]
            dropped += len(self.lanes.get(n, [])) - len(kept)
            self.lanes[n] = kept
        custom = [n for n in self.lane_order if n not in LANE_ORDER and n not in OLD_AGENT_LANES]
        self.lane_order = list(LANE_ORDER) + custom
        return dropped

    # ---- reads -------------------------------------------------------------
    def tombstones(self):
        """Keys in Passed, muted or not. Opportunities and unshipped work never
        come back from Passed; obligations consult `mutes` instead."""
        return {k for k in map(card_key, self.lanes.get("Passed", [])) if k}

    def mutes(self):
        """{key: (last, kinds)} for obligation cards dragged to Passed. Muting
        the same item twice leaves two cards; the newer fingerprint wins."""
        out = {}
        for line in self.lanes.get("Passed", []):
            k, m = card_key(line), MUTE_RE.search(line)
            if not k or not m:
                continue
            last, kinds = m.group(1), set(filter(None, m.group(2).split(",")))
            if k not in out or last > out[k][0]:
                out[k] = (last, kinds)
        return out

    def claimed(self):
        """{key: (lane, line)} for everything in the user's active lanes."""
        out = {}
        for lane in ("Todo", "Doing", "Done"):
            for line in self.lanes.get(lane, []):
                k = card_key(line)
                if k:
                    out[k] = (lane, line)
        return out

    # ---- writes ------------------------------------------------------------
    def set_agent_lane(self, name, lines):
        if name not in AGENT_LANES:
            raise ValueError(f"{name} is not an agent-owned lane")
        self.lanes[name] = list(lines)

    def annotate(self, key, note):
        """Append/replace an annotation on a card in a user lane. Never moves it."""
        for lane in ("Todo", "Doing", "Done"):
            for i, line in enumerate(self.lanes.get(lane, [])):
                if card_key(line) == key:
                    base = ANNOT_RE.sub("", line).rstrip()
                    self.lanes[lane][i] = f"{base} <!--a-->{note}<!--/a-->" if note else base
                    return True
        return False

    def render(self):
        parts = [HEADER]
        for name in self.lane_order:
            parts.append(f"\n## {name}\n\n")
            body = self.lanes.get(name, [])
            parts.append(("\n".join(body) + "\n") if body else "")
        collapse = '"list-collapse":[' + ",".join("false" for _ in self.lane_order) + ']'
        settings = self.settings_block
        if not settings:
            settings = '%% kanban:settings\n```\n{"kanban-plugin":"board",' + collapse + '}\n```\n%%\n'
        else:
            # Keep list-collapse in step with the lane count or Kanban mis-renders.
            settings = re.sub(r'"list-collapse":\[[^\]]*\]', collapse, settings)
        return "".join(parts) + "\n\n" + settings.rstrip() + "\n"

    def save(self):
        """Atomic write: a crash must never leave a half-written board."""
        os.makedirs(os.path.dirname(self.path), exist_ok=True)
        tmp = self.path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            f.write(self.render())
        os.replace(tmp, self.path)
