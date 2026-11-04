//! Floresta triage: what is waiting on you, and one short DM a morning about what is
//! new in it.
//!
//! Order of operations matters:
//!   1. Read the board FIRST -- mutes, tombstones and claims are user state and must
//!      be honoured before anything is decided.
//!   2. Gather deterministically: one snapshot of the repo, then obligations,
//!      unshipped work and opportunities as pure functions of it. A source that fails
//!      leaves what it fed as it was, and the run says so.
//!   3. Ask the model for gists and an opportunity ranking. Either can fail; the
//!      board and the DM are still written, from computed reasons and ranking order.
//!   4. Write the board deterministically, on top of the file as it is by then. Only
//!      then write briefings, which can take minutes and must not hold the board up.
//!   5. DM if a slot is due.

mod board;
mod brief;
mod claims;
mod config;
mod footprint;
mod gh;
mod model;
mod notify;
mod obligations;
mod opportunities;
mod proc;
mod project;
mod prompts;
mod render;
mod snapshot;
#[cfg(test)]
mod testkit;
mod text;
mod timestamp;
mod unshipped;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use chrono::{DateTime, Days, FixedOffset, Local, SecondsFormat, TimeDelta, Utc};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Deserializer, Serialize};

use board::{AgentLane, Board, Card, CardKey, Changes, Claim, ClaimLane, GitHubItem};
use brief::{BriefKind, Target};
use footprint::Footprint;
use model::Ranked;
use obligations::{Obligation, Reason, ReasonKind, Seen, Viewer};
use opportunities::Opportunity;
use render::{Degraded, Dm};
use snapshot::{Item, Snapshot};
use text::basename;
use timestamp::Timestamp;
use unshipped::{Origin, Unshipped};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    tool: Tool,
}

#[derive(Subcommand)]
enum Tool {
    /// Refresh the board, and DM when a slot is due
    Refresh(Refresh),
    /// Survey the org board's columns
    Survey,
}

#[derive(Args)]
struct Refresh {
    /// The kanban file
    #[arg(long, default_value_os_t = config::default_board())]
    board: PathBuf,
    /// Where caches and the last-run record live
    #[arg(long)]
    state: PathBuf,
    /// Render to stdout, do not touch the vault or send anything
    #[arg(long, conflicts_with = "notify")]
    dry_run: bool,
    /// Skip gists and the judge
    #[arg(long)]
    no_model: bool,
    /// Skip writing briefing notes for newly claimed cards
    #[arg(long)]
    no_briefings: bool,
    /// Briefing notes to write in one run
    #[arg(long, default_value_t = config::MAX_BRIEFINGS)]
    max_briefings: usize,
    /// Beat the heartbeat, and DM when a slot is due
    #[arg(long)]
    notify: bool,
    /// DM regardless of the schedule
    #[arg(long, requires = "notify")]
    dm_now: bool,
}

fn log(message: &str) {
    eprintln!("[{}] {message}", Local::now().format("%H:%M:%S"));
}

