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
//!   * Anything unrecognised in a board is passed through untouched, and a file that
//!     is not a whole board is refused rather than overwritten.
//!
//! Card identity is the GitHub URL, not the card text, so the user can rewrite a
//! card's wording freely without the agent losing track of it. A card with no URL
//! of its own (a local branch) carries a `<!--k:...-->` key instead, which also
//! wins over any URL in the text -- an unshipped card links the issue it fixes,
//! and must not be mistaken for that issue.
//!
//! The file has other writers (Obsidian, LiveSync, you) and no schema, so three
//! things stand in for one:
//!
//!   * `Card` is the only way text reaches an agent lane, and it keeps text from
//!     outside from becoming markers or new lines.
//!   * `Board::open` refuses anything without the front matter and the settings block
//!     that Obsidian writes last, so a partial file is never saved back as the truth.
//!   * `Board::commit` applies a run's changes to the file as it is at write time, so
//!     a card you drag during a run is kept. `rename` has no condition and Obsidian
//!     takes no locks, so a write in the instant between that read and the rename can
//!     still be lost. Nothing here can close that window.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use regex::{Captures, Regex};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::config;
use crate::obligations::{Fingerprint, ReasonKind, Seen};
use crate::timestamp::Timestamp;

const LANE_ORDER: [&str; 7] = [
    "Needs me",
    "Opportunities",
    "Unshipped",
    "Todo",
    "Doing",
    "Done",
    PASSED,
];
const PASSED: &str = "Passed";

/// What a board that does not exist yet starts from.
const SEED: &str = "---\n\nkanban-plugin: board\n\n---\n\n\
                    %% kanban:settings\n```\n{\"kanban-plugin\":\"board\",\"list-collapse\":[]}\n```\n%%\n";

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a constant pattern compiles")
}

static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"https://github\.com/([\w.-]+)/([\w.-]+)/(?:issues|pull)/(\d+)"));
static GITHUB_KEY_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"^([\w.-]+)/([\w.-]+)#(\d+)$"));
static KEY_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"<!--k:(.*?)-->"));
static MUTE_RE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"<!--t:([^|>]*)\|([^|>]*)(?:\|([^|>]*))?-->"));
static LANE_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"^##\s+(.*?)\s*$"));
static SETTINGS_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"(?m)^%%\s*kanban:settings"));
static FRONT_RE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?ms)\A---\s*$.*?^kanban-plugin:\s*board\s*$.*?^---\s*$"));
/// An annotation the agent previously appended, so it can be replaced not stacked.
static ANNOT_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"\s*<!--a-->.*?<!--/a-->"));
static COLLAPSE_RE: LazyLock<Regex> = LazyLock::new(|| regex(r#""list-collapse":\[([^\]]*)\]"#));

/// An issue or PR. The owner and name are lowercase, because GitHub ignores their
/// case and a URL can spell them either way.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GitHubItem {
    owner: String,
    name: String,
    number: u64,
}

impl GitHubItem {
    /// The issue or PR at the first GitHub URL in `text`.
    pub fn of_url(text: &str) -> Option<Self> {
        let found = URL_RE.captures(text)?;
        Self::from_captures(&found)
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn number(&self) -> u64 {
        self.number
    }

    fn from_captures(found: &Captures<'_>) -> Option<Self> {
        Some(Self {
            owner: found[1].to_lowercase(),
            name: found[2].to_lowercase(),
            number: found[3].parse().ok()?,
        })
    }

    /// In one of the repos you work in, not a fork of one.
    fn is_upstream(&self) -> bool {
        config::PROJECTS.iter().any(|project| {
            self.owner.eq_ignore_ascii_case(project.repo.owner)
                && self.name.eq_ignore_ascii_case(project.repo.name)
        })
    }
}

/// What a card is about.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CardKey {
    GitHub(GitHubItem),
    Branch(String),
}

impl CardKey {
    pub fn of_url(text: &str) -> Option<Self> {
        GitHubItem::of_url(text).map(Self::GitHub)
    }

    pub fn branch(name: &str) -> Self {
        Self::Branch(name.to_owned())
    }

    /// What a card or the DM calls it: `cdk#2778`. A bare number could be any repo's, and
    /// a fork has its upstream's name but not its numbers, so its owner stays.
    pub fn short(&self) -> String {
        match self {
            Self::GitHub(item) if item.is_upstream() => format!("{}#{}", item.name, item.number),
            Self::GitHub(_) => self.to_string(),
            Self::Branch(name) => name.clone(),
        }
    }

    /// The issue or PR, for a key GitHub can be asked about.
    pub fn github(&self) -> Option<&GitHubItem> {
        match self {
            Self::GitHub(item) => Some(item),
            Self::Branch(_) => None,
        }
    }
}

impl fmt::Display for CardKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitHub(item) => write!(f, "{}/{}#{}", item.owner, item.name, item.number),
            Self::Branch(name) => write!(f, "branch:{name}"),
        }
    }
}

