//! Cards for the board and the DM text. Formatting only -- every decision about
//! what appears, and in what order, has been made before anything gets here.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::board::{key_marker, mute_marker};
use crate::model::Pick;
use crate::obligations::{Obligation, ReasonKind, fingerprint};
use crate::opportunities::{Opportunity, Source};
use crate::snapshot::{Item, PrDetail};
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

fn ci_word(state: Option<&str>) -> &'static str {
    match state {
        Some("SUCCESS") => "green",
        Some("FAILURE" | "ERROR") => "red",
        Some("PENDING" | "EXPECTED") => "running",
        _ => "none",
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

pub fn ob_card(ob: &Obligation<'_>, gist: Option<&str>) -> String {
    let item: &Item = ob.item;
    let sized = ob.has_kind(ReasonKind::ReviewRequested) || ob.has_kind(ReasonKind::StaleAck);
    let size_text = match item.pr() {
        Some(pr) if sized => format!(" · {}", size(pr)),
        _ => String::new(),
    };
    let tail = gist.map_or(String::new(), |gist| format!(" *({gist})*"));
    let (last, kinds) = fingerprint(ob);
    format!(
        "- [ ] {} [#{}]({}) {} · {}{size_text}{tail} {}",
        ob_icon(ob),
        item.number,
        item.url,
        prefix(&item.title, 80),
        reasons_text(ob),
        mute_marker(&last, &kinds)
    )
}

pub fn opp_card(o: &Opportunity<'_>, why: &str) -> String {
    let item = o.item;
    let mut bits = Vec::new();
    if let Some(pr) = item.pr() {
        bits.push(size(pr));
        bits.push(format!("CI {}", ci_word(o.ci())));
        if o.mergeable() == Some("CONFLICTING") {
            bits.push("conflicts".to_owned());
        }
    } else if let Some(label) = item.labels.first() {
        bits.push(label.clone());
    }
    bits.push(format!("{:.0}d", o.age_days));
    if !o.note.is_empty() {
        bits.push(o.note.clone());
    }
    let tail = if why.is_empty() {
        String::new()
    } else {
        format!(" *({why})*")
    };
    format!(
        "- [ ] {} [#{}]({}) {} · {}{tail}",
        opp_icon(o),
        item.number,
        item.url,
        prefix(&item.title, 80),
        bits.join(" · ")
    )
}

pub fn unshipped_card(u: &Unshipped) -> String {
    let head = match &u.origin {
        Origin::ForkPr {
            number,
            url,
            additions,
            deletions,
        } => format!(
            "[fork#{number}]({url}) {} · +{additions}/-{deletions} · {} commit{} · last {}",
            prefix(&u.title, 80),
            u.commits,
            plural(u.commits),
            u.last
        ),
        Origin::Branch => format!(
            "`{}` {} · {} commit{} · last {} · no PR anywhere",
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
    format!("- [ ] ⚡ {head}{link} {}", key_marker(&u.key))
}

// ---------------------------------------------------------------- DM

/// The phone message: every obligation, then the top picks, then counts. Plain
/// text: Telegram autolinks bare URLs, and markdown would have to survive whatever
/// transport carries it.
#[derive(Default)]
pub struct Dm<'a, 'o, 'p> {
    pub today: &'a str,
    pub obs: &'a [Obligation<'o>],
    pub gists: BTreeMap<String, String>,
    pub claimed_lane: HashMap<String, &'static str>,
    pub picks: &'a [Pick<'p, 'o>],
    pub judged: bool,
    pub unshipped: usize,
    pub new_keys: HashSet<String>,
    pub warnings: &'a [String],
    pub claim_alerts: &'a [String],
    pub muted: usize,
    pub briefed: &'a [String],
    pub cap: usize,
}

impl Dm<'_, '_, '_> {
    fn obligations(&self, lines: &mut Vec<String>) {
        if self.obs.is_empty() {
            lines.push("✅ Nothing is blocked on you.".to_owned());
            return;
        }
        lines.push(format!("🔴 Needs you · {}", self.obs.len()));
        for (index, ob) in self.obs.iter().enumerate() {
            let new = if self.new_keys.contains(&ob.key) {
                "🆕 "
            } else {
                ""
            };
            let place = self
                .claimed_lane
                .get(&ob.key)
                .map_or(String::new(), |lane| format!(" (in your {lane})"));
            lines.push(format!(
                "{}. {new}{} #{} {}{place}",
                index + 1,
                ob_icon(ob),
                ob.item.number,
                prefix(&ob.item.title, 70),
            ));
            lines.push(format!("   {}", reasons_text(ob)));
            if let Some(gist) = self.gists.get(&ob.key).filter(|gist| !gist.is_empty()) {
                lines.push(format!("   {gist}"));
            }
            lines.push(format!("   {}", ob.item.url));
        }
    }

    fn top_picks(&self, lines: &mut Vec<String>) {
        let top = &self.picks[..self.picks.len().min(self.cap)];
        if top.is_empty() {
            return;
        }
        let caveat = if self.judged {
            ""
        } else {
            " (ranking order — judge unavailable)"
        };
        lines.push(format!("🎯 Worth a look{caveat}"));
        for (index, pick) in top.iter().enumerate() {
            let o = pick.opp;
            let item = o.item;
            let new = if self.new_keys.contains(&o.key) {
                "🆕 "
            } else {
                ""
            };
            lines.push(format!(
                "{}. {new}{} #{} {}",
                index + 1,
                opp_icon(o),
                item.number,
                prefix(&item.title, 70)
            ));
            let mut facts = match item.pr() {
                Some(pr) => format!("{} · CI {}", size(pr), ci_word(o.ci())),
                None => format!("{:.0}d old", o.age_days),
            };
            if !o.note.is_empty() {
                facts.push_str(&format!(" · {}", o.note));
            }
            let est = if pick.est.is_empty() {
                String::new()
            } else {
                format!("~{} · ", pick.est)
            };
            lines.push(format!("   {est}{facts}"));
            let said = if pick.pitch.is_empty() {
                &pick.why
            } else {
                &pick.pitch
            };
            if !said.is_empty() {
                lines.push(format!("   {said}"));
            }
            lines.push(format!("   {}", item.url));
        }
        lines.push(String::new());
    }

    fn counts(&self) -> String {
        let rest = self.picks.len().saturating_sub(self.cap);
        let mut counts = Vec::new();
        if rest > 0 {
            let ending = if rest == 1 { "y" } else { "ies" };
            counts.push(format!("{rest} more opportunit{ending}"));
        }
        if self.unshipped > 0 {
            counts.push(format!("⚡ {} unshipped", self.unshipped));
        }
        if self.muted > 0 {
            counts.push(format!("🔕 {} muted", self.muted));
        }
        if counts.is_empty() {
            "nothing else".to_owned()
        } else {
            counts.join(" · ")
        }
    }

    pub fn render(&self) -> String {
        let mut lines = vec![format!("Floresta · {}", self.today), String::new()];
        for alert in self.claim_alerts {
            lines.push(format!("⚠ {alert}"));
        }
        if !self.claim_alerts.is_empty() {
            lines.push(String::new());
        }
        self.obligations(&mut lines);
        lines.push(String::new());
        self.top_picks(&mut lines);
        lines.push(format!("📋 board: {}", self.counts()));
        if !self.briefed.is_empty() {
            lines.push(format!("📝 briefed: {}", self.briefed.join(", ")));
        }
        for warning in self.warnings {
            lines.push(format!("⚠ {warning}"));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    //! The DM lists every obligation, however many there are.

    use super::*;
    use crate::notify::chunks;
    use crate::obligations::collect;
    use crate::snapshot::Item;
    use crate::testkit::{ME, day, me, now, pr, snap};

    fn waiting_prs() -> Vec<Item> {
        (0..25)
            .map(|i| {
                pr(100 + i)
                    .requested(ME, &day(i as i64 + 1).to_string())
                    .title(&"t".repeat(120))
            })
            .collect()
    }

    #[test]
    fn the_message_lists_every_obligation_uncapped() {
        let viewer = me();
        let snapshot = snap(waiting_prs(), vec![]);
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let message = Dm {
            today: "Wed 15 Oct 08:07",
            obs: &obs,
            cap: 3,
            ..Dm::default()
        }
        .render();
        for item in &snapshot.prs {
            assert!(
                message.contains(&format!("#{} ", item.number)),
                "#{}",
                item.number
            );
        }
        assert!(!message.contains("]("), "plain text, no markdown links");

        let parts = chunks(&message, 3900);
        assert!(parts.len() > 1, "split for Telegram");
        assert!(parts.iter().all(|part| part.chars().count() <= 3900));
        assert_eq!(parts.join("\n"), message, "split, not truncated");
    }

    #[test]
    fn an_empty_day_says_so() {
        let message = Dm {
            today: "d",
            cap: 3,
            ..Dm::default()
        }
        .render();
        assert!(message.contains("Nothing is blocked on you"), "{message}");
    }

    #[test]
    fn an_obligation_you_already_took_says_where() {
        let viewer = me();
        let snapshot = snap(waiting_prs(), vec![]);
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let message = Dm {
            today: "d",
            obs: &obs[..1],
            claimed_lane: HashMap::from([(obs[0].key.clone(), "Doing")]),
            cap: 3,
            ..Dm::default()
        }
        .render();
        assert!(message.contains("(in your Doing)"), "{message}");
    }

    /// Half days round to even, as the card has always shown them.
    #[test]
    fn a_cards_age_rounds_half_days_to_even() {
        let item = pr(7);
        for (age_days, want) in [(0.5, "0d"), (1.5, "2d"), (2.5, "2d"), (3.5, "4d")] {
            let opp = Opportunity {
                key: item.key.clone(),
                item: &item,
                sources: vec![Source::Unreviewed],
                note: String::new(),
                age_days,
                areas: vec![],
                merge: None,
            };
            let card = opp_card(&opp, "");
            assert!(card.ends_with(&format!(" · {want}")), "{age_days}: {card}");
        }
    }
}
