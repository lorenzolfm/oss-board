//! What is blocked on you: the six obligation rules, as pure functions.
//!
//! An obligation is found by code alone. The model may add a gist to one but can
//! never hide, drop or reorder it -- that is the whole difference from an
//! opportunity. Each item carries a list of reasons; each reason has
//!
//!   kind  also what a mute remembers
//!   when  how long it has been waiting (ordering), and what re-arms a mute on it:
//!         a newer event, for a state of the PR like a merge conflict a new head, and
//!         for your own unshipped work only going away and coming back
//!   text  the phone-sized explanation

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::board::CardKey;
use crate::config;
use crate::snapshot::{
    CommentKind, Detail, Interdiff, IssueDetail, Item, MergeState, PrDetail, Review, ReviewState,
    Snapshot,
};
use crate::text::{basename, plural, prefix};
use crate::timestamp::Timestamp;

/// Code-review ACKs name the commit they reviewed. "Concept ACK" names none and is
/// not a code review, so it cannot go stale. `\bACK` keeps NACK out.
static ACK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:ut|t|cr|re-?)?ACK\s+([0-9a-f]{7,40})\b").expect("a constant pattern compiles")
});
/// Someone else's approval on your PR is good news, not a request for a reply.
static APPROVAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:(?:re-?|ut|t|cr|concept|approach)\s*)?ACK\b|^\s*LGTM\b")
        .expect("a constant pattern compiles")
});

/// Declared in the order of their names, which is the order a fingerprint lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReasonKind {
    Assigned,
    ChangesRequested,
    CiFailed,
    Conflict,
    Mention,
    PendingReview,
    Reply,
    ReviewRequested,
    StaleAck,
    Thread,
    UnresolvedThread,
    Unshipped,
}

impl ReasonKind {
    const ALL: [Self; 12] = [
        Self::Assigned,
        Self::ChangesRequested,
        Self::CiFailed,
        Self::Conflict,
        Self::Mention,
        Self::PendingReview,
        Self::Reply,
        Self::ReviewRequested,
        Self::StaleAck,
        Self::Thread,
        Self::UnresolvedThread,
        Self::Unshipped,
    ];

    /// A kind no longer known is simply not remembered, so its card comes back.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }

    /// The name written into a card's fingerprint and the gist cache.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Assigned => "assigned",
            Self::ChangesRequested => "changes_requested",
            Self::CiFailed => "ci_failed",
            Self::Conflict => "conflict",
            Self::Mention => "mention",
            Self::PendingReview => "pending_review",
            Self::Reply => "reply",
            Self::ReviewRequested => "review_requested",
            Self::StaleAck => "stale_ack",
            Self::Thread => "thread",
            Self::UnresolvedThread => "unresolved_thread",
            Self::Unshipped => "unshipped",
        }
    }

    /// Whether this reason is an ask by its nature. For the rest, someone has to have
    /// actually asked, which only the gist pass can read.
    fn is_an_ask(self) -> bool {
        matches!(
            self,
            Self::Assigned
                | Self::ChangesRequested
                | Self::CiFailed
                | Self::Conflict
                | Self::ReviewRequested
        )
    }
}

/// How a reason is dated, which is also what re-arms a mute on it.
#[derive(Clone, Debug)]
pub enum When {
    /// Someone has waited since `first`, and `last` is the newest event behind that.
    /// A newer event is a new trigger.
    Events { first: Timestamp, last: Timestamp },
    /// True of this head, `since` a time where one can be seen. A merge conflict has
    /// none. A new head is a new trigger whatever its commit date says: a commit made
    /// last week and pushed today is still a push.
    Head {
        oid: String,
        since: Option<Timestamp>,
    },
    /// True since `since`, which only orders it. That date is your own work on it, which
    /// is no new trigger, so only its first appearance is one.
    Standing { since: Timestamp },
}

#[derive(Clone, Debug)]
pub struct Reason {
    pub kind: ReasonKind,
    pub when: When,
    pub text: String,
}

impl Reason {
    fn at(kind: ReasonKind, at: Timestamp, text: String) -> Self {
        Self::between(kind, at, at, text)
    }

    pub fn standing(kind: ReasonKind, since: Timestamp, text: String) -> Self {
        Self {
            kind,
            when: When::Standing { since },
            text,
        }
    }

    fn between(kind: ReasonKind, first: Timestamp, last: Timestamp, text: String) -> Self {
        Self {
            kind,
            when: When::Events { first, last },
            text,
        }
    }

    fn on_head(kind: ReasonKind, pr: &PrDetail, since: Option<Timestamp>, text: String) -> Self {
        Self {
            kind,
            when: When::Head {
                oid: pr.head.clone(),
                since,
            },
            text,
        }
    }

    fn since(&self) -> Option<Timestamp> {
        match &self.when {
            When::Events { first, .. } => Some(*first),
            When::Head { since, .. } => *since,
            When::Standing { since } => Some(*since),
        }
    }

    fn last(&self) -> Option<Timestamp> {
        match &self.when {
            When::Events { last, .. } => Some(*last),
            When::Head { since, .. } => *since,
            When::Standing { .. } => None,
        }
    }

    fn head(&self) -> Option<&str> {
        match &self.when {
            When::Events { .. } | When::Standing { .. } => None,
            When::Head { oid, .. } => Some(oid),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Obligation<'a> {
    pub item: &'a Item,
    pub reasons: Vec<Reason>,
    /// Context that informs, but is not part of what a mute remembers.
    pub notes: Vec<String>,
    pub interdiff: Option<Interdiff>,
}

impl<'a> Obligation<'a> {
    fn new(item: &'a Item, reasons: Vec<Reason>) -> Self {
        Self {
            item,
            reasons,
            notes: Vec::new(),
            interdiff: None,
        }
    }

    /// Longest-waiting first. A state with no visible start (a conflict) sorts by
    /// when the item last moved, so it neither jumps the queue nor sinks.
    pub fn sort_key(&self) -> Timestamp {
        self.reasons
            .iter()
            .filter_map(Reason::since)
            .min()
            .unwrap_or(self.item.updated)
    }

    pub fn has_kind(&self, kind: ReasonKind) -> bool {
        self.reasons.iter().any(|reason| reason.kind == kind)
    }
}

/// You, as the rules see you.
pub struct Viewer {
    pub login: String,
    mention: Regex,
}

impl Viewer {
    pub fn new(login: &str) -> Self {
        // Consuming the characters around the login stands in for lookaround,
        // which the regex crate does not have. Only whether it matched is used.
        let pattern = format!(r"(?i)(?:^|[^\w/-])@{}(?:$|[^\w-])", regex::escape(login));
        Self {
            login: login.to_owned(),
            mention: Regex::new(&pattern).expect("an escaped login compiles"),
        }
    }

    pub fn is_mentioned_in(&self, body: &str) -> bool {
        self.mention.is_match(body)
    }

    fn is(&self, login: &str) -> bool {
        self.login == login
    }
}

/// One entry of the main conversation: a comment, or the item's own body.
#[derive(Clone, Copy)]
struct Said<'a> {
    author: &'a str,
    bot: bool,
    at: Timestamp,
    body: &'a str,
    /// `None` for the item's own body.
    kind: Option<CommentKind>,
}

/// The main conversation, with the item's own body as its first comment so a
/// "cc @you" in a PR description counts like any other mention.
fn stream(item: &Item) -> impl Iterator<Item = Said<'_>> {
    let head = Said {
        author: &item.author,
        bot: item.author_bot,
        at: item.created,
        body: &item.body,
        kind: None,
    };
    std::iter::once(head).chain(item.comments.iter().map(|comment| Said {
        author: &comment.author,
        bot: comment.bot,
        at: comment.at,
        body: &comment.body,
        kind: Some(comment.kind),
    }))
}