impl FromStr for CardKey {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        if let Some(name) = text.strip_prefix("branch:") {
            return Ok(Self::branch(name));
        }
        GITHUB_KEY_RE
            .captures(text)
            .and_then(|found| GitHubItem::from_captures(&found))
            .map(Self::GitHub)
            .ok_or_else(|| format!("not a card key: {text}"))
    }
}

/// State files key their maps by this, as the text a marker holds.
impl Serialize for CardKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CardKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// A marker that does not hold a key says nothing, so the card's URL decides.
pub fn card_key(line: &str) -> Option<CardKey> {
    KEY_RE
        .captures(line)
        .and_then(|found| found[1].parse().ok())
        .or_else(|| CardKey::of_url(line))
}

/// Text from outside (GitHub, the model), on one line and unable to open an HTML
/// comment. Markers are the only syntax a card carries, so that is all it needs.
fn inert(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("<!--", "<! --")
}

/// One line the agent writes. These constructors are the only way to make one, and
/// the agent's lanes take nothing else.
#[derive(Clone, Debug)]
pub struct Card(String);

impl Card {
    pub fn plain(body: &str) -> Self {
        Self(format!("- [ ] {}", inert(body)))
    }

    /// An obligation: dragged to Passed, this card mutes it.
    pub fn with_fingerprint(body: &str, fingerprint: &Fingerprint) -> Self {
        let head = fingerprint
            .head
            .as_ref()
            .map_or(String::new(), |head| format!("|{head}"));
        Self(format!(
            "- [ ] {} <!--t:{}|{}{head}-->",
            inert(body),
            fingerprint.last_text(),
            fingerprint.kinds_text()
        ))
    }

    pub fn with_key(body: &str, key: &CardKey) -> Self {
        Self(format!("- [ ] {} <!--k:{key}-->", inert(body)))
    }

    #[cfg(test)]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A lane of yours that holds claimed cards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimLane {
    Todo,
    Doing,
    Done,
}

impl ClaimLane {
    const ALL: [Self; 3] = [Self::Todo, Self::Doing, Self::Done];

    pub fn name(self) -> &'static str {
        match self {
            Self::Todo => "Todo",
            Self::Doing => "Doing",
            Self::Done => "Done",
        }
    }

    /// Still in progress, unlike Done.
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Done)
    }
}

