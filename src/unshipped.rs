//! Your work on a project that exists but never reached upstream.
//!
//! Two sources:
//!   * open PRs against your own fork. They look reviewed-and-done there, but
//!     nobody upstream has seen them.
//!   * local branches carrying commits you wrote that upstream does not have.
//!
//! The false positives the old stranded scan produced are the design here:
//!   * a branch counts only for commits *you* authored that `git cherry` says are
//!     not upstream -- so a checkout of someone else's PR for review never does;
//!   * anything already carried by an upstream PR of yours, in any state, is
//!     hidden: matched on title, branch name, or commit. A fork PR you later opened
//!     upstream (fork #8 -> #1330) is not stranded;
//!   * branches named after a PR (`pr-1105`, `review-1252`) are review checkouts.

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use regex::Regex;
use serde::Deserialize;

use crate::board::{CardKey, GitHubItem};
use crate::config::{self, LocalClone, Project, Repo};
use crate::proc;
use crate::snapshot::{self, Item, UpstreamPr};
use crate::timestamp::Timestamp;

const GIT_TIMEOUT: Duration = Duration::from_secs(30);

static CHECKOUT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:pr|review)[-_]?\d+(?:-head)?$").expect("a constant pattern compiles")
});

/// A reference to one of `repo`'s issues, spelled out or as a bare `#N`. Consuming the
/// character before a bare `#` stands in for the lookbehind the regex crate does not
/// have: a match never starts inside the previous one, since that ends in a digit, which
/// already rules the next `#` out. It also means only a match naming the repo holds a `/`.
fn ref_re(repo: Repo) -> Regex {
    let repo = regex::escape(&repo.to_string());
    Regex::new(&format!(
        r"(?i)(?:github\.com/{repo}/(?:issues|pull)/|{repo}#|(?:^|[^\w/])#)(\d+)"
    ))
    .expect("an escaped repo compiles")
}

const STOP: [&str; 39] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "into", "is", "it",
    "its", "of", "on", "or", "per", "the", "to", "with", "without", "when", "not", "no", "via",
    "this", "that", "fix", "feat", "chore", "refactor", "test", "tests", "docs", "make", "add",
    "use",
];

/// The fewest words a guess can share with an issue's title.
const GUESS_MIN_WORDS: usize = 3;

/// Which upstream issue a piece of work fixes, and how sure we are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkHow {
    /// It names the issue explicitly.
    Refs,
    /// Title match on at least `GUESS_MIN_WORDS` shared words, which is why the card says
    /// "probably".
    Guess,
}

#[derive(Clone, Debug)]
pub struct IssueLink {
    pub key: CardKey,
    pub url: String,
    pub how: LinkHow,
}

#[derive(Clone, Debug)]
pub enum Origin {
    ForkPr {
        item: GitHubItem,
        url: String,
        additions: u64,
        deletions: u64,
    },
    Branch,
}

#[derive(Clone, Debug)]
pub struct Unshipped {
    pub repo: Repo,
    pub origin: Origin,
    pub title: String,
    pub branch: String,
    pub commits: usize,
    pub last: NaiveDate,
    pub issue: Option<IssueLink>,
}

impl Unshipped {
    /// What its card is about. Two clones can both have a `fix-tests`, and they are not one
    /// card.
    pub fn key(&self) -> CardKey {
        match &self.origin {
            Origin::ForkPr { item, .. } => CardKey::GitHub(item.clone()),
            Origin::Branch => CardKey::branch(&format!("{}/{}", self.repo.label(), self.branch)),
        }
    }
}

fn git(path: &Path, args: &[&str]) -> Result<String> {
    proc::git(path, args, GIT_TIMEOUT)
}

/// The email your commits are recognised by. Without one every commit would read as
/// someone else's, and the lane as nothing to ship, so no email is a failed read.
pub fn my_email(clone: &Path) -> Result<String> {
    let email = git(clone, &["config", "user.email"])?.trim().to_lowercase();
    if email.is_empty() {
        bail!("user.email is empty in {}", clone.display());
    }
    Ok(email)
}