/// Your newest activity of any kind on the item: a comment, a submitted review
/// (even one with no body), an inline reply. Any of them is an answer to whatever
/// came before it.
pub fn engaged_at(item: &Item, me: &Viewer) -> Option<Timestamp> {
    let spoke = stream(item)
        .filter(|said| me.is(said.author))
        .map(|said| said.at);
    let pr_activity = item.pr().into_iter().flat_map(|pr| {
        let reviewed = pr
            .reviews
            .iter()
            .filter(|r| me.is(&r.author) && r.state != ReviewState::Pending)
            .map(|r| r.at);
        let replied = pr
            .threads
            .iter()
            .flat_map(|t| &t.comments)
            .filter(|c| me.is(&c.author))
            .map(|c| c.at);
        reviewed.chain(replied)
    });
    spoke.chain(pr_activity).max()
}

fn norm(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Whether `body` quote-replies one of your comments. GitHub's "Quote reply" pastes
/// your text behind `>` and mentions nobody -- it is how a maintainer answers you
/// without an @, so it counts as one.
fn quotes(body: &str, mine: &[Said<'_>]) -> bool {
    let quoted = body
        .lines()
        .filter_map(|line| line.strip_prefix('>'))
        .collect::<Vec<_>>()
        .join(" ");
    let quoted = norm(&quoted);
    if quoted.chars().count() < 20 {
        return false;
    }
    let probe = prefix(&quoted, 60);
    mine.iter().any(|said| norm(said.body).contains(probe))
}

/// Whether a comment is recent enough to still be asking anything.
fn is_live(at: Timestamp, now: DateTime<Utc>) -> bool {
    at.days_ago(now) <= config::CONVERSATION_TTL_DAYS
}

// ---------------------------------------------------------------- the rules

/// Rule 2: the newest comment isn't yours, and it mentions you, or the item is
/// yours (author or assignee). Busy threads you once commented on do not count --
/// that is the notifications inbox this exists to replace.
fn conversation(item: &Item, me: &Viewer, now: DateTime<Utc>) -> Vec<Reason> {
    let mut out = Vec::new();
    let said: Vec<Said<'_>> = stream(item).collect();
    let mine: Vec<Said<'_>> = said.iter().copied().filter(|s| me.is(s.author)).collect();
    let mut mine_at = engaged_at(item, me);
    if let Detail::Pr(pr) = &item.detail
        && me.is(&item.author)
    {
        mine_at = mine_at.max(pr.pushed_at); // a push answers too
    }
    let assigned_to_me = item.assignees.iter().any(|login| me.is(login));
    if mine_at.is_none()
        && assigned_to_me
        && let Detail::Issue(issue) = &item.detail
    {
        // Chatter from before you were handed it was never addressed to you. Once
        // you have spoken, though, an answer to you stays an answer even if the
        // assignment came later.
        mine_at = issue.assigned_at.get(&me.login).copied();
    }
    let after = said
        .iter()
        .filter(|s| !me.is(s.author) && !s.bot && Some(s.at) > mine_at && is_live(s.at, now));
    let (pinged, others): (Vec<&Said<'_>>, Vec<&Said<'_>>) =
        after.partition(|s| me.is_mentioned_in(s.body) || quotes(s.body, &mine));

    if let (Some(first), Some(newest)) = (pinged.first(), pinged.last()) {
        let verb = if !me.is_mentioned_in(first.body) {
            "replied to you"
        } else if first.body.to_lowercase().contains("review") {
            "asked you to review"
        } else {
            "mentioned you"
        };
        let text = format!("{} {verb} {} ago", first.author, first.at.ago(now));
        out.push(Reason::between(
            ReasonKind::Mention,
            first.at,
            newest.at,
            text,
        ));
    }

    if me.is(&item.author) || assigned_to_me {
        // Verdicts have rules of their own (changes_requested) or ask nothing.
        let replies: Vec<&&Said<'_>> = others
            .iter()
            .filter(|s| match s.kind {
                None => false,
                Some(CommentKind::Review(
                    ReviewState::Approved | ReviewState::ChangesRequested | ReviewState::Dismissed,
                )) => false,
                Some(_) => !APPROVAL_RE.is_match(s.body),
            })
            .collect();
        if let (Some(first), Some(newest)) = (replies.first(), replies.last()) {
            let n = replies.len();
            let text = format!(
                "{n} comment{} since yours, last by {} {} ago",
                plural(n),
                newest.author,
                newest.at.ago(now)
            );
            out.push(Reason::between(
                ReasonKind::Reply,
                first.at,
                newest.at,
                text,
            ));
        }
    }
    out
}

/// Rule 2, inline half: someone answered in a review thread you're in.
fn threads(item: &Item, pr: &PrDetail, me: &Viewer, now: DateTime<Utc>) -> Vec<Reason> {
    if me.is(&item.author) {
        return Vec::new(); // your own PR's threads are rule 3's business
    }
    // Any activity of yours answers what came before it, in this thread or not. Without
    // write access you cannot resolve a thread, so an ACK is all you can say to one.
    let engaged = engaged_at(item, me);
    let mut waiting = Vec::new();
    for thread in &pr.threads {
        let comments: Vec<_> = thread.comments.iter().filter(|c| !c.bot).collect();
        if thread.resolved || !comments.iter().any(|c| me.is(&c.author)) {
            continue;
        }
        let Some(newest) = comments.last().filter(|c| !me.is(&c.author)) else {
            continue;
        };
        let Some(first) = comments
            .iter()
            .find(|c| Some(c.at) > engaged && !me.is(&c.author) && is_live(c.at, now))
        else {
            continue;
        };
        waiting.push((*first, *newest, thread.path.as_str()));
    }
    waiting.sort_by_key(|(first, _, _)| first.at);
    let Some((first, _, path)) = waiting.first() else {
        return Vec::new();
    };
    let n = waiting.len();
    let place = if n == 1 {
        format!(" ({})", basename(path))
    } else {
        String::new()
    };
    let text = format!(
        "{} replied in {n} of your review thread{}{place} {} ago",
        first.author,
        plural(n),
        first.at.ago(now)
    );
    let last = waiting
        .iter()
        .map(|(_, newest, _)| newest.at)
        .fold(first.at, Timestamp::max);
    vec![Reason::between(ReasonKind::Thread, first.at, last, text)]
}

/// Rule 1. Informal asks ("@you can you review") arrive via `conversation`.
fn review_request(item: &Item, pr: &PrDetail, me: &Viewer, now: DateTime<Utc>) -> Vec<Reason> {
    if !pr.requested.iter().any(|login| me.is(login)) {
        return Vec::new();
    }
    let at = pr
        .requested_at
        .get(&me.login)
        .copied()
        .unwrap_or(item.created);
    // GitHub holds the request open until a review is submitted. A comment of yours with
    // nothing pushed since has said your piece, and the next move is the author's.
    let answered = engaged_at(item, me)
        .is_some_and(|mine| mine > at && pr.pushed_at.is_some_and(|head| head <= mine));
    if answered {
        return Vec::new();
    }
    let text = format!("review requested {} ago", at.ago(now));
    vec![Reason::at(ReasonKind::ReviewRequested, at, text)]
}

/// Rule 3: your PR, when there is something for you to do on it.
fn own_pr(
    item: &Item,
    pr: &PrDetail,
    me: &Viewer,
    now: DateTime<Utc>,
    merge: Option<&MergeState>,
) -> Vec<Reason> {
    if !me.is(&item.author) {
        return Vec::new();
    }
    let mut out = Vec::new();
    // Dated by what someone else said. Your own reply and a bot's are no new trigger, and a
    // thread with nothing else in it is dated from when it opened.
    let lasts: Vec<Timestamp> = pr
        .threads
        .iter()
        .filter(|t| !t.resolved)
        .filter_map(|t| {
            let others = t.comments.iter().filter(|c| !me.is(&c.author) && !c.bot);
            let said = others.map(|c| c.at).max();
            said.or_else(|| t.comments.iter().map(|c| c.at).min())
        })
        .collect();
    if let (Some(oldest), Some(newest)) = (lasts.iter().min(), lasts.iter().max()) {
        let n = lasts.len();
        // The ruleset refuses to merge with an open thread, even a typo nit.
        let text = format!(
            "{n} unresolved thread{} block{} merge",
            plural(n),
            if n == 1 { "s" } else { "" }
        );
        out.push(Reason::between(
            ReasonKind::UnresolvedThread,
            *oldest,
            *newest,
            text,
        ));
    }

    // Each reviewer's newest verdict, in the order they first gave one.
    let mut latest: Vec<&Review> = Vec::new();
    for review in &pr.reviews {
        let is_verdict = matches!(
            review.state,
            ReviewState::Approved | ReviewState::ChangesRequested | ReviewState::Dismissed
        );
        if me.is(&review.author) || !is_verdict {
            continue;
        }
        match latest.iter_mut().find(|held| held.author == review.author) {
            Some(held) => *held = review,
            None => latest.push(review),
        }
    }
    let blocking: Vec<&Review> = latest
        .into_iter()
        .filter(|r| r.state == ReviewState::ChangesRequested && Some(r.at) > pr.pushed_at)
        .collect();
    let oldest = blocking.iter().min_by_key(|r| r.at);
    let newest = blocking.iter().map(|r| r.at).max();
    if let (Some(oldest), Some(newest)) = (oldest, newest) {
        let text = format!(
            "{} requested changes {} ago",
            oldest.author,
            oldest.at.ago(now)
        );
        out.push(Reason::between(
            ReasonKind::ChangesRequested,
            oldest.at,
            newest,
            text,
        ));
    }

    if let Some(merge) = merge {
        if merge.is_conflicting() {
            let text = "conflicts with its base, needs a rebase".to_owned();
            out.push(Reason::on_head(ReasonKind::Conflict, pr, None, text));
        }
        if merge.is_red() {
            let text = "CI is red".to_owned();
            out.push(Reason::on_head(ReasonKind::CiFailed, pr, None, text));
        }
    }
    out
}

/// The commit and time of your newest code-review ACK on this PR.
pub fn latest_ack(item: &Item, pr: &PrDetail, me: &Viewer) -> Option<(String, Timestamp)> {
    let inline = pr.threads.iter().flat_map(|t| &t.comments);
    let mut best: Option<(String, Timestamp)> = None;
    for comment in item.comments.iter().chain(inline) {
        if !me.is(&comment.author) {
            continue;
        }
        for found in ACK_RE.captures_iter(&comment.body) {
            if best.as_ref().is_none_or(|(_, at)| comment.at >= *at) {
                best = Some((found[1].to_lowercase(), comment.at));
            }
        }
    }
    best
}

/// Rule 4: you ACKed a commit the PR has since moved past.
fn stale_ack(item: &Item, pr: &PrDetail, me: &Viewer, now: DateTime<Utc>) -> Vec<Reason> {
    if me.is(&item.author) {
        return Vec::new();
    }
    let Some((acked, at)) = latest_ack(item, pr, me) else {
        return Vec::new();
    };
    if pr.head.to_lowercase().starts_with(&acked) {
        return Vec::new();
    }
    let short = prefix(&acked, 7);
    if let Some(pushed) = pr.pushed_at.filter(|pushed| *pushed > at) {
        let text = format!("pushed {} ago, past your ACK {short}", pushed.ago(now));
        return vec![Reason::on_head(
            ReasonKind::StaleAck,
            pr,
            Some(pushed),
            text,
        )];
    }
    // A head older than the ACK means the ACK named some other commit. That is still
    // a fact about this head, and the next push is a new trigger whatever its date.
    let text = format!("your ACK {short} is not the PR head");
    vec![Reason::on_head(ReasonKind::StaleAck, pr, Some(at), text)]
}

/// Rule 5: assigned to you and nothing links a fix yet.
fn assigned(item: &Item, issue: &IssueDetail, me: &Viewer, now: DateTime<Utc>) -> Vec<Reason> {
    if !item.assignees.iter().any(|login| me.is(login)) || issue.has_linked_pr {
        return Vec::new();
    }
    let at = issue
        .assigned_at
        .get(&me.login)
        .copied()
        .unwrap_or(item.created);
    let quiet = item
        .comments
        .iter()
        .map(|c| c.at)
        .fold(item.created, Timestamp::max);
    let mut bits = vec![
        format!("assigned {} ago", at.ago(now)),
        format!("quiet {}", quiet.ago(now)),
    ];
    if let Some(milestone) = &item.milestone
        && let Some(due) = milestone.due_on
    {
        bits.push(format!("{} due {}", milestone.title, due.date()));
    }
    vec![Reason::at(ReasonKind::Assigned, at, bits.join(" · "))]
}

/// Rule 6: a review you started and never submitted. Only its author can see a
/// PENDING review, which is why this works at all.
fn pending_review(pr: &PrDetail, me: &Viewer, now: DateTime<Utc>) -> Vec<Reason> {
    let oldest = pr
        .reviews
        .iter()
        .filter(|r| me.is(&r.author) && r.state == ReviewState::Pending)
        .map(|r| r.at)
        .min();
    let Some(at) = oldest else {
        return Vec::new();
    };
    let text = format!(
        "you started a review {} ago and never submitted it",
        at.ago(now)
    );
    vec![Reason::at(ReasonKind::PendingReview, at, text)]
}

// ---------------------------------------------------------------- assembly

/// Every obligation, oldest wait first. `merge` holds the state of the PRs whose
/// mergeability was asked for -- your own.
pub fn collect<'a>(
    snap: &'a Snapshot,
    me: &Viewer,
    now: DateTime<Utc>,
    merge: &HashMap<CardKey, MergeState>,
) -> Vec<Obligation<'a>> {
    let mut out = Vec::new();
    for item in &snap.items {
        let reasons = match &item.detail {
            Detail::Pr(pr) => {
                let mut reasons = review_request(item, pr, me, now);
                reasons.extend(conversation(item, me, now));
                reasons.extend(threads(item, pr, me, now));
                reasons.extend(own_pr(item, pr, me, now, merge.get(&item.key)));
                reasons.extend(stale_ack(item, pr, me, now));
                reasons.extend(pending_review(pr, me, now));
                reasons
            }
            Detail::Issue(issue) => {
                let mut reasons = conversation(item, me, now);
                reasons.extend(assigned(item, issue, me, now));
                reasons
            }
        };
        if !reasons.is_empty() {
            out.push(Obligation::new(item, reasons));
        }
    }
    out.sort_by_key(Obligation::sort_key);
    out
}

