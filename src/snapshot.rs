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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::board::key_of;
use crate::gh::{self, GhError};
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
    pub due_on: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PrDetail {
    pub draft: bool,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    pub head: Option<String>,
    pub head_at: Option<Timestamp>,
    pub review_decision: Option<String>,
    pub requested: Vec<String>,
    pub requested_at: HashMap<String, Timestamp>,
    pub reviews: Vec<Review>,
    pub threads: Vec<Thread>,
    pub files: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct IssueDetail {
    pub assigned_at: HashMap<String, Timestamp>,
    pub linked_prs: Vec<u64>,
}

#[derive(Clone, Debug)]
pub enum Detail {
    Pr(PrDetail),
    Issue(IssueDetail),
}

#[derive(Clone, Debug)]
pub struct Item {
    pub key: String,
    pub number: u64,
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
        self.pr().and_then(|pr| pr.head.as_deref())
    }
}

pub struct Snapshot {
    pub prs: Vec<Item>,
    pub issues: Vec<Item>,
}

#[derive(Clone, Debug, Default)]
pub struct MergeState {
    pub mergeable: Option<String>,
    pub ci: Option<String>,
}

/// One of your upstream PRs, in any state: what unshipped work is checked against.
pub struct UpstreamPr {
    pub title: String,
    pub head_ref: String,
    pub commits: HashSet<String>,
}