// ---------------------------------------------------------------- matching

fn split_alnum(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|ch: char| !ch.is_ascii_lowercase() && !ch.is_ascii_digit())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

fn norm_title(title: &str) -> String {
    split_alnum(title).join(" ")
}

/// Comparable words: lowercase, split on punctuation (config.toml is two words),
/// stopwords out, crude suffix stemming so parse/parsed/parsing meet.
fn tokens(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for word in split_alnum(text) {
        if word.len() < 3
            || STOP.contains(&word.as_str())
            || word.bytes().all(|b| b.is_ascii_digit())
        {
            continue;
        }
        let mut word = word.as_str();
        for suffix in ["ing", "ed", "es", "s"] {
            if let Some(stem) = word.strip_suffix(suffix)
                && stem.len() >= 4
            {
                word = stem;
                break;
            }
        }
        let bytes = word.as_bytes();
        if bytes.len() >= 4 && bytes[bytes.len() - 1] == bytes[bytes.len() - 2] {
            word = &word[..word.len() - 1]; // logg -> log
        }
        out.insert(word.to_owned());
    }
    out
}

/// What a piece of work says about itself: its branch's name, and its titles without a prefix
/// like `fix(chain):`, which is whatever comes before the first `: ` if it has no whitespace
/// in it. That prefix says which part of the project the work is in, not what it does, so a
/// word from it is no sign of the same problem. A branch's name isn't split that way, so
/// `fix/chain-tip` still says `chain`. An issue's title keeps its prefix, because there it is
/// often the subject.
fn work_words<'a>(branch: &str, titles: impl IntoIterator<Item = &'a String>) -> HashSet<String> {
    let mut words = tokens(branch);
    for title in titles {
        let said = title
            .split_once(": ")
            .filter(|(prefix, _)| !prefix.contains(char::is_whitespace))
            .map_or(title.as_str(), |(_, said)| said);
        words.extend(tokens(said));
    }
    words
}

/// The open upstream issue this work most likely fixes.
///
/// An explicit reference wins. Bare `#N` in a fork PR means the fork's own numbering
/// unless N is too big to be one, hence the >= 100 floor. Failing that, a title
/// match on at least `GUESS_MIN_WORDS` shared words.
fn link_issue<'a>(
    refs: &Regex,
    text: &str,
    words: &HashSet<String>,
    issues: &[&'a Item],
) -> Option<(&'a Item, LinkHow)> {
    for found in refs.captures_iter(text) {
        let Ok(number) = found[1].parse::<u64>() else {
            continue;
        };
        let named = found[0].contains('/');
        if (number >= 100 || named)
            && let Some(issue) = issues.iter().find(|issue| issue.is_number(number))
        {
            return Some((*issue, LinkHow::Refs));
        }
    }
    let mut best = None;
    let mut score = 0;
    for issue in issues.iter().copied() {
        let shared = tokens(&issue.title).intersection(words).count();
        if shared > score {
            best = Some(issue);
            score = shared;
        }
    }
    if score >= GUESS_MIN_WORDS {
        best.map(|issue| (issue, LinkHow::Guess))
    } else {
        None
    }
}

// ---------------------------------------------------------------- sources

/// An open PR against your own fork.
pub struct ForkPr {
    pub title: String,
    pub body: String,
    pub url: String,
    pub branch: String,
    pub head: String,
    pub last: NaiveDate,
    pub additions: u64,
    pub deletions: u64,
    pub commits: usize,
}

