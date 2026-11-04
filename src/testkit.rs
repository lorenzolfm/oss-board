//! Snapshot-shaped fixtures for the tests: the same values `snapshot::fetch`
//! produces, so the rules are exercised exactly as production calls them.

use std::collections::HashMap;

use chrono::{DateTime, SecondsFormat, TimeDelta, TimeZone, Utc};

use crate::board::CardKey;
use crate::obligations::Viewer;
use crate::opportunities::{Opportunity, Source};
use crate::snapshot::{
    Comment, CommentKind, Detail, IssueDetail, Item, PrDetail, Review, ReviewState, Snapshot,
    Thread, fold_reviews, item_key,
};
use crate::timestamp::Timestamp;

pub const ME: &str = "lorenzolfm";
const REPO_URL: &str = "https://github.com/getfloresta/Floresta";

pub fn me() -> Viewer {
    Viewer::new(ME)
}

pub fn key(text: &str) -> CardKey {
    text.parse().unwrap()
}

/// An unreviewed PR or new issue, as `opportunities::collect` hands it over.
pub fn opportunity(item: &Item) -> Opportunity<'_> {
    Opportunity {
        item,
        sources: vec![Source::Unreviewed],
        note: None,
        age_days: 2.0,
        areas: vec![],
        merge: None,
    }
}

pub fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

/// The time `n` days before `now()`.
pub fn day(n: i64) -> Timestamp {
    let at = now() - TimeDelta::days(n);
    Timestamp::parse(&at.to_rfc3339_opts(SecondsFormat::Secs, true)).unwrap()
}

pub fn c(author: &str, days_ago: i64, body: &str) -> Comment {
    Comment {
        author: author.to_owned(),
        bot: false,
        at: day(days_ago),
        body: body.to_owned(),
        kind: CommentKind::Comment,
    }
}

pub fn bot(author: &str, days_ago: i64, body: &str) -> Comment {
    Comment {
        bot: true,
        ..c(author, days_ago, body)
    }
}

pub fn review(author: &str, days_ago: i64, state: ReviewState, body: &str) -> Review {
    Review {
        author: author.to_owned(),
        bot: false,
        state,
        body: body.to_owned(),
        at: day(days_ago),
    }
}

pub fn thread(comments: Vec<Comment>) -> Thread {
    Thread {
        resolved: false,
        path: "crates/floresta-wire/src/node.rs".to_owned(),
        comments,
    }
}

impl Thread {
    pub fn resolved(mut self) -> Self {
        self.resolved = true;
        self
    }
}

fn item(number: u64, title: String, url: String, detail: Detail) -> Item {
    Item {
        key: item_key(&url).unwrap(),
        number,
        title,
        url,
        author: "alice".to_owned(),
        author_bot: false,
        created: day(30),
        updated: day(1),
        body: String::new(),
        milestone: None,
        labels: vec![],
        assignees: vec![],
        comments: vec![],
        detail,
    }
}

pub fn pr(number: u64) -> Item {
    item(
        number,
        format!("pr {number}"),
        format!("{REPO_URL}/pull/{number}"),
        Detail::Pr(PrDetail {
            draft: false,
            additions: 10,
            deletions: 2,
            changed_files: 1,
            head: "f".repeat(40),
            pushed_at: Some(day(20)),
            review_decision: None,
            requested: vec![],
            requested_at: HashMap::new(),
            reviews: vec![],
            threads: vec![],
            files: vec!["crates/floresta-wire/src/node.rs".to_owned()],
        }),
    )
}

pub fn issue(number: u64) -> Item {
    item(
        number,
        format!("issue {number}"),
        format!("{REPO_URL}/issues/{number}"),
        Detail::Issue(IssueDetail {
            assigned_at: HashMap::new(),
            linked_prs: vec![],
        }),
    )
}

pub fn snap(prs: Vec<Item>, issues: Vec<Item>) -> Snapshot {
    Snapshot {
        items: prs.into_iter().chain(issues).collect(),
    }
}

/// Builders, each returning the item so a fixture reads as one expression.
impl Item {
    fn pr_mut(&mut self) -> &mut PrDetail {
        match &mut self.detail {
            Detail::Pr(pr) => pr,
            Detail::Issue(_) => panic!("a PR-only fixture field on an issue"),
        }
    }

    pub fn author(mut self, author: &str) -> Self {
        self.author = author.to_owned();
        self
    }

    pub fn title(mut self, title: &str) -> Self {
        self.title = title.to_owned();
        self
    }

    pub fn body(mut self, body: &str) -> Self {
        self.body = body.to_owned();
        self
    }

    pub fn created(mut self, at: Timestamp) -> Self {
        self.created = at;
        self
    }

    pub fn updated(mut self, at: Timestamp) -> Self {
        self.updated = at;
        self
    }

    pub fn draft(mut self) -> Self {
        self.pr_mut().draft = true;
        self
    }

    pub fn with_head(mut self, head: &str) -> Self {
        self.pr_mut().head = head.to_owned();
        self
    }

    pub fn pushed_at(mut self, at: Timestamp) -> Self {
        self.pr_mut().pushed_at = Some(at);
        self
    }

    pub fn requested(mut self, who: &str, at: &str) -> Self {
        let pr = self.pr_mut();
        pr.requested.push(who.to_owned());
        pr.requested_at
            .insert(who.to_owned(), Timestamp::parse(at).unwrap());
        self
    }

    pub fn threads(mut self, threads: Vec<Thread>) -> Self {
        self.pr_mut().threads = threads;
        self
    }

    /// As the snapshot does: a review with a body is part of the conversation.
    pub fn reviews(mut self, reviews: Vec<Review>) -> Self {
        fold_reviews(&mut self.comments, &reviews);
        self.pr_mut().reviews = reviews;
        self
    }

    /// Adds to the conversation, oldest first, so it composes with `reviews`.
    pub fn comments(mut self, comments: Vec<Comment>) -> Self {
        self.comments.extend(comments);
        self.comments.sort_by_key(|comment| comment.at);
        self
    }

    pub fn assigned(mut self, who: &str, at: Timestamp) -> Self {
        self.assignees.push(who.to_owned());
        if let Detail::Issue(issue) = &mut self.detail {
            issue.assigned_at.insert(who.to_owned(), at);
        }
        self
    }

    pub fn linked_pr(mut self, number: u64) -> Self {
        if let Detail::Issue(issue) = &mut self.detail {
            issue.linked_prs.push(number);
        }
        self
    }
}