/// Attach a reason to the item's obligation, creating it if needed.
pub fn add_reason<'a>(obs: &mut Vec<Obligation<'a>>, item: &'a Item, reason: Reason) {
    if let Some(ob) = obs.iter_mut().find(|ob| ob.item.key == item.key) {
        ob.reasons.push(reason);
        return;
    }
    obs.push(Obligation::new(item, vec![reason]));
    obs.sort_by_key(Obligation::sort_key);
}

// ---------------------------------------------------------------- mutes

/// What a mute remembers, kept in the card itself so dragging it to Passed carries it
/// along: the newest event behind the card, which kinds of reason it had, and the head
/// any of them belong to.
#[derive(Clone, Debug)]
pub struct Fingerprint {
    pub last: Option<Timestamp>,
    pub kinds: BTreeSet<ReasonKind>,
    pub head: Option<String>,
}

impl Fingerprint {
    pub fn last_text(&self) -> String {
        self.last.map_or(String::new(), |last| last.to_string())
    }

    pub fn kinds_text(&self) -> String {
        let kinds: Vec<&str> = self.kinds.iter().map(|kind| kind.as_str()).collect();
        kinds.join(",")
    }
}

pub fn fingerprint(ob: &Obligation<'_>) -> Fingerprint {
    Fingerprint {
        last: ob.reasons.iter().filter_map(Reason::last).max(),
        kinds: ob.reasons.iter().map(|reason| reason.kind).collect(),
        head: ob.reasons.iter().find_map(Reason::head).map(str::to_owned),
    }
}

