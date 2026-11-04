//! Cards for the board and the DM text. Formatting only -- every decision about
//! what appears, and in what order, has been made before anything gets here.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::board::{Card, CardKey, ClaimLane};
use crate::model::Gist;
use crate::obligations::{Obligation, ReasonKind, fingerprint};
use crate::opportunities::{Opportunity, Source};
use crate::snapshot::{Ci, Item, MergeState, PrDetail};
use crate::text::{plural, prefix};
use crate::unshipped::{LinkHow, Origin, Unshipped};

/// First match wins, so the order is which reason names the card.
const ICONS: [(ReasonKind, &str); 12] = [
    (ReasonKind::PendingReview, "📝"),
    (ReasonKind::ReviewRequested, "👀"),
    (ReasonKind::StaleAck, "♻️"),
    (ReasonKind::ChangesRequested, "🔧"),
    (ReasonKind::UnresolvedThread, "🔧"),
    (ReasonKind::Conflict, "🔧"),
    (ReasonKind::CiFailed, "🔧"),
    (ReasonKind::Mention, "💬"),
    (ReasonKind::Thread, "💬"),
    (ReasonKind::Reply, "💬"),
    (ReasonKind::Assigned, "📌"),
    (ReasonKind::Unshipped, "⚡"),
];
const OPP_ICONS: [(Source, &str); 4] = [
    (Source::NeedsReview, "📋"),
    (Source::Moved, "🔁"),
    (Source::Unreviewed, "🔍"),
    (Source::NewIssue, "📥"),
];

/// A source this run could not read. What it would have said is unknown, so the
/// heartbeat and `last_run.json` always say so, and the DM does when it touches what the
/// DM says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Degraded {
    Unshipped,
    Project,
    Merge,
    MergeComputing,
    OpportunityMerge,
    Claims,
    Footprint,
    LastDm,
    Model,
}

impl Degraded {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unshipped => "unshipped",
            Self::Project => "project",
            Self::Merge => "merge",
            Self::MergeComputing => "merge_computing",
            Self::OpportunityMerge => "opportunity_merge",
            Self::Claims => "claims",
            Self::Footprint => "footprint",
            Self::LastDm => "last_dm",
            Self::Model => "model",
        }
    }

    /// `None` for what only feeds the opportunity ranking, which the DM does not carry.
    fn warning(self) -> Option<&'static str> {
        match self {
            Self::Unshipped => Some("Unshipped not refreshed: its source failed"),
            Self::Merge => Some("merge state unread: a conflict or red CI may be missing"),
            Self::MergeComputing => Some("GitHub is still computing conflicts: one may be missing"),
            Self::Claims => {
                Some("claimed cards not checked upstream: their notes are as they were")
            }
            Self::LastDm => Some("last_dm.json unreadable: everything reads as new"),
            Self::Model => Some(
                "the model did not answer: the board falls back to computed reasons and ranking order",
            ),
            Self::Project | Self::Footprint | Self::OpportunityMerge => None,
        }
    }
}