/// One run at a time: a manual one and the timer's would share every temporary file.
/// `None` if another run holds it. The kernel releases it when this process ends.
fn take_lock(state: &Path) -> Result<Option<File>> {
    let file = File::create(state.join("lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(err.into()),
    }
}

// ---------------------------------------------------------------- schedule

/// The DM slot this run should answer for, or `None`.
///
/// The timer is hourly; the DM is once a day. A run sends when the most recent slot
/// at or before `now` has not been answered yet. A missed slot (the desktop was off)
/// is sent by the first run after it -- unless it is so old that the next one is the
/// better message.
fn due_slot(
    now: DateTime<FixedOffset>,
    slots: &[(u32, u32)],
    last_sent: Option<DateTime<FixedOffset>>,
) -> Option<DateTime<FixedOffset>> {
    let mut latest = None;
    for &(hour, minute) in slots {
        for back in [0, 1] {
            let slot = (now.date_naive() - Days::new(back))
                .and_hms_opt(hour, minute, 0)
                .and_then(|naive| naive.and_local_timezone(now.timezone()).single())
                .filter(|slot| *slot <= now);
            latest = latest.max(slot);
        }
    }
    let slot = latest.filter(|slot| now - *slot <= TimeDelta::hours(config::SLOT_GRACE_HOURS))?;
    last_sent.is_none_or(|sent| sent < slot).then_some(slot)
}

/// The record of the last DM. Any corruption reads as never sent, because a duplicate
/// DM is the cheap failure.
#[derive(Default, Deserialize, Serialize)]
struct LastDm {
    at: Option<DateTime<FixedOffset>>,
    #[serde(default, deserialize_with = "seen_records")]
    seen: BTreeMap<CardKey, Seen>,
}

/// Parsed once, here. Earlier versions wrote a bare time for how far you had read, and a
/// bare head for an opportunity, which says nothing about an obligation.
fn seen_records<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<CardKey, Seen>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Wire {
        Triggers(Seen),
        Legacy(String),
    }
    let wire = BTreeMap::<CardKey, Wire>::deserialize(deserializer)?;
    let records = wire.into_iter().filter_map(|(key, record)| match record {
        Wire::Triggers(told) => Some((key, told)),
        Wire::Legacy(text) => Timestamp::parse(&text).map(|at| (key, Seen::read_up_to(at))),
    });
    Ok(records.collect())
}

// ---------------------------------------------------------------- gathering

/// `fork#N` or a named branch, as a card says it.
fn unshipped_name(u: &Unshipped, quote: &str) -> String {
    match &u.origin {
        Origin::ForkPr { number, .. } => format!("fork#{number}"),
        Origin::Branch => format!("{quote}{}{quote}", u.branch),
    }
}

/// An unshipped fix for an issue that is yours -- assigned to you, or one you were
/// @-mentioned on -- is an obligation on that issue, not a card in the Unshipped
/// lane. Returns the keys promoted.
fn promote_unshipped<'a>(
    obs: &mut Vec<Obligation<'a>>,
    items: &[Unshipped],
    snap: &'a Snapshot,
    me: &Viewer,
) -> HashSet<CardKey> {
    let mut promoted = HashSet::new();
    for u in items {
        let number = u.issue.as_ref().map(|issue| issue.number);
        let Some((issue, _)) = snap.issues().find(|(i, _)| Some(i.number) == number) else {
            continue;
        };
        let mentioned = me.is_mentioned_in(&issue.body)
            || issue.comments.iter().any(|c| me.is_mentioned_in(&c.body));
        if !mentioned && !issue.assignees.contains(&me.login) {
            continue;
        }
        let what = match &u.origin {
            Origin::ForkPr { .. } => unshipped_name(u, ""),
            Origin::Branch => format!("branch {}", unshipped_name(u, "`")),
        };
        let at = Timestamp::at_midnight(u.last);
        let text = format!("your {what} would fix this and never went upstream");
        let reason = Reason::standing(ReasonKind::Unshipped, at, text);
        obligations::add_reason(obs, issue, reason);
        promoted.insert(u.key.clone());
    }
    promoted
}