#[derive(Deserialize)]
struct ForkAuthor {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForkCommits {
    total_count: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForkNode {
    title: String,
    body: String,
    url: String,
    updated_at: Timestamp,
    additions: u64,
    deletions: u64,
    head_ref_name: String,
    head_ref_oid: String,
    author: Option<ForkAuthor>,
    commits: ForkCommits,
}

const FORK_FIELDS: &str = "
  title body url updatedAt additions deletions
  headRefName headRefOid author { login }
  commits(first: 1) { totalCount }
";

fn fork_prs(fork: Repo, me: &str) -> Result<Vec<ForkPr>> {
    let nodes: Vec<ForkNode> = snapshot::page_through(fork, "pullRequests", FORK_FIELDS, 50)?;
    // Newest first as read. The lane lists the oldest first.
    Ok(nodes
        .into_iter()
        .rev()
        .filter(|node| {
            node.author
                .as_ref()
                .is_some_and(|author| author.login.eq_ignore_ascii_case(me))
        })
        .map(|node| ForkPr {
            title: node.title,
            body: node.body,
            url: node.url,
            branch: node.head_ref_name,
            head: node.head_ref_oid,
            last: node.updated_at.date(),
            additions: node.additions,
            deletions: node.deletions,
            commits: node.commits.total_count,
        })
        .collect())
}

struct LocalBranch {
    branch: String,
    title: String,
    subjects: Vec<String>,
    commits: usize,
    last: NaiveDate,
    oids: HashSet<String>,
}

/// Branches with commits you wrote that upstream does not carry.
fn local_branches(local: &LocalClone, me: &str) -> Result<Vec<LocalBranch>> {
    let clone = local.path.as_path();
    let mut out = Vec::new();
    let refs = git(
        clone,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )?;
    for branch in refs.split_whitespace() {
        if branch == local.base
            || config::BRANCH_IGNORE.iter().any(|p| branch.starts_with(p))
            || CHECKOUT_RE.is_match(branch)
        {
            continue;
        }
        // `git cherry` compares patches, so a commit cherry-picked or rebased into
        // upstream counts as shipped even though its hash differs.
        let cherry = git(clone, &["cherry", &local.upstream_ref, branch])?;
        let fresh: Vec<&str> = cherry
            .lines()
            .filter_map(|line| line.strip_prefix("+ "))
            .map(str::trim)
            .collect();
        if fresh.is_empty() {
            continue;
        }
        let mut args = vec!["log", "--no-walk", "--format=%H%x09%ae%x09%cs%x09%s"];
        args.extend(&fresh);
        let log = git(clone, &args)?;
        let mut mine: Vec<(&str, NaiveDate, &str)> = Vec::new();
        for line in log.lines() {
            let mut fields = line.split('\t');
            let oid = fields.next().unwrap_or("");
            let email = fields.next().unwrap_or("");
            let day = fields.next().unwrap_or("");
            let subject = fields.next().unwrap_or("");
            if email.to_lowercase() == me {
                let day = day
                    .parse()
                    .with_context(|| format!("git log gave {oid} the date {day:?}"))?;
                mine.push((oid, day, subject));
            }
        }
        mine.sort_by_key(|(_, day, _)| *day);
        let Some((_, last, title)) = mine.last().copied() else {
            continue;
        };
        out.push(LocalBranch {
            branch: branch.to_owned(),
            title: title.to_owned(),
            subjects: mine.iter().map(|(_, _, s)| (*s).to_owned()).collect(),
            commits: mine.len(),
            last,
            oids: mine.iter().map(|(oid, _, _)| (*oid).to_owned()).collect(),
        });
    }
    Ok(out)
}

fn link_of(
    refs: &Regex,
    text: &str,
    words: &HashSet<String>,
    issues: &[&Item],
) -> Option<IssueLink> {
    link_issue(refs, text, words, issues).map(|(issue, how)| IssueLink {
        key: issue.key.clone(),
        url: issue.url.clone(),
        how,
    })
}

/// Every piece of unshipped work, given your fork's PRs. `upstream_mine` is
/// `snapshot::mine`: every PR you opened upstream.
fn assemble(
    prs: Vec<ForkPr>,
    issues: &[&Item],
    upstream_mine: &[UpstreamPr],
    local: &LocalClone,
) -> Result<Vec<Unshipped>> {
    let repo = local.repo;
    let refs = ref_re(repo);
    let up_titles: HashSet<String> = upstream_mine.iter().map(|p| norm_title(&p.title)).collect();
    let up_refs: HashSet<&str> = upstream_mine.iter().map(|p| p.head_ref.as_str()).collect();
    let up_oids: HashSet<&String> = upstream_mine.iter().flat_map(|p| &p.commits).collect();
    let fork_refs: HashSet<String> = prs.iter().map(|p| p.branch.clone()).collect();

    let mut items = Vec::new();
    for pr in prs {
        if up_titles.contains(&norm_title(&pr.title))
            || up_refs.contains(pr.branch.as_str())
            || up_oids.contains(&pr.head)
        {
            continue;
        }
        let words = work_words(&pr.branch, [&pr.title]);
        let text = format!("{}\n{}", pr.body, pr.title);
        items.push(Unshipped {
            repo,
            issue: link_of(&refs, &text, &words, issues),
            origin: Origin::ForkPr {
                item: snapshot::github_item(&pr.url)?,
                url: pr.url,
                additions: pr.additions,
                deletions: pr.deletions,
            },
            title: pr.title,
            branch: pr.branch,
            commits: pr.commits,
            last: pr.last,
        });
    }

    let me = my_email(&local.path)?;
    for branch in local_branches(local, &me)? {
        if up_refs.contains(branch.branch.as_str())
            || fork_refs.contains(&branch.branch)
            || branch.oids.iter().any(|oid| up_oids.contains(oid))
        {
            continue;
        }
        let words = work_words(&branch.branch, &branch.subjects);
        let text = format!("{}\n{}", branch.subjects.join("\n"), branch.title);
        items.push(Unshipped {
            repo,
            issue: link_of(&refs, &text, &words, issues),
            origin: Origin::Branch,
            title: branch.title,
            branch: branch.branch,
            commits: branch.commits,
            last: branch.last,
        });
    }
    Ok(items)
}

pub fn run(
    project: &Project,
    issues: &[&Item],
    upstream_mine: &[UpstreamPr],
    me: &str,
) -> Result<Vec<Unshipped>> {
    assemble(
        fork_prs(project.fork, me)?,
        issues,
        upstream_mine,
        &project.local(),
    )
}

#[cfg(test)]
mod tests {
    //! Unshipped work, against real git repos built in a temp dir.
    //!
    //! The old stranded scan's failures are the cases: a review checkout reported
    //! as your work, a fork PR already upstream under the same title (fork #8 /
    //! #1330), and a cherry-picked commit counted as unshipped because its hash
    //! changed.

