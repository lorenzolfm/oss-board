//! An org project board's "Needs Review" column.
//!
//! Needs a token with `read:project`; without it the query fails and the source is
//! simply absent from that run. `survey` exists for the one-off check that the
//! column is curated -- items moving in and out recently, not a name nobody uses. It
//! reads every project's board, including one a run does not read yet.

use std::collections::HashSet;

use serde::Deserialize;

use crate::config::{self, ProjectColumn, Repo, ReviewColumn};
use crate::gh::{self, GhError, ItemState, Page};
use crate::timestamp::Timestamp;

#[derive(Deserialize)]
struct Data {
    organization: Organization,
}

#[derive(Deserialize)]
struct Organization {
    #[serde(rename = "projectV2")]
    project: Project,
}

#[derive(Deserialize)]
struct Project {
    items: Page<ItemNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemNode {
    updated_at: Timestamp,
    field_value_by_name: Option<FieldValue>,
    content: Option<Content>,
}

/// Empty for a field that is not a single select.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FieldValue {
    name: Option<String>,
    updated_at: Option<Timestamp>,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum Content {
    PullRequest(Tracked),
    Issue(Tracked),
    /// A draft, which has no number or repo.
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Tracked {
    number: u64,
    state: ItemState,
    repository: TrackedRepo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackedRepo {
    name_with_owner: String,
}

struct ProjectItem {
    tracked: Option<Tracked>,
    status: Option<String>,
    moved: Timestamp,
}

impl From<ItemNode> for ProjectItem {
    fn from(node: ItemNode) -> Self {
        let (status, status_moved) = node
            .field_value_by_name
            .map_or((None, None), |value| (value.name, value.updated_at));
        Self {
            tracked: match node.content {
                Some(Content::PullRequest(tracked) | Content::Issue(tracked)) => Some(tracked),
                Some(Content::Other) | None => None,
            },
            status,
            moved: status_moved.unwrap_or(node.updated_at),
        }
    }
}

impl ProjectItem {
    fn is_open(&self) -> bool {
        self.tracked
            .as_ref()
            .is_some_and(|tracked| tracked.state == ItemState::Open)
    }

    /// Its number, if it is open, in `repo`, and in `column`. Both ignore case.
    fn in_column(&self, repo: &str, column: &str) -> Option<u64> {
        let tracked = self.tracked.as_ref().filter(|_| self.is_open())?;
        let status = self.status.as_deref()?;
        (tracked
            .repository
            .name_with_owner
            .eq_ignore_ascii_case(repo)
            && status.eq_ignore_ascii_case(column))
        .then_some(tracked.number)
    }
}

fn items(org: &str, number: u64) -> Result<Vec<ProjectItem>, GhError> {
    let mut nodes: Vec<ItemNode> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let after = cursor
            .as_ref()
            .map_or(String::new(), |c| format!(", after: \"{c}\""));
        let data: Data = gh::query(&format!(
            r#"query {{ organization(login: "{org}") {{
          projectV2(number: {number}) {{ items(first: 100{after}) {{
            pageInfo {{ hasNextPage endCursor }}
            nodes {{
              updatedAt
              fieldValueByName(name: "Status") {{
                ... on ProjectV2ItemFieldSingleSelectValue {{ name updatedAt }} }}
              content {{ __typename
                ... on PullRequest {{ number state repository {{ nameWithOwner }} }}
                ... on Issue {{ number state repository {{ nameWithOwner }} }} }}
            }} }} }} }} }}"#
        ))?;
        match data.organization.project.items.drain_into(&mut nodes)? {
            Some(next) => cursor = Some(next),
            None => return Ok(nodes.into_iter().map(ProjectItem::from).collect()),
        }
    }
}

/// Open item numbers of `repo` sitting in the column.
pub fn needs_review(repo: Repo, cfg: &ProjectColumn) -> Result<HashSet<u64>, GhError> {
    let repo = repo.to_string();
    Ok(items(cfg.org, cfg.number)?
        .iter()
        .filter_map(|item| item.in_column(&repo, cfg.column))
        .collect())
}

