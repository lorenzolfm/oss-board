//! Which parts of each project are yours, computed rather than declared.
//!
//! There is no CODEOWNERS to use. Instead: every file your commits touched, on the
//! upstream branch and on your unshipped branches, rolled up to its crate. The judge
//! gets this as context for ranking; nothing is ever excluded on it.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use anyhow::Result;

use crate::config::LocalClone;
use crate::proc;

const GIT_LOG_TIMEOUT: Duration = Duration::from_secs(60);

/// Commits touching each area, most-touched first.
#[derive(Clone, Debug, Default)]
pub struct Footprint(Vec<(String, u64)>);

impl Footprint {
    pub fn get(&self, area: &str) -> u64 {
        self.0
            .iter()
            .find(|(name, _)| name == area)
            .map_or(0, |(_, count)| *count)
    }

    fn contains(&self, area: &str) -> bool {
        self.0.iter().any(|(name, _)| name == area)
    }

    pub fn areas(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(name, _)| name.as_str())
    }

    /// Several repos' footprints as one. Crates are named after their project, so what
    /// adds up is the likes of `docs` and `(root)`, which only rank anything by a little.
    pub fn merged(all: impl IntoIterator<Item = Self>) -> Self {
        let mut counts: HashMap<String, u64> = HashMap::new();
        for (name, count) in all.into_iter().flat_map(|fp| fp.0) {
            *counts.entry(name).or_insert(0) += count;
        }
        let mut ranked: Vec<(String, u64)> = counts.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Self(ranked)
    }
}

fn area(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if matches!(parts[0], "crates" | "bin") && parts.len() > 2 {
        return parts[..2].join("/");
    }
    if parts.len() > 1 {
        parts[0].to_owned()
    } else {
        "(root)".to_owned()
    }
}

/// `extra` adds refs to the upstream branch, e.g. your unshipped branches. A history
/// that cannot be read is an error, so the run can say the judge ranked without it.
pub fn compute(email: &str, local: &LocalClone, extra: &[String]) -> Result<Footprint> {
    let mut counts: HashMap<String, u64> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let refs = std::iter::once(local.upstream_ref.as_str()).chain(extra.iter().map(String::as_str));
    for reference in refs {
        let log = proc::git(
            &local.path,
            &[
                "log",
                reference,
                &format!("--author={email}"),
                "--no-merges",
                "--name-only",
                "--format=%x00",
            ],
            GIT_LOG_TIMEOUT,
        )?;
        for commit in log.split('\0') {
            let areas: BTreeSet<String> = commit
                .split('\n')
                .filter(|line| !line.trim().is_empty())
                .map(area)
                .collect();
            for name in areas {
                let count = counts.entry(name.clone()).or_insert(0);
                if *count == 0 {
                    order.push(name);
                }
                *count += 1;
            }
        }
    }
    let mut ranked: Vec<(String, u64)> = order
        .into_iter()
        .map(|name| {
            let count = counts[&name];
            (name, count)
        })
        .collect();
    ranked.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    Ok(Footprint(ranked))
}

/// The areas of `files` you have worked in, best first.
pub fn overlap(files: &[String], fp: &Footprint) -> Vec<String> {
    let mut hit: Vec<String> = Vec::new();
    for file in files {
        let name = area(file);
        if fp.contains(&name) && !hit.contains(&name) {
            hit.push(name);
        }
    }
    hit.sort_by_key(|name| std::cmp::Reverse(fp.get(name)));
    hit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_roll_up_to_their_crate() {
        for (path, want) in [
            ("crates/floresta-wire/src/node.rs", "crates/floresta-wire"),
            ("bin/florestad/src/main.rs", "bin/florestad"),
            ("crates/README.md", "crates"),
            ("doc/rpc/getblock.md", "doc"),
            ("Cargo.toml", "(root)"),
        ] {
            assert_eq!(area(path), want, "{path}");
        }
    }

    #[test]
    fn repos_merge_into_one_list_most_touched_first() {
        let floresta = Footprint(vec![
            ("crates/floresta-wire".to_owned(), 40),
            ("docs".to_owned(), 2),
        ]);
        let cdk = Footprint(vec![
            ("crates/cdk-common".to_owned(), 7),
            ("docs".to_owned(), 1),
        ]);
        let merged = Footprint::merged([floresta, cdk]);
        assert_eq!(
            merged.areas().collect::<Vec<_>>(),
            ["crates/floresta-wire", "crates/cdk-common", "docs"]
        );
        assert_eq!(merged.get("docs"), 3);
    }

    #[test]
    fn overlap_lists_areas_you_touched_most_touched_first_without_repeats() {
        let fp = Footprint(vec![
            ("crates/floresta-wire".to_owned(), 40),
            ("bin/florestad".to_owned(), 3),
        ]);
        let files = [
            "bin/florestad/src/a.rs",
            "crates/floresta-chain/src/b.rs",
            "crates/floresta-wire/src/c.rs",
            "bin/florestad/src/d.rs",
        ]
        .map(str::to_owned);
        assert_eq!(
            overlap(&files, &fp),
            ["crates/floresta-wire", "bin/florestad"]
        );
    }
}