    use super::*;
    use crate::testkit::{issue, key};
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use tempfile::TempDir;

    const MINE: &str = "me@example.com";
    const OTHER: &str = "other@example.com";
    const UPSTREAM: &str = "upstream/master";
    const FLORESTA: Repo = Repo {
        owner: "getfloresta",
        name: "Floresta",
    };
    const CDK: Repo = Repo {
        owner: "cashubtc",
        name: "cdk",
    };

    fn run_git(path: &Path, args: &[&str], email: &str) {
        let out = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .env("GIT_AUTHOR_NAME", "x")
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", "x")
            .env("GIT_COMMITTER_EMAIL", email)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn commit(path: &Path, name: &str, email: &str, message: &str) {
        fs::write(path.join(name), name).unwrap();
        run_git(path, &["add", name], email);
        run_git(path, &["commit", "-qm", message], email);
    }

    struct Repos {
        _tmp: TempDir,
        clone: PathBuf,
    }

    fn branch(repos: &Repos, name: &str, files: &[&str], email: &str, message: Option<&str>) {
        let clone = &repos.clone;
        run_git(clone, &["checkout", "-q", "-b", name, UPSTREAM], email);
        for file in files {
            let default = format!("add {file}");
            commit(clone, file, email, message.unwrap_or(&default));
        }
        run_git(clone, &["checkout", "-q", "master"], email);
    }

    fn repos() -> Repos {
        let tmp = TempDir::new().unwrap();
        let up = tmp.path().join("upstream");
        fs::create_dir(&up).unwrap();
        run_git(&up, &["init", "-q", "-b", "master"], MINE);
        commit(&up, "base", OTHER, "add base");
        let clone = tmp.path().join("clone");
        run_git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "-o",
                "upstream",
                up.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
            MINE,
        );
        run_git(&clone, &["config", "user.email", MINE], MINE);
        let repos = Repos { _tmp: tmp, clone };

        branch(
            &repos,
            "dedup-config-parse",
            &["cfg1", "cfg2"],
            MINE,
            Some("fix(node): parse config.toml once"),
        );
        branch(&repos, "review-of-bobs-pr", &["bob1"], OTHER, None); // someone else's commits
        branch(&repos, "pr-1105", &["c1"], MINE, None); // a PR checkout by name
        branch(&repos, "backup/old-wip", &["w1"], MINE, None);
        branch(&repos, "already-up", &["picked"], MINE, None);
        // Upstream takes the "already-up" patch by cherry-pick: same change, new hash.
        run_git(
            &up,
            &["fetch", "-q", repos.clone.to_str().unwrap(), "already-up"],
            OTHER,
        );
        run_git(&up, &["cherry-pick", "FETCH_HEAD"], OTHER);
        run_git(&repos.clone, &["fetch", "-q", "upstream"], MINE);
        branch(&repos, "shipped-as-pr", &["s1"], MINE, None);
        repos
    }

