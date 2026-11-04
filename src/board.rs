//! Read, merge and write the obsidian-kanban board.
//!
//! Contract, and the reason this is deterministic code rather than model output:
//!
//!   * The agent owns three lanes and rewrites them wholesale.
//!   * The user's lanes (Todo / Doing / Done / Passed) are preserved EXACTLY,
//!     except that annotations may be appended to a card's text.
//!   * Passed means two things, told apart by the card itself:
//!       - an obligation card carries a `<!--t:...-->` fingerprint; dragged to
//!         Passed it is MUTED until a new trigger re-arms it;
//!       - anything else dragged to Passed is a permanent tombstone.
//!   * Anything unrecognised in the file is passed through untouched.
//!
//! Card identity is the GitHub URL, not the card text, so the user can rewrite a
//! card's wording freely without the agent losing track of it. A card with no URL
//! of its own (a local branch) carries a `<!--k:...-->` key instead, which also
//! wins over any URL in the text -- an unshipped card links the issue it fixes,
//! and must not be mistaken for that issue.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use regex::{NoExpand, Regex};

use crate::timestamp::Timestamp;

pub const USER_LANES: [&str; 4] = ["Todo", "Doing", "Done", "Passed"];
const LANE_ORDER: [&str; 7] = [
    "Needs me",
    "Opportunities",
    "Unshipped",
    "Todo",
    "Doing",
    "Done",
    "Passed",
];
/// The 91-repo board's lanes. Their presence is what marks a board to migrate.
const OLD_AGENT_LANES: [&str; 3] = ["Issues", "To Review", "Stranded Work"];
/// The user's lanes that hold claimed cards.
const CLAIM_LANES: [&str; 3] = ["Todo", "Doing", "Done"];

const HEADER: &str = "---\n\nkanban-plugin: board\n\n---\n";

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a constant pattern compiles")
}

