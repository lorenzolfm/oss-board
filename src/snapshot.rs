//! One read of everything open in the repo, normalised for the rules.
//!
//! Everything downstream -- obligations, opportunities, the judge -- is a pure
//! function of this snapshot, so the rules can be tested against fixtures with no
//! network at all.
//!
//! Two queries per item kind, deliberately split:
//!   * the bulk pass pages through every open PR and issue with the conversation
//!     attached. It never asks for `mergeable` or the CI rollup: both force GitHub
//!     to compute state on demand, and that is what 502'd the old 91-repo scan.
//!   * `merge_state` asks for those two only for the handful of PRs that need them
//!     (yours, and the opportunity shortlist).
//!
//! The newest 60 comments and reviews, 100 threads and 30 comments per thread are
//! read. A mention older than that on a busier thread is missed, which is why the
//! caps are generous.
//!
//! The wire structs follow GitHub's schema: a field the schema promises is required,
//! so a response without it fails the read. A list the rules decide from must be
//! there even where the schema lets it be null, because no list is not an empty one.
//! `labels` and `files` only decorate a card, so those two may be missing.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::board::{CardKey, GitHubItem};
use crate::config::{Project, Repo};
use crate::gh::{self, Conn, GhError, Login, Page};
use crate::timestamp::Timestamp;

/// reviewThreads x comments make PR pages heavy.
const PR_PAGE: u32 = 10;
const ISSUE_PAGE: u32 = 25;
const MERGE_CHUNK: usize = 10;
const MINE_CHUNK: usize = 20;

const PR_FIELDS: &str = r#"
  number title url body isDraft createdAt updatedAt
  additions deletions changedFiles headRefName headRefOid
  author { __typename login }
  milestone { title dueOn }
  labels(first: 15) { nodes { name } }
  assignees(first: 5) { nodes { login } }
  reviewDecision
  reviewRequests(first: 15) { nodes { requestedReviewer { ... on User { login } } } }
  commits(last: 1) { nodes { commit { oid committedDate } } }
  reviews(last: 60) { totalCount nodes {
    author { __typename login } state body submittedAt createdAt commit { oid } } }
  comments(last: 60) { totalCount nodes { author { __typename login } body createdAt } }
  reviewThreads(first: 100) { totalCount nodes {
    isResolved isOutdated path
    comments(first: 30) { totalCount nodes { author { __typename login } body createdAt } } } }
  timelineItems(last: 20, itemTypes: [REVIEW_REQUESTED_EVENT]) { nodes {
    ... on ReviewRequestedEvent { createdAt requestedReviewer { ... on User { login } } } } }
  forcePushes: timelineItems(last: 1, itemTypes: [HEAD_REF_FORCE_PUSHED_EVENT]) { nodes {
    ... on HeadRefForcePushedEvent { createdAt } } }
  files(first: 100) { nodes { path } }
"#;

const ISSUE_FIELDS: &str = r#"
  number title url body createdAt updatedAt
  author { __typename login }
  milestone { title dueOn }
  labels(first: 15) { nodes { name } }
  assignees(first: 5) { nodes { login } }
  comments(last: 60) { totalCount nodes { author { __typename login } body createdAt } }
  closedByPullRequestsReferences(first: 5, includeClosedPrs: false) {
    nodes { number state author { __typename login } } }
  timelineItems(last: 10, itemTypes: [ASSIGNED_EVENT]) { nodes {
    ... on AssignedEvent { createdAt assignee { ... on User { login } } } } }
"#;