    fn clone_at(path: &Path) -> LocalClone {
        LocalClone {
            repo: FLORESTA,
            path: path.to_owned(),
            upstream_ref: UPSTREAM.to_owned(),
            base: "master",
        }
    }

    #[test]
    fn local_branches_are_only_commits_you_wrote_that_upstream_lacks() {
        let repos = repos();
        let found = local_branches(&clone_at(&repos.clone), MINE).unwrap();
        let names: Vec<&str> = found.iter().map(|b| b.branch.as_str()).collect();
        assert!(names.contains(&"dedup-config-parse"), "{names:?}");
        let dedup = found
            .iter()
            .find(|b| b.branch == "dedup-config-parse")
            .unwrap();
        assert_eq!(dedup.commits, 2);
        assert!(
            !names.contains(&"review-of-bobs-pr"),
            "a review checkout of others' work"
        );
        assert!(
            !names.contains(&"pr-1105"),
            "a branch named after a PR is a checkout"
        );
        assert!(
            !names.contains(&"backup/old-wip"),
            "ignored prefixes stay ignored"
        );
        assert!(
            !names.contains(&"already-up"),
            "a cherry-picked patch counts as shipped"
        );
    }

    /// A lane computed from a clone that could not be read would look like one with
    /// nothing left to ship.
    #[test]
    fn a_clone_that_cannot_be_read_is_a_failure_not_an_empty_list() {
        let missing = TempDir::new().unwrap().path().join("gone");
        assert!(local_branches(&clone_at(&missing), MINE).is_err());
        assert!(my_email(&missing).is_err());

        let repos = repos();
        let no_such_ref = LocalClone {
            upstream_ref: "upstream/no-such-ref".to_owned(),
            ..clone_at(&repos.clone)
        };
        assert!(
            local_branches(&no_such_ref, MINE).is_err(),
            "a ref git cannot resolve is not a branch with nothing new on it"
        );

        run_git(&repos.clone, &["config", "user.email", ""], MINE);
        assert!(
            my_email(&repos.clone).is_err(),
            "no email is no way to tell your commits from anyone's"
        );
    }

    /// A clone can have a branch named after another project's base, and it is work there.
    #[test]
    fn only_the_projects_own_base_is_never_work() {
        let repos = repos();
        branch(&repos, "develop", &["d1"], MINE, None);
        let listed = |base| {
            let local = LocalClone {
                base,
                ..clone_at(&repos.clone)
            };
            let found = local_branches(&local, MINE).unwrap();
            found.iter().any(|b| b.branch == "develop")
        };
        assert!(!listed("develop"));
        assert!(listed("master"));
    }