/// Say whether the push past your ACK was a rebase or new code. Computed, not
/// guessed: the PR's diff at the ACKed commit against its diff now.
fn enrich_stale_acks(obs: &mut [Obligation<'_>], me: &Viewer) {
    for ob in obs {
        let item = ob.item;
        let Some(pr) = item.pr() else { continue };
        if !ob.has_kind(ReasonKind::StaleAck) {
            continue;
        }
        let Some((acked, _)) = obligations::latest_ack(item, pr, me) else {
            continue;
        };
        let Some(diff) = snapshot::ack_interdiff(config::REPO, &acked, &pr.head) else {
            continue;
        };
        let suffix = if diff.is_rebase_only() {
            " · rebase only, re-ACK is quick".to_owned()
        } else {
            let files: Vec<&str> = diff.changed.iter().take(3).map(|f| basename(f)).collect();
            let more = match diff.changed.len() {
                n if n > 3 => format!(" +{}", n - 3),
                _ => String::new(),
            };
            format!(" · code changed in {}{more}", files.join(", "))
        };
        for reason in &mut ob.reasons {
            if reason.kind == ReasonKind::StaleAck {
                reason.text.push_str(&suffix);
            }
        }
        ob.interdiff = Some(diff);
    }
}

/// What the briefing pass did: the links to add to cards, and how many notes it wrote.
#[derive(Default)]
struct Briefings {
    links: Vec<(CardKey, String)>,
    written: usize,
}

/// Briefing notes for newly claimed cards. A note under its real name is complete, so
/// one that exists only needs linking. A linked card with no note is one whose note
/// you deleted, and it is left that way.
fn write_briefings(
    claimed: &[Claim],
    snap: &Snapshot,
    me: &str,
    vault_dir: &Path,
    limit: usize,
    today: &str,
) -> Briefings {
    let items: HashMap<&CardKey, &Item> = snap.items.iter().map(|item| (&item.key, item)).collect();
    let mut out = Briefings::default();
    for claim in claimed {
        let Some(item) = items.get(&claim.key).copied() else {
            continue; // closed, or not upstream
        };
        if claim.lane == ClaimLane::Done || (item.pr().is_some() && item.author == me) {
            continue; // finished, or you wrote it
        }
        let path = brief::note_path(vault_dir, config::REPO, item.number);
        let link = brief::wikilink(&path);
        if claim.line.contains(&link) {
            continue;
        }
        if path.exists() {
            out.links.push((claim.key.clone(), link));
            continue;
        }
        if out.written >= limit {
            continue;
        }
        let kind = if item.pr().is_some() {
            BriefKind::Review
        } else {
            BriefKind::Issue
        };
        log(&format!("briefing {} ({})...", claim.key, kind.as_str()));
        let target = Target {
            repo: config::REPO,
            number: item.number,
            url: &item.url,
            kind,
            today,
            me,
        };
        match brief::write_briefing(vault_dir, &target) {
            Err(why) => log(&format!("  skipped ({why})")),
            Ok(written) => {
                log(&format!("  written: {}", written.display()));
                out.links.push((claim.key.clone(), link));
                out.written += 1;
            }
        }
    }
    out
}

#[derive(Serialize)]
struct Counts {
    obligations: usize,
    muted: usize,
    opportunities: usize,
    candidates: usize,
    unshipped: usize,
    judged: bool,
    gists: usize,
}

#[derive(Serialize)]
struct LastRun<'a> {
    generated: String,
    degraded: &'a [&'a str],
    counts: Counts,
}