/// A card in one of the user's active lanes.
#[derive(Clone, Debug)]
pub struct Claim {
    pub key: CardKey,
    pub lane: ClaimLane,
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

/// What a run decided. It holds only what the agent owns, and `Board::commit` is the
/// only way to apply it, so a run has no way to write back the lanes it read.
#[derive(Debug, Default)]
pub struct Changes {
    /// A lane left out keeps its cards.
    pub lanes: Vec<(AgentLane, Vec<Card>)>,
    /// `(key, note)`, replacing what the card says. An empty note clears it.
    pub notes: Vec<(CardKey, String)>,
    /// `(key, wikilink)`, added to the active card if it lacks it.
    pub links: Vec<(CardKey, String)>,
}

#[derive(Debug)]
struct Lane {
    name: String,
    cards: Vec<String>,
}

#[derive(Debug)]
pub struct Board {
    path: PathBuf,
    /// The file as read. `None` if it does not exist yet.
    source: Option<String>,
    /// Everything before the first lane, verbatim.
    front: String,
    lanes: Vec<Lane>,
    settings: String,
}

impl Board {
    /// A missing file is a new board. A file that is not a whole board is an error,
    /// so it is neither overwritten nor read as a board with nothing on it.
    pub fn open(path: &Path) -> Result<Self> {
        let source = match fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("reading the board at {}", path.display()));
            }
        };
        let text = source.as_deref().unwrap_or(SEED);
        let Some(found) = SETTINGS_RE.find(text) else {
            bail!(
                "{} has no kanban settings block: not a board, or a partial write. Leaving it alone",
                path.display()
            );
        };
        let (body, settings) = text.split_at(found.start());
        if !FRONT_RE.is_match(body) || !settings.trim_end().ends_with("%%") {
            bail!(
                "{} is not a whole kanban board. Leaving it alone",
                path.display()
            );
        }
        let settings = settings.to_owned();
        let mut board = Self {
            path: path.to_owned(),
            source: None,
            front: String::new(),
            lanes: Vec::new(),
            settings,
        };
        let mut front = Vec::new();
        let mut current = None;
        for line in body.lines() {
            if let Some(found) = LANE_RE.captures(line) {
                current = Some(board.ensure_lane(&found[1]));
            } else if let Some(index) = current {
                if !line.trim().is_empty() {
                    board.lanes[index].cards.push(line.to_owned());
                }
            } else {
                front.push(line);
            }
        }
        board.front = format!("{}\n", front.join("\n").trim_end());
        for name in LANE_ORDER {
            board.ensure_lane(name);
        }
        board.source = source;
        Ok(board)
    }

    /// Whether the file existed when it was opened.
    pub fn existed(&self) -> bool {
        self.source.is_some()
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

    fn cards(&self, name: &str) -> &[String] {
        self.lanes
            .iter()
            .find(|lane| lane.name == name)
            .map_or(&[], |lane| lane.cards.as_slice())
    }

    /// Keys in Passed, muted or not. Opportunities and unshipped work never come
    /// back from Passed; obligations consult `mutes` instead.
    pub fn tombstones(&self) -> HashSet<CardKey> {
        self.cards(PASSED)
            .iter()
            .filter_map(|line| card_key(line))
            .collect()
    }

    /// Obligation cards dragged to Passed. Muting the same item twice leaves two
    /// cards, and both count.
    pub fn mutes(&self) -> HashMap<CardKey, Seen> {
        let mut out: HashMap<CardKey, Seen> = HashMap::new();
        for line in self.cards(PASSED) {
            let (Some(key), Some(found)) = (card_key(line), MUTE_RE.captures(line)) else {
                continue;
            };
            let last = Timestamp::parse(&found[1]);
            let head = found.get(3).map(|head| head.as_str());
            let mute = out.entry(key).or_default();
            for kind in found[2].split(',').filter_map(ReasonKind::parse) {
                mute.remember(kind, last, head);
            }
        }
        out
    }

    /// Everything in the user's active lanes, in board order. A card in two lanes
    /// keeps its first position but reports the later lane.
    pub fn claimed(&self) -> Vec<Claim> {
        let mut out: Vec<Claim> = Vec::new();
        for lane in ClaimLane::ALL {
            for line in self.cards(lane.name()) {
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

    /// Cards in Needs me that are there for a kind of reason this run could not read,
    /// unless `computed` has a fresher card for the item. A source that was not read
    /// has not cleared anything, so they stay.
    pub fn carried(&self, unread: &[ReasonKind], computed: &HashSet<&CardKey>) -> Vec<Card> {
        let is_held = |line: &&String| {
            let (Some(key), Some(found)) = (card_key(line), MUTE_RE.captures(line)) else {
                return false;
            };
            !computed.contains(&key)
                && found[2]
                    .split(',')
                    .filter_map(ReasonKind::parse)
                    .any(|kind| unread.contains(&kind))
        };
        self.cards(AgentLane::NeedsMe.name())
            .iter()
            .filter(is_held)
            .cloned()
            .map(Card)
            .collect()
    }

    fn set_agent_lane(&mut self, lane: AgentLane, cards: Vec<Card>) {
        let index = self.ensure_lane(lane.name());
        self.lanes[index].cards = cards.into_iter().map(|card| card.0).collect();
    }

    /// Append/replace an annotation on a card in a user lane. Never moves it.
    fn annotate(&mut self, key: &CardKey, note: &str) {
        let note = inert(note);
        for lane in ClaimLane::ALL {
            let index = self.ensure_lane(lane.name());
            for line in &mut self.lanes[index].cards {
                if card_key(line).as_ref() != Some(key) {
                    continue;
                }
                let stripped = ANNOT_RE.replace_all(line, "");
                let base = stripped.trim_end();
                *line = if note.is_empty() {
                    base.to_owned()
                } else {
                    format!("{base} <!--a-->{note}<!--/a-->")
                };
                return;
            }
        }
    }

    /// Add a briefing's wikilink to the card(s) in Todo/Doing that claimed it.
    fn link_note(&mut self, key: &CardKey, link: &str) {
        for lane in ClaimLane::ALL.into_iter().filter(|lane| lane.is_active()) {
            let index = self.ensure_lane(lane.name());
            for line in &mut self.lanes[index].cards {
                if card_key(line).as_ref() == Some(key) && !line.contains(link) {
                    line.push(' ');
                    line.push_str(link);
                }
            }
        }
    }

    fn apply(&mut self, changes: Changes) {
        for (lane, cards) in changes.lanes {
            self.set_agent_lane(lane, cards);
        }
        for (key, note) in &changes.notes {
            self.annotate(key, note);
        }
        for (key, link) in &changes.links {
            self.link_note(key, link);
        }
    }

    /// Apply `changes` to the file as it is now, not as it was when this was read, so
    /// nothing you did during the run is lost. Returns whether the file had moved.
    pub fn commit(&self, changes: Changes) -> Result<bool> {
        let mut current = Self::open(&self.path)?;
        let moved = current.source != self.source;
        current.apply(changes);
        current.save()?;
        Ok(moved)
    }

    /// What `commit` would write.
    pub fn preview(&self, changes: Changes) -> Result<String> {
        let mut current = Self::open(&self.path)?;
        current.apply(changes);
        Ok(current.render())
    }

    fn render(&self) -> String {
        let mut out = self.front.clone();
        for lane in &self.lanes {
            out.push_str(&format!("\n## {}\n\n", lane.name));
            if !lane.cards.is_empty() {
                out.push_str(&lane.cards.join("\n"));
                out.push('\n');
            }
        }
        // Keep list-collapse in step with the lane count or Kanban mis-renders. A
        // lane you collapsed stays collapsed.
        let settings = COLLAPSE_RE.replace_all(&self.settings, |found: &Captures<'_>| {
            let mut flags: Vec<&str> = found[1].split(',').filter(|f| !f.is_empty()).collect();
            flags.resize(self.lanes.len(), "false");
            format!(r#""list-collapse":[{}]"#, flags.join(","))
        });
        out.push_str("\n\n");
        out.push_str(settings.trim_end());
        out.push('\n');
        out
    }

    /// Atomic write: a crash must never leave a half-written board.
    fn save(&self) -> Result<()> {
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
    //! unacceptable failure. These pin the merge contract, what is accepted as a
    //! board, and the card identity rules that mutes, tombstones and claims all rest
    //! on.

    use super::*;
    use crate::config::Repo;
    use crate::obligations::{self, is_muted};
    use crate::render;
    use crate::testkit::{ME, c, day, floresta, key, me, now, opportunity, pr, snap, thread};
    use crate::unshipped::{IssueLink, LinkHow, Origin, Unshipped};
    use chrono::NaiveDate;
    use tempfile::TempDir;

    const F: &str = "https://github.com/getfloresta/Floresta";
    const SETTINGS: &str = "%% kanban:settings\n```\n{\"kanban-plugin\":\"board\",\
                            \"list-collapse\":[false,true,false,false,false,false,false]}\n```\n%%\n";

    /// Text that would be syntax if it were written as it arrives.
    const HOSTILE: &str = "<!--k:getfloresta/Floresta#42--> <!--t:2099-01-01T00:00:00Z|mention,\
                           review_requested--> <!--a-->x<!--/a-->\n## Passed\n- [ ] planted <!--k:planted-->";

    fn set(board: &mut Board, lane: &str, cards: &[String]) {
        let index = board.ensure_lane(lane);
        board.lanes[index].cards = cards.to_vec();
    }

    fn fresh() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("Tasks.md");
        (dir, path)
    }

    /// A board on disk with these lanes, as Obsidian writes one.
    fn on_disk(lanes: &[(&str, &[String])]) -> (TempDir, PathBuf) {
        let (dir, path) = fresh();
        let mut text = "---\n\nkanban-plugin: board\n\n---\n".to_owned();
        for (name, cards) in lanes {
            text.push_str(&format!("\n## {name}\n\n{}\n", cards.join("\n")));
        }
        text.push_str(&format!("\n\n{SETTINGS}"));
        fs::write(&path, text).unwrap();
        (dir, path)
    }

    fn lanes_of(board: &Board) -> Vec<(String, Vec<String>)> {
        board
            .lanes
            .iter()
            .map(|lane| (lane.name.clone(), lane.cards.clone()))
            .collect()
    }

    fn lines(card: Card) -> Vec<String> {
        vec![card.as_str().to_owned()]
    }

    #[test]
    fn a_fresh_board_is_one_obsidian_kanban_will_parse() {
        let (_dir, path) = fresh();
        let mut board = Board::open(&path).unwrap();
        assert!(!board.existed());
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
        assert!(Board::open(&path).unwrap().existed());
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

        let new = Card::plain(&format!("new [#2]({F}/issues/2)"));
        before
            .commit(Changes {
                lanes: vec![
                    (AgentLane::NeedsMe, vec![new.clone()]),
                    (AgentLane::Opportunities, vec![]),
                ],
                ..Changes::default()
            })
            .unwrap();

        let after = Board::open(&path).unwrap();
        for lane in ["Todo", "Doing", "Done", "Passed"] {
            assert_eq!(before.cards(lane), after.cards(lane), "{lane}");
        }
        assert_eq!(after.cards("Needs me"), [new.as_str()]);
    }

    /// The window a run holds the board open for is 45s at the median and 17 minutes
    /// at worst. A card you dragged in it must survive the write.
    #[test]
    fn a_card_dragged_during_the_run_survives_the_write() {
        let dragged = format!("- [ ] just claimed [#3]({F}/issues/3)");
        let (_dir, path) = on_disk(&[("Todo", &[])]);
        let board = Board::open(&path).unwrap();

        let (_elsewhere, moved) = on_disk(&[("Todo", std::slice::from_ref(&dragged))]);
        fs::copy(&moved, &path).unwrap();

        let changed = board
            .commit(Changes {
                lanes: vec![(AgentLane::NeedsMe, vec![Card::plain("from this run")])],
                ..Changes::default()
            })
            .unwrap();
        assert!(changed, "the run says the file moved under it");
        let after = Board::open(&path).unwrap();
        assert_eq!(after.cards("Todo"), [dragged]);
        assert_eq!(after.cards("Needs me"), ["- [ ] from this run"]);

        let unchanged = after.commit(Changes::default()).unwrap();
        assert!(!unchanged, "nothing moved this time");
    }

    /// run skips the Unshipped lane when its source fails; the guard rests on an
    /// unset lane keeping its cards.
    #[test]
    fn a_lane_the_run_did_not_set_keeps_its_cards() {
        let (_dir, path) = fresh();
        let wip = Card::with_key("⚡ `wip`", &CardKey::branch("wip"));
        let board = Board::open(&path).unwrap();
        board
            .commit(Changes {
                lanes: vec![(AgentLane::Unshipped, vec![wip.clone()])],
                ..Changes::default()
            })
            .unwrap();

        let board = Board::open(&path).unwrap();
        board
            .commit(Changes {
                lanes: vec![(AgentLane::NeedsMe, vec![])],
                ..Changes::default()
            })
            .unwrap();
        assert_eq!(
            Board::open(&path).unwrap().cards("Unshipped"),
            [wip.as_str()]
        );
    }

    #[test]
    fn annotations_replace_rather_than_stack_and_never_move_a_card() {
        let key = key("getfloresta/floresta#99");
        let card = format!("- [ ] my words [#99]({F}/issues/99) ^abc");
        let (_dir, path) = on_disk(&[("Todo", &[card])]);
        for note in ["first", "second"] {
            Board::open(&path)
                .unwrap()
                .commit(Changes {
                    notes: vec![(key.clone(), note.to_owned())],
                    ..Changes::default()
                })
                .unwrap();
        }
        let line = Board::open(&path).unwrap().cards("Todo")[0].clone();
        assert_eq!(line.matches("<!--a-->").count(), 1, "{line}");
        assert!(line.contains("second"), "{line}");
        assert!(line.contains("^abc") && line.contains("my words"), "{line}");

        Board::open(&path)
            .unwrap()
            .commit(Changes {
                notes: vec![(key, String::new())],
                ..Changes::default()
            })
            .unwrap();
        assert!(!Board::open(&path).unwrap().cards("Todo")[0].contains("<!--a-->"));
    }

    /// A finished card keeps its text as it is, even if the item is also in progress.
    #[test]
    fn a_note_links_from_cards_in_progress_only() {
        let card = format!("- [ ] x [#8]({F}/pull/8)");
        let (_dir, path) = on_disk(&[
            ("Doing", std::slice::from_ref(&card)),
            ("Done", std::slice::from_ref(&card)),
        ]);
        Board::open(&path)
            .unwrap()
            .commit(Changes {
                links: vec![(key("getfloresta/floresta#8"), "[[Floresta-8]]".to_owned())],
                ..Changes::default()
            })
            .unwrap();
        let board = Board::open(&path).unwrap();
        assert_eq!(board.cards("Doing"), [format!("{card} [[Floresta-8]]")]);
        assert_eq!(board.cards("Done"), [card]);
    }

    #[test]
    fn a_key_marker_beats_any_url_in_the_text_and_a_bad_one_says_nothing() {
        let card = format!(
            "- [ ] ⚡ `dedup` probably fixes [#1308]({F}/issues/1308) <!--k:branch:dedup-->"
        );
        assert_eq!(card_key(&card), Some(CardKey::branch("dedup")));
        let reworded = format!("- [ ] TOTALLY rewritten [x]({F}/issues/42) blah");
        assert_eq!(card_key(&reworded), Some(key("getfloresta/floresta#42")));
        let bad_marker = format!("- [ ] x [y]({F}/issues/42) <!--k:not a key-->");
        assert_eq!(card_key(&bad_marker), Some(key("getfloresta/floresta#42")));
        assert_eq!(card_key("- [ ] buy milk"), None);
    }

    /// GitHub ignores the case of a repo, and a pasted URL can spell it either way.
    #[test]
    fn a_key_is_the_same_however_its_repo_is_spelled() {
        let spellings = [
            card_key("- [ ] [x](https://github.com/GetFloresta/floresta/pull/1)"),
            card_key(&format!("- [ ] [x]({F}/issues/1)")),
            "getfloresta/Floresta#1".parse().ok(),
        ];
        for spelling in spellings {
            assert_eq!(spelling, Some(key("getfloresta/floresta#1")));
        }
        assert_ne!(
            CardKey::branch("Fix"),
            CardKey::branch("fix"),
            "a branch name's case is its own"
        );
        for bad in ["", "planted", "a/b#x", "a/b", "#1"] {
            assert!(bad.parse::<CardKey>().is_err(), "{bad}");
        }
    }

    /// A DM line has no link on it, so its name is all that says which item it means.
    #[test]
    fn a_short_name_tells_a_fork_from_its_upstream() {
        for (text, want) in [
            ("getfloresta/Floresta#6", "floresta#6"),
            ("cashubtc/cdk#6", "cdk#6"),
            ("lorenzolfm/floresta#6", "lorenzolfm/floresta#6"),
            ("branch:cdk/wip", "cdk/wip"),
        ] {
            assert_eq!(key(text).short(), want, "{text}");
        }
    }

    #[test]
    fn every_rendered_card_yields_its_key_back_through_the_parser() {
        let viewer = me();
        for title in ["an ordinary title", HOSTILE] {
            let snapshot = snap(
                vec![pr(1336).title(title).requested(ME, "2026-10-08T17:14:49Z")],
                vec![],
            );
            let obs = obligations::collect(&snapshot, &viewer, now(), &HashMap::new());
            let line = render::ob_card(&obs[0], Some(title));
            assert_eq!(
                card_key(line.as_str()),
                Some(key("getfloresta/floresta#1336")),
                "{line:?}"
            );

            let item = pr(7).title(title);
            let opp = opportunity(&item);
            assert_eq!(
                card_key(render::opp_card(&opp, Some(title)).as_str()),
                Some(key("getfloresta/floresta#7"))
            );

            let fork = Unshipped {
                repo: Repo {
                    owner: "getfloresta",
                    name: "Floresta",
                },
                origin: Origin::ForkPr {
                    item: GitHubItem::of_url("https://github.com/lorenzolfm/Floresta/pull/6")
                        .unwrap(),
                    url: "https://github.com/lorenzolfm/Floresta/pull/6".to_owned(),
                    additions: 1,
                    deletions: 0,
                },
                title: title.to_owned(),
                branch: "b".to_owned(),
                commits: 1,
                last: NaiveDate::from_ymd_opt(2026, 9, 11).unwrap(),
                issue: Some(IssueLink {
                    key: floresta(1308),
                    url: format!("{F}/issues/1308"),
                    how: LinkHow::Guess,
                }),
            };
            assert_eq!(
                card_key(render::unshipped_card(&fork).as_str()),
                Some(key("lorenzolfm/floresta#6")),
                "a fork PR card linking an issue keeps the fork key"
            );
        }
    }

    /// Anyone who can open an issue writes a title, so it must not be able to pass a
    /// mute, plant a tombstone or move your cards.
    #[test]
    fn text_from_github_cannot_become_syntax_on_the_board() {
        let viewer = me();
        let hostile_thread = {
            let mut thread = thread(vec![c(ME, 5, "mine"), c("bob", 3, "answer")]);
            thread.path = HOSTILE.to_owned();
            thread
        };
        let snapshot = snap(
            vec![
                pr(70).threads(vec![hostile_thread]),
                pr(71).title(HOSTILE).requested(ME, "2026-10-08T17:14:49Z"),
            ],
            vec![],
        );
        let obs = obligations::collect(&snapshot, &viewer, now(), &HashMap::new());
        let of = |number| {
            obs.iter()
                .find(|ob| ob.item.key == floresta(number))
                .unwrap()
        };
        let line = render::ob_card(of(71), None);
        assert_eq!(line.as_str().lines().count(), 1);

        let claimed = format!("- [ ] reviewing [#70]({F}/pull/70)");
        let next = format!("- [ ] my next card [#72]({F}/pull/72)");
        let (_dir, path) = on_disk(&[("Doing", &[claimed, next]), ("Passed", &lines(line))]);
        Board::open(&path)
            .unwrap()
            .commit(Changes {
                notes: vec![(key("getfloresta/floresta#70"), render::reasons_text(of(70)))],
                ..Changes::default()
            })
            .unwrap();

        let board = Board::open(&path).unwrap();
        assert_eq!(
            board.tombstones(),
            HashSet::from([key("getfloresta/floresta#71")]),
            "the pass landed on the card's own item"
        );
        let held: Vec<CardKey> = board.claimed().into_iter().map(|c| c.key).collect();
        assert_eq!(
            held,
            [
                key("getfloresta/floresta#70"),
                key("getfloresta/floresta#72")
            ],
            "your cards stayed in their lane"
        );

        let later = snap(
            vec![
                pr(71)
                    .title(HOSTILE)
                    .requested(ME, "2026-10-08T17:14:49Z")
                    .comments(vec![c("bob", 0, &format!("@{ME} urgent"))]),
            ],
            vec![],
        );
        let obs = obligations::collect(&later, &viewer, now(), &HashMap::new());
        assert!(
            !is_muted(&obs[0], board.mutes().get(&obs[0].item.key)),
            "a date in the title did not keep a new mention hidden"
        );
    }

    #[test]
    fn in_passed_an_obligation_card_is_a_mute_and_anything_else_a_tombstone() {
        let viewer = me();
        let snapshot = snap(vec![pr(1336).requested(ME, "2026-10-08T17:14:49Z")], vec![]);
        let obs = obligations::collect(&snapshot, &viewer, now(), &HashMap::new());
        let dragged = render::ob_card(&obs[0], Some("a gist")).as_str().to_owned();

        let (_dir, path) = on_disk(&[(
            "Passed",
            &[
                dragged,
                format!("- [ ] nope [#1]({F}/issues/1)"),
                format!(
                    "- [ ] older mute [#1336]({F}/pull/1336) <!--t:2026-01-01T00:00:00Z|mention-->"
                ),
            ],
        )]);

        let board = Board::open(&path).unwrap();
        let tombstones = board.tombstones();
        assert!(tombstones.contains(&key("getfloresta/floresta#1336")));
        assert!(tombstones.contains(&key("getfloresta/floresta#1")));
        let mutes = board.mutes();
        assert_eq!(mutes.len(), 1, "only the fingerprinted card mutes");
        assert!(
            is_muted(&obs[0], mutes.get(&obs[0].item.key)),
            "the dragged card is muted as rendered"
        );
    }

    /// Each card remembers its own kinds. One with no time must not lose to one with
    /// a time, or the conflict you muted returns every run.
    #[test]
    fn muting_an_item_twice_counts_both_cards_in_either_order() {
        let viewer = me();
        let merge = HashMap::from([(floresta(61), crate::snapshot::MergeState::conflicting())]);
        let snapshot = snap(vec![pr(61).author(ME).pushed_at(day(2))], vec![]);
        let obs = obligations::collect(&snapshot, &viewer, now(), &merge);
        let dragged = render::ob_card(&obs[0], None).as_str().to_owned();
        let older = format!("- [ ] old [#61]({F}/pull/61) <!--t:2026-01-01T00:00:00Z|mention-->");
        for passed in [
            [dragged.clone(), older.clone()],
            [older.clone(), dragged.clone()],
        ] {
            let (_dir, path) = on_disk(&[("Passed", &passed)]);
            let mutes = Board::open(&path).unwrap().mutes();
            assert!(is_muted(&obs[0], mutes.get(&obs[0].item.key)), "{passed:?}");
        }
    }

    /// Two cards for one kind: the newer holds, wherever it sits in the lane.
    #[test]
    fn of_two_cards_for_a_kind_the_newer_one_counts() {
        let viewer = me();
        let snapshot = snap(vec![pr(5).requested(ME, "2026-10-08T17:14:49Z")], vec![]);
        let obs = obligations::collect(&snapshot, &viewer, now(), &HashMap::new());
        let newer = render::ob_card(&obs[0], None).as_str().to_owned();
        let older =
            format!("- [ ] x [#5]({F}/pull/5) <!--t:2026-01-01T00:00:00Z|review_requested-->");
        for passed in [[newer.clone(), older.clone()], [older, newer]] {
            let (_dir, path) = on_disk(&[("Passed", &passed)]);
            let mutes = Board::open(&path).unwrap().mutes();
            assert!(is_muted(&obs[0], mutes.get(&obs[0].item.key)), "{passed:?}");
        }
    }

    /// A card from before a state remembered its head says nothing about which head
    /// it was muted on, so it comes back once rather than hide every later push.
    #[test]
    fn an_old_card_with_no_head_does_not_hold_a_state_on_any_head() {
        let viewer = me();
        let merge = HashMap::from([(floresta(61), crate::snapshot::MergeState::red())]);
        let snapshot = snap(vec![pr(61).author(ME)], vec![]);
        let obs = obligations::collect(&snapshot, &viewer, now(), &merge);
        let passed = [format!("- [ ] c [#61]({F}/pull/61) <!--t:|ci_failed-->")];
        let (_dir, path) = on_disk(&[("Passed", &passed)]);
        let mutes = Board::open(&path).unwrap().mutes();
        assert!(!is_muted(&obs[0], mutes.get(&obs[0].item.key)));
    }

    #[test]
    fn a_mute_on_a_kind_nothing_knows_any_more_holds_nothing() {
        let viewer = me();
        let snapshot = snap(vec![pr(5).requested(ME, "2026-10-08T17:14:49Z")], vec![]);
        let obs = obligations::collect(&snapshot, &viewer, now(), &HashMap::new());
        let passed = [format!(
            "- [ ] x [#5]({F}/pull/5) <!--t:2099-01-01T00:00:00Z|a_retired_kind-->"
        )];
        let (_dir, path) = on_disk(&[("Passed", &passed)]);
        let mutes = Board::open(&path).unwrap().mutes();
        assert!(!is_muted(&obs[0], mutes.get(&obs[0].item.key)));
    }

    /// A source that was not read has not cleared anything. Only cards it was behind
    /// stay, and a card this run computed afresh wins.
    #[test]
    fn a_card_is_carried_only_for_a_kind_of_reason_that_was_not_read() {
        let red = format!("- [ ] 🔧 [#9]({F}/pull/9) x <!--t:|conflict,ci_failed|abc-->");
        let replied =
            format!("- [ ] 💬 [#10]({F}/pull/10) x <!--t:2026-01-01T00:00:00Z|mention-->");
        let fresh_red = format!("- [ ] 🔧 [#11]({F}/pull/11) x <!--t:|conflict|abc-->");
        let (_dir, path) = on_disk(&[("Needs me", &[red.clone(), replied, fresh_red])]);
        let board = Board::open(&path).unwrap();
        let computed_key = key("getfloresta/floresta#11");
        let computed = HashSet::from([&computed_key]);

        let carried = board.carried(&[ReasonKind::Conflict], &computed);
        let carried: Vec<&str> = carried.iter().map(Card::as_str).collect();
        assert_eq!(carried, [red.as_str()]);
        assert!(board.carried(&[], &computed).is_empty(), "all was read");
    }

    #[test]
    fn a_file_that_is_not_a_whole_board_is_refused_and_left_alone() {
        let whole = format!(
            "---\n\nkanban-plugin: board\n\n---\n\n## Todo\n\n- [ ] mine\n\n## Passed\n\n\n\n{SETTINGS}"
        );
        let cut_at_lane = &whole[..whole.find("## Passed").unwrap()];
        let cut_in_settings = &whole[..whole.rfind("%%").unwrap()];
        for text in [
            "",
            "# Notes\n\nA paragraph I care about.\n\n- a list\n",
            cut_at_lane,
            cut_in_settings,
            &format!("# Notes\n\n{SETTINGS}"),
        ] {
            let (_dir, path) = fresh();
            fs::write(&path, text).unwrap();
            assert!(Board::open(&path).is_err(), "{text:?}");
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
    }

    #[test]
    fn what_a_board_holds_beyond_the_lanes_passes_through() {
        let text = format!(
            "---\n\nkanban-plugin: board\ntags: [keep-me]\n\n---\n\nA note of mine.\n\n\
             ## Issues\n\n- [ ] buy milk\n\n## Someday\n\n- [ ] custom lane card\n\n\n{SETTINGS}"
        );
        let (_dir, path) = fresh();
        fs::write(&path, &text).unwrap();
        Board::open(&path)
            .unwrap()
            .commit(Changes::default())
            .unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.starts_with(
            "---\n\nkanban-plugin: board\ntags: [keep-me]\n\n---\n\nA note of mine.\n\n## Issues\n\n- [ ] buy milk\n"
        ));
        let board = Board::open(&path).unwrap();
        assert_eq!(board.cards("Someday"), ["- [ ] custom lane card"]);
        assert_eq!(board.cards("Issues"), ["- [ ] buy milk"]);
    }

    #[test]
    fn a_collapsed_lane_stays_collapsed_and_the_list_tracks_the_lane_count() {
        let (_dir, path) = on_disk(&[("Needs me", &[])]);
        Board::open(&path)
            .unwrap()
            .commit(Changes::default())
            .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(r#""list-collapse":[false,true,false,false,false,false,false]"#),
            "{text}"
        );

        let added = text.replace("## Done", "## Someday\n\n## Done");
        fs::write(&path, added).unwrap();
        Board::open(&path)
            .unwrap()
            .commit(Changes::default())
            .unwrap();
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains(r#""list-collapse":[false,true,false,false,false,false,false,false]"#),
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
        assert_eq!(lanes_of(&original), lanes_of(&round_tripped));
    }
}