/// The newest event behind each kind of reason, in the order `kinds_text` lists them.
/// `Seen` compares times per kind, so anything keyed on what may be new has to as well: one
/// time across them lets a head dated ahead hide a newer ping.
pub fn last_per_kind(ob: &Obligation<'_>) -> String {
    let mut newest: BTreeMap<ReasonKind, Option<Timestamp>> = BTreeMap::new();
    for reason in &ob.reasons {
        let held = newest.entry(reason.kind).or_default();
        *held = (*held).max(reason.last());
    }
    let times = newest
        .values()
        .map(|at| at.map_or(String::new(), |at| at.to_string()));
    times.collect::<Vec<_>>().join(",")
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct SeenKind {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    heads: BTreeSet<String>,
}

/// What you have seen of an item: for each kind of reason, the newest event you saw it
/// with and the heads you saw it on. A reason this does not cover is a new trigger. It is
/// what every muted card for an item adds up to, and what `last_dm.json` keeps per item.
/// A fingerprint whose time does not parse (a hand-edited card) counts as older than
/// everything.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(
    from = "BTreeMap<String, SeenKind>",
    into = "BTreeMap<&'static str, SeenKind>"
)]
pub struct Seen(BTreeMap<ReasonKind, SeenKind>);

/// A kind no longer known is simply not remembered, so its card comes back.
impl From<BTreeMap<String, SeenKind>> for Seen {
    fn from(wire: BTreeMap<String, SeenKind>) -> Self {
        let known = wire
            .into_iter()
            .filter_map(|(kind, seen)| Some((ReasonKind::parse(&kind)?, seen)));
        Self(known.collect())
    }
}

impl From<Seen> for BTreeMap<&'static str, SeenKind> {
    fn from(seen: Seen) -> Self {
        let kinds = seen.0.into_iter();
        kinds.map(|(kind, seen)| (kind.as_str(), seen)).collect()
    }
}

impl Seen {
    /// A record from before kinds were named, which says only how far you had read.
    /// It names no head, so a state of the PR reads as new once.
    pub fn read_up_to(last: Timestamp) -> Self {
        let mut seen = Self::default();
        for kind in ReasonKind::ALL {
            seen.remember(kind, Some(last), None);
        }
        seen
    }

    pub fn remember(&mut self, kind: ReasonKind, last: Option<Timestamp>, head: Option<&str>) {
        let seen = self.0.entry(kind).or_default();
        seen.last = seen.last.max(last);
        seen.heads.extend(head.map(str::to_owned));
    }

    /// Only what this says about these kinds, if anything.
    fn about(&self, kinds: &[ReasonKind]) -> Option<Self> {
        let kept = self.0.iter().filter(|(kind, _)| kinds.contains(kind));
        let kept: BTreeMap<_, _> = kept.map(|(kind, seen)| (*kind, seen.clone())).collect();
        (!kept.is_empty()).then_some(Self(kept))
    }

    /// Remember a reason as `covers` will test it.
    fn saw(&mut self, reason: &Reason) {
        match &reason.when {
            When::Events { last, .. } => self.remember(reason.kind, Some(*last), None),
            When::Head { oid, .. } => self.remember(reason.kind, None, Some(oid)),
            When::Standing { .. } => self.remember(reason.kind, None, None),
        }
    }

    fn covers(&self, reason: &Reason) -> bool {
        let Some(seen) = self.0.get(&reason.kind) else {
            return false;
        };
        match &reason.when {
            When::Events { last, .. } => Some(*last) <= seen.last,
            When::Head { oid, .. } => seen.heads.contains(oid),
            When::Standing { .. } => true,
        }
    }
}

/// A mute holds until a NEW trigger: a reason kind that wasn't there when you muted
/// it, a timed event newer than the newest one you saw, or a new head for a state of
/// the PR. General activity on the item never re-arms it -- only what would have made
/// it an obligation in the first place.
pub fn is_muted(ob: &Obligation<'_>, mute: Option<&Seen>) -> bool {
    mute.is_some_and(|mute| ob.reasons.iter().all(|reason| mute.covers(reason)))
}

// ---------------------------------------------------------------- the DM

/// The reasons that are a new trigger since the last DM, by the test a mute uses.
fn new_reasons<'o>(ob: &'o Obligation<'_>, told: Option<&Seen>) -> Vec<&'o Reason> {
    let is_new = |reason: &&Reason| !told.is_some_and(|told| told.covers(reason));
    ob.reasons.iter().filter(is_new).collect()
}

/// Everything the DM may interrupt you with, in the order of `obs`: a new trigger since
/// the last DM that is an ask, either by its nature or on the model's reading. An item
/// the model has no verdict on is kept, because a dropped ask cannot be seen to be
/// missing and a capped message can afford a false alarm.
pub fn new_asks<'o, 'a>(
    obs: &'o [Obligation<'a>],
    seen: &BTreeMap<CardKey, Seen>,
    asks: impl Fn(&CardKey) -> Option<bool>,
) -> Vec<&'o Obligation<'a>> {
    obs.iter()
        .filter(|ob| {
            let new = new_reasons(ob, seen.get(&ob.item.key));
            !new.is_empty()
                && (new.iter().any(|reason| reason.kind.is_an_ask())
                    || asks(&ob.item.key).unwrap_or(true))
        })
        .collect()
}

