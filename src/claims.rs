//! Status of things the user has claimed, so their cards can be annotated.
//!
//! The agent never moves a card out of Todo/Doing/Done. It only appends a note
//! saying what changed upstream, because the decision stays the user's.

use std::collections::HashMap;

use serde_json::Value;

use crate::gh;
use crate::text::prefix;

const STEP: usize = 5;

#[derive(Debug, Default)]
pub struct Status {
    pub notes: Vec<String>,
    pub urgent: bool,
}

fn alias_query(alias: usize, key: &str) -> Option<String> {
    let (repo, number) = key.rsplit_once('#')?;
    let (owner, name) = repo.split_once('/')?;
    Some(format!(
        r#"
  n{alias}: repository(owner: "{owner}", name: "{name}") {{
    issueOrPullRequest(number: {number}) {{
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
  }}"#
    ))
}

/// `keys` are `owner/repo#number`. An unreadable batch leaves its cards without a
/// note, and that is all it costs.
pub fn fetch(keys: &[String], me: &str) -> HashMap<String, Status> {
    let mut out = HashMap::new();
    for batch in keys.chunks(STEP) {
        let body: String = batch
            .iter()
            .enumerate()
            .filter_map(|(alias, key)| alias_query(alias, key))
            .collect();
        let Ok(data) = gh::graphql(&format!("query {{{body}\n}}")) else {
            continue;
        };
        for (alias, key) in batch.iter().enumerate() {
            let node = &data[format!("n{alias}")]["issueOrPullRequest"];
            if !node.is_null() {
                out.insert(key.clone(), interpret(node, me));
            }
        }
    }
    out
}

fn interpret(node: &Value, me: &str) -> Status {
    let mut status = Status::default();
    match node["__typename"].as_str() {
        Some("Issue") => interpret_issue(node, me, &mut status),
        Some("PullRequest") => interpret_pr(node, me, &mut status),
        _ => {}
    }
    status
}

fn date_of(value: &Value) -> &str {
    prefix(value.as_str().unwrap_or(""), 10)
}

fn interpret_issue(node: &Value, me: &str, status: &mut Status) {
    if node["state"] == "CLOSED" {
        let closed = date_of(&node["closedAt"]);
        status.notes.push(format!("closed upstream {closed}"));
        status.urgent = true;
    }
    let assignees = node["assignees"]["nodes"].as_array();
    let other = assignees
        .into_iter()
        .flatten()
        .filter_map(|assignee| assignee["login"].as_str())
        .find(|login| *login != me);
    if let Some(other) = other {
        status.notes.push(format!("assigned to {other}"));
        status.urgent = true;
    }
    for event in node["timelineItems"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let source = &event["source"];
        if source["__typename"] != "PullRequest" {
            continue;
        }
        let author = source["author"]["login"].as_str();
        if let Some(author) = author.filter(|author| !author.is_empty() && *author != me)
            && source["state"] == "OPEN"
        {
            let number = &source["number"];
            status
                .notes
                .push(format!("PR #{number} opened by {author} — stop"));
            status.urgent = true;
            break;
        }
    }
}

fn interpret_pr(node: &Value, me: &str, status: &mut Status) {
    if node["merged"].as_bool().unwrap_or(false) {
        let whose = if node["author"]["login"].as_str() == Some(me) {
            "your PR "
        } else {
            ""
        };
        let merged = date_of(&node["mergedAt"]);
        status
            .notes
            .push(format!("{whose}merged {merged} — drag to Done"));
    } else if node["state"] == "CLOSED" {
        status.notes.push("closed without merge".to_owned());
        status.urgent = true;
    }
    // Review feedback on an open PR is an obligation now, with rules of its own;
    // repeating it here would say the same thing twice.
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn what_a_claim_is_told_about_an_issue() {
        for (name, node, notes, urgent) in [
            (
                "closed upstream",
                json!({"__typename": "Issue", "state": "CLOSED", "closedAt": "2026-10-01T10:00:00Z"}),
                vec!["closed upstream 2026-10-01"],
                true,
            ),
            (
                "someone else took it",
                json!({"__typename": "Issue", "state": "OPEN",
                       "assignees": {"nodes": [{"login": "me"}, {"login": "bob"}]}}),
                vec!["assigned to bob"],
                true,
            ),
            (
                "someone else opened a fix",
                json!({"__typename": "Issue", "state": "OPEN", "timelineItems": {"nodes": [
                    {"source": {"__typename": "Issue"}},
                    {"source": {"__typename": "PullRequest", "number": 9, "state": "OPEN",
                                "author": {"login": "bob"}}},
                ]}}),
                vec!["PR #9 opened by bob — stop"],
                true,
            ),
            (
                "your own open PR is not news",
                json!({"__typename": "Issue", "state": "OPEN", "timelineItems": {"nodes": [
                    {"source": {"__typename": "PullRequest", "number": 9, "state": "OPEN",
                                "author": {"login": "me"}}},
                ]}}),
                vec![],
                false,
            ),
        ] {
            let status = interpret(&node, "me");
            assert_eq!(status.notes, notes, "{name}");
            assert_eq!(status.urgent, urgent, "{name}");
        }
    }

    #[test]
    fn what_a_claim_is_told_about_a_pr() {
        for (name, node, notes, urgent) in [
            (
                "your PR merged",
                json!({"__typename": "PullRequest", "merged": true, "state": "MERGED",
                       "mergedAt": "2026-10-02T00:00:00Z", "author": {"login": "me"}}),
                vec!["your PR merged 2026-10-02 — drag to Done"],
                false,
            ),
            (
                "someone's PR merged",
                json!({"__typename": "PullRequest", "merged": true, "state": "MERGED",
                       "mergedAt": "2026-10-02T00:00:00Z", "author": {"login": "bob"}}),
                vec!["merged 2026-10-02 — drag to Done"],
                false,
            ),
            (
                "closed without merge",
                json!({"__typename": "PullRequest", "merged": false, "state": "CLOSED"}),
                vec!["closed without merge"],
                true,
            ),
            (
                "open is not news here",
                json!({"__typename": "PullRequest", "merged": false, "state": "OPEN"}),
                vec![],
                false,
            ),
        ] {
            let status = interpret(&node, "me");
            assert_eq!(status.notes, notes, "{name}");
            assert_eq!(status.urgent, urgent, "{name}");
        }
    }
}