fn ob_icon(ob: &Obligation<'_>) -> &'static str {
    ICONS
        .iter()
        .find(|(kind, _)| ob.has_kind(*kind))
        .map_or("•", |(_, icon)| icon)
}

fn opp_icon(o: &Opportunity<'_>) -> &'static str {
    OPP_ICONS
        .iter()
        .find(|(source, _)| o.sources.contains(source))
        .map_or("•", |(_, icon)| icon)
}

fn ci_word(merge: Option<MergeState>) -> &'static str {
    let Some(merge) = merge else {
        return "unknown";
    };
    match merge.ci {
        None => "none",
        Some(Ci::Success) => "green",
        Some(Ci::Failure | Ci::Error) => "red",
        Some(Ci::Pending | Ci::Expected) => "running",
    }
}

fn size(pr: &PrDetail) -> String {
    format!("+{}/-{} {}f", pr.additions, pr.deletions, pr.changed_files)
}

/// The reasons, then any context notes -- notes inform, but are not part of what a
/// mute remembers.
pub fn reasons_text(ob: &Obligation<'_>) -> String {
    ob.reasons
        .iter()
        .map(|reason| reason.text.as_str())
        .chain(ob.notes.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" · ")
}

// ---------------------------------------------------------------- board

pub fn ob_card(ob: &Obligation<'_>, gist: Option<&str>) -> Card {
    let item: &Item = ob.item;
    let sized = ob.has_kind(ReasonKind::ReviewRequested) || ob.has_kind(ReasonKind::StaleAck);
    let size_text = match item.pr() {
        Some(pr) if sized => format!(" · {}", size(pr)),
        _ => String::new(),
    };
    let tail = gist.map_or(String::new(), |gist| format!(" *({gist})*"));
    let body = format!(
        "{} [{}]({}) {} · {}{size_text}{tail}",
        ob_icon(ob),
        item.key.short(),
        item.url,
        prefix(&item.title, 80),
        reasons_text(ob),
    );
    Card::with_fingerprint(&body, &fingerprint(ob))
}

pub fn opp_card(o: &Opportunity<'_>, why: Option<&str>) -> Card {
    let item = o.item;
    let mut bits = Vec::new();
    if let Some(pr) = item.pr() {
        bits.push(size(pr));
        bits.push(format!("CI {}", ci_word(o.merge)));
        if o.merge.is_some_and(MergeState::is_conflicting) {
            bits.push("conflicts".to_owned());
        }
    } else if let Some(label) = item.labels.first() {
        bits.push(label.clone());
    }
    bits.push(format!("{:.0}d", o.age_days));
    if let Some(note) = &o.note {
        bits.push(note.clone());
    }
    let tail = why.map_or(String::new(), |why| format!(" *({why})*"));
    Card::plain(&format!(
        "{} [{}]({}) {} · {}{tail}",
        opp_icon(o),
        item.key.short(),
        item.url,
        prefix(&item.title, 80),
        bits.join(" · ")
    ))
}

pub fn unshipped_card(u: &Unshipped) -> Card {
    let repo = u.repo.label();
    let head = match &u.origin {
        Origin::ForkPr {
            number,
            url,
            additions,
            deletions,
        } => format!(
            "[{repo} fork#{number}]({url}) {} · +{additions}/-{deletions} · {} commit{} · last {}",
            prefix(&u.title, 80),
            u.commits,
            plural(u.commits),
            u.last
        ),
        Origin::Branch => format!(
            "{repo} `{}` {} · {} commit{} · last {} · no PR anywhere",
            u.branch,
            prefix(&u.title, 70),
            u.commits,
            plural(u.commits),
            u.last
        ),
    };
    let link = u.issue.as_ref().map_or(String::new(), |issue| {
        let verb = match issue.how {
            LinkHow::Refs => "fixes",
            LinkHow::Guess => "probably fixes",
        };
        format!(" · {verb} [#{}]({})", issue.number, issue.url)
    });
    Card::with_key(&format!("⚡ {head}{link}"), &u.key)
}

// ---------------------------------------------------------------- DM

/// The morning message: one header line, then only the new asks. What was not new is on
/// the board and is not repeated here. Plain text: Telegram autolinks bare URLs, and
/// markdown would have to survive whatever transport carries it.
pub struct Dm<'a, 'o> {
    pub asks: &'a [&'a Obligation<'o>],
    pub on_board: usize,
    pub gists: &'a BTreeMap<CardKey, Gist>,
    pub claimed_lane: &'a HashMap<CardKey, ClaimLane>,
    pub degraded: &'a BTreeSet<Degraded>,
    pub claim_alerts: &'a [String],
}

impl Dm<'_, '_> {
    fn ask(&self, ob: &Obligation<'_>, lines: &mut Vec<String>) {
        let item = ob.item;
        let place = self
            .claimed_lane
            .get(&item.key)
            .map_or(String::new(), |lane| format!(" (in your {})", lane.name()));
        let said = self.gists.get(&item.key).map_or_else(
            || format!("{} · {}", prefix(&item.title, 70), reasons_text(ob)),
            |gist| gist.text.clone(),
        );
        lines.push(format!("{} {said}{place}", item.key.short()));
        lines.push(item.url.clone());
    }

    pub fn render(&self) -> String {
        let new = match self.asks.len() {
            0 => "nothing new".to_owned(),
            n => format!("{n} new"),
        };
        let mut lines = vec![format!("OSS · {new} · {} on the board", self.on_board)];
        for alert in self.claim_alerts {
            lines.push(format!("⚠ {alert}"));
        }
        lines.extend(
            self.degraded
                .iter()
                .filter_map(|source| source.warning())
                .map(|warning| format!("⚠ {warning}")),
        );
        if self
            .asks
            .iter()
            .any(|ob| !self.gists.contains_key(&ob.item.key))
        {
            lines.push("⚠ not filtered: the model gave no verdict".to_owned());
        }
        for ob in self.asks {
            lines.push(String::new());
            self.ask(ob, &mut lines);
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obligations::collect;
    use crate::snapshot::Mergeability;
    use crate::testkit::{ME, day, me, now, opportunity, pr, snap};

    fn message(
        asks: &[&Obligation<'_>],
        gists: &BTreeMap<CardKey, Gist>,
        degraded: &[Degraded],
    ) -> String {
        Dm {
            asks,
            on_board: 2,
            gists,
            claimed_lane: &HashMap::new(),
            degraded: &degraded.iter().copied().collect(),
            claim_alerts: &[],
        }
        .render()
    }

    #[test]
    fn a_quiet_morning_is_one_line() {
        assert_eq!(
            message(&[], &BTreeMap::new(), &[]),
            "OSS · nothing new · 2 on the board"
        );
    }

    #[test]
    fn an_ask_is_its_gist_and_a_link() {
        let viewer = me();
        let snapshot = snap(vec![pr(7).requested(ME, &day(1).to_string())], vec![]);
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let gist = Gist {
            text: "wants a yes/no".to_owned(),
            asks: true,
        };
        let gists = BTreeMap::from([(obs[0].item.key.clone(), gist)]);
        assert_eq!(
            message(&[&obs[0]], &gists, &[]),
            "OSS · 1 new · 2 on the board\n\n\
             floresta#7 wants a yes/no\n\
             https://github.com/getfloresta/Floresta/pull/7"
        );
    }

    #[test]
    fn with_no_verdict_an_ask_shows_its_computed_reasons_and_says_it_was_not_filtered() {
        let viewer = me();
        let snapshot = snap(vec![pr(7).requested(ME, &day(1).to_string())], vec![]);
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let text = message(&[&obs[0]], &BTreeMap::new(), &[]);
        assert!(
            text.contains("floresta#7 pr 7 · review requested 1d ago"),
            "{text}"
        );
        assert!(text.contains("not filtered"), "{text}");
    }

    /// Those not warned about only feed the opportunity ranking, which the DM does not carry.
    #[test]
    fn only_a_source_that_touches_what_the_dm_says_is_warned_about() {
        for (source, warned) in [
            (Degraded::Unshipped, true),
            (Degraded::Merge, true),
            (Degraded::MergeComputing, true),
            (Degraded::Claims, true),
            (Degraded::LastDm, true),
            (Degraded::Project, false),
            (Degraded::Footprint, false),
        ] {
            let text = message(&[], &BTreeMap::new(), &[source]);
            assert_eq!(text.contains('⚠'), warned, "{source:?}: {text}");
        }
    }

    #[test]
    fn an_obligation_you_already_took_says_where() {
        let viewer = me();
        let snapshot = snap(vec![pr(7).requested(ME, &day(1).to_string())], vec![]);
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let text = Dm {
            asks: &[&obs[0]],
            on_board: 0,
            gists: &BTreeMap::new(),
            claimed_lane: &HashMap::from([(obs[0].item.key.clone(), ClaimLane::Doing)]),
            degraded: &BTreeSet::new(),
            claim_alerts: &[],
        }
        .render();
        assert!(text.contains("(in your Doing)"), "{text}");
    }

    /// Half days round to even, as the card has always shown them.
    #[test]
    fn a_cards_age_rounds_half_days_to_even() {
        let item = pr(7);
        for (age_days, want) in [(0.5, "0d"), (1.5, "2d"), (2.5, "2d"), (3.5, "4d")] {
            let opp = Opportunity {
                age_days,
                ..opportunity(&item)
            };
            let card = opp_card(&opp, None);
            assert!(
                card.as_str().ends_with(&format!(" · {want}")),
                "{age_days}: {card:?}"
            );
        }
    }

    #[test]
    fn a_card_tells_no_checks_from_not_having_read_them() {
        let item = pr(7);
        for (merge, want) in [
            (None, "CI unknown"),
            (
                Some(MergeState {
                    mergeable: Mergeability::Mergeable,
                    ci: None,
                }),
                "CI none",
            ),
            (Some(MergeState::red()), "CI red"),
        ] {
            let opp = Opportunity {
                merge,
                ..opportunity(&item)
            };
            let card = opp_card(&opp, None);
            assert!(card.as_str().contains(want), "{want}: {card:?}");
        }
    }
}