/// What to remember once a DM is delivered. `obs` is every obligation, muted ones too:
/// an item leaves this record only by clearing. A mute is not a clearing, and neither is
/// a kind this run could not read, so what was remembered of those stays. An ask `held`
/// back by the cap keeps what was remembered of it, so it stays new for the next DM.
pub fn seen_after(
    obs: &[Obligation<'_>],
    held: &[&Obligation<'_>],
    seen: &BTreeMap<CardKey, Seen>,
    unread: &[ReasonKind],
) -> BTreeMap<CardKey, Seen> {
    let mut after: BTreeMap<CardKey, Seen> = seen
        .iter()
        .filter_map(|(key, told)| Some((key.clone(), told.about(unread)?)))
        .collect();
    for ob in obs {
        let key = &ob.item.key;
        if held.iter().any(|held| held.item.key == *key) {
            after.remove(key);
            after.extend(seen.get(key).map(|told| (key.clone(), told.clone())));
            continue;
        }
        let told = after.entry(key.clone()).or_default();
        ob.reasons.iter().for_each(|reason| told.saw(reason));
    }
    after
}

#[cfg(test)]
mod tests {
    //! The obligation rules, through `collect` -- the one entry point the run uses.
    //!
    //! Several cases replay what the live Floresta data showed while this was
    //! built: a quote-reply with no @ (#1336), an answer that predated the
    //! assignment (#167), a mention already answered by an inline review (#1075),
    //! and the busy PR you once commented on that must NOT count (#1161's
    //! conversation).

    use super::*;
    use crate::snapshot::{Ci, Comment, Mergeability};
    use crate::testkit::{
        ME, bot, c, day, floresta, issue, key, me, now, pr, review, snap, thread,
    };

    fn merge_of(
        number: u64,
        mergeable: Mergeability,
        ci: Option<Ci>,
    ) -> HashMap<CardKey, MergeState> {
        HashMap::from([(floresta(number), MergeState { mergeable, ci })])
    }

    #[test]
    fn a_fingerprint_lists_kinds_by_name_so_a_gist_cache_still_matches() {
        let viewer = me();
        let snapshot = snap(
            vec![pr(1).requested(ME, "2026-10-08T17:14:49Z").comments(vec![c(
                "bob",
                3,
                &at_me("@me ping"),
            )])],
            vec![],
        );
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        assert_eq!(
            fingerprint(&obs[0]).kinds_text(),
            "mention,review_requested"
        );
        for kind in ReasonKind::ALL {
            assert_eq!(ReasonKind::parse(kind.as_str()), Some(kind));
        }
    }

    fn kinds_with(item: Item, merge: &HashMap<CardKey, MergeState>) -> Vec<&'static str> {
        let viewer = me();
        let snapshot = match item.detail {
            Detail::Pr(_) => snap(vec![item], vec![]),
            Detail::Issue(_) => snap(vec![], vec![item]),
        };
        let obs = collect(&snapshot, &viewer, now(), merge);
        let mut kinds: Vec<_> = obs
            .iter()
            .flat_map(|ob| ob.reasons.iter().map(|r| r.kind.as_str()))
            .collect();
        kinds.sort_unstable();
        kinds
    }

    fn check(cases: Vec<(&str, Item, Vec<&str>)>) {
        for (name, item, want) in cases {
            assert_eq!(kinds_with(item, &HashMap::new()), want, "{name}");
        }
    }

    fn at_me(text: &str) -> String {
        text.replace("@me", &format!("@{ME}"))
    }

    #[test]
    fn what_counts_as_a_conversation_waiting_on_you() {
        check(vec![
            (
                "review requested",
                pr(1).requested(ME, &day(5).to_string()),
                vec!["review_requested"],
            ),
            (
                "@-mention after your last word",
                pr(2).comments(vec![c(ME, 10, "a comment"), c("bob", 3, &at_me("@me thoughts?"))]),
                vec!["mention"],
            ),
            (
                "mention answered by an inline reply since (#1075)",
                pr(3)
                    .comments(vec![c("bob", 9, &at_me("cc @me"))])
                    .threads(vec![thread(vec![c(ME, 4, "nit")])]),
                vec![],
            ),
            (
                "mention answered by a review with no body",
                pr(4)
                    .comments(vec![c("bob", 9, &at_me("@me ping"))])
                    .reviews(vec![review(ME, 4, ReviewState::Commented, "")]),
                vec![],
            ),
            (
                "quote-reply without an @ (#1336)",
                pr(5).comments(vec![
                    c(ME, 5, "Imagine a scenario where we are still in DownloadingHeaders and a peer stalls"),
                    c("davidson", 1, "> Imagine a scenario where we are still in DownloadingHeaders\n\nYes, fixed."),
                ]),
                vec!["mention"],
            ),
            (
                "busy PR you once commented on is not an obligation",
                pr(6).comments(vec![
                    c(ME, 40, "a comment"),
                    c("bob", 30, "a comment"),
                    c("carol", 20, "a comment"),
                    c("bob", 2, "a comment"),
                ]),
                vec![],
            ),
            (
                "mention of a longer login is not you",
                pr(7).comments(vec![c("bob", 2, &format!("@{ME}-bot please run"))]),
                vec![],
            ),
            (
                "bots never wait on you",
                pr(8)
                    .author(ME)
                    .pushed_at(day(10))
                    .comments(vec![bot("ci[bot]", 2, "coverage report")]),
                vec![],
            ),
            (
                "PR description cc's you",
                pr(9)
                    .created(day(3))
                    .body(&at_me("Fixes a stall. cc @me for review")),
                vec!["mention"],
            ),
        ]);
    }

    #[test]
    fn an_informal_ask_reads_as_one() {
        let viewer = me();
        let snapshot = snap(
            vec![pr(9).created(day(3)).body(&at_me("cc @me for review"))],
            vec![],
        );
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let text = &obs[0].reasons[0].text;
        assert!(text.contains("asked you to review"), "{text}");
    }

    #[test]
    fn assigned_issues() {
        check(vec![
            (
                "an answer to you stays one after a later assignment (#167)",
                issue(167).assigned(ME, day(8)).comments(vec![
                    c(ME, 40, "What's the scope?"),
                    c("csgui", 9, "Run a node and re-level logs"),
                ]),
                vec!["assigned", "reply"],
            ),
            (
                "chatter from before you were handed it is not addressed to you",
                issue(11)
                    .assigned(ME, day(5))
                    .comments(vec![c("bob", 20, "a comment"), c("carol", 10, "a comment")]),
                vec!["assigned"],
            ),
            (
                "an assigned issue with a fix linked is not idle",
                issue(12).assigned(ME, day(5)).linked_pr(),
                vec![],
            ),
            (
                "someone else's issue you commented on is not yours",
                issue(13).comments(vec![c(ME, 10, "a comment"), c("bob", 2, "a comment")]),
                vec![],
            ),
        ]);
    }

    #[test]
    fn your_own_prs() {
        let own = |n| pr(n).author(ME);
        check(vec![
            (
                "a comment since your push",
                own(20)
                    .pushed_at(day(10))
                    .comments(vec![c("bob", 3, "a comment")]),
                vec!["reply"],
            ),
            (
                "a push answers the comment before it",
                own(21)
                    .pushed_at(day(2))
                    .comments(vec![c("bob", 3, "a comment")]),
                vec![],
            ),
            (
                "an ACK is good news, not a request",
                own(22).pushed_at(day(10)).comments(vec![c(
                    "bob",
                    3,
                    "ACK 1a2b3c4d\n\nran the tests",
                )]),
                vec![],
            ),
            (
                "an APPROVED review asks nothing",
                own(23).pushed_at(day(10)).reviews(vec![review(
                    "bob",
                    3,
                    ReviewState::Approved,
                    "ACK ffffff1",
                )]),
                vec![],
            ),
            (
                "changes requested since your push",
                own(24).pushed_at(day(10)).reviews(vec![review(
                    "bob",
                    3,
                    ReviewState::ChangesRequested,
                    "needs a test",
                )]),
                vec!["changes_requested"],
            ),
            (
                "changes requested before your latest push is answered",
                own(25).pushed_at(day(2)).reviews(vec![review(
                    "bob",
                    3,
                    ReviewState::ChangesRequested,
                    "needs a test",
                )]),
                vec![],
            ),
            (
                "an unresolved thread blocks merge, whoever spoke last",
                own(26).pushed_at(day(2)).threads(vec![thread(vec![c(
                    "jaoleal",
                    30,
                    "typo: is_backfiling",
                )])]),
                vec!["unresolved_thread"],
            ),
            (
                "a resolved thread does not",
                own(27).threads(vec![thread(vec![c("bob", 3, "a comment")]).resolved()]),
                vec![],
            ),
            (
                "a thread with only your own words in it blocks merge too",
                own(35).threads(vec![thread(vec![c(ME, 3, "a comment")])]),
                vec!["unresolved_thread"],
            ),
        ]);
    }

    #[test]
    fn conflicts_and_red_ci_come_from_merge_state() {
        let merge = merge_of(28, Mergeability::Conflicting, Some(Ci::Failure));
        assert_eq!(
            kinds_with(pr(28).author(ME).draft(), &merge),
            ["ci_failed", "conflict"]
        );
    }

    /// Two repos' #28 are two PRs, so one's state says nothing about the other.
    #[test]
    fn a_merge_state_belongs_to_the_item_not_its_number() {
        let conflict = MergeState {
            mergeable: Mergeability::Conflicting,
            ci: None,
        };
        let merge = HashMap::from([(key("cashubtc/cdk#28"), conflict)]);
        assert!(kinds_with(pr(28).author(ME), &merge).is_empty());
        let on_cdk = pr(28).author(ME).in_repo("cashubtc/cdk");
        assert_eq!(kinds_with(on_cdk, &merge), ["conflict"]);
    }

    #[test]
    fn a_pr_with_nothing_wrong_or_still_being_computed_asks_nothing() {
        for merge in [
            merge_of(29, Mergeability::Mergeable, None),
            merge_of(29, Mergeability::Unknown, Some(Ci::Success)),
            merge_of(29, Mergeability::Mergeable, Some(Ci::Pending)),
        ] {
            assert!(
                kinds_with(pr(29).author(ME), &merge).is_empty(),
                "{merge:?}"
            );
        }
    }

    #[test]
    fn review_threads_on_others_prs() {
        check(vec![
            (
                "someone answered in your thread",
                pr(30).threads(vec![thread(vec![c(ME, 9, "why?"), c("bob", 2, "because")])]),
                vec!["thread"],
            ),
            (
                "you had the last word",
                pr(31).threads(vec![thread(vec![
                    c("bob", 9, "a comment"),
                    c(ME, 2, "a comment"),
                ])]),
                vec![],
            ),
            (
                "a resolved thread is done",
                pr(32).threads(vec![
                    thread(vec![c(ME, 9, "a comment"), c("bob", 2, "a comment")]).resolved(),
                ]),
                vec![],
            ),
            (
                "a reply in the same second as yours cannot be told from before it",
                pr(34).threads(vec![thread(vec![c(ME, 2, "why?"), c("bob", 2, "because")])]),
                vec![],
            ),
            (
                "a thread you are not in is not yours",
                pr(33).threads(vec![thread(vec![
                    c("bob", 9, "a comment"),
                    c("carol", 2, "a comment"),
                ])]),
                vec![],
            ),
        ]);
    }

    /// Without write access you cannot resolve a thread, so these would never clear.
    #[test]
    fn a_later_word_of_yours_answers_a_thread_you_cannot_resolve() {
        let replied = |n| pr(n).threads(vec![thread(vec![c(ME, 9, "nit"), c("bob", 5, "done")])]);
        check(vec![
            (
                "an ACK of the head since the reply (#1417)",
                replied(70).reviews(vec![review(ME, 3, ReviewState::Approved, "ACK fffffff")]),
                vec![],
            ),
            (
                "a comment elsewhere on the PR since the reply (#1412)",
                replied(71).comments(vec![c(ME, 3, "one more thing")]),
                vec![],
            ),
            (
                "a word of yours from before the reply answers nothing",
                replied(72).reviews(vec![review(ME, 7, ReviewState::Commented, "some notes")]),
                vec!["thread"],
            ),
        ]);
    }

    #[test]
    fn a_review_request_you_have_answered_short_of_a_review() {
        let requested = |n| pr(n).requested(ME, &day(10).to_string());
        let said = || vec![c(ME, 5, "what about the pokes?")];
        check(vec![
            (
                "a comment with nothing pushed since (#1336)",
                requested(73).comments(said()),
                vec![],
            ),
            (
                "a push since your comment",
                requested(74).pushed_at(day(2)).comments(said()),
                vec!["review_requested"],
            ),
            (
                "asked again since your comment",
                pr(75).requested(ME, &day(2).to_string()).comments(said()),
                vec!["review_requested"],
            ),
            (
                "a comment from before the request",
                requested(76).comments(vec![c(ME, 15, "a comment")]),
                vec!["review_requested"],
            ),
        ]);
    }

    #[test]
    fn a_conversation_left_quiet_stops_asking_and_a_state_does_not() {
        check(vec![
            (
                "a mention nobody followed up",
                issue(80).comments(vec![c("bob", 15, &at_me("@me thoughts?"))]),
                vec![],
            ),
            (
                "a mention still inside the window",
                issue(81).comments(vec![c("bob", 13, &at_me("@me thoughts?"))]),
                vec!["mention"],
            ),
            (
                "a reply in your review thread",
                pr(82).threads(vec![thread(vec![
                    c(ME, 30, "why?"),
                    c("bob", 15, "because"),
                ])]),
                vec![],
            ),
            (
                "a reply on your own issue",
                issue(83)
                    .author(ME)
                    .comments(vec![c("bob", 15, "a comment")]),
                vec![],
            ),
            (
                "a formal review request",
                pr(84).requested(ME, &day(60).to_string()),
                vec!["review_requested"],
            ),
            (
                "an assignment",
                issue(85).assigned(ME, day(60)),
                vec!["assigned"],
            ),
            (
                "an unresolved thread on your own PR",
                pr(86)
                    .author(ME)
                    .pushed_at(day(40))
                    .threads(vec![thread(vec![c("bob", 30, "a nit")])]),
                vec!["unresolved_thread"],
            ),
        ]);
    }

    #[test]
    fn a_new_ping_brings_it_back_dated_from_itself() {
        let viewer = me();
        let snapshot = snap(
            vec![],
            vec![issue(87).comments(vec![
                c("bob", 40, &at_me("@me thoughts?")),
                c("carol", 2, &at_me("@me still?")),
            ])],
        );
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        assert_eq!(obs[0].reasons[0].text, "carol mentioned you 2d ago");
    }

    #[test]
    fn stale_acks_and_unsubmitted_reviews() {
        let head = "abcdef0123456789abcdef0123456789abcdef01";
        check(vec![
            (
                "an ACK the PR moved past",
                pr(40)
                    .with_head(head)
                    .pushed_at(day(2))
                    .comments(vec![c(ME, 5, "ACK 1234567")]),
                vec!["stale_ack"],
            ),
            (
                "an ACK of the current head",
                pr(41)
                    .with_head(head)
                    .comments(vec![c(ME, 5, "utACK abcdef0")]),
                vec![],
            ),
            (
                "Concept ACK names no commit",
                pr(42)
                    .with_head(head)
                    .comments(vec![c(ME, 5, "Concept ACK")]),
                vec![],
            ),
            (
                "NACK is not an ACK",
                pr(43)
                    .with_head(head)
                    .comments(vec![c(ME, 5, "NACK 1234567, see above")]),
                vec![],
            ),
            (
                "only your newest ACK counts",
                pr(44)
                    .with_head(head)
                    .comments(vec![c(ME, 9, "ACK 1234567"), c(ME, 2, "re-ACK abcdef0")]),
                vec![],
            ),
            (
                "a review you never submitted",
                pr(45).reviews(vec![review(ME, 6, ReviewState::Pending, "")]),
                vec!["pending_review"],
            ),
        ]);
    }

    #[test]
    fn longest_wait_first_and_a_bare_state_sorts_by_when_the_item_last_moved() {
        let viewer = me();
        let snapshot = snap(
            vec![
                pr(50).requested(ME, &day(2).to_string()),
                pr(51).requested(ME, &day(40).to_string()),
                pr(52).author(ME).updated(day(5)),
            ],
            vec![issue(53).comments(vec![c("bob", 12, &at_me("@me"))])],
        );
        let merge = merge_of(52, Mergeability::Conflicting, None);
        let order: Vec<u64> = collect(&snapshot, &viewer, now(), &merge)
            .iter()
            .map(|ob| ob.item.number())
            .collect();
        assert_eq!(order, [51, 53, 52, 50]);
    }

    fn mute_of(ob: &Obligation<'_>) -> Seen {
        let fp = fingerprint(ob);
        let mut mute = Seen::default();
        for kind in fp.kinds {
            mute.remember(kind, fp.last, fp.head.as_deref());
        }
        mute
    }

    #[test]
    fn a_mute_holds_until_a_new_trigger_not_general_activity() {
        let viewer = me();
        let empty = HashMap::new();
        let base = pr(60).comments(vec![
            c(ME, 10, "a comment"),
            c("bob", 5, &at_me("@me can you look?")),
        ]);
        let snapshot = snap(vec![base.clone()], vec![]);
        let mute = mute_of(&collect(&snapshot, &viewer, now(), &empty)[0]);

        let is_muted_after = |item: Item| {
            let snapshot = snap(vec![item], vec![]);
            let obs = collect(&snapshot, &viewer, now(), &empty);
            is_muted(&obs[0], Some(&mute))
        };
        assert!(
            is_muted_after(base.clone().updated(day(0)).comments(vec![c(
                "carol",
                1,
                "+1 from me"
            )])),
            "unrelated comments keep it muted"
        );
        assert!(
            !is_muted_after(
                base.clone()
                    .comments(vec![c("carol", 1, &at_me("@me also this"))])
            ),
            "a newer mention re-arms it"
        );
        assert!(
            !is_muted_after(base.clone().requested(ME, &day(1).to_string())),
            "a new kind of reason re-arms it"
        );
    }

    /// A push answers whatever the state was. The commit date of what was pushed says
    /// nothing about that: this one predates what you muted.
    #[test]
    fn a_muted_state_of_your_pr_holds_on_its_head_and_a_push_rearms_it() {
        let viewer = me();
        for merge in [
            merge_of(61, Mergeability::Conflicting, None),
            merge_of(61, Mergeability::Mergeable, Some(Ci::Failure)),
        ] {
            let before = pr(61)
                .author(ME)
                .with_head(&"a".repeat(40))
                .pushed_at(day(5))
                .threads(vec![thread(vec![c("bob", 2, "a nit")])]);
            let snapshot = snap(vec![before.clone()], vec![]);
            let obs = collect(&snapshot, &viewer, now(), &merge);
            let mute = mute_of(&obs[0]);
            assert!(is_muted(&obs[0], Some(&mute)), "same head: {merge:?}");

            let pushed = before.with_head(&"b".repeat(40)).pushed_at(day(4));
            let snapshot = snap(vec![pushed], vec![]);
            let obs = collect(&snapshot, &viewer, now(), &merge);
            assert!(!is_muted(&obs[0], Some(&mute)), "new head: {merge:?}");
        }
    }

    #[test]
    fn a_muted_stale_ack_rearms_on_the_next_push() {
        let viewer = me();
        let acked = |head: &str, pushed| {
            pr(63)
                .with_head(&head.repeat(40))
                .pushed_at(pushed)
                .comments(vec![c(ME, 9, "ACK 1234567")])
        };
        let snapshot = snap(vec![acked("a", day(3))], vec![]);
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let mute = mute_of(&obs[0]);
        assert!(is_muted(&obs[0], Some(&mute)));

        let snapshot = snap(vec![acked("b", day(4))], vec![]);
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        assert!(
            !is_muted(&obs[0], Some(&mute)),
            "an older commit, pushed later"
        );
    }

    /// These commits are dated before the ACK and pushed after it, so their dates say
    /// nothing about the push. The first head is either side of the ACK, because which
    /// side decides how the reason is dated.
    #[test]
    fn a_push_dated_before_your_ack_still_rearms_a_muted_stale_ack() {
        let viewer = me();
        let acked = |head: &str, pushed| {
            pr(64)
                .with_head(&head.repeat(40))
                .pushed_at(pushed)
                .comments(vec![c(ME, 9, "ACK 1234567")])
        };
        for (first, next) in [(day(12), day(11)), (day(3), day(12))] {
            let snapshot = snap(vec![acked("a", first)], vec![]);
            let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
            let mute = mute_of(&obs[0]);
            assert!(is_muted(&obs[0], Some(&mute)), "the head you muted");

            let snapshot = snap(vec![acked("b", next)], vec![]);
            let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
            assert!(!is_muted(&obs[0], Some(&mute)), "a new head");
        }
    }

    #[test]
    fn a_hand_edited_fingerprint_brings_the_obligation_back() {
        let viewer = me();
        let snapshot = snap(vec![pr(62).requested(ME, &day(3).to_string())], vec![]);
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let mut mute = Seen::default();
        mute.remember(
            ReasonKind::ReviewRequested,
            Timestamp::parse("not a time"),
            None,
        );
        assert!(!is_muted(&obs[0], Some(&mute)));
    }

    // ------------------------------------------------------------ the DM

    /// What a delivered DM leaves behind when nothing was held back.
    fn seen_of(obs: &[Obligation<'_>]) -> BTreeMap<CardKey, Seen> {
        seen_after(obs, &[], &BTreeMap::new(), &[])
    }

    /// The numbers the DM would carry, given a model that always says the same thing.
    fn asking(
        item: Item,
        merge: &HashMap<CardKey, MergeState>,
        seen: &BTreeMap<CardKey, Seen>,
        verdict: Option<bool>,
    ) -> Vec<u64> {
        let viewer = me();
        let snapshot = snap(vec![item], vec![]);
        let obs = collect(&snapshot, &viewer, now(), merge);
        new_asks(&obs, seen, |_| verdict)
            .iter()
            .map(|ob| ob.item.number())
            .collect()
    }

    fn seen_on(item: Item, merge: &HashMap<CardKey, MergeState>) -> BTreeMap<CardKey, Seen> {
        let viewer = me();
        let snapshot = snap(vec![item], vec![]);
        seen_of(&collect(&snapshot, &viewer, now(), merge))
    }

    #[test]
    fn what_the_last_dm_saw_stays_quiet_until_a_new_trigger() {
        let empty = HashMap::new();
        let base = pr(90).comments(vec![c("bob", 5, &at_me("@me ping"))]);
        let seen = seen_on(base.clone(), &empty);
        assert!(asking(base.clone(), &empty, &seen, Some(true)).is_empty());
        assert!(
            asking(
                base.clone().comments(vec![c("carol", 4, "+1 from me")]),
                &empty,
                &seen,
                Some(true)
            )
            .is_empty(),
            "general activity"
        );
        assert_eq!(
            asking(
                base.comments(vec![c("carol", 2, &at_me("@me also this"))]),
                &empty,
                &seen,
                Some(true)
            ),
            [90],
            "a newer mention"
        );
    }

    /// The newest time alone cannot tell these apart: neither of them moves it.
    #[test]
    fn a_new_kind_or_a_new_head_is_new_though_the_time_has_not_moved() {
        let clean = HashMap::new();
        let conflict = merge_of(91, Mergeability::Conflicting, None);
        let mine = pr(91)
            .author(ME)
            .pushed_at(day(5))
            .threads(vec![thread(vec![c("bob", 2, "a nit")])]);
        let seen = seen_on(mine.clone(), &clean);
        assert_eq!(asking(mine.clone(), &conflict, &seen, Some(false)), [91]);

        let seen = seen_on(mine.clone(), &conflict);
        assert!(asking(mine.clone(), &conflict, &seen, Some(false)).is_empty());
        let pushed = mine.with_head(&"b".repeat(40));
        assert_eq!(asking(pushed, &conflict, &seen, Some(false)), [91]);
    }

    /// #167 loses its mention to the expiry and stays assigned. Nothing happened.
    #[test]
    fn a_reason_going_away_is_not_new() {
        let viewer = me();
        let mentioned =
            issue(92)
                .assigned(ME, day(8))
                .comments(vec![c("bob", 5, &at_me("@me scope?"))]);
        let snapshot = snap(vec![], vec![mentioned]);
        let seen = seen_of(&collect(&snapshot, &viewer, now(), &HashMap::new()));

        let quiet = issue(92).assigned(ME, day(8));
        let snapshot = snap(vec![], vec![quiet]);
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        assert!(new_asks(&obs, &seen, |_| Some(true)).is_empty());
    }

    #[test]
    fn the_model_decides_only_what_is_not_an_ask_by_its_nature() {
        let empty = HashMap::new();
        let none = BTreeMap::new();
        let mention = || pr(93).comments(vec![c("bob", 5, &at_me("@me thanks!"))]);
        assert_eq!(asking(mention(), &empty, &none, Some(true)), [93]);
        assert!(asking(mention(), &empty, &none, Some(false)).is_empty());
        assert_eq!(
            asking(mention(), &empty, &none, None),
            [93],
            "no verdict keeps it"
        );

        let requested = pr(94).requested(ME, &day(5).to_string());
        assert_eq!(asking(requested.clone(), &empty, &none, Some(false)), [94]);

        let seen = seen_on(requested.clone(), &empty);
        let then_thanked = requested.comments(vec![c("bob", 2, &at_me("@me thanks!"))]);
        assert!(
            asking(then_thanked, &empty, &seen, Some(false)).is_empty(),
            "only what is new counts: the request was already seen"
        );
    }

    #[test]
    fn an_ask_held_back_by_the_cap_stays_new_for_the_next_dm() {
        let viewer = me();
        let pinged = |comments| pr(99).comments(comments);
        let first = c("bob", 8, &at_me("@me first"));
        let told = seen_on(pinged(vec![first.clone()]), &HashMap::new());
        let snapshot = snap(
            vec![
                pr(95).requested(ME, &day(9).to_string()),
                pr(96).requested(ME, &day(7).to_string()),
                pinged(vec![first, c("bob", 2, &at_me("@me second"))]),
            ],
            vec![],
        );
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let waiting = new_asks(&obs, &told, |_| None);
        let (shown, held) = waiting.split_at(1);
        assert_eq!(shown[0].item.number(), 95);

        let seen = seen_after(&obs, held, &told, &[]);
        let next: Vec<u64> = new_asks(&obs, &seen, |_| None)
            .iter()
            .map(|ob| ob.item.number())
            .collect();
        assert_eq!(next, [99, 96], "new when held, with a record or without");
    }

    /// What the next DM says about a conflict you were told about on this PR, after a morning
    /// that read `between` with these kinds unread.
    fn conflict_after(
        between: Item,
        between_merge: &HashMap<CardKey, MergeState>,
        unread: &[ReasonKind],
        next: Item,
    ) -> Vec<u64> {
        let viewer = me();
        let conflict = merge_of(91, Mergeability::Conflicting, None);
        let told = seen_on(next.clone(), &conflict);
        let snapshot = snap(vec![between], vec![]);
        let obs = collect(&snapshot, &viewer, now(), between_merge);
        let seen = seen_after(&obs, &[], &told, unread);
        asking(next, &conflict, &seen, Some(false))
    }

    /// The whole run proves the plain case. These are what the record must tell apart.
    #[test]
    fn only_clearing_forgets_what_you_were_told() {
        let head = "a".repeat(40);
        let mine = pr(91).author(ME).with_head(&head).pushed_at(day(5));
        let listed = mine
            .clone()
            .threads(vec![thread(vec![c("bob", 2, "a nit")])]);
        let computing = merge_of(91, Mergeability::Unknown, None);
        let clean = merge_of(91, Mergeability::Mergeable, None);
        let unread = [ReasonKind::Conflict];

        assert!(
            conflict_after(listed.clone(), &computing, &unread, listed.clone()).is_empty(),
            "unread, with another reason keeping the item listed"
        );
        assert_eq!(
            conflict_after(mine.clone(), &clean, &[], mine.clone()),
            [91],
            "read and clean, so it cleared and coming back is news"
        );
    }

    /// What is kept for an unread kind is what you were told, so it stops at that head.
    #[test]
    fn a_conflict_on_a_head_pushed_while_it_went_unread_is_new() {
        let viewer = me();
        let conflict = merge_of(91, Mergeability::Conflicting, None);
        let mine = |head: &str| {
            pr(91)
                .author(ME)
                .with_head(&head.repeat(40))
                .pushed_at(day(5))
        };
        let told = seen_on(mine("a"), &conflict);
        let snapshot = snap(vec![mine("b")], vec![]);
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let seen = seen_after(&obs, &[], &told, &[ReasonKind::Conflict]);
        assert_eq!(asking(mine("b"), &conflict, &seen, Some(false)), [91]);
    }

    /// What you said yourself is not news to you, and a request you answered with "this
    /// weekend" would otherwise come back the next morning.
    #[test]
    fn only_someone_elses_word_on_your_own_thread_is_new() {
        let empty = HashMap::new();
        let asked = || c("bob", 5, "can you rename this?");
        let own = |comments: Vec<Comment>| pr(88).author(ME).threads(vec![thread(comments)]);
        let seen = seen_on(own(vec![asked()]), &empty);
        for (name, word, new) in [
            ("your reply", c(ME, 1, "this weekend"), false),
            ("a bot's", bot("cov[bot]", 1, "coverage report"), false),
            ("bob's", c("bob", 1, "any news?"), true),
        ] {
            let got = asking(own(vec![asked(), word]), &empty, &seen, Some(true));
            assert_eq!(!got.is_empty(), new, "{name}");
        }
    }

    /// Its date is your own work on it, which nobody has to tell you about. The record goes
    /// through the file, because what it keeps of this kind has no time and no head.
    #[test]
    fn unshipped_work_is_new_once_whatever_you_do_to_it() {
        let viewer = me();
        let snapshot = snap(vec![], vec![issue(89).assigned(ME, day(20))]);
        let plain = || collect(&snapshot, &viewer, now(), &HashMap::new());
        let with_fork = |worked_on| {
            let mut obs = plain();
            let text = "your fork#6 would fix this".to_owned();
            let reason = Reason::standing(ReasonKind::Unshipped, worked_on, text);
            add_reason(&mut obs, &snapshot.items[0], reason);
            obs
        };
        let carried = |obs: &[Obligation<'_>], seen: &BTreeMap<CardKey, Seen>| {
            !new_asks(obs, seen, |_| Some(true)).is_empty()
        };
        let before = seen_of(&plain());
        assert!(carried(&with_fork(day(5)), &before), "a new kind of reason");

        let told = seen_of(&with_fork(day(5)));
        let file = serde_json::to_string(&told).unwrap();
        let told = serde_json::from_str(&file).unwrap();
        assert!(!carried(&with_fork(day(5)), &told), "nothing moved");
        assert!(!carried(&with_fork(day(1)), &told), "you worked on it");
    }

    /// A mention the record has seen, on an item whose head was committed ahead of the next
    /// ping. Dates from different kinds of reason say nothing about each other.
    #[test]
    fn a_commit_dated_ahead_does_not_cover_a_newer_mention() {
        let empty = HashMap::new();
        let acked = |comments: Vec<Comment>| {
            let mut said = vec![c(ME, 9, "ACK 1234567"), c("bob", 5, &at_me("@me first"))];
            said.extend(comments);
            pr(98)
                .with_head(&"a".repeat(40))
                .pushed_at(day(-30))
                .comments(said)
        };
        let seen = seen_on(acked(vec![]), &empty);
        let pinged = acked(vec![c("bob", 1, &at_me("@me a question"))]);
        assert_eq!(asking(pinged, &empty, &seen, Some(true)), [98]);
    }
}