/// Print every column of each org board, biggest first.
pub fn survey() -> Result<(), GhError> {
    for project in &config::PROJECTS {
        let (cfg, read) = match &project.column {
            ReviewColumn::Curated(cfg) => (cfg, "a run reads it"),
            ReviewColumn::Unproven(cfg) => (cfg, "a run does not read it yet"),
        };
        println!(
            "{}: {}'s board #{}, {read}",
            project.repo, cfg.org, cfg.number
        );
        survey_board(cfg)?;
    }
    Ok(())
}

fn survey_board(cfg: &ProjectColumn) -> Result<(), GhError> {
    let mut columns: Vec<(Option<String>, Vec<ProjectItem>)> = Vec::new();
    for item in items(cfg.org, cfg.number)? {
        match columns
            .iter_mut()
            .find(|(status, _)| *status == item.status)
        {
            Some((_, held)) => held.push(item),
            None => columns.push((item.status.clone(), vec![item])),
        }
    }
    columns.sort_by_key(|(_, held)| std::cmp::Reverse(held.len()));
    for (status, held) in &columns {
        let open = held.iter().filter(|item| item.is_open()).count();
        let newest = held.iter().map(|item| item.moved).max();
        println!(
            "{:<20} {:>4} items  {open:>4} open  last moved {}",
            status.as_deref().unwrap_or("None"),
            held.len(),
            newest.map_or(String::new(), |at| at.date().to_string())
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn item(node: Value) -> ProjectItem {
        serde_json::from_value::<ItemNode>(node).unwrap().into()
    }

    fn pr(state: &str, repo: &str, status: Value) -> Value {
        json!({
            "updatedAt": "2026-10-01T00:00:00Z",
            "fieldValueByName": status,
            "content": {"__typename": "PullRequest", "number": 7, "state": state,
                        "repository": {"nameWithOwner": repo}},
        })
    }

    #[test]
    fn what_counts_as_sitting_in_the_column() {
        let column = json!({"name": "Needs review", "updatedAt": "2026-10-02T00:00:00Z"});
        for (name, node, want) in [
            (
                "open, here, in any case",
                pr(
                    "OPEN",
                    "GetFloresta/floresta",
                    json!({"name": "NEEDS REVIEW"}),
                ),
                Some(7),
            ),
            (
                "merged",
                pr("MERGED", "getfloresta/Floresta", column.clone()),
                None,
            ),
            ("another repo", pr("OPEN", "a/b", column.clone()), None),
            (
                "another column",
                pr("OPEN", "getfloresta/Floresta", json!({"name": "Done"})),
                None,
            ),
            (
                "a field that is not a single select",
                pr("OPEN", "getfloresta/Floresta", json!({})),
                None,
            ),
            (
                "no status at all",
                pr("OPEN", "getfloresta/Floresta", Value::Null),
                None,
            ),
            (
                "a draft has no number",
                json!({"updatedAt": "2026-10-01T00:00:00Z", "fieldValueByName": column.clone(),
                       "content": {"__typename": "DraftIssue"}}),
                None,
            ),
            (
                "content this token may not see",
                json!({"updatedAt": "2026-10-01T00:00:00Z", "fieldValueByName": column.clone(),
                       "content": null}),
                None,
            ),
        ] {
            let got = item(node).in_column("getfloresta/Floresta", "Needs review");
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn a_response_without_the_project_is_not_an_empty_column() {
        for data in [
            json!({"organization": {"projectV2": null}}),
            json!({"organization": null}),
            json!({"organization": {"projectV2": {"items": {"nodes": []}}}}),
        ] {
            assert!(
                serde_json::from_value::<Data>(data.clone()).is_err(),
                "{data}"
            );
        }
    }
}