    fn fork_pr(number: u64, title: &str, branch: &str, head: &str) -> ForkPr {
        ForkPr {
            title: title.to_owned(),
            body: String::new(),
            url: format!("https://github.com/lorenzolfm/Floresta/pull/{number}"),
            branch: branch.to_owned(),
            head: head.repeat(40),
            last: NaiveDate::from_ymd_opt(2026, 9, 12).unwrap(),
            additions: 1,
            deletions: 0,
            commits: 2,
        }
    }

    fn upstream_pr(title: &str, head_ref: &str) -> UpstreamPr {
        UpstreamPr {
            title: title.to_owned(),
            head_ref: head_ref.to_owned(),
            commits: HashSet::new(),
        }
    }

    fn linkable_issues() -> Vec<Item> {
        vec![
            issue(1308).title("config.toml is read and parsed three times per startup"),
            issue(167).title("[tracking-issue] Audit logging and identify improvements"),
            issue(6).title("Unrelated wording entirely"),
        ]
    }

    #[test]
    fn what_an_upstream_pr_already_carries_is_hidden() {
        let repos = repos();
        let shipped_title = "fix(wire): report block download progress on a time interval";
        let prs = vec![
            fork_pr(8, shipped_title, "fix/time-log", "a"),
            fork_pr(
                6,
                "Read and parse config.toml once per boot",
                "other-name",
                "b",
            ),
        ];
        let upstream = vec![
            upstream_pr(shipped_title, "fix/time-based-ibd-progress-log"),
            upstream_pr("x", "shipped-as-pr"),
        ];
        let issues = linkable_issues();
        let issues: Vec<&Item> = issues.iter().collect();
        let items = assemble(prs, &issues, &upstream, &clone_at(&repos.clone)).unwrap();
        let keys: Vec<CardKey> = items.iter().map(Unshipped::key).collect();
        assert!(
            !keys.contains(&key("lorenzolfm/floresta#8")),
            "already upstream under its title: {keys:?}"
        );
        assert!(
            keys.contains(&key("lorenzolfm/floresta#6")),
            "nobody upstream has seen it: {keys:?}"
        );
        assert!(
            !keys.contains(&CardKey::branch("floresta/shipped-as-pr")),
            "upstream merged it as a PR: {keys:?}"
        );
        assert!(
            keys.contains(&CardKey::branch("floresta/dedup-config-parse")),
            "an unpushed branch stays: {keys:?}"
        );
    }

    #[test]
    fn linking_work_to_the_issue_it_fixes() {
        let issues = linkable_issues();
        let issues: Vec<&Item> = issues.iter().collect();
        let refs = ref_re(FLORESTA);
        let link = |text: &str, words: &str| {
            link_issue(&refs, text, &tokens(words), &issues)
                .map(|(issue, how)| (issue.number(), how))
        };
        assert_eq!(
            link(
                "",
                "Read and parse config.toml once per boot dedup-config-parse"
            ),
            Some((1308, LinkHow::Guess)),
            "fork #6 probably fixes #1308"
        );
        assert_eq!(
            link("", "feat(florestad): prototype OTLP/HTTP JSON log export"),
            None,
            "one shared word is not a link (OTLP vs the logging audit)"
        );
        assert_eq!(
            link("See https://github.com/getfloresta/Floresta/issues/167", ""),
            Some((167, LinkHow::Refs)),
            "an explicit upstream URL wins"
        );
        assert_eq!(
            link("follow-up to #6", ""),
            None,
            "a small bare #N is the fork's own numbering, not upstream"
        );
        assert_eq!(
            link("see getfloresta/floresta#6", ""),
            Some((6, LinkHow::Refs)),
            "naming the repo makes even a small number upstream's"
        );
    }