fn run(args: &Refresh) -> Result<()> {
    fs::create_dir_all(&args.state)?;
    let Some(_lock) = take_lock(&args.state)? else {
        log("another run holds the lock, so this one leaves the work to it");
        return Ok(());
    };
    let state_path = |name: &str| args.state.join(name);
    let now = Utc::now();
    let local = now.with_timezone(&Local).fixed_offset();
    let vault_dir = args.board.parent().unwrap_or(Path::new(".")).to_owned();
    let mut degraded: BTreeSet<Degraded> = BTreeSet::new();
    // The kinds of reason this run cannot say anything about.
    let mut unread_kinds: Vec<ReasonKind> = Vec::new();

    // 1. user state first
    let board = Board::open(&args.board)?;
    let tombstones = board.tombstones();
    let mutes = board.mutes();
    let claimed = board.claimed();
    log(&format!(
        "board: {} · {} passed ({} muted) · {} claimed",
        if board.existed() { "existing" } else { "new" },
        tombstones.len(),
        mutes.len(),
        claimed.len()
    ));

    // 2. gather. The snapshot is the one source that may fail the run: without it
    //    every lane would be computed from nothing, so the board is left exactly as
    //    it was and the heartbeat goes down.
    let login = gh::viewer_login()?;
    let me = Viewer::new(&login);
    log(&format!("snapshot of {} as {login}...", config::REPO));
    let snap = snapshot::fetch(config::REPO)?;
    log(&format!(
        "  {} open PRs, {} open issues",
        snap.prs().count(),
        snap.issues().count()
    ));
    let my_prs: Vec<u64> = snap
        .prs()
        .filter(|(item, _)| item.author == login)
        .map(|(item, _)| item.number)
        .collect();
    let merge = snapshot::merge_state(config::REPO, &my_prs);
    if !merge.unread.is_empty() {
        log(&format!("  merge state unread for {:?}", merge.unread));
        degraded.insert(Degraded::Merge);
        unread_kinds.extend([ReasonKind::Conflict, ReasonKind::CiFailed]);
    }
    let computing = merge.computing();
    if !computing.is_empty() {
        log(&format!(
            "  GitHub is still computing conflicts for {computing:?}"
        ));
        degraded.insert(Degraded::MergeComputing);
        unread_kinds.push(ReasonKind::Conflict);
    }
    let mut obs = obligations::collect(&snap, &me, now, &merge.states);
    enrich_stale_acks(&mut obs, &me);

    // Neither of these may cost the board. A failed source leaves its lane as it
    // was: stale cards are recoverable, deleted ones are not.
    let open_issues: Vec<&Item> = snap.issues().map(|(item, _)| item).collect();
    let unshipped_result = snapshot::mine(config::REPO, &login)
        .map_err(anyhow::Error::from)
        .and_then(|mine| unshipped::run(&open_issues, &mine, &login));
    let read_unshipped = match unshipped_result {
        Ok(items) => Some(items),
        Err(err) => {
            log(&format!("  unshipped failed ({err:#})"));
            degraded.insert(Degraded::Unshipped);
            unread_kinds.push(ReasonKind::Unshipped);
            None
        }
    };
    let items = read_unshipped.as_deref().unwrap_or_default();
    let promoted = promote_unshipped(&mut obs, items, &snap, &me);
    // Work you have already done for an issue that is waiting on you is worth saying
    // on that card, even when it isn't yours enough to promote.
    for u in items.iter().filter(|u| !promoted.contains(&u.key)) {
        let Some(number) = u.issue.as_ref().map(|issue| issue.number) else {
            continue;
        };
        for ob in obs.iter_mut().filter(|ob| ob.item.number == number) {
            let what = unshipped_name(u, "`");
            ob.notes.push(format!("⚡ your {what} may fix this"));
        }
    }
    let is_claimed = |key: &CardKey| claimed.iter().any(|claim| claim.key == *key);
    let shelf: Vec<&Unshipped> = items
        .iter()
        .filter(|u| {
            !promoted.contains(&u.key) && !tombstones.contains(&u.key) && !is_claimed(&u.key)
        })
        .collect();

    let visible: Vec<Obligation<'_>> = obs
        .iter()
        .filter(|ob| !obligations::is_muted(ob, mutes.get(&ob.item.key)))
        .cloned()
        .collect();
    let n_muted = obs.len() - visible.len();
    let promoted_note = if promoted.is_empty() {
        String::new()
    } else {
        format!(" · {} promoted", promoted.len())
    };
    log(&format!(
        "  {} obligations ({n_muted} muted) · {} unshipped{promoted_note}",
        visible.len(),
        shelf.len()
    ));

    let branch_refs: Vec<String> = items
        .iter()
        .filter(|u| matches!(u.origin, Origin::Branch))
        .map(|u| format!("{}..{}", config::UPSTREAM_REF, u.branch))
        .collect();
    let clones = config::clones();
    // Ranking context, never a filter, so it is not worth the board either.
    let footprint_result = unshipped::my_emails(&clones)
        .and_then(|emails| footprint::compute(&emails, &clones, &branch_refs));
    let fp = footprint_result.unwrap_or_else(|err| {
        log(&format!("  footprint unread ({err:#})"));
        degraded.insert(Degraded::Footprint);
        Footprint::default()
    });

    let mut column = HashSet::new();
    if config::PROJECT.enabled {
        match project::needs_review(config::REPO, &config::PROJECT) {
            Ok(numbers) => column = numbers,
            Err(err) => {
                log(&format!("  project board unreadable ({err})"));
                degraded.insert(Degraded::Project);
            }
        }
    }
    let skip: HashSet<CardKey> = obs
        .iter()
        .map(|ob| ob.item.key.clone())
        .chain(claimed.iter().map(|claim| claim.key.clone()))
        .collect();
    let mut opps: Vec<Opportunity<'_>> =
        opportunities::collect(&snap, &me, now, &skip, &tombstones, &fp, &column);
    let pr_numbers: Vec<u64> = opps
        .iter()
        .filter(|o| o.item.pr().is_some())
        .map(|o| o.item.number)
        .collect();
    let opp_merge = snapshot::merge_state(config::REPO, &pr_numbers);
    if !opp_merge.unread.is_empty() {
        log(&format!("  merge state unread for {:?}", opp_merge.unread));
        degraded.insert(Degraded::Merge);
    }
    for o in &mut opps {
        o.merge = opp_merge.states.get(&o.item.number).copied();
    }
    opportunities::fallback_rank(&mut opps, &fp);
    log(&format!("  {} opportunity candidates", opps.len()));

    // 3. model. Gists are decoration; the ranking falls back to fallback_rank.
    let (gists, ranked) = if args.no_model {
        (
            BTreeMap::new(),
            Ranked::mechanical(&opps, config::OPPORTUNITY_CAP),
        )
    } else {
        let gists = model::gists(&visible, &me, &state_path("gists.json"), model::claude)?;
        let ranked = model::rank(
            &opps,
            &fp,
            config::OPPORTUNITY_CAP,
            &state_path("rank.json"),
            config::JUDGE_TTL_HOURS,
            model::claude,
        )?;
        (gists, ranked)
    };
    log(&format!(
        "  {} gists · ranking {}",
        gists.len(),
        if ranked.is_judged() {
            "judged"
        } else {
            "mechanical"
        }
    ));

    // 4. write. An obligation you have dragged into Todo/Doing is yours already: it
    //    stays on that card, as an annotation, instead of appearing twice.
    let active: HashMap<CardKey, ClaimLane> = claimed
        .iter()
        .filter(|claim| claim.lane.is_active())
        .map(|claim| (claim.key.clone(), claim.lane))
        .collect();
    let mut needs_me: Vec<Card> = visible
        .iter()
        .filter(|ob| !active.contains_key(&ob.item.key))
        .map(|ob| render::ob_card(ob, gists.get(&ob.item.key).map(|gist| gist.text.as_str())))
        .collect();
    let computed: HashSet<&CardKey> = visible.iter().map(|ob| &ob.item.key).collect();
    needs_me.extend(board.carried(&unread_kinds, &computed));
    let mut changes = Changes::default();
    changes.lanes.push((AgentLane::NeedsMe, needs_me));
    changes.lanes.push((
        AgentLane::Opportunities,
        ranked
            .picks()
            .iter()
            .map(|(opp, verdict)| render::opp_card(opp, verdict.and_then(|v| v.why.as_deref())))
            .collect(),
    ));
    if read_unshipped.is_some() {
        changes.lanes.push((
            AgentLane::Unshipped,
            shelf.iter().map(|u| render::unshipped_card(u)).collect(),
        ));
    } else {
        log("  Unshipped left untouched: its source failed");
    }

    let on_github: Vec<GitHubItem> = claimed
        .iter()
        .filter_map(|claim| claim.key.github().cloned())
        .collect();
    let status = claims::fetch(&on_github, &login);
    if on_github.iter().any(|item| !status.contains_key(item)) {
        log("  some claimed cards could not be read");
        degraded.insert(Degraded::Claims);
    }
    let mut claim_alerts: Vec<String> = Vec::new();
    for claim in &claimed {
        let held = match claim.key.github() {
            // Not read, which says nothing, so the card keeps what it says.
            Some(item) if !status.contains_key(item) => continue,
            Some(item) => status.get(item),
            None => None,
        };
        let mut notes = Vec::new();
        if let Some(ob) = visible.iter().find(|ob| ob.item.key == claim.key) {
            notes.push(render::reasons_text(ob));
        }
        notes.extend(held.iter().flat_map(|s| s.notes.iter().cloned()));
        changes.notes.push((claim.key.clone(), notes.join(" · ")));
        if let (Some(held), Some(item)) = (held.filter(|s| s.urgent), claim.key.github()) {
            claim_alerts.push(format!("#{} — {}", item.number(), held.notes.join(" · ")));
        }
    }

    if args.dry_run {
        println!("{}", board.preview(changes)?);
    } else {
        if board.commit(changes)? {
            log("the board changed during this run; what you did is kept");
        }
        log(&format!("wrote {}", args.board.display()));
    }

    let today = local.date_naive().to_string();
    if !args.no_briefings && !args.dry_run {
        let done = write_briefings(
            &claimed,
            &snap,
            &login,
            &vault_dir,
            args.max_briefings,
            &today,
        );
        if !done.links.is_empty() {
            board.commit(Changes {
                links: done.links,
                ..Changes::default()
            })?;
        }
    }

    // 5. DM
    let last_dm = match model::read_state::<LastDm>(&state_path("last_dm.json")) {
        Ok(found) => found.unwrap_or_default(),
        Err(err) => {
            log(&format!("  last_dm.json unreadable ({err})"));
            degraded.insert(Degraded::LastDm);
            LastDm::default()
        }
    };
    let waiting = obligations::new_asks(&visible, &last_dm.seen, |key| {
        gists.get(key).map(|gist| gist.asks)
    });
    let (asks, held) = waiting.split_at(waiting.len().min(config::DM_ASKS));
    let n_gists = gists.len();
    let n_picks = ranked.picks().len();
    let judged = ranked.is_judged();
    let text = Dm {
        asks,
        on_board: visible.len() - asks.len(),
        gists: &gists,
        claimed_lane: &active,
        degraded: &degraded,
        claim_alerts: &claim_alerts,
    }
    .render();
    fs::write(state_path("summary.txt"), &text)?;
    let slot = due_slot(local, &config::DM_SLOTS, last_dm.at);
    if args.dry_run {
        println!("{text}");
    } else if args.notify && (slot.is_some() || args.dm_now) {
        let results = notify::dispatch(&text);
        for delivery in &results {
            let verdict = if delivery.ok { "ok" } else { "FAILED" };
            log(&format!(
                "notify {}: {verdict} — {}",
                delivery.transport, delivery.detail
            ));
        }
        if results.iter().any(|delivery| delivery.ok) {
            let record = LastDm {
                at: Some(local),
                seen: obligations::seen_after(&obs, held, &last_dm.seen, &unread_kinds),
            };
            model::save(&state_path("last_dm.json"), &record)?;
        }
    } else if args.notify {
        let sent = last_dm.at.map_or("never".to_owned(), |at| at.to_rfc3339());
        log(&format!("DM not due; last sent {sent}"));
    }

    let degraded: Vec<&str> = degraded.iter().map(|source| source.as_str()).collect();
    let record = LastRun {
        generated: now.to_rfc3339_opts(SecondsFormat::Micros, false),
        degraded: &degraded,
        counts: Counts {
            obligations: visible.len(),
            muted: n_muted,
            opportunities: n_picks,
            candidates: opps.len(),
            unshipped: shelf.len(),
            judged,
            gists: n_gists,
        },
    };
    fs::write(
        state_path("last_run.json"),
        serde_json::to_string_pretty(&record)?,
    )?;

    if args.notify {
        // A lane that silently goes stale every run is the exact failure this monitor
        // exists to catch, so say so in the beat.
        let degraded_note = if degraded.is_empty() {
            String::new()
        } else {
            format!(" · degraded: {}", degraded.join(","))
        };
        let message = format!("{} need you · {n_picks} opp{degraded_note}", visible.len(),);
        let push_url = std::env::var("UPTIME_KUMA_PUSH_URL").ok();
        let (ok, detail) = notify::heartbeat(push_url.as_deref(), "up", &message);
        log(&format!(
            "heartbeat: {} — {detail}",
            if ok { "ok" } else { "skipped" }
        ));
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let (result, notify) = match &cli.tool {
        Tool::Survey => (project::survey().map_err(anyhow::Error::from), false),
        Tool::Refresh(args) => (run(args), args.notify),
    };
    let Err(err) = result else {
        return ExitCode::SUCCESS;
    };
    log(&format!("FAILED: {err:#}"));
    if notify {
        let push_url = std::env::var("UPTIME_KUMA_PUSH_URL").ok();
        notify::heartbeat(push_url.as_deref(), "down", &format!("{err:#}"));
    }
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obligations::{collect, new_asks};
    use crate::snapshot::MergeState;
    use crate::testkit::{ME, c, day, me, now, pr, snap};
    use chrono::TimeZone;

    const SLOTS: [(u32, u32); 1] = [(8, 7)];

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<FixedOffset> {
        FixedOffset::west_opt(3 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 10, day, hour, minute, 0)
            .unwrap()
    }

    #[test]
    fn once_a_morning_from_an_hourly_timer() {
        type Clock = (u32, u32, u32);
        let cases: [(&str, Clock, Option<Clock>, Option<Clock>); 7] = [
            (
                "the first run after the slot sends",
                (15, 8, 7),
                Some((14, 8, 7)),
                Some((15, 8, 7)),
            ),
            (
                "the next hourly run does not",
                (15, 9, 7),
                Some((15, 8, 7)),
                None,
            ),
            (
                "nothing goes out in the evening",
                (15, 19, 23),
                Some((15, 8, 7)),
                None,
            ),
            (
                "a run before the slot does not",
                (16, 7, 7),
                Some((15, 8, 7)),
                None,
            ),
            (
                "a slot missed while the desktop was off is sent late",
                (15, 11, 30),
                Some((14, 8, 7)),
                Some((15, 8, 7)),
            ),
            (
                "one missed by more than the grace waits for tomorrow",
                (15, 17, 0),
                Some((14, 8, 7)),
                None,
            ),
            ("a first run ever sends", (15, 8, 7), None, Some((15, 8, 7))),
        ];
        let clock = |(day, hour, minute): Clock| at(day, hour, minute);
        for (name, now, last, want) in cases {
            let got = due_slot(clock(now), &SLOTS, last.map(clock));
            assert_eq!(got, want.map(clock), "{name}");
        }
    }

    /// The previous version of this program, and every run since, wrote these.
    #[test]
    fn a_last_dm_from_an_earlier_run_is_understood() {
        let record: LastDm = serde_json::from_str(
            r#"{"at": "2026-10-15T08:07:14.123456-03:00",
                "seen": {"getfloresta/Floresta#7": "2026-10-08T17:14:49Z"}}"#,
        )
        .unwrap();
        assert_eq!(due_slot(at(15, 9, 7), &SLOTS, record.at), None);
        assert!(
            record
                .seen
                .contains_key(&"getfloresta/floresta#7".parse().unwrap())
        );
    }

    /// Whether item 97 is new to a `last_dm.json` holding this entry for it.
    fn is_new_to(item: Item, merge: &HashMap<u64, MergeState>, entry: &str) -> bool {
        let text = format!(r#"{{"at": null, "seen": {{"getfloresta/Floresta#97": {entry}}}}}"#);
        let record: LastDm = serde_json::from_str(&text).unwrap();
        let snapshot = snap(vec![item], vec![]);
        let viewer = me();
        let obs = collect(&snapshot, &viewer, now(), merge);
        !new_asks(&obs, &record.seen, |_| Some(true)).is_empty()
    }

    #[test]
    fn what_a_record_says_about_a_mention() {
        let mention = format!("@{ME} ping");
        let at_day = |n| format!(r#""{}""#, day(n));
        let cases = [
            ("a bare time at the mention", at_day(5), false),
            ("a bare time before the mention", at_day(6), true),
            (
                "a bare head says nothing",
                r#""d6c385e521c36cc9c24bc534db3f4b1693194a20""#.to_owned(),
                true,
            ),
            (
                "a kind no longer known does not cost the file",
                format!(
                    r#"{{"a_retired_kind": {{"last": "{0}"}}, "mention": {{"last": "{0}"}}}}"#,
                    day(5)
                ),
                false,
            ),
        ];
        for (name, entry, new) in cases {
            let item = pr(97).comments(vec![c("bob", 5, &mention)]);
            assert_eq!(is_new_to(item, &HashMap::new(), &entry), new, "{name}");
        }
    }

    /// A bare time names no head, so the morning after deploy repeats this once.
    #[test]
    fn a_conflict_on_a_bare_time_reads_as_new() {
        let merge = HashMap::from([(97, MergeState::conflicting())]);
        let item = pr(97).author(ME).pushed_at(day(5));
        assert!(is_new_to(item, &merge, &format!(r#""{}""#, day(0))));
    }
}
