//! What is blocked on you: the six obligation rules, as pure functions.
//!
//! An obligation is found by code alone. The model may add a gist to one but can
//! never hide, drop or reorder it -- that is the whole difference from an
//! opportunity. Each item carries a list of reasons; each reason has
//!
//!   kind  also what a mute remembers
//!   at    when the other side started waiting (ordering), or `None` for a pure
//!         state like a merge conflict that has no start time we can see
//!   last  the newest event behind it (re-arming a mute)
//!   text  the phone-sized explanation

use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use regex::Regex;

use crate::board::Mute;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReasonKind {
    ReviewRequested,
    Mention,
    Reply,
    Thread,
    ChangesRequested,
    UnresolvedThread,
    Conflict,
    CiFailed,
    StaleAck,
    Assigned,
    PendingReview,
    Unshipped,
}

impl ReasonKind {
    /// The name written into a card's fingerprint and the gist cache.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReviewRequested => "review_requested",
            Self::Mention => "mention",
            Self::Reply => "reply",
            Self::Thread => "thread",
            Self::ChangesRequested => "changes_requested",
            Self::UnresolvedThread => "unresolved_thread",
            Self::Conflict => "conflict",
            Self::CiFailed => "ci_failed",
            Self::StaleAck => "stale_ack",
            Self::Assigned => "assigned",
            Self::PendingReview => "pending_review",
            Self::Unshipped => "unshipped",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Reason {
    pub kind: ReasonKind,
    pub at: Option<Timestamp>,
    pub last: Option<Timestamp>,
    pub text: String,
}

impl Reason {
    pub fn new(kind: ReasonKind, at: Option<Timestamp>, text: String) -> Self {
        Self {
            kind,
            at,
            last: at,
            text,
        }
    }

    fn until(mut self, last: Timestamp) -> Self {
        self.last = Some(last);
        self
    }
}

#[derive(Clone, Debug)]
pub struct Obligation<'a> {
    pub key: String,
    pub item: &'a Item,
    pub reasons: Vec<Reason>,
    /// Context that informs, but is not part of what a mute remembers.
    pub notes: Vec<String>,
    pub interdiff: Option<Interdiff>,
}

impl<'a> Obligation<'a> {
    fn new(item: &'a Item, reasons: Vec<Reason>) -> Self {
        Self {
            key: item.key.clone(),
            item,
            reasons,
            notes: Vec::new(),
            interdiff: None,
        }
    }

    /// Longest-waiting first. A state with no visible start (a conflict) sorts by
    /// when the item last moved, so it neither jumps the queue nor sinks.
    fn sort_key(&self) -> Timestamp {
        self.reasons
            .iter()
            .filter_map(|reason| reason.at)
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
        mine_at = mine_at.max(pr.head_at); // a push answers too
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
        .filter(|s| !me.is(s.author) && !s.bot && Some(s.at) > mine_at);
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
        out.push(Reason::new(ReasonKind::Mention, Some(first.at), text).until(newest.at));
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
            out.push(Reason::new(ReasonKind::Reply, Some(first.at), text).until(newest.at));
        }
    }
    out
}

