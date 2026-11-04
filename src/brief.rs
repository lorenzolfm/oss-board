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
//! It writes to a temporary name and only a checked note is moved into place, so a
//! note under its real name is always a complete one.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use crate::config::Repo;
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
    pub repo: Repo,
    pub number: u64,
    pub url: &'a str,
    pub kind: BriefKind,
    pub today: &'a str,
    pub me: &'a str,
}

pub fn note_path(vault_dir: &Path, repo: Repo, number: u64) -> PathBuf {
    let raw = format!("{}-{number}", repo.name);
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
        .replace("%(repo)s", &target.repo.to_string())
        .replace("%(number)s", &target.number.to_string())
        .replace("%(url)s", target.url)
        .replace("%(path)s", &path.to_string_lossy())
        .replace("%(today)s", target.today)
        .replace("%(me)s", target.me)
}

/// Where the agent writes. A dotfile, so Obsidian and LiveSync leave it alone.
fn draft_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.tmp"))
}

/// A run killed mid-briefing leaves its draft, which must not pass for this agent's work.
fn clear_draft(draft: &Path) -> Result<(), String> {
    match fs::remove_file(draft) {
        Err(err) if err.kind() != ErrorKind::NotFound => Err(err.to_string()),
        _ => Ok(()),
    }
}

fn write_draft(draft: &Path, target: &Target<'_>, token: &str) -> Result<(), String> {
    let out = proc::run(
        Command::new("claude")
            .args(["-p", &prompt(target, draft)])
            .env("GH_TOKEN", token)
            .env("GITHUB_TOKEN", token),
        TIMEOUT,
    )
    .map_err(|failure| failure.to_string())?;
    if !out.success {
        return Err(format!("rc={:?}", out.code));
    }
    let Ok(text) = fs::read_to_string(draft) else {
        return Err("no file written".to_owned());
    };
    if text.trim().chars().count() < MIN_CHARS {
        return Err("too thin, discarded".to_owned());
    }
    Ok(())
}

/// The path of a note just written, or why there is none. A failed attempt leaves
/// nothing behind, so the next run tries again.
pub fn write_briefing(vault_dir: &Path, target: &Target<'_>) -> Result<PathBuf, String> {
    let path = note_path(vault_dir, target.repo, target.number);
    fs::create_dir_all(vault_dir).map_err(|err| err.to_string())?;
    // This subprocess is a full agent with Bash and the `gh` CLI, so the prompt's "do
    // NOT post anything" is guidance, not a boundary -- the token is the boundary.
    // Pin it to the unit's read-only GH_TOKEN explicitly: with the var merely
    // inherited, a manual run leaves it unset and `gh` falls back to the user's own
    // keyring auth, which carries `repo` (write). Refuse instead. The agent could unset
    // it too, which is why the unit hides that login from it.
    let token = std::env::var("GH_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
        .ok_or("no read-only GH_TOKEN; refusing to fall back to user auth")?;
    let draft = draft_path(&path);
    clear_draft(&draft)?;
    let written = write_draft(&draft, target, &token)
        .and_then(|()| fs::rename(&draft, &path).map_err(|err| err.to_string()));
    if written.is_err() {
        // There may be nothing to remove, and the failure to report is the one above.
        let _ = fs::remove_file(&draft);
    }
    written.map(|()| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_is_named_after_the_repo_and_number_and_links_back() {
        let repo = Repo {
            owner: "getfloresta",
            name: "Floresta",
        };
        let path = note_path(Path::new("/vault"), repo, 1298);
        assert_eq!(path, Path::new("/vault/Floresta-1298.md"));
        assert_eq!(wikilink(&path), "[[Floresta-1298]]");
    }

    #[test]
    fn unsafe_characters_cannot_leave_the_vault() {
        let repo = Repo {
            owner: "a",
            name: "we ird~name",
        };
        let path = note_path(Path::new("/vault"), repo, 1);
        assert_eq!(path, Path::new("/vault/we-ird-name-1.md"));
    }
}
