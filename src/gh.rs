//! GitHub access via the `gh` CLI.
//!
//! Deliberately avoids the Search API: during design it returned materially wrong
//! counts (382 open PRs for bitcoin/bitcoin reported as 0), so everything here uses
//! GraphQL repository() queries or REST, which are authoritative.
//!
//! Responses are parsed here, once, into typed structs. A response of another shape
//! is a failed read like any other, because an empty list parsed from nothing would
//! empty a lane.

use std::fmt;
use std::process::Command;
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::proc::{self, Failure};
use crate::text::prefix;

const RETRIES: u64 = 3;
const TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug)]
pub struct GhError(pub String);

impl fmt::Display for GhError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GhError {}

/// The state of an issue or a PR.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum ItemState {
    Open,
    Closed,
    Merged,
}

/// A login from a union: a team asked for as a user has none.
#[derive(Deserialize)]
pub struct Login {
    pub login: Option<String>,
}

/// A nested list. The schema lets the list and each node be null. A null list is a
/// failed read, and a null node is one this token may not see.
#[derive(Deserialize)]
pub struct Conn<T> {
    nodes: Vec<Option<T>>,
}

impl<T> Conn<T> {
    pub fn into_nodes(self) -> impl Iterator<Item = T> {
        self.nodes.into_iter().flatten()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

/// One page of a list that is read to its end.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    page_info: PageInfo,
    nodes: Vec<Option<T>>,
}

impl<T> Page<T> {
    /// Moves the nodes into `out` and returns the next page's cursor, if there is a
    /// next page.
    pub fn drain_into(self, out: &mut Vec<T>) -> Result<Option<String>, GhError> {
        out.extend(self.nodes.into_iter().flatten());
        match (self.page_info.has_next_page, self.page_info.end_cursor) {
            (false, _) => Ok(None),
            (true, Some(end)) => Ok(Some(end)),
            (true, None) => Err(GhError("hasNextPage with no cursor".to_owned())),
        }
    }
}

#[derive(Deserialize)]
struct Envelope {
    data: Option<Value>,
    errors: Option<Value>,
}

/// Run one GraphQL request and return its `data` object.
///
/// Keep queries small: a large multi-alias query is rejected with an HTML error
/// page rather than JSON, which is why callers chunk.
fn graphql(query: &str) -> Result<Value, GhError> {
    let mut last = String::new();
    for attempt in 0..RETRIES {
        let result = proc::run(
            Command::new("gh").args(["api", "graphql", "-f", &format!("query={query}")]),
            TIMEOUT,
        );
        match result {
            Err(Failure::Spawn(err)) => return Err(GhError(format!("cannot run gh: {err}"))),
            Err(failure @ Failure::TimedOut(_)) => last = failure.to_string(),
            Ok(out) if out.success => match serde_json::from_str::<Envelope>(&out.stdout) {
                Err(_) => {
                    last = format!(
                        "non-JSON response: {}",
                        prefix(&out.stdout, 200).replace('\n', " ")
                    );
                }
                Ok(Envelope {
                    data: Some(data), ..
                }) => return Ok(data),
                Ok(Envelope { errors, .. }) => {
                    let said = errors.unwrap_or(Value::Null).to_string();
                    last = format!("no data: {}", prefix(&said, 300));
                }
            },
            Ok(out) => {
                let said = if out.stderr.is_empty() {
                    &out.stdout
                } else {
                    &out.stderr
                };
                last = prefix(said, 300).replace('\n', " ");
            }
        }
        thread::sleep(Duration::from_secs(2 * (attempt + 1)));
    }
    Err(GhError(format!(
        "graphql failed after {RETRIES} attempts: {last}"
    )))
}

/// A GraphQL response in the shape the caller expects.
pub fn query<T: DeserializeOwned>(query: &str) -> Result<T, GhError> {
    serde_json::from_value(graphql(query)?)
        .map_err(|err| GhError(format!("unexpected response: {err}")))
}

pub fn rest(path: &str, paginate: bool, jq: Option<&str>) -> Result<String, GhError> {
    let mut cmd = Command::new("gh");
    cmd.args(["api", path]);
    if paginate {
        cmd.arg("--paginate");
    }
    if let Some(jq) = jq {
        cmd.args(["--jq", jq]);
    }
    match proc::run(&mut cmd, TIMEOUT) {
        Err(Failure::TimedOut(_)) => Err(GhError(format!("rest {path} timed out"))),
        Err(failure) => Err(GhError(format!("rest {path} failed: {failure}"))),
        Ok(out) if out.success => Ok(out.stdout),
        Ok(out) => {
            let said = if out.stderr.is_empty() {
                &out.stdout
            } else {
                &out.stderr
            };
            Err(GhError(format!(
                "rest {path} failed: {}",
                prefix(said, 300)
            )))
        }
    }
}

pub fn viewer_login() -> Result<String, GhError> {
    #[derive(Deserialize)]
    struct Viewer {
        login: String,
    }
    #[derive(Deserialize)]
    struct Data {
        viewer: Viewer,
    }
    let data: Data = query("query { viewer { login } }")?;
    Ok(data.viewer.login)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page(value: Value) -> Result<Page<u64>, serde_json::Error> {
        serde_json::from_value(value)
    }

    #[test]
    fn a_page_with_no_list_or_no_page_info_is_not_an_empty_page() {
        for value in [
            json!({"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": null}),
            json!({"pageInfo": {"hasNextPage": false, "endCursor": null}}),
            json!({"nodes": []}),
            json!({"pageInfo": {"endCursor": null}, "nodes": []}),
        ] {
            assert!(page(value.clone()).is_err(), "{value}");
        }
    }

    #[test]
    fn a_page_says_where_the_next_one_starts_and_a_null_node_is_skipped() {
        let mut out = Vec::new();
        let next = page(json!({
            "pageInfo": {"hasNextPage": true, "endCursor": "abc"},
            "nodes": [1, null, 2],
        }))
        .unwrap()
        .drain_into(&mut out)
        .unwrap();
        assert_eq!(next.as_deref(), Some("abc"));
        assert_eq!(out, [1, 2]);

        let last = page(json!({
            "pageInfo": {"hasNextPage": false, "endCursor": "abc"},
            "nodes": [],
        }))
        .unwrap()
        .drain_into(&mut out)
        .unwrap();
        assert_eq!(last, None);
    }

    #[test]
    fn more_pages_with_no_cursor_fails_instead_of_rereading_the_first() {
        let result = page(json!({
            "pageInfo": {"hasNextPage": true, "endCursor": null},
            "nodes": [],
        }))
        .unwrap()
        .drain_into(&mut Vec::new());
        assert!(result.is_err());
    }
}
