//! The org project board's "Needs Review" column.
//!
//! Needs a token with `read:project`; without it the query fails and the source is
//! simply absent from that run. `survey` exists for the one-off check that the
//! column is curated -- items moving in and out recently, not a name nobody uses.

use std::collections::HashSet;

use serde_json::Value;

use crate::config::{self, ProjectColumn};
use crate::gh::{self, GhError};
use crate::text::prefix;

struct ProjectItem {
    repo: Option<String>,
    number: Option<u64>,
    state: Option<String>,
    status: Option<String>,
    moved: Option<String>,
}

fn owned(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

fn items(org: &str, number: u64) -> Result<Vec<ProjectItem>, GhError> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let after = cursor
            .as_ref()
            .map_or(String::new(), |c| format!(", after: \"{c}\""));
        let data = gh::graphql(&format!(
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
        let page = &data["organization"]["projectV2"]["items"];
        for node in page["nodes"].as_array().into_iter().flatten() {
            let content = &node["content"];
            let status = &node["fieldValueByName"];
            out.push(ProjectItem {
                repo: owned(&content["repository"]["nameWithOwner"]),
                number: content["number"].as_u64(),
                state: owned(&content["state"]),
                status: owned(&status["name"]),
                moved: owned(&status["updatedAt"]).or_else(|| owned(&node["updatedAt"])),
            });
        }
        if page["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
            return Ok(out);
        }
        cursor = owned(&page["pageInfo"]["endCursor"]);
    }
}

/// Open item numbers of `repo` sitting in the column.
pub fn needs_review(repo: &str, cfg: &ProjectColumn) -> Result<HashSet<u64>, GhError> {
    let repo = repo.to_lowercase();
    let column = cfg.column.to_lowercase();
    Ok(items(cfg.org, cfg.number)?
        .into_iter()
        .filter(|item| {
            item.repo.as_deref().unwrap_or("").to_lowercase() == repo
                && item.state.as_deref() == Some("OPEN")
                && item.status.as_deref().unwrap_or("").to_lowercase() == column
        })
        .filter_map(|item| item.number)
        .collect())
}

/// Print every column of the org board, biggest first.
pub fn survey() -> Result<(), GhError> {
    let cfg = &config::PROJECT;
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
        let open = held
            .iter()
            .filter(|item| item.state.as_deref() == Some("OPEN"))
            .count();
        let newest = held
            .iter()
            .filter_map(|item| item.moved.as_deref())
            .max()
            .unwrap_or("");
        println!(
            "{:<20} {:>4} items  {open:>4} open  last moved {}",
            status.as_deref().unwrap_or("None"),
            held.len(),
            prefix(newest, 10)
        );
    }
    Ok(())
}