// ---------------------------------------------------------------- the model

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewState {
    Pending,
    Commented,
    Approved,
    ChangesRequested,
    Dismissed,
    #[serde(other)]
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentKind {
    Comment,
    Thread,
    /// A review's body: part of the conversation, exactly like a comment.
    Review(ReviewState),
}

#[derive(Clone, Debug)]
pub struct Comment {
    pub author: String,
    pub bot: bool,
    pub at: Timestamp,
    pub body: String,
    pub kind: CommentKind,
}

#[derive(Clone, Debug)]
pub struct Review {
    pub author: String,
    pub bot: bool,
    pub state: ReviewState,
    pub body: String,
    pub at: Timestamp,
}

#[derive(Clone, Debug)]
pub struct Thread {
    pub resolved: bool,
    pub path: String,
    pub comments: Vec<Comment>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Milestone {
    pub title: String,
    pub due_on: Option<Timestamp>,
}

#[derive(Clone, Debug)]
pub struct PrDetail {
    pub draft: bool,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    pub head: String,
    /// The head was pushed no earlier than this. GitHub keeps no push time, so a
    /// force-push is dated by its event and a fast-forward only by the commit it adds,
    /// which can be older than the push.
    pub pushed_at: Option<Timestamp>,
    pub review_decision: Option<ReviewDecision>,
    pub requested: Vec<String>,
    pub requested_at: HashMap<String, Timestamp>,
    pub reviews: Vec<Review>,
    pub threads: Vec<Thread>,
    pub files: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct IssueDetail {
    pub assigned_at: HashMap<String, Timestamp>,
    /// An open PR says it closes this.
    pub has_linked_pr: bool,
}

#[derive(Clone, Debug)]
pub enum Detail {
    Pr(PrDetail),
    Issue(IssueDetail),
}

#[derive(Clone, Debug)]
pub struct Item {
    pub key: CardKey,
    /// Only the queries in this file spell it. Numbers repeat between repos, so everywhere
    /// else an item is its key.
    number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub author_bot: bool,
    pub created: Timestamp,
    pub updated: Timestamp,
    pub body: String,
    pub milestone: Option<Milestone>,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    /// Oldest first, with the bodies of submitted reviews folded in.
    pub comments: Vec<Comment>,
    pub detail: Detail,
}

impl Item {
    pub fn pr(&self) -> Option<&PrDetail> {
        match &self.detail {
            Detail::Pr(pr) => Some(pr),
            Detail::Issue(_) => None,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self.detail {
            Detail::Pr(_) => "pr",
            Detail::Issue(_) => "issue",
        }
    }

    pub fn head(&self) -> Option<&str> {
        self.pr().map(|pr| pr.head.as_str())
    }

    /// Whether a number read from text about this item's repo names it.
    pub fn is_number(&self, number: u64) -> bool {
        self.number == number
    }
}

#[cfg(test)]
impl Item {
    pub fn number(&self) -> u64 {
        self.number
    }

    pub fn fixture(
        number: u64,
        title: String,
        url: String,
        created: Timestamp,
        updated: Timestamp,
        detail: Detail,
    ) -> Self {
        Self {
            key: item_key(&url).unwrap(),
            number,
            title,
            url,
            author: "alice".to_owned(),
            author_bot: false,
            created,
            updated,
            body: String::new(),
            milestone: None,
            labels: vec![],
            assignees: vec![],
            comments: vec![],
            detail,
        }
    }
}

/// Every open item in one repo, PRs first. One list, so nothing can be filed under the wrong
/// kind, and it says whose, so nothing can ask one repo about another's.
pub struct Snapshot {
    pub project: &'static Project,
    pub items: Vec<Item>,
}

impl Snapshot {
    pub fn prs(&self) -> impl Iterator<Item = (&Item, &PrDetail)> {
        self.items.iter().filter_map(|item| match &item.detail {
            Detail::Pr(pr) => Some((item, pr)),
            Detail::Issue(_) => None,
        })
    }

    pub fn issues(&self) -> impl Iterator<Item = (&Item, &IssueDetail)> {
        self.items.iter().filter_map(|item| match &item.detail {
            Detail::Pr(_) => None,
            Detail::Issue(issue) => Some((item, issue)),
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Mergeability {
    Mergeable,
    Conflicting,
    #[serde(other)]
    Unknown,
}

impl Mergeability {
    /// As GitHub spells it, which is what the judge is shown.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Mergeable => "MERGEABLE",
            Self::Conflicting => "CONFLICTING",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Ci {
    Success,
    Failure,
    Error,
    Pending,
    Expected,
}

impl Ci {
    /// As GitHub spells it, which is what the judge is shown.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Success => "SUCCESS",
            Self::Failure => "FAILURE",
            Self::Error => "ERROR",
            Self::Pending => "PENDING",
            Self::Expected => "EXPECTED",
        }
    }
}

/// A PR's merge state, read. Not having read it is a missing `MergeState`, never one
/// of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergeState {
    pub mergeable: Mergeability,
    /// `None`: no checks are configured.
    pub ci: Option<Ci>,
}

impl MergeState {
    pub fn is_conflicting(self) -> bool {
        self.mergeable == Mergeability::Conflicting
    }

    pub fn is_red(self) -> bool {
        matches!(self.ci, Some(Ci::Failure | Ci::Error))
    }

    #[cfg(test)]
    pub fn conflicting() -> Self {
        Self {
            mergeable: Mergeability::Conflicting,
            ci: None,
        }
    }

    #[cfg(test)]
    pub fn red() -> Self {
        Self {
            mergeable: Mergeability::Mergeable,
            ci: Some(Ci::Failure),
        }
    }
}

/// What `merge_state` could and could not find out.
#[derive(Debug, Default)]
pub struct MergeRead {
    pub states: HashMap<CardKey, MergeState>,
    pub unread: Vec<CardKey>,
}

impl MergeRead {
    /// PRs GitHub has not finished testing for conflicts. It answers `UNKNOWN` while it
    /// recomputes, as it does after the base moves, so these say nothing about a conflict
    /// either way. Their CI was read.
    pub fn computing(&self) -> Vec<&CardKey> {
        let mut keys: Vec<&CardKey> = self
            .states
            .iter()
            .filter(|(_, state)| state.mergeable == Mergeability::Unknown)
            .map(|(key, _)| key)
            .collect();
        keys.sort_unstable();
        keys
    }
}

/// One of your upstream PRs, in any state: what unshipped work is checked against.
pub struct UpstreamPr {
    pub title: String,
    pub head_ref: String,
    pub commits: HashSet<String>,
}

/// What changed between the commit you ACKed and the head, wherever in the base
/// each sits.
#[derive(Clone, Debug)]
pub struct Interdiff {
    pub changed: Vec<String>,
}

impl Interdiff {
    pub fn is_rebase_only(&self) -> bool {
        self.changed.is_empty()
    }
}

/// A review's body is part of the conversation exactly like a comment; an empty
/// APPROVED click is not.
pub fn fold_reviews(comments: &mut Vec<Comment>, reviews: &[Review]) {
    for review in reviews {
        if !review.body.trim().is_empty() && review.state != ReviewState::Pending {
            comments.push(Comment {
                author: review.author.clone(),
                bot: review.bot,
                at: review.at,
                body: review.body.clone(),
                kind: CommentKind::Review(review.state),
            });
        }
    }
    comments.sort_by_key(|comment| comment.at);
}

/// An issue or PR, from the URL GitHub gave for it.
pub fn github_item(url: &str) -> Result<GitHubItem, GhError> {
    GitHubItem::of_url(url).ok_or_else(|| GhError(format!("not an issue or PR url: {url}")))
}

pub fn item_key(url: &str) -> Result<CardKey, GhError> {
    github_item(url).map(CardKey::GitHub)
}

// ---------------------------------------------------------------- the wire

/// `repository { <alias>: ... }`: a connection, or a batch of aliased lookups.
#[derive(Deserialize)]
struct Repository<T> {
    repository: HashMap<String, T>,
}

fn nullable<T>(conn: Option<Conn<T>>) -> impl Iterator<Item = T> {
    conn.into_iter().flat_map(Conn::into_nodes)
}

#[derive(Deserialize)]
struct Actor {
    #[serde(rename = "__typename")]
    typename: String,
    login: String,
}

/// A deleted account has no author at all.
fn actor(author: Option<Actor>) -> (String, bool) {
    match author {
        None => ("ghost".to_owned(), false),
        Some(author) => {
            let bot = author.typename == "Bot" || author.login.ends_with("[bot]");
            (author.login, bot)
        }
    }
}

fn logins(conn: Conn<Login>) -> Vec<String> {
    conn.into_nodes().filter_map(|a| a.login).collect()
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

fn label_names(conn: Option<Conn<Label>>) -> Vec<String> {
    nullable(conn).map(|l| l.name).collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentNode {
    author: Option<Actor>,
    body: String,
    created_at: Timestamp,
}

impl CommentNode {
    fn into_comment(self, kind: CommentKind) -> Comment {
        let (author, bot) = actor(self.author);
        Comment {
            author,
            bot,
            at: self.created_at,
            body: self.body,
            kind,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewNode {
    author: Option<Actor>,
    state: ReviewState,
    body: String,
    submitted_at: Option<Timestamp>,
    created_at: Timestamp,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadNode {
    is_resolved: bool,
    path: String,
    comments: Conn<CommentNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeadCommit {
    committed_date: Timestamp,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: HeadCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewRequestNode {
    requested_reviewer: Option<Login>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewRequestedEvent {
    created_at: Timestamp,
    requested_reviewer: Option<Login>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForcePush {
    created_at: Timestamp,
}

#[derive(Deserialize)]
struct FileNode {
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrNode {
    number: u64,
    title: String,
    url: String,
    body: String,
    is_draft: bool,
    created_at: Timestamp,
    updated_at: Timestamp,
    additions: u64,
    deletions: u64,
    changed_files: u64,
    head_ref_oid: String,
    author: Option<Actor>,
    milestone: Option<Milestone>,
    labels: Option<Conn<Label>>,
    assignees: Conn<Login>,
    review_decision: Option<ReviewDecision>,
    review_requests: Conn<ReviewRequestNode>,
    commits: Conn<CommitNode>,
    reviews: Conn<ReviewNode>,
    comments: Conn<CommentNode>,
    review_threads: Conn<ThreadNode>,
    timeline_items: Conn<ReviewRequestedEvent>,
    force_pushes: Conn<ForcePush>,
    files: Option<Conn<FileNode>>,
}

impl TryFrom<PrNode> for Item {
    type Error = GhError;

    fn try_from(n: PrNode) -> Result<Self, GhError> {
        let (author, author_bot) = actor(n.author);
        let reviews: Vec<Review> = n
            .reviews
            .into_nodes()
            .map(|r| {
                let (author, bot) = actor(r.author);
                Review {
                    author,
                    bot,
                    state: r.state,
                    body: r.body,
                    at: r.submitted_at.unwrap_or(r.created_at),
                }
            })
            .collect();
        let mut comments: Vec<Comment> = n
            .comments
            .into_nodes()
            .map(|c| c.into_comment(CommentKind::Comment))
            .collect();
        fold_reviews(&mut comments, &reviews);
        let threads = n
            .review_threads
            .into_nodes()
            .map(|t| Thread {
                resolved: t.is_resolved,
                path: t.path,
                comments: t
                    .comments
                    .into_nodes()
                    .map(|c| c.into_comment(CommentKind::Thread))
                    .collect(),
            })
            .collect();
        let requested: BTreeSet<String> = n
            .review_requests
            .into_nodes()
            .filter_map(|r| r.requested_reviewer?.login)
            .collect();
        let mut requested_at: HashMap<String, Timestamp> = HashMap::new();
        for event in n.timeline_items.into_nodes() {
            if let Some(who) = event.requested_reviewer.and_then(|r| r.login) {
                let held = requested_at.entry(who).or_insert(event.created_at);
                *held = (*held).max(event.created_at);
            }
        }
        let committed = n.commits.into_nodes().next();
        let forced = n.force_pushes.into_nodes().next();
        let pushed_at = committed
            .map(|node| node.commit.committed_date)
            .max(forced.map(|push| push.created_at));
        Ok(Self {
            key: item_key(&n.url)?,
            number: n.number,
            title: n.title,
            url: n.url,
            author,
            author_bot,
            created: n.created_at,
            updated: n.updated_at,
            body: n.body,
            milestone: n.milestone,
            labels: label_names(n.labels),
            assignees: logins(n.assignees),
            comments,
            detail: Detail::Pr(PrDetail {
                draft: n.is_draft,
                additions: n.additions,
                deletions: n.deletions,
                changed_files: n.changed_files,
                head: n.head_ref_oid,
                pushed_at,
                review_decision: n.review_decision,
                requested: requested.into_iter().collect(),
                requested_at,
                reviews,
                threads,
                files: nullable(n.files).map(|f| f.path).collect(),
            }),
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssignedEvent {
    created_at: Timestamp,
    assignee: Option<Login>,
}

/// That there is one is all the rules read.
#[derive(Deserialize)]
struct LinkedPrNode {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueNode {
    number: u64,
    title: String,
    url: String,
    body: String,
    created_at: Timestamp,
    updated_at: Timestamp,
    author: Option<Actor>,
    milestone: Option<Milestone>,
    labels: Option<Conn<Label>>,
    assignees: Conn<Login>,
    comments: Conn<CommentNode>,
    closed_by_pull_requests_references: Conn<LinkedPrNode>,
    timeline_items: Conn<AssignedEvent>,
}

impl TryFrom<IssueNode> for Item {
    type Error = GhError;

    fn try_from(n: IssueNode) -> Result<Self, GhError> {
        let (author, author_bot) = actor(n.author);
        let mut assigned_at: HashMap<String, Timestamp> = HashMap::new();
        for event in n.timeline_items.into_nodes() {
            if let Some(who) = event.assignee.and_then(|a| a.login) {
                let held = assigned_at.entry(who).or_insert(event.created_at);
                *held = (*held).max(event.created_at);
            }
        }
        let mut comments: Vec<Comment> = n
            .comments
            .into_nodes()
            .map(|c| c.into_comment(CommentKind::Comment))
            .collect();
        comments.sort_by_key(|comment| comment.at);
        Ok(Self {
            key: item_key(&n.url)?,
            number: n.number,
            title: n.title,
            url: n.url,
            author,
            author_bot,
            created: n.created_at,
            updated: n.updated_at,
            body: n.body,
            milestone: n.milestone,
            labels: label_names(n.labels),
            assignees: logins(n.assignees),
            comments,
            detail: Detail::Issue(IssueDetail {
                assigned_at,
                has_linked_pr: n
                    .closed_by_pull_requests_references
                    .into_nodes()
                    .next()
                    .is_some(),
            }),
        })
    }
}

/// Every open node of a connection, read to its last page.
pub fn page_through<T: DeserializeOwned>(
    repo: Repo,
    conn: &str,
    fields: &str,
    size: u32,
) -> Result<Vec<T>, GhError> {
    let Repo { owner, name } = repo;
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let after = cursor
            .as_ref()
            .map_or(String::new(), |c| format!(", after: \"{c}\""));
        let query = format!(
            r#"query {{ repository(owner: "{owner}", name: "{name}") {{
          {conn}(states: OPEN, first: {size}{after}, orderBy: {{field: CREATED_AT, direction: DESC}}) {{
            pageInfo {{ hasNextPage endCursor }}
            nodes {{ {fields} }}
          }} }} }}"#
        );
        let mut data: Repository<Page<T>> = gh::query(&query)?;
        let page = data
            .repository
            .remove(conn)
            .ok_or_else(|| GhError(format!("no {conn} in the response")))?;
        match page.drain_into(&mut out)? {
            Some(next) => cursor = Some(next),
            None => return Ok(out),
        }
    }
}

/// Every open item in `project`'s repo.
pub fn fetch(project: &'static Project) -> Result<Snapshot, GhError> {
    let repo = project.repo;
    let prs: Vec<PrNode> = page_through(repo, "pullRequests", PR_FIELDS, PR_PAGE)?;
    let issues: Vec<IssueNode> = page_through(repo, "issues", ISSUE_FIELDS, ISSUE_PAGE)?;
    let prs = prs.into_iter().map(Item::try_from);
    let issues = issues.into_iter().map(Item::try_from);
    Ok(Snapshot {
        project,
        items: prs.chain(issues).collect::<Result<_, _>>()?,
    })
}

#[derive(Deserialize)]
struct Rollup {
    state: Ci,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RollupCommit {
    status_check_rollup: Option<Rollup>,
}

#[derive(Deserialize)]
struct RollupNode {
    commit: RollupCommit,
}

#[derive(Deserialize)]
struct MergeNode {
    mergeable: Mergeability,
    commits: Conn<RollupNode>,
}

impl From<MergeNode> for MergeState {
    fn from(node: MergeNode) -> Self {
        let rollup = node
            .commits
            .into_nodes()
            .next()
            .and_then(|commit| commit.commit.status_check_rollup);
        Self {
            mergeable: node.mergeable,
            ci: rollup.map(|rollup| rollup.state),
        }
    }
}

impl Snapshot {
    /// `mergeable` and the CI rollup for just the PRs `wanted` picks. What could not be read
    /// is listed as such, because it cannot be told from a PR with nothing wrong with it.
    pub fn merge_state(&self, wanted: impl Fn(&Item) -> bool) -> MergeRead {
        let Repo { owner, name } = self.project.repo;
        let mut read = MergeRead::default();
        let mut prs: Vec<&Item> = self
            .prs()
            .map(|(item, _)| item)
            .filter(|item| wanted(item))
            .collect();
        prs.sort_unstable_by_key(|item| item.number);
        for chunk in prs.chunks(MERGE_CHUNK) {
            let body: String = chunk
                .iter()
                .map(|item| {
                    let n = item.number;
                    format!(
                        r#"
          p{n}: pullRequest(number: {n}) {{ mergeable
            commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state }} }} }} }} }}"#
                    )
                })
                .collect();
            let query = format!(
                "query {{ repository(owner: \"{owner}\", name: \"{name}\") {{{body}\n}} }}"
            );
            let mut answered = match gh::query::<Repository<Option<MergeNode>>>(&query) {
                Ok(data) => data.repository,
                Err(err) => {
                    eprintln!("[snapshot] merge state unread: {err}");
                    HashMap::new()
                }
            };
            for item in chunk {
                match answered.remove(&format!("p{}", item.number)).flatten() {
                    Some(node) => {
                        read.states.insert(item.key.clone(), node.into());
                    }
                    None => read.unread.push(item.key.clone()),
                }
            }
        }
        read
    }
}

#[derive(Deserialize)]
struct NumberOnly {
    number: u64,
}

#[derive(Deserialize)]
struct OidCommit {
    oid: String,
}

#[derive(Deserialize)]
struct OidNode {
    commit: OidCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpstreamNode {
    title: String,
    head_ref_name: String,
    commits: Conn<OidNode>,
}

/// Every PR you ever opened upstream, any state. REST's issue list takes a creator
/// filter and returns PRs too, so this is one request instead of paging 1000+ PRs.
pub fn mine(repo: Repo, me: &str) -> Result<Vec<UpstreamPr>, GhError> {
    let Repo { owner, name } = repo;
    let raw = gh::rest(
        &format!("repos/{repo}/issues?creator={me}&state=all&per_page=100"),
        true,
        Some(".[] | select(.pull_request) | {number, title, state}"),
    )?;
    let numbers: Vec<u64> = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<NumberOnly>(line)
                .map(|n| n.number)
                .map_err(|err| GhError(format!("unexpected issue list line: {err}")))
        })
        .collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for chunk in numbers.chunks(MINE_CHUNK) {
        let body: String = chunk
            .iter()
            .map(|n| {
                format!(
                    r#"
          p{n}: pullRequest(number: {n}) {{
            number title state headRefName headRefOid
            commits(first: 100) {{ nodes {{ commit {{ oid }} }} }} }}"#
                )
            })
            .collect();
        let data: Repository<Option<UpstreamNode>> = gh::query(&format!(
            "query {{ repository(owner: \"{owner}\", name: \"{name}\") {{{body}\n}} }}"
        ))?;
        for node in data.repository.into_values().flatten() {
            out.push(UpstreamPr {
                title: node.title,
                head_ref: node.head_ref_name,
                commits: node.commits.into_nodes().map(|c| c.commit.oid).collect(),
            });
        }
    }
    Ok(out)
}

#[derive(Deserialize)]
struct DiffFile {
    filename: String,
    patch: String,
}

/// `file -> patch without hunk headers` for the PR as it stood at `sha`. Three-dot
/// compare diffs from the merge base, which is the PR's own diff at that commit no
/// matter how far the base has moved since.
fn pr_diff(project: &Project, sha: &str) -> Result<BTreeMap<String, String>, GhError> {
    let Project { repo, base, .. } = project;
    let raw = gh::rest(
        &format!("repos/{repo}/compare/{base}...{sha}"),
        false,
        Some(r#"[.files[] | {filename, patch: (.patch // "")}]"#),
    )?;
    let files: Vec<DiffFile> = serde_json::from_str(&raw)
        .map_err(|err| GhError(format!("unexpected compare response: {err}")))?;
    Ok(files
        .into_iter()
        .map(|file| {
            let patch = file
                .patch
                .lines()
                .filter(|line| !line.starts_with("@@"))
                .collect::<Vec<_>>()
                .join("\n");
            (file.filename, patch)
        })
        .collect())
}

/// `None` if either side cannot be compared (an ACKed commit GitHub has since
/// dropped).
pub fn ack_interdiff(project: &Project, acked: &str, head: &str) -> Option<Interdiff> {
    let before = pr_diff(project, acked).ok()?;
    let after = pr_diff(project, head).ok()?;
    let files: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    let changed: Vec<String> = files
        .into_iter()
        .filter(|file| before.get(*file) != after.get(*file))
        .cloned()
        .collect();
    Some(Interdiff { changed })
}