    /// The first three linked on the board on 2026-11-05, and none of that work fixes its
    /// issue. Both kinds of work build their words apart from `link_issue`, so both are read.
    #[test]
    fn a_guess_takes_three_words_past_the_prefix_of_a_title() {
        let cases = [
            (
                "a prefix and two words",
                "fix(chain): stop truncating the accumulator file on reorg",
                "flat-store-roots-truncation",
                "getblockchaininfo reports progress=1.0 / ibd=false when the chain store stops accepting blocks early",
                false,
            ),
            (
                "a prefix both titles share a word of",
                "fix(cdk-sql-common): guard the mint quote write on the stored totals",
                "guard-mint-quote-write",
                "bug(cdk-mint-rpc): force-paid mint quote RPC can over credit quotes",
                false,
            ),
            (
                "two words and no help from the prefix",
                "refactor(cdk-common): tie locked rows to their transaction at compile time",
                "acquired-branded-transaction",
                "Wallet database update_mint_url leaves transactions, melt_quote, and mint rows on the old URL",
                false,
            ),
            (
                "three words after a prefix",
                "fix(node): read and parse config.toml once per boot",
                "parse-config-once",
                "config.toml is read and parsed three times per startup",
                true,
            ),
            (
                "a colon further on is not a prefix",
                "Read and parse config.toml: once per boot",
                "read-config-boot",
                "config.toml is read and parsed three times per startup",
                true,
            ),
            (
                "the branch's name is words too",
                "Read it once per boot",
                "config-toml",
                "config.toml is read and parsed three times per startup",
                true,
            ),
            (
                "an issue's prefix is words too",
                "feat(wallet): find a mint over nostr by discoverability",
                "wallet-finds-mints",
                "cdk-nostr: add optional NIP-87 mint discoverability types and query helpers",
                true,
            ),
        ];
        let repos = repos();
        let local = clone_at(&repos.clone);
        for (index, (name, title, branch_name, issue_title, links)) in cases.into_iter().enumerate()
        {
            let upstream = issue(1).title(issue_title);
            let issues = [&upstream];
            let want = links.then(|| upstream.key.clone());
            let linked = |prs: Vec<ForkPr>| {
                assemble(prs, &issues, &[], &local)
                    .unwrap()
                    .into_iter()
                    .find(|u| u.branch == branch_name)
                    .expect("the work is listed")
                    .issue
                    .map(|link| link.key)
            };
            let file = format!("case{index}");
            branch(&repos, branch_name, &[&file], MINE, Some(title));
            assert_eq!(linked(vec![]), want, "as a local branch: {name}");
            let pr = fork_pr(6, title, branch_name, "a");
            assert_eq!(linked(vec![pr]), want, "as a fork PR: {name}");
        }
    }

    #[test]
    fn a_reference_counts_only_for_the_repo_it_names() {
        let issues = linkable_issues();
        let issues: Vec<&Item> = issues.iter().collect();
        let refs = ref_re(CDK);
        let link = |text: &str| {
            link_issue(&refs, text, &HashSet::new(), &issues).map(|(issue, _)| issue.number())
        };
        assert_eq!(link("see cashubtc/cdk#6"), Some(6));
        assert_eq!(
            link("https://github.com/cashubtc/cdk/issues/167"),
            Some(167)
        );
        assert_eq!(
            link("fixes #167"),
            Some(167),
            "a bare number is this repo's"
        );
        assert_eq!(link("see getfloresta/floresta#167"), None);
        assert_eq!(
            link("https://github.com/getfloresta/Floresta/issues/167"),
            None
        );
    }

    /// Two clones can both have a `fix-tests`, and dragging one to Passed is not about
    /// the other.
    #[test]
    fn a_branch_is_keyed_by_its_repo_too() {
        let repos = repos();
        branch(&repos, "fix-tests", &["a"], MINE, Some("fix the tests"));
        let keys = |repo| {
            let local = LocalClone {
                repo,
                ..clone_at(&repos.clone)
            };
            let items = assemble(vec![], &[], &[], &local).unwrap();
            items.iter().map(Unshipped::key).collect::<Vec<_>>()
        };
        assert!(keys(FLORESTA).contains(&CardKey::branch("floresta/fix-tests")));
        assert!(keys(CDK).contains(&CardKey::branch("cdk/fix-tests")));
    }

    #[test]
    fn stemming_makes_parse_parsed_and_parsing_meet() {
        assert_eq!(tokens("parsed"), tokens("parsing"));
        assert_eq!(tokens("logging"), HashSet::from(["log".to_owned()]));
    }
}