/// Rule 2, inline half: someone answered in a review thread you're in.
fn threads(item: &Item, pr: &PrDetail, me: &Viewer, now: DateTime<Utc>) -> Vec<Reason> {
    if me.is(&item.author) {
        return Vec::new(); // your own PR's threads are rule 3's business
    }
    let mut waiting = Vec::new();
    for thread in &pr.threads {
        let comments: Vec<_> = thread.comments.iter().filter(|c| !c.bot).collect();
        if thread.resolved || !comments.iter().any(|c| me.is(&c.author)) {
            continue;
        }
        let Some(newest) = comments.last().filter(|c| !me.is(&c.author)) else {
            continue;
        };
        let mine_at = comments
            .iter()
            .filter(|c| me.is(&c.author))
            .map(|c| c.at)
            .max();
        let Some(first) = comments
            .iter()
            .find(|c| Some(c.at) > mine_at && !me.is(&c.author))
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
    let place = if n == 1 && !path.is_empty() {
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
    let reason = Reason::new(ReasonKind::Thread, Some(first.at), text);
    match waiting.iter().map(|(_, newest, _)| newest.at).max() {
        Some(last) => vec![reason.until(last)],
        None => vec![reason],
    }
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
    let text = format!("review requested {} ago", at.ago(now));
    vec![Reason::new(ReasonKind::ReviewRequested, Some(at), text)]
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
    let lasts: Vec<Timestamp> = pr
        .threads
        .iter()
        .filter(|t| !t.resolved)
        .filter_map(|t| t.comments.iter().map(|c| c.at).max())
        .collect();
    if let (Some(oldest), Some(newest)) = (lasts.iter().min(), lasts.iter().max()) {
        let n = lasts.len();
        // The ruleset refuses to merge with an open thread, even a typo nit.
        let text = format!(
            "{n} unresolved thread{} block{} merge",
            plural(n),
            if n == 1 { "s" } else { "" }
        );
        out.push(Reason::new(ReasonKind::UnresolvedThread, Some(*oldest), text).until(*newest));
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
        .filter(|r| r.state == ReviewState::ChangesRequested && Some(r.at) > pr.head_at)
        .collect();
    let oldest = blocking.iter().min_by_key(|r| r.at);
    let newest = blocking.iter().map(|r| r.at).max();
    if let (Some(oldest), Some(newest)) = (oldest, newest) {
        let text = format!(
            "{} requested changes {} ago",
            oldest.author,
            oldest.at.ago(now)
        );
        out.push(Reason::new(ReasonKind::ChangesRequested, Some(oldest.at), text).until(newest));
    }

    if let Some(merge) = merge {
        if merge.mergeable.as_deref() == Some("CONFLICTING") {
            let text = "conflicts with master, needs a rebase".to_owned();
            out.push(Reason::new(ReasonKind::Conflict, None, text));
        }
        if matches!(merge.ci.as_deref(), Some("FAILURE" | "ERROR")) {
            out.push(Reason::new(
                ReasonKind::CiFailed,
                None,
                "CI is red".to_owned(),
            ));
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
    let (Some((acked, at)), Some(head)) = (latest_ack(item, pr, me), pr.head.as_deref()) else {
        return Vec::new();
    };
    if head.to_lowercase().starts_with(&acked) {
        return Vec::new();
    }
    let short = prefix(&acked, 7);
    if let Some(pushed) = pr.head_at.filter(|pushed| *pushed > at) {
        let text = format!("pushed {} ago, past your ACK {short}", pushed.ago(now));
        return vec![Reason::new(ReasonKind::StaleAck, Some(pushed), text)];
    }
    // A head older than the ACK means the ACK named some other commit.
    let text = format!("your ACK {short} is not the PR head");
    vec![Reason::new(ReasonKind::StaleAck, Some(at), text)]
}

/// Rule 5: assigned to you and nothing links a fix yet.
fn assigned(item: &Item, issue: &IssueDetail, me: &Viewer, now: DateTime<Utc>) -> Vec<Reason> {
    if !item.assignees.iter().any(|login| me.is(login)) || !issue.linked_prs.is_empty() {
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
        && let Some(due) = milestone.due_on.as_deref().filter(|due| !due.is_empty())
    {
        bits.push(format!("{} due {}", milestone.title, prefix(due, 10)));
    }
    vec![Reason::new(
        ReasonKind::Assigned,
        Some(at),
        bits.join(" · "),
    )]
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
    vec![Reason::new(ReasonKind::PendingReview, Some(at), text)]
}

// ---------------------------------------------------------------- assembly

/// Every obligation, oldest wait first. `merge` holds the state of the PRs whose
/// mergeability was asked for -- your own.
pub fn collect<'a>(
    snap: &'a Snapshot,
    me: &Viewer,
    now: DateTime<Utc>,
    merge: &HashMap<u64, MergeState>,
) -> Vec<Obligation<'a>> {
    let mut out = Vec::new();
    for item in snap.prs.iter().chain(&snap.issues) {
        let reasons = match &item.detail {
            Detail::Pr(pr) => {
                let mut reasons = review_request(item, pr, me, now);
                reasons.extend(conversation(item, me, now));
                reasons.extend(threads(item, pr, me, now));
                reasons.extend(own_pr(item, pr, me, now, merge.get(&item.number)));
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
    if let Some(ob) = obs.iter_mut().find(|ob| ob.item.number == item.number) {
        ob.reasons.push(reason);
        return;
    }
    obs.push(Obligation::new(item, vec![reason]));
    obs.sort_by_key(Obligation::sort_key);
}

// ---------------------------------------------------------------- mutes

/// What a mute remembers: the newest event behind the card, and which kinds of
/// reason it had. Kept in the card itself so dragging it to Passed carries it along.
pub fn fingerprint(ob: &Obligation<'_>) -> (String, Vec<&'static str>) {
    let last = ob
        .reasons
        .iter()
        .filter_map(|reason| reason.last)
        .max()
        .map_or(String::new(), |last| last.to_string());
    let kinds: BTreeSet<&'static str> = ob.reasons.iter().map(|r| r.kind.as_str()).collect();
    (last, kinds.into_iter().collect())
}

/// A mute holds until a NEW trigger: a reason kind that wasn't there when you muted
/// it, or a timed event newer than the newest one you saw. General activity on the
/// item never re-arms it -- only what would have made it an obligation in the first
/// place.
pub fn is_muted(ob: &Obligation<'_>, mute: Option<&Mute>) -> bool {
    let Some(mute) = mute else {
        return false;
    };
    ob.reasons.iter().all(|reason| {
        mute.kinds.contains(reason.kind.as_str())
            && !reason.last.is_some_and(|last| Some(last) > mute.last)
    })
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
    use crate::testkit::{ME, bot, c, day, issue, me, now, pr, review, snap, thread};

    fn merge_of(
        number: u64,
        mergeable: Option<&str>,
        ci: Option<&str>,
    ) -> HashMap<u64, MergeState> {
        HashMap::from([(
            number,
            MergeState {
                mergeable: mergeable.map(str::to_owned),
                ci: ci.map(str::to_owned),
            },
        )])
    }

    fn kinds_with(item: Item, merge: &HashMap<u64, MergeState>) -> Vec<&'static str> {
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
                    .head_at(day(10))
                    .comments(vec![bot("ci[bot]", 2, "coverage report")]),
                vec![],
            ),
            (
                "PR description cc's you",
                pr(9).body(&at_me("Fixes a stall. cc @me for review")),
                vec!["mention"],
            ),
        ]);
    }

    #[test]
    fn an_informal_ask_reads_as_one() {
        let viewer = me();
        let snapshot = snap(vec![pr(9).body(&at_me("cc @me for review"))], vec![]);
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
                    c("csgui", 29, "Run a node and re-level logs"),
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
                issue(12).assigned(ME, day(5)).linked_pr(1327),
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
                    .head_at(day(10))
                    .comments(vec![c("bob", 3, "a comment")]),
                vec!["reply"],
            ),
            (
                "a push answers the comment before it",
                own(21)
                    .head_at(day(2))
                    .comments(vec![c("bob", 3, "a comment")]),
                vec![],
            ),
            (
                "an ACK is good news, not a request",
                own(22).head_at(day(10)).comments(vec![c(
                    "bob",
                    3,
                    "ACK 1a2b3c4d\n\nran the tests",
                )]),
                vec![],
            ),
            (
                "an APPROVED review asks nothing",
                own(23).head_at(day(10)).reviews(vec![review(
                    "bob",
                    3,
                    ReviewState::Approved,
                    "ACK ffffff1",
                )]),
                vec![],
            ),
            (
                "changes requested since your push",
                own(24).head_at(day(10)).reviews(vec![review(
                    "bob",
                    3,
                    ReviewState::ChangesRequested,
                    "needs a test",
                )]),
                vec!["changes_requested"],
            ),
            (
                "changes requested before your latest push is answered",
                own(25).head_at(day(2)).reviews(vec![review(
                    "bob",
                    3,
                    ReviewState::ChangesRequested,
                    "needs a test",
                )]),
                vec![],
            ),
            (
                "an unresolved thread blocks merge, whoever spoke last",
                own(26).head_at(day(2)).threads(vec![thread(vec![c(
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
        ]);
    }

    #[test]
    fn conflicts_and_red_ci_come_from_merge_state() {
        let merge = merge_of(28, Some("CONFLICTING"), Some("FAILURE"));
        assert_eq!(
            kinds_with(pr(28).author(ME).draft(), &merge),
            ["ci_failed", "conflict"]
        );
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

    #[test]
    fn stale_acks_and_unsubmitted_reviews() {
        let head = "abcdef0123456789abcdef0123456789abcdef01";
        check(vec![
            (
                "an ACK the PR moved past",
                pr(40)
                    .with_head(head)
                    .head_at(day(2))
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
                pr(52).author(ME).updated(day(10)),
            ],
            vec![issue(53).comments(vec![c("bob", 20, &at_me("@me"))])],
        );
        let merge = merge_of(52, Some("CONFLICTING"), None);
        let order: Vec<u64> = collect(&snapshot, &viewer, now(), &merge)
            .iter()
            .map(|ob| ob.item.number)
            .collect();
        assert_eq!(order, [51, 53, 52, 50]);
    }

    fn mute_of(ob: &Obligation<'_>) -> Mute {
        let (last, kinds) = fingerprint(ob);
        Mute {
            last: Timestamp::parse(&last),
            kinds: kinds.into_iter().map(str::to_owned).collect(),
        }
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

    #[test]
    fn a_muted_conflict_stays_muted_while_it_persists() {
        let viewer = me();
        let merge = merge_of(61, Some("CONFLICTING"), None);
        let snapshot = snap(vec![pr(61).author(ME).head_at(day(2))], vec![]);
        let obs = collect(&snapshot, &viewer, now(), &merge);
        let mute = mute_of(&obs[0]);
        assert!(is_muted(&obs[0], Some(&mute)));
    }

    #[test]
    fn a_hand_edited_fingerprint_brings_the_obligation_back() {
        let viewer = me();
        let snapshot = snap(vec![pr(62).requested(ME, &day(3).to_string())], vec![]);
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let mute = Mute {
            last: Timestamp::parse("not a time"),
            kinds: BTreeSet::from(["review_requested".to_owned()]),
        };
        assert!(!is_muted(&obs[0], Some(&mute)));
    }
}
