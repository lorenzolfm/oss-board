//! GitHub access via the `gh` CLI.
//!
//! Deliberately avoids the Search API: during design it returned materially wrong
//! counts (382 open PRs for bitcoin/bitcoin reported as 0), so everything here uses
//! GraphQL repository() queries or REST, which are authoritative.

use std::fmt;
use std::process::Command;
use std::thread;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::proc::{self, Failure};
use crate::text::{is_truthy, prefix};

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

/// Run one GraphQL request and return its `data` object.
///
/// Keep queries small: a large multi-alias query is rejected with an HTML error
/// page rather than JSON, which is why callers chunk.
pub fn graphql(query: &str) -> Result<Value, GhError> {
    let mut last = String::new();
    for attempt in 0..RETRIES {
        let result = proc::run(
            Command::new("gh").args(["api", "graphql", "-f", &format!("query={query}")]),
            TIMEOUT,
        );
        match result {
            Err(Failure::Spawn(err)) => return Err(GhError(format!("cannot run gh: {err}"))),
            Err(failure @ Failure::TimedOut(_)) => last = failure.to_string(),
            Ok(out) if out.success => match serde_json::from_str::<Value>(&out.stdout) {
                Err(_) => {
                    last = format!(
                        "non-JSON response: {}",
                        prefix(&out.stdout, 200).replace('\n', " ")
                    );
                }
                Ok(mut payload) => {
                    if !is_truthy(&payload["errors"]) || is_truthy(&payload["data"]) {
                        return Ok(payload["data"].take());
                    }
                    last = prefix(&payload["errors"].to_string(), 300).to_owned();
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

/// A response in the shape the caller expects. A surprise here is a failed read,
/// like any other.
pub fn decode<T: DeserializeOwned>(value: Value) -> Result<T, GhError> {
    serde_json::from_value(value).map_err(|err| GhError(format!("unexpected response: {err}")))
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
    let data = graphql("query { viewer { login } }")?;
    data["viewer"]["login"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| GhError("no viewer login in the response".to_owned()))
}