static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"https://github\.com/([\w.-]+/[\w.-]+)/(?:issues|pull)/(\d+)"));
static GITHUB_KEY_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"^[\w.-]+/[\w.-]+#\d+$"));
static KEY_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"<!--k:(.*?)-->"));
static MUTE_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"<!--t:([^|>]*)\|([^>]*)-->"));
static LANE_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"^##\s+(.*?)\s*$"));
static SETTINGS_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"(?m)^%%\s*kanban:settings"));
/// An annotation the agent previously appended, so it can be replaced not stacked.
static ANNOT_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"\s*<!--a-->.*?<!--/a-->"));
static COLLAPSE_RE: LazyLock<Regex> = LazyLock::new(|| regex(r#""list-collapse":\[[^\]]*\]"#));

/// `owner/repo#N` for an issue or PR URL.
pub fn key_of(url: &str) -> Option<String> {
    let found = URL_RE.captures(url)?;
    Some(format!("{}#{}", &found[1], &found[2]))
}

/// True for `owner/repo#N` -- the keys GitHub can be asked about.
pub fn is_github_key(key: &str) -> bool {
    GITHUB_KEY_RE.is_match(key)
}

pub fn card_key(line: &str) -> Option<String> {
    match KEY_RE.captures(line) {
        Some(found) => Some(found[1].to_owned()),
        None => key_of(line),
    }
}

pub fn key_marker(key: &str) -> String {
    format!("<!--k:{key}-->")
}

pub fn mute_marker(last: &str, kinds: &[&str]) -> String {
    format!("<!--t:{last}|{}-->", kinds.join(","))
}

/// What a muted obligation remembers: the newest event behind it, and which kinds
/// of reason it had. A fingerprint that does not parse as a time (a hand-edited
/// card) counts as older than everything, so the obligation comes back rather
/// than stay hidden.
#[derive(Clone, Debug, PartialEq)]
pub struct Mute {
    pub last: Option<Timestamp>,
    pub kinds: BTreeSet<String>,
}

/// A card in one of the user's active lanes.
#[derive(Clone, Debug)]
pub struct Claim {
    pub key: String,
    pub lane: &'static str,
    pub line: String,
}

#[derive(Clone, Copy, Debug)]
pub enum AgentLane {
    NeedsMe,
    Opportunities,
    Unshipped,
}

impl AgentLane {
    fn name(self) -> &'static str {
        match self {
            Self::NeedsMe => "Needs me",
            Self::Opportunities => "Opportunities",
            Self::Unshipped => "Unshipped",
        }
    }
}

#[derive(Debug)]
struct Lane {
    name: String,
    cards: Vec<String>,
}

#[derive(Debug)]
pub struct Board {
    path: PathBuf,
    lanes: Vec<Lane>,
    settings_block: String,
    pub existed: bool,
}

impl Board {
    pub fn open(path: &Path) -> Result<Self> {
        let existed = path.exists();
        let mut board = Self {
            path: path.to_owned(),
            lanes: Vec::new(),
            settings_block: String::new(),
            existed,
        };
        if existed {
            let text = fs::read_to_string(path)
                .with_context(|| format!("reading the board at {}", path.display()))?;
            board.parse(&text);
        }
        for name in LANE_ORDER {
            board.ensure_lane(name);
        }
        Ok(board)
    }

    fn ensure_lane(&mut self, name: &str) -> usize {
        if let Some(index) = self.lanes.iter().position(|lane| lane.name == name) {
            return index;
        }
        self.lanes.push(Lane {
            name: name.to_owned(),
            cards: Vec::new(),
        });
        self.lanes.len() - 1
    }

    fn parse(&mut self, text: &str) {
        let (cards, settings) = match SETTINGS_RE.find(text) {
            Some(found) => text.split_at(found.start()),
            None => (text, ""),
        };
        self.settings_block = settings.to_owned();
        let mut current = None;
        for line in cards.lines() {
            if let Some(found) = LANE_RE.captures(line) {
                current = Some(self.ensure_lane(&found[1]));
            } else if let Some(index) = current
                && !line.trim().is_empty()
            {
                self.lanes[index].cards.push(line.to_owned());
            }
        }
    }

    fn cards(&self, name: &str) -> &[String] {
        self.lanes
            .iter()
            .find(|lane| lane.name == name)
            .map_or(&[], |lane| lane.cards.as_slice())
    }

    /// One-time conversion of the old multi-repo board, in place.
    ///
    /// The old agent lanes are dropped and the user lanes keep only the cards
    /// `keep(key)` accepts -- you chose to overwrite rather than archive. A board
    /// with none of the old lanes is never touched, so after the first run your
    /// lanes are preserved exactly again. Returns cards dropped.
    pub fn migrate(&mut self, keep: impl Fn(&str) -> bool) -> usize {
        let is_old = |lane: &Lane| OLD_AGENT_LANES.contains(&lane.name.as_str());
        if !self.lanes.iter().any(is_old) {
            return 0;
        }
        let mut dropped = 0;
        let mut lanes = Vec::new();
        for mut lane in std::mem::take(&mut self.lanes) {
            if is_old(&lane) {
                dropped += lane.cards.len();
                continue;
            }
            if USER_LANES.contains(&lane.name.as_str()) {
                let before = lane.cards.len();
                lane.cards
                    .retain(|line| card_key(line).is_some_and(|key| keep(&key)));
                dropped += before - lane.cards.len();
            }
            lanes.push(lane);
        }
        // The expected lanes first, in order, then whatever custom lanes you added.
        lanes.sort_by_key(|lane| {
            LANE_ORDER
                .iter()
                .position(|name| *name == lane.name)
                .unwrap_or(LANE_ORDER.len())
        });
        self.lanes = lanes;
        dropped
    }

    /// Keys in Passed, muted or not. Opportunities and unshipped work never come
    /// back from Passed; obligations consult `mutes` instead.
    pub fn tombstones(&self) -> HashSet<String> {
        self.cards("Passed")
            .iter()
            .filter_map(|line| card_key(line))
            .collect()
    }

    /// Obligation cards dragged to Passed. Muting the same item twice leaves two
    /// cards; the newer fingerprint wins.
    pub fn mutes(&self) -> HashMap<String, Mute> {
        let mut out = HashMap::new();
        for line in self.cards("Passed") {
            let (Some(key), Some(found)) = (card_key(line), MUTE_RE.captures(line)) else {
                continue;
            };
            let mute = Mute {
                last: Timestamp::parse(&found[1]),
                kinds: found[2]
                    .split(',')
                    .filter(|kind| !kind.is_empty())
                    .map(str::to_owned)
                    .collect(),
            };
            let newer = |held: &Mute| mute.last > held.last;
            if out.get(&key).is_none_or(newer) {
                out.insert(key, mute);
            }
        }
        out
    }

    /// Everything in the user's active lanes, in board order. A card in two lanes
    /// keeps its first position but reports the later lane.
    pub fn claimed(&self) -> Vec<Claim> {
        let mut out: Vec<Claim> = Vec::new();
        for lane in CLAIM_LANES {
            for line in self.cards(lane) {
                let Some(key) = card_key(line) else { continue };
                let claim = Claim {
                    key,
                    lane,
                    line: line.clone(),
                };
                match out.iter_mut().find(|held| held.key == claim.key) {
                    Some(held) => *held = claim,
                    None => out.push(claim),
                }
            }
        }
        out
    }

    pub fn set_agent_lane(&mut self, lane: AgentLane, cards: Vec<String>) {
        let index = self.ensure_lane(lane.name());
        self.lanes[index].cards = cards;
    }

    /// Append/replace an annotation on a card in a user lane. Never moves it.
    pub fn annotate(&mut self, key: &str, note: &str) -> bool {
        for name in CLAIM_LANES {
            let index = self.ensure_lane(name);
            for line in &mut self.lanes[index].cards {
                if card_key(line).as_deref() != Some(key) {
                    continue;
                }
                let stripped = ANNOT_RE.replace_all(line, "");
                let base = stripped.trim_end();
                *line = if note.is_empty() {
                    base.to_owned()
                } else {
                    format!("{base} <!--a-->{note}<!--/a-->")
                };
                return true;
            }
        }
        false
    }

    /// Add a briefing's wikilink to the card(s) in Todo/Doing that claimed it.
    pub fn link_note(&mut self, key: &str, link: &str) {
        for name in ["Todo", "Doing"] {
            let index = self.ensure_lane(name);
            for line in &mut self.lanes[index].cards {
                if card_key(line).as_deref() == Some(key) && !line.contains(link) {
                    line.push(' ');
                    line.push_str(link);
                }
            }
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::from(HEADER);
        for lane in &self.lanes {
            out.push_str(&format!("\n## {}\n\n", lane.name));
            if !lane.cards.is_empty() {
                out.push_str(&lane.cards.join("\n"));
                out.push('\n');
            }
        }
        let flags = vec!["false"; self.lanes.len()].join(",");
        let collapse = format!(r#""list-collapse":[{flags}]"#);
        let settings = if self.settings_block.is_empty() {
            format!(
                "%% kanban:settings\n```\n{{\"kanban-plugin\":\"board\",{collapse}}}\n```\n%%\n"
            )
        } else {
            // Keep list-collapse in step with the lane count or Kanban mis-renders.
            COLLAPSE_RE
                .replace_all(&self.settings_block, NoExpand(&collapse))
                .into_owned()
        };
        out.push_str("\n\n");
        out.push_str(settings.trim_end());
        out.push('\n');
        out
    }

    /// Atomic write: a crash must never leave a half-written board.
    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        fs::write(&tmp, self.render())?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing the board at {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    //! The board holds the user's own decisions; losing a card is the one
    //! unacceptable failure. These pin the merge contract, the one-time migration
    //! from the multi-repo board, and the card identity rules that mutes,
    //! tombstones and claims all rest on.

    use super::*;
    use crate::obligations::{self, is_muted};
    use crate::opportunities::{Opportunity, Source};
    use crate::render;
    use crate::testkit::{ME, me, now, pr, snap};
    use crate::unshipped::{IssueLink, LinkHow, Origin, Unshipped};
    use tempfile::TempDir;

    const F: &str = "https://github.com/getfloresta/Floresta";

    fn set(board: &mut Board, lane: &str, cards: &[String]) {
        let index = board.ensure_lane(lane);
        board.lanes[index].cards = cards.to_vec();
    }

    fn snapshot_of_lanes(board: &Board) -> Vec<(String, Vec<String>)> {
        board
            .lanes
            .iter()
            .map(|lane| (lane.name.clone(), lane.cards.clone()))
            .collect()
    }

    fn fresh() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("Tasks.md");
        (dir, path)
    }

    fn lane_order_of(board: &Board) -> Vec<&str> {
        board.lanes.iter().map(|lane| lane.name.as_str()).collect()
    }

    #[test]
    fn a_fresh_board_is_one_obsidian_kanban_will_parse() {
        let (_dir, path) = fresh();
        let mut board = Board::open(&path).unwrap();
        set(
            &mut board,
            "Needs me",
            &[format!("- [ ] x [#1]({F}/issues/1)")],
        );
        board.save().unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("kanban-plugin: board"));
        let lanes: Vec<_> = text.lines().filter_map(|l| l.strip_prefix("## ")).collect();
        assert_eq!(lanes, LANE_ORDER);
        let flags = vec!["false"; LANE_ORDER.len()].join(",");
        assert!(text.contains(&format!(r#""list-collapse":[{flags}]"#)));
    }

    #[test]
    fn user_lanes_survive_an_agent_rewrite_byte_for_byte() {
        let (_dir, path) = fresh();
        let mut board = Board::open(&path).unwrap();
        set(
            &mut board,
            "Todo",
            &[format!("- [ ] my words [#99]({F}/issues/99) ^abc")],
        );
        set(
            &mut board,
            "Doing",
            &[format!("- [ ] reviewing [#5]({F}/pull/5) [[Floresta-5]]")],
        );
        set(
            &mut board,
            "Done",
            &[format!("- [x] shipped [#7]({F}/pull/7)")],
        );
        set(
            &mut board,
            "Passed",
            &[format!("- [ ] nope [#1]({F}/issues/1)")],
        );
        board.save().unwrap();
        let before = Board::open(&path).unwrap();

        let mut board = Board::open(&path).unwrap();
        let new = format!("- [ ] new [#2]({F}/issues/2)");
        board.set_agent_lane(AgentLane::NeedsMe, vec![new.clone()]);
        board.set_agent_lane(AgentLane::Opportunities, vec![]);
        board.save().unwrap();

        let after = Board::open(&path).unwrap();
        for lane in USER_LANES {
            assert_eq!(before.cards(lane), after.cards(lane), "{lane}");
        }
        assert_eq!(after.cards("Needs me"), [new]);
    }

    /// run skips the Unshipped lane when its source fails; the guard rests on an
    /// unset lane keeping its cards.
    #[test]
    fn a_lane_the_run_did_not_set_keeps_its_cards() {
        let (_dir, path) = fresh();
        let wip = "- [ ] ⚡ `wip` <!--k:branch:wip-->".to_owned();
        let mut board = Board::open(&path).unwrap();
        board.set_agent_lane(AgentLane::Unshipped, vec![wip.clone()]);
        board.save().unwrap();

        let mut board = Board::open(&path).unwrap();
        board.set_agent_lane(AgentLane::NeedsMe, vec![]);
        board.save().unwrap();
        assert_eq!(Board::open(&path).unwrap().cards("Unshipped"), [wip]);
    }

    #[test]
    fn annotations_replace_rather_than_stack_and_never_move_a_card() {
        let (_dir, path) = fresh();
        let key = "getfloresta/Floresta#99";
        let mut board = Board::open(&path).unwrap();
        set(
            &mut board,
            "Todo",
            &[format!("- [ ] my words [#99]({F}/issues/99) ^abc")],
        );
        assert!(board.annotate(key, "first"));
        board.save().unwrap();

        let mut board = Board::open(&path).unwrap();
        board.annotate(key, "second");
        board.save().unwrap();
        let line = Board::open(&path).unwrap().cards("Todo")[0].clone();
        assert_eq!(line.matches("<!--a-->").count(), 1, "{line}");
        assert!(line.contains("second"), "{line}");
        assert!(line.contains("^abc") && line.contains("my words"), "{line}");

        let mut board = Board::open(&path).unwrap();
        board.annotate(key, "");
        board.save().unwrap();
        assert!(!Board::open(&path).unwrap().cards("Todo")[0].contains("<!--a-->"));
    }

    #[test]
    fn a_key_marker_beats_any_url_in_the_text() {
        let card = format!(
            "- [ ] ⚡ `dedup` probably fixes [#1308]({F}/issues/1308) <!--k:branch:dedup-->"
        );
        assert_eq!(card_key(&card).as_deref(), Some("branch:dedup"));
        let reworded = format!("- [ ] TOTALLY rewritten [x]({F}/issues/42) blah");
        assert_eq!(
            card_key(&reworded).as_deref(),
            Some("getfloresta/Floresta#42")
        );
        assert!(!is_github_key("branch:dedup"));
        assert!(is_github_key("lorenzolfm/Floresta#6"));
    }

    #[test]
    fn every_rendered_card_yields_its_key_back_through_the_parser() {
        let viewer = me();
        let snapshot = snap(vec![pr(1336).requested(ME, "2026-10-08T17:14:49Z")], vec![]);
        let obs = obligations::collect(&snapshot, &viewer, now(), &HashMap::new());
        let line = render::ob_card(&obs[0], Some("a gist"));
        assert_eq!(
            card_key(&line).as_deref(),
            Some("getfloresta/Floresta#1336"),
            "{line}"
        );

        let item = pr(7);
        let opp = Opportunity {
            key: "getfloresta/Floresta#7".to_owned(),
            item: &item,
            sources: vec![Source::Unreviewed],
            note: String::new(),
            age_days: 3.0,
            areas: vec![],
            merge: None,
        };
        assert_eq!(
            card_key(&render::opp_card(&opp, "why")).as_deref(),
            Some("getfloresta/Floresta#7")
        );

        let fork = Unshipped {
            key: "lorenzolfm/Floresta#6".to_owned(),
            origin: Origin::ForkPr {
                number: 6,
                url: "https://github.com/lorenzolfm/Floresta/pull/6".to_owned(),
                additions: 1,
                deletions: 0,
            },
            title: "t".to_owned(),
            branch: "b".to_owned(),
            commits: 1,
            last: "2026-09-11".to_owned(),
            issue: Some(IssueLink {
                number: 1308,
                url: format!("{F}/issues/1308"),
                how: LinkHow::Guess,
            }),
        };
        assert_eq!(
            card_key(&render::unshipped_card(&fork)).as_deref(),
            Some("lorenzolfm/Floresta#6"),
            "a fork PR card linking an issue keeps the fork key"
        );
    }

    #[test]
    fn in_passed_an_obligation_card_is_a_mute_and_anything_else_a_tombstone() {
        let viewer = me();
        let snapshot = snap(vec![pr(1336).requested(ME, "2026-10-08T17:14:49Z")], vec![]);
        let obs = obligations::collect(&snapshot, &viewer, now(), &HashMap::new());
        let line = render::ob_card(&obs[0], Some("a gist"));

        let (_dir, path) = fresh();
        let mut board = Board::open(&path).unwrap();
        set(
            &mut board,
            "Passed",
            &[
                line,
                format!("- [ ] nope [#1]({F}/issues/1)"),
                format!(
                    "- [ ] older mute [#1336]({F}/pull/1336) <!--t:2026-01-01T00:00:00Z|mention-->"
                ),
            ],
        );
        board.save().unwrap();

        let board = Board::open(&path).unwrap();
        let tombstones = board.tombstones();
        assert!(tombstones.contains("getfloresta/Floresta#1336"));
        assert!(tombstones.contains("getfloresta/Floresta#1"));
        let mutes = board.mutes();
        assert_eq!(mutes.len(), 1, "only the fingerprinted card mutes");
        let mute = &mutes["getfloresta/Floresta#1336"];
        assert_eq!(
            *mute,
            Mute {
                last: Timestamp::parse("2026-10-08T17:14:49Z"),
                kinds: BTreeSet::from(["review_requested".to_owned()]),
            },
            "the newer fingerprint wins"
        );
        assert!(
            is_muted(&obs[0], Some(mute)),
            "the dragged card is muted as rendered"
        );
    }

    fn old_board() -> String {
        format!(
            "---\n\nkanban-plugin: board\n\n---\n
## Issues\n\n- [ ] [a/b#1](https://github.com/a/b/issues/1)\n
## To Review\n\n- [ ] [c/d#2](https://github.com/c/d/pull/2)\n
## Stranded Work\n\n- [ ] ⚡ bitcoin/bips `pr2152` — 4 commit(s) ahead\n
## Todo\n\n## Doing\n\n- [ ] [getfloresta/Floresta#1298]({F}/issues/1298) mine [[Floresta-1298]]
- [ ] [bitcoindevkit/bdk-ffi#1109](https://github.com/bitcoindevkit/bdk-ffi/issues/1109)\n
## Someday\n\n- [ ] custom lane card\n
## Done\n\n## Passed\n\n- [ ] [lorenzolfm/Floresta#4](https://github.com/lorenzolfm/Floresta/pull/4) x
- [ ] [renovatebot/renovate#5](https://github.com/renovatebot/renovate/issues/5)\n"
        )
    }

    fn is_floresta(key: &str) -> bool {
        let key = key.to_lowercase();
        key.starts_with("getfloresta/floresta#") || key.starts_with("lorenzolfm/floresta#")
    }

    #[test]
    fn the_multi_repo_board_migrates_once() {
        let (_dir, path) = fresh();
        fs::write(&path, old_board()).unwrap();
        let mut board = Board::open(&path).unwrap();
        let dropped = board.migrate(is_floresta);
        board.save().unwrap();

        let mut board = Board::open(&path).unwrap();
        let mut order = LANE_ORDER.to_vec();
        order.push("Someday");
        assert_eq!(
            lane_order_of(&board),
            order,
            "new lanes first, custom lane after"
        );
        assert_eq!(
            board.cards("Doing"),
            [format!(
                "- [ ] [getfloresta/Floresta#1298]({F}/issues/1298) mine [[Floresta-1298]]"
            )],
            "the Floresta claim is kept verbatim"
        );
        assert_eq!(
            board.tombstones(),
            HashSet::from(["lorenzolfm/Floresta#4".to_owned()]),
            "the fork tombstone is kept, other repos dropped"
        );
        assert_eq!(dropped, 5);

        let later = "- [ ] [bitcoindevkit/bdk-ffi#1](https://github.com/bitcoindevkit/bdk-ffi/issues/1) added later";
        set(&mut board, "Todo", &[later.to_owned()]);
        assert_eq!(
            board.migrate(is_floresta),
            0,
            "a migrated board is never migrated again"
        );
        assert_eq!(
            board.cards("Todo").len(),
            1,
            "so a card you add later survives"
        );
    }

    /// Skipped where the sample board does not exist, such as in the Nix sandbox.
    #[test]
    fn the_real_vault_board_round_trips_without_losing_a_card() {
        let Some(home) = std::env::home_dir() else {
            return;
        };
        let Ok(text) = fs::read_to_string(home.join("Documents/Vault/Bitcoin/Tasks.md")) else {
            return;
        };
        let (_dir, path) = fresh();
        fs::write(&path, text).unwrap();
        let original = Board::open(&path).unwrap();
        original.save().unwrap();
        let round_tripped = Board::open(&path).unwrap();
        assert_eq!(
            snapshot_of_lanes(&original),
            snapshot_of_lanes(&round_tripped)
        );
    }
}