/// What changed between the commit you ACKed and the head, wherever in master
/// each sits.
#[derive(Clone, Debug, Serialize)]
pub struct Interdiff {
    pub rebase_only: bool,
    pub changed: Vec<String>,
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

pub fn item_key(url: &str) -> String {
    key_of(url).unwrap_or_else(|| url.to_owned())
}

// ---------------------------------------------------------------- the wire

#[derive(Deserialize)]
struct Conn<T> {
    nodes: Option<Vec<Option<T>>>,
}

fn nodes<T>(conn: Option<Conn<T>>) -> Vec<T> {
    conn.and_then(|conn| conn.nodes)
        .unwrap_or_default()
        .into_iter()
        .flatten()
        .collect()
}

#[derive(Deserialize)]
struct Actor {
    #[serde(rename = "__typename")]
    typename: Option<String>,
    login: Option<String>,
}

fn actor(author: Option<Actor>) -> (String, bool) {
    let (typename, login) = author.map_or((None, None), |a| (a.typename, a.login));
    let login = login.unwrap_or_else(|| "ghost".to_owned());
    let bot = typename.as_deref() == Some("Bot") || login.ends_with("[bot]");
    (login, bot)
}

#[derive(Deserialize)]
struct Login {
    login: Option<String>,
}

fn logins(conn: Option<Conn<Login>>) -> Vec<String> {
    nodes(conn).into_iter().filter_map(|a| a.login).collect()
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentNode {
    author: Option<Actor>,
    body: Option<String>,
    created_at: Timestamp,
}

impl CommentNode {
    fn into_comment(self, kind: CommentKind) -> Comment {
        let (author, bot) = actor(self.author);
        Comment {
            author,
            bot,
            at: self.created_at,
            body: self.body.unwrap_or_default(),
            kind,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewNode {
    author: Option<Actor>,
    state: ReviewState,
    body: Option<String>,
    submitted_at: Option<Timestamp>,
    created_at: Timestamp,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadNode {
    is_resolved: Option<bool>,
    path: Option<String>,
    comments: Option<Conn<CommentNode>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeadCommit {
    committed_date: Option<Timestamp>,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: Option<HeadCommit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewRequestNode {
    requested_reviewer: Option<Login>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewRequestedEvent {
    created_at: Option<Timestamp>,
    requested_reviewer: Option<Login>,
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
    body: Option<String>,
    is_draft: Option<bool>,
    created_at: Timestamp,
    updated_at: Timestamp,
    additions: Option<u64>,
    deletions: Option<u64>,
    changed_files: Option<u64>,
    head_ref_oid: Option<String>,
    author: Option<Actor>,
    milestone: Option<Milestone>,
    labels: Option<Conn<Label>>,
    assignees: Option<Conn<Login>>,
    review_decision: Option<String>,
    review_requests: Option<Conn<ReviewRequestNode>>,
    commits: Option<Conn<CommitNode>>,
    reviews: Option<Conn<ReviewNode>>,
    comments: Option<Conn<CommentNode>>,
    review_threads: Option<Conn<ThreadNode>>,
    timeline_items: Option<Conn<ReviewRequestedEvent>>,
    files: Option<Conn<FileNode>>,
}

impl From<PrNode> for Item {
    fn from(n: PrNode) -> Self {
        let (author, author_bot) = actor(n.author);
        let reviews: Vec<Review> = nodes(n.reviews)
            .into_iter()
            .map(|r| {
                let (author, bot) = actor(r.author);
                Review {
                    author,
                    bot,
                    state: r.state,
                    body: r.body.unwrap_or_default(),
                    at: r.submitted_at.unwrap_or(r.created_at),
                }
            })
            .collect();
        let mut comments: Vec<Comment> = nodes(n.comments)
            .into_iter()
            .map(|c| c.into_comment(CommentKind::Comment))
            .collect();
        fold_reviews(&mut comments, &reviews);
        let threads = nodes(n.review_threads)
            .into_iter()
            .map(|t| Thread {
                resolved: t.is_resolved.unwrap_or(false),
                path: t.path.unwrap_or_default(),
                comments: nodes(t.comments)
                    .into_iter()
                    .map(|c| c.into_comment(CommentKind::Thread))
                    .collect(),
            })
            .collect();
        let requested: BTreeSet<String> = nodes(n.review_requests)
            .into_iter()
            .filter_map(|r| r.requested_reviewer?.login)
            .filter(|login| !login.is_empty())
            .collect();
        let mut requested_at: HashMap<String, Timestamp> = HashMap::new();
        for event in nodes(n.timeline_items) {
            if let (Some(who), Some(at)) = (
                event.requested_reviewer.and_then(|r| r.login),
                event.created_at,
            ) {
                let held = requested_at.entry(who).or_insert(at);
                *held = (*held).max(at);
            }
        }
        let head_at = nodes(n.commits)
            .into_iter()
            .next()
            .and_then(|node| node.commit?.committed_date);
        Self {
            key: item_key(&n.url),
            number: n.number,
            title: n.title,
            url: n.url,
            author,
            author_bot,
            created: n.created_at,
            updated: n.updated_at,
            body: n.body.unwrap_or_default(),
            milestone: n.milestone,
            labels: nodes(n.labels).into_iter().map(|l| l.name).collect(),
            assignees: logins(n.assignees),
            comments,
            detail: Detail::Pr(PrDetail {
                draft: n.is_draft.unwrap_or(false),
                additions: n.additions.unwrap_or(0),
                deletions: n.deletions.unwrap_or(0),
                changed_files: n.changed_files.unwrap_or(0),
                head: n.head_ref_oid,
                head_at,
                review_decision: n.review_decision,
                requested: requested.into_iter().collect(),
                requested_at,
                reviews,
                threads,
                files: nodes(n.files).into_iter().map(|f| f.path).collect(),
            }),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssignedEvent {
    created_at: Option<Timestamp>,
    assignee: Option<Login>,
}

#[derive(Deserialize)]
struct LinkedPrNode {
    number: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueNode {
    number: u64,
    title: String,
    url: String,
    body: Option<String>,
    created_at: Timestamp,
    updated_at: Timestamp,
    author: Option<Actor>,
    milestone: Option<Milestone>,
    labels: Option<Conn<Label>>,
    assignees: Option<Conn<Login>>,
    comments: Option<Conn<CommentNode>>,
    closed_by_pull_requests_references: Option<Conn<LinkedPrNode>>,
    timeline_items: Option<Conn<AssignedEvent>>,
}

impl From<IssueNode> for Item {
    fn from(n: IssueNode) -> Self {
        let (author, author_bot) = actor(n.author);
        let mut assigned_at: HashMap<String, Timestamp> = HashMap::new();
        for event in nodes(n.timeline_items) {
            if let (Some(who), Some(at)) = (event.assignee.and_then(|a| a.login), event.created_at)
            {
                let held = assigned_at.entry(who).or_insert(at);
                *held = (*held).max(at);
            }
        }
        let mut comments: Vec<Comment> = nodes(n.comments)
            .into_iter()
            .map(|c| c.into_comment(CommentKind::Comment))
            .collect();
        comments.sort_by_key(|comment| comment.at);
        Self {
            key: item_key(&n.url),
            number: n.number,
            title: n.title,
            url: n.url,
            author,
            author_bot,
            created: n.created_at,
            updated: n.updated_at,
            body: n.body.unwrap_or_default(),
            milestone: n.milestone,
            labels: nodes(n.labels).into_iter().map(|l| l.name).collect(),
            assignees: logins(n.assignees),
            comments,
            detail: Detail::Issue(IssueDetail {
                assigned_at,
                linked_prs: nodes(n.closed_by_pull_requests_references)
                    .into_iter()
                    .map(|p| p.number)
                    .collect(),
            }),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: Option<bool>,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page<T> {
    page_info: Option<PageInfo>,
    nodes: Option<Vec<Option<T>>>,
}

fn split_repo(repo: &str) -> Result<(&str, &str), GhError> {
    repo.split_once('/')
        .ok_or_else(|| GhError(format!("not owner/name: {repo}")))
}

/// Every open node of a connection. A response without the connection fails the
/// read: an empty list here would empty every lane.
fn page_through<T: serde::de::DeserializeOwned>(
    repo: &str,
    conn: &str,
    fields: &str,
    size: u32,
) -> Result<Vec<T>, GhError> {
    let (owner, name) = split_repo(repo)?;
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
        let mut data = gh::graphql(&query)?;
        let page: Page<T> = gh::decode(data["repository"][conn].take())?;
        out.extend(page.nodes.unwrap_or_default().into_iter().flatten());
        match page.page_info {
            Some(PageInfo {
                has_next_page: Some(true),
                end_cursor,
            }) => cursor = end_cursor,
            _ => return Ok(out),
        }
    }
}

/// Every open item in `repo`.
pub fn fetch(repo: &str) -> Result<Snapshot, GhError> {
    let prs: Vec<PrNode> = page_through(repo, "pullRequests", PR_FIELDS, PR_PAGE)?;
    let issues: Vec<IssueNode> = page_through(repo, "issues", ISSUE_FIELDS, ISSUE_PAGE)?;
    Ok(Snapshot {
        prs: prs.into_iter().map(Item::from).collect(),
        issues: issues.into_iter().map(Item::from).collect(),
    })
}

/// `mergeable` and the CI rollup for just these PRs. An unreadable chunk degrades
/// those cards, never the run.
pub fn merge_state(repo: &str, numbers: &[u64]) -> HashMap<u64, MergeState> {
    let mut out = HashMap::new();
    let Ok((owner, name)) = split_repo(repo) else {
        return out;
    };
    let numbers: Vec<u64> = numbers
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for chunk in numbers.chunks(MERGE_CHUNK) {
        let body: String = chunk
            .iter()
            .map(|n| {
                format!(
                    r#"
          p{n}: pullRequest(number: {n}) {{ mergeable
            commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state }} }} }} }} }}"#
                )
            })
            .collect();
        let query =
            format!("query {{ repository(owner: \"{owner}\", name: \"{name}\") {{{body}\n}} }}");
        let Ok(data) = gh::graphql(&query) else {
            continue;
        };
        for n in chunk {
            let node = &data["repository"][format!("p{n}")];
            let rollup = &node["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["state"];
            out.insert(
                *n,
                MergeState {
                    mergeable: node["mergeable"].as_str().map(str::to_owned),
                    ci: Some(rollup.as_str().unwrap_or("NONE").to_owned()),
                },
            );
        }
    }
    out
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
    commits: Option<Conn<OidNode>>,
}

/// Every PR you ever opened upstream, any state. REST's issue list takes a creator
/// filter and returns PRs too, so this is one request instead of paging 1000+ PRs.
pub fn mine(repo: &str, me: &str) -> Result<Vec<UpstreamPr>, GhError> {
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
    let (owner, name) = split_repo(repo)?;
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
        let mut data = gh::graphql(&format!(
            "query {{ repository(owner: \"{owner}\", name: \"{name}\") {{{body}\n}} }}"
        ))?;
        for n in chunk {
            let node = data["repository"][format!("p{n}")].take();
            if node.is_null() {
                continue;
            }
            let node: UpstreamNode = gh::decode(node)?;
            out.push(UpstreamPr {
                title: node.title,
                head_ref: node.head_ref_name,
                commits: nodes(node.commits)
                    .into_iter()
                    .map(|c| c.commit.oid)
                    .collect(),
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
/// matter how far master has moved since.
fn pr_diff(repo: &str, sha: &str) -> Result<BTreeMap<String, String>, GhError> {
    let raw = gh::rest(
        &format!("repos/{repo}/compare/master...{sha}"),
        false,
        Some(r#"[.files[] | {filename, patch: (.patch // "")}]"#),
    )?;
    let raw = if raw.is_empty() { "[]" } else { raw.as_str() };
    let files: Vec<DiffFile> = serde_json::from_str(raw)
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
pub fn ack_interdiff(repo: &str, acked: &str, head: &str) -> Option<Interdiff> {
    let before = pr_diff(repo, acked).ok()?;
    let after = pr_diff(repo, head).ok()?;
    let files: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    let changed: Vec<String> = files
        .into_iter()
        .filter(|file| before.get(*file) != after.get(*file))
        .cloned()
        .collect();
    Some(Interdiff {
        rebase_only: changed.is_empty(),
        changed,
    })
}
