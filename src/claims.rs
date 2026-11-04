//! Status of things the user has claimed, so their cards can be annotated.
//!
//! The agent never moves a card out of Todo/Doing/Done. It only appends a note
//! saying what changed upstream, because the decision stays the user's.

use std::collections::HashMap;

use serde::Deserialize;

use crate::board::GitHubItem;
use crate::gh::{self, Conn, ItemState, Login};
use crate::timestamp::Timestamp;

const STEP: usize = 5;

#[derive(Debug, Default)]
pub struct Status {
    pub notes: Vec<String>,
    pub urgent: bool,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum Claimed {
    Issue(ClaimedIssue),
    PullRequest(ClaimedPr),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimedIssue {
    state: ItemState,
    closed_at: Option<Timestamp>,
    assignees: Conn<Login>,
    timeline_items: Conn<CrossReference>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimedPr {
    state: ItemState,
    merged_at: Option<Timestamp>,
    author: Option<Login>,
}

#[derive(Deserialize)]
struct CrossReference {
    source: ReferenceSource,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum ReferenceSource {
    PullRequest(ReferencingPr),
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ReferencingPr {
    number: u64,
    state: ItemState,
    author: Option<Login>,
}

#[derive(Deserialize)]
struct Alias {
    #[serde(rename = "issueOrPullRequest")]
    node: Option<Claimed>,
}

fn alias_query(alias: usize, item: &GitHubItem) -> String {
    format!(
        r#"
  n{alias}: repository(owner: "{}", name: "{}") {{
    issueOrPullRequest(number: {}) {{
      __typename
      ... on Issue {{
        state closedAt
        assignees(first: 5) {{ nodes {{ login }} }}
        timelineItems(last: 20, itemTypes: [CROSS_REFERENCED_EVENT]) {{
          nodes {{ ... on CrossReferencedEvent {{ source {{
            __typename
            ... on PullRequest {{ number state author {{ login }} }}
          }} }} }}
        }}
      }}
      ... on PullRequest {{
        state mergedAt author {{ login }}
      }}
    }}
  }}"#,
        item.owner(),
        item.name(),
        item.number(),
    )
}

/// The status of each item that could be read. An item missing from the result was
/// not read, which says nothing about it, so its card must keep what it says.
pub fn fetch(items: &[GitHubItem], me: &str) -> HashMap<GitHubItem, Status> {
    let mut out = HashMap::new();
    for batch in items.chunks(STEP) {
        let body: String = batch
            .iter()
            .enumerate()
            .map(|(alias, item)| alias_query(alias, item))
            .collect();
        let mut data: HashMap<String, Option<Alias>> =
            match gh::query(&format!("query {{{body}\n}}")) {
                Ok(data) => data,
                Err(err) => {
                    eprintln!("[claims] unread: {err}");
                    continue;
                }
            };
        for (alias, item) in batch.iter().enumerate() {
            let node = data.remove(&format!("n{alias}")).flatten();
            if let Some(node) = node.and_then(|alias| alias.node) {
                out.insert(item.clone(), interpret(node, me));
            }
        }
    }
    out
}

fn interpret(node: Claimed, me: &str) -> Status {
    let mut status = Status::default();
    match node {
        Claimed::Issue(issue) => interpret_issue(issue, me, &mut status),
        Claimed::PullRequest(pr) => interpret_pr(pr, me, &mut status),
    }
    status
}

fn dated(what: &str, at: Option<Timestamp>) -> String {
    match at {
        Some(at) => format!("{what} {}", at.date()),
        None => what.to_owned(),
    }
}

fn interpret_issue(issue: ClaimedIssue, me: &str, status: &mut Status) {
    if issue.state == ItemState::Closed {
        status.notes.push(dated("closed upstream", issue.closed_at));
        status.urgent = true;
    }
    let other = issue
        .assignees
        .into_nodes()
        .filter_map(|assignee| assignee.login)
        .find(|login| login != me);
    if let Some(other) = other {
        status.notes.push(format!("assigned to {other}"));
        status.urgent = true;
    }
    for event in issue.timeline_items.into_nodes() {
        let ReferenceSource::PullRequest(source) = event.source else {
            continue;
        };
        let author = source.author.and_then(|author| author.login);
        if let Some(author) = author.filter(|author| author != me)
            && source.state == ItemState::Open
        {
            let number = source.number;
            status
                .notes
                .push(format!("PR #{number} opened by {author} — stop"));
            status.urgent = true;
            break;
        }
    }
}

fn interpret_pr(pr: ClaimedPr, me: &str, status: &mut Status) {
    match pr.state {
        ItemState::Merged => {
            let author = pr.author.and_then(|author| author.login);
            let whose = if author.as_deref() == Some(me) {
                "your PR "
            } else {
                ""
            };
            let merged = dated("merged", pr.merged_at);
            status.notes.push(format!("{whose}{merged} — drag to Done"));
        }
        ItemState::Closed => {
            status.notes.push("closed without merge".to_owned());
            status.urgent = true;
        }
        // Review feedback on an open PR is an obligation now, with rules of its own;
        // repeating it here would say the same thing twice.
        ItemState::Open => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn issue(state: &str, closed_at: Value, assignees: Value, events: Value) -> Value {
        json!({
            "__typename": "Issue",
            "state": state,
            "closedAt": closed_at,
            "assignees": {"nodes": assignees},
            "timelineItems": {"nodes": events},
        })
    }

    fn read(node: Value) -> Status {
        interpret(serde_json::from_value(node).unwrap(), "me")
    }

    #[test]
    fn what_a_claim_is_told_about_an_issue() {
        for (name, node, notes, urgent) in [
            (
                "closed upstream",
                issue(
                    "CLOSED",
                    json!("2026-10-01T10:00:00Z"),
                    json!([]),
                    json!([]),
                ),
                vec!["closed upstream 2026-10-01"],
                true,
            ),
            (
                "someone else took it",
                issue(
                    "OPEN",
                    Value::Null,
                    json!([{"login": "me"}, {"login": "bob"}]),
                    json!([]),
                ),
                vec!["assigned to bob"],
                true,
            ),
            (
                "someone else opened a fix",
                issue(
                    "OPEN",
                    Value::Null,
                    json!([]),
                    json!([
                        {"source": {"__typename": "Issue"}},
                        {"source": {"__typename": "PullRequest", "number": 9, "state": "OPEN",
                                    "author": {"login": "bob"}}},
                    ]),
                ),
                vec!["PR #9 opened by bob — stop"],
                true,
            ),
            (
                "your own open PR is not news",
                issue(
                    "OPEN",
                    Value::Null,
                    json!([]),
                    json!([
                        {"source": {"__typename": "PullRequest", "number": 9, "state": "OPEN",
                                    "author": {"login": "me"}}},
                    ]),
                ),
                vec![],
                false,
            ),
        ] {
            let status = read(node);
            assert_eq!(status.notes, notes, "{name}");
            assert_eq!(status.urgent, urgent, "{name}");
        }
    }

    #[test]
    fn what_a_claim_is_told_about_a_pr() {
        for (name, node, notes, urgent) in [
            (
                "your PR merged",
                json!({"__typename": "PullRequest", "state": "MERGED",
                       "mergedAt": "2026-10-02T00:00:00Z", "author": {"login": "me"}}),
                vec!["your PR merged 2026-10-02 — drag to Done"],
                false,
            ),
            (
                "someone's PR merged",
                json!({"__typename": "PullRequest", "state": "MERGED",
                       "mergedAt": "2026-10-02T00:00:00Z", "author": {"login": "bob"}}),
                vec!["merged 2026-10-02 — drag to Done"],
                false,
            ),
            (
                "closed without merge",
                json!({"__typename": "PullRequest", "state": "CLOSED", "mergedAt": null,
                       "author": {"login": "bob"}}),
                vec!["closed without merge"],
                true,
            ),
            (
                "open is not news here",
                json!({"__typename": "PullRequest", "state": "OPEN", "mergedAt": null,
                       "author": null}),
                vec![],
                false,
            ),
        ] {
            let status = read(node);
            assert_eq!(status.notes, notes, "{name}");
            assert_eq!(status.urgent, urgent, "{name}");
        }
    }

    #[test]
    fn a_response_of_another_shape_is_not_read_as_good_news() {
        for node in [
            json!({"__typename": "Discussion"}),
            json!({"__typename": "Issue", "state": "OPEN"}),
            json!({"__typename": "PullRequest", "state": "DRAFTED", "mergedAt": null, "author": null}),
        ] {
            assert!(
                serde_json::from_value::<Claimed>(node.clone()).is_err(),
                "{node}"
            );
        }
    }
}
