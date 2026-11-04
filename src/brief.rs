//! Write a briefing note when the user claims a card.
//!
//! This is the feature that converts a scarce evening from "orient myself" into
//! "write the fix" -- or, for a review, into "read the diff in the right order". It
//! runs only for items actually chosen, never for candidates, and is capped per run
//! to bound cost.
//!
//! Two prompts: an issue gets the fix-oriented brief, someone else's PR gets a
//! review brief. Your own PR gets none -- you wrote it.
//!
//! The note is authored by a `claude -p` subprocess because it needs judgement
//! (which files matter, what the thread actually concluded) rather than a template.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use crate::proc;
use crate::prompts;

const TIMEOUT: Duration = Duration::from_secs(600);
/// A briefing that is mostly empty is worse than none -- it looks answered.
const MIN_CHARS: usize = 300;

static UNSAFE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^A-Za-z0-9._-]+").expect("a constant pattern compiles"));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BriefKind {
    Issue,
    Review,
}

impl BriefKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Review => "review",
        }
    }
}

/// What to brief.
pub struct Target<'a> {
    pub repo: &'a str,
    pub number: u64,
    pub url: &'a str,
    pub kind: BriefKind,
    pub today: &'a str,
    pub me: &'a str,
}

pub fn note_path(vault_dir: &Path, repo: &str, number: u64) -> PathBuf {
    let name = repo.rsplit('/').next().unwrap_or(repo);
    let raw = format!("{name}-{number}");
    let slug = UNSAFE_RE.replace_all(&raw, "-");
    vault_dir.join(format!("{}.md", slug.trim_matches('-')))
}

pub fn wikilink(path: &Path) -> String {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    format!("[[{stem}]]")
}

fn prompt(target: &Target<'_>, path: &Path) -> String {
    let template = match target.kind {
        BriefKind::Issue => prompts::BRIEF_ISSUE,
        BriefKind::Review => prompts::BRIEF_REVIEW,
    };
    template
        .replace("%(kind)s", target.kind.as_str())
        .replace("%(repo)s", target.repo)
        .replace("%(number)s", &target.number.to_string())
        .replace("%(url)s", target.url)
        .replace("%(path)s", &path.to_string_lossy())
        .replace("%(today)s", target.today)
        .replace("%(me)s", target.me)
}

/// The note's path and whether it was written just now, or why there is none.
pub fn write_briefing(
    vault_dir: &Path,
    target: &Target<'_>,
) -> Result<(PathBuf, &'static str), String> {
    let path = note_path(vault_dir, target.repo, target.number);
    if path.exists() {
        return Ok((path, "exists"));
    }
    fs::create_dir_all(vault_dir).map_err(|err| err.to_string())?;
    // This subprocess is a full agent with Bash and the `gh` CLI, so the prompt's "do
    // NOT post anything" is guidance, not a boundary -- the token is the boundary.
    // Pin it to the unit's read-only GH_TOKEN explicitly: with the var merely
    // inherited, a manual run leaves it unset and `gh` falls back to the user's own
    // keyring auth, which carries `repo` (write). Refuse instead.
    let token = std::env::var("GH_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
        .ok_or("no read-only GH_TOKEN; refusing to fall back to user auth")?;
    let out = proc::run(
        Command::new("claude")
            .args(["-p", &prompt(target, &path)])
            .env("GH_TOKEN", &token)
            .env("GITHUB_TOKEN", &token),
        TIMEOUT,
    )
    .map_err(|failure| failure.to_string())?;
    if !out.success {
        return Err(format!("rc={:?}", out.code));
    }
    let Ok(text) = fs::read_to_string(&path) else {
        return Err("no file written".to_owned());
    };
    if text.trim().chars().count() < MIN_CHARS {
        fs::remove_file(&path).map_err(|err| err.to_string())?;
        return Err("too thin, discarded".to_owned());
    }
    Ok((path, "written"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_is_named_after_the_repo_and_number_and_links_back() {
        let path = note_path(Path::new("/vault"), "getfloresta/Floresta", 1298);
        assert_eq!(path, Path::new("/vault/Floresta-1298.md"));
        assert_eq!(wikilink(&path), "[[Floresta-1298]]");
    }

    #[test]
    fn unsafe_characters_cannot_leave_the_vault() {
        let path = note_path(Path::new("/vault"), "a/we ird~name", 1);
        assert_eq!(path, Path::new("/vault/we-ird-name-1.md"));
    }
}
