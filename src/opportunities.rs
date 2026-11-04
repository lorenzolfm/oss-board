//! What nobody is waiting on you for, but you would want to know about.
//!
//! Four sources, as agreed:
//!   new_issue     opened in the last NEW_ISSUE_DAYS, unassigned, nothing linked
//!   unreviewed    non-draft PR with zero approvals that you have not touched
//!   moved         a PR you engaged with that got commits or comments since
//!   needs_review  on the org board's Needs Review column (when enabled)
//!
//! An item that is already an obligation is never an opportunity too, and anything
//! in Passed stays gone. Ranking belongs to the judge; `fallback_rank` is only what
//! the board shows when the judge is unavailable.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::board::CardKey;
use crate::config;
use crate::footprint::{self, Footprint};
use crate::obligations::{Viewer, engaged_at};
use crate::snapshot::{Item, MergeState, PrDetail, ReviewDecision, ReviewState, Snapshot};
use crate::text::plural;
use crate::timestamp::Timestamp;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    NewIssue,
    Unreviewed,
    Moved,
    NeedsReview,
}

impl Source {
    fn priority(self) -> u8 {
        match self {
            Self::NeedsReview => 3,
            Self::Moved => 2,
            Self::Unreviewed | Self::NewIssue => 1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Opportunity<'a> {
    pub item: &'a Item,
    pub sources: Vec<Source>,
    pub note: Option<String>,
    pub age_days: f64,
    pub areas: Vec<String>,
    /// `None`: not read.
    pub merge: Option<MergeState>,
}

impl Opportunity<'_> {
    /// A PR that cannot merge as it stands: draft, conflicting, or red.
    fn is_blocked(&self) -> bool {
        self.item.pr().is_some_and(|pr| pr.draft)
            || self
                .merge
                .is_some_and(|merge| merge.is_conflicting() || merge.is_red())
    }
}

fn since_you(item: &Item, pr: &PrDetail, me: &Viewer, mine_at: Timestamp) -> (bool, usize) {
    let inline = pr.threads.iter().flat_map(|t| &t.comments);
    let others = item
        .comments
        .iter()
        .chain(inline)
        .filter(|c| c.author != me.login && !c.bot && c.at > mine_at)
        .count();
    (pr.head_at.is_some_and(|pushed| pushed > mine_at), others)
}

/// One decimal place, rounded as the DM and the judge have always seen it: on the
/// exact value, not on `x * 10`.
fn one_decimal(value: f64) -> f64 {
    format!("{value:.1}").parse().unwrap_or(value)
}

fn opportunity<'a>(
    item: &'a Item,
    sources: Vec<Source>,
    note: Option<String>,
    now: DateTime<Utc>,
    fp: &Footprint,
) -> Opportunity<'a> {
    let files = item.pr().map_or(&[][..], |pr| pr.files.as_slice());
    Opportunity {
        item,
        sources,
        note,
        age_days: one_decimal(item.created.days_ago(now)),
        areas: footprint::overlap(files, fp),
        merge: None,
    }
}

/// `skip` holds the obligation and claimed keys; `column` the numbers on Needs Review.
pub fn collect<'a>(
    snap: &'a Snapshot,
    me: &Viewer,
    now: DateTime<Utc>,
    skip: &HashSet<CardKey>,
    tombstones: &HashSet<CardKey>,
    fp: &Footprint,
    column: &HashSet<u64>,
) -> Vec<Opportunity<'a>> {
    let is_out = |item: &Item| {
        skip.contains(&item.key)
            || tombstones.contains(&item.key)
            || item.author == me.login
            || item.author_bot
    };
    let mut out = Vec::new();
    for (item, pr) in snap.prs().filter(|(item, _)| !is_out(item)) {
        let mut sources = Vec::new();
        let mut note = None;
        if let Some(mine_at) = engaged_at(item, me) {
            let (pushed, n) = since_you(item, pr, me, mine_at);
            if pushed || n > 0 {
                sources.push(Source::Moved);
                let mut bits = Vec::new();
                if pushed {
                    bits.push("pushed".to_owned());
                }
                if n > 0 {
                    bits.push(format!("{n} comment{}", plural(n)));
                }
                note = Some(format!("{} since you", bits.join(" + ")));
            }
        } else if !pr.draft
            && pr.review_decision != Some(ReviewDecision::ChangesRequested)
            && !pr.reviews.iter().any(|r| r.state == ReviewState::Approved)
        {
            sources.push(Source::Unreviewed);
        }
        if column.contains(&item.number) {
            sources.push(Source::NeedsReview);
        }
        if !sources.is_empty() {
            out.push(opportunity(item, sources, note, now, fp));
        }
    }
    for (item, issue) in snap.issues().filter(|(item, _)| !is_out(item)) {
        let mut sources = Vec::new();
        if item.created.days_ago(now) <= config::NEW_ISSUE_DAYS
            && item.assignees.is_empty()
            && issue.linked_prs.is_empty()
        {
            sources.push(Source::NewIssue);
        }
        if column.contains(&item.number) {
            sources.push(Source::NeedsReview);
        }
        if !sources.is_empty() {
            out.push(opportunity(item, sources, None, now, fp));
        }
    }
    out
}

/// Anything mergeable before anything blocked; within that, maintainers' column,
/// then threads you're already in, then your areas, then smaller and newer.
/// Mechanical on purpose: the DM says so.
pub fn fallback_rank(opps: &mut [Opportunity<'_>], fp: &Footprint) {
    let priority = |o: &Opportunity<'_>| o.sources.iter().map(|s| s.priority()).max().unwrap_or(0);
    let weight = |o: &Opportunity<'_>| o.areas.iter().map(|a| fp.get(a)).sum::<u64>();
    let size = |o: &Opportunity<'_>| o.item.pr().map_or(0, |pr| pr.additions + pr.deletions);
    opps.sort_by(|a, b| {
        a.is_blocked()
            .cmp(&b.is_blocked())
            .then_with(|| priority(b).cmp(&priority(a)))
            .then_with(|| weight(b).cmp(&weight(a)))
            .then_with(|| size(a).cmp(&size(b)))
            .then_with(|| a.age_days.total_cmp(&b.age_days))
    });
}
