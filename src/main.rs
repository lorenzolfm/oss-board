//! Floresta triage: what is blocked on you, then what is worth your evening.
//!
//! Order of operations matters:
//!   1. Read the board FIRST -- mutes, tombstones and claims are user state and must
//!      be honoured before anything is decided.
//!   2. Gather deterministically: one snapshot of the repo, then obligations,
//!      unshipped work and opportunities as pure functions of it.
//!   3. Ask the model for gists and an opportunity ranking. Either can fail; the
//!      board and the DM are still written, from computed reasons and ranking order,
//!      and say so.
//!   4. Write the board deterministically, then DM if a slot is due.

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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use chrono::{DateTime, Days, FixedOffset, Local, SecondsFormat, TimeDelta, Utc};
use clap::{Parser, Subcommand};
use serde::Serialize;
use serde_json::{Value, json};

use board::{AgentLane, Board, Claim};
use brief::{BriefKind, Target};
use model::{Pick, Ranked};
use obligations::{Obligation, Reason, ReasonKind, Viewer};
use opportunities::Opportunity;
use render::Dm;
use snapshot::{Item, Snapshot};
use text::basename;
use timestamp::Timestamp;
use unshipped::{Origin, Unshipped};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// The kanban file [default: ~/Documents/Vault/Opensource/Tasks.md]
    #[arg(long)]
    board: Option<PathBuf>,
    /// Where caches and the last-run record live
    #[arg(long, default_value = "state")]
    state: PathBuf,
    /// Render to stdout, do not touch the vault or send anything
    #[arg(long)]
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
    /// DM regardless of the schedule (with --notify)
    #[arg(long)]
    dm_now: bool,
    #[command(subcommand)]
    command: Option<Tool>,
}

#[derive(Subcommand)]
enum Tool {
    /// Survey the org board's columns
    Survey,
}

fn log(message: &str) {
    eprintln!("[{}] {message}", Local::now().format("%H:%M:%S"));
}

fn is_floresta(key: &str) -> bool {
    let key = key.to_lowercase();
    [config::REPO, config::FORK]
        .iter()
        .any(|repo| key.starts_with(&format!("{}#", repo.to_lowercase())))
}

// ---------------------------------------------------------------- schedule

/// The DM slot this run should answer for, or `None`.
///
/// The timer is hourly; the DM is twice a day. A run sends when the most recent slot
/// at or before `now` has not been answered yet. A missed slot (the desktop was off)
/// is sent by the first run after it -- unless it is so old that the next one is the
/// better message.
fn due_slot(
    now: DateTime<FixedOffset>,
    slots: &[(u32, u32)],
    last_sent: Option<&str>,
) -> Result<Option<DateTime<FixedOffset>>> {
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
    let Some(slot) = latest else {
        return Ok(None);
    };
    if now - slot > TimeDelta::hours(config::SLOT_GRACE_HOURS) {
        return Ok(None);
    }
    if let Some(last_sent) = last_sent {
        let last_sent = DateTime::parse_from_rfc3339(last_sent)
            .with_context(|| format!("last_dm.json has a bad timestamp: {last_sent}"))?;
        if last_sent >= slot {
            return Ok(None);
        }
    }
    Ok(Some(slot))
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
) -> HashSet<String> {
    let mut promoted = HashSet::new();
    for u in items {
        let number = u.issue.as_ref().map(|issue| issue.number);
        let Some(issue) = snap.issues.iter().find(|i| Some(i.number) == number) else {
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
        let at = Timestamp::parse(&format!("{}T00:00:00Z", u.last));
        let text = format!("your {what} would fix this and never went upstream");
        obligations::add_reason(obs, issue, Reason::new(ReasonKind::Unshipped, at, text));
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
        let Some(head) = pr.head.as_deref() else {
            continue;
        };
        let Some(diff) = snapshot::ack_interdiff(config::REPO, &acked, head) else {
            continue;
        };
        let suffix = if diff.rebase_only {
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

/// Briefing notes for newly claimed cards. After the lanes are set, so a briefing
/// failure cannot cost the board write.
fn write_briefings(
    board: &mut Board,
    claimed: &[Claim],
    snap: &Snapshot,
    me: &str,
    vault_dir: &Path,
    limit: usize,
    today: &str,
) -> Vec<String> {
    let items: HashMap<&str, &Item> = snap
        .prs
        .iter()
        .chain(&snap.issues)
        .map(|item| (item.key.as_str(), item))
        .collect();
    let mut out = Vec::new();
    for claim in claimed {
        if out.len() >= limit {
            break;
        }
        let Some(item) = items.get(claim.key.as_str()).copied() else {
            continue; // closed, or not upstream
        };
        if claim.lane == "Done" || (item.pr().is_some() && item.author == me) {
            continue; // finished, or you wrote it
        }
        let Some((repo, _)) = claim.key.rsplit_once('#') else {
            continue;
        };
        let path = brief::note_path(vault_dir, repo, item.number);
        if path.exists() || claim.line.contains(&brief::wikilink(&path)) {
            continue;
        }
        let kind = if item.pr().is_some() {
            BriefKind::Review
        } else {
            BriefKind::Issue
        };
        log(&format!("briefing {} ({})...", claim.key, kind.as_str()));
        let target = Target {
            repo,
            number: item.number,
            url: &item.url,
            kind,
            today,
            me,
        };
        match brief::write_briefing(vault_dir, &target) {
            Err(why) => log(&format!("  skipped ({why})")),
            Ok((written, why)) => {
                log(&format!("  {why}: {}", written.display()));
                board.link_note(&claim.key, &brief::wikilink(&written));
                out.push(format!("#{}", item.number));
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

fn run(args: &Args) -> Result<()> {
    fs::create_dir_all(&args.state)?;
    let state_path = |name: &str| args.state.join(name);
    let board_path = args.board.clone().unwrap_or_else(config::default_board);
    let now = Utc::now();
    let local = now.with_timezone(&Local).fixed_offset();
    let vault_dir = board_path.parent().unwrap_or(Path::new(".")).to_owned();
    let mut warnings: Vec<String> = Vec::new();
    let mut degraded: Vec<&str> = Vec::new();

    // 1. user state first
    let mut board = Board::open(&board_path)?;
    let dropped = board.migrate(is_floresta);
    if dropped > 0 {
        log(&format!(
            "board: migrated from the multi-repo layout, {dropped} non-Floresta cards dropped"
        ));
    }
    let tombstones = board.tombstones();
    let mutes = board.mutes();
    let claimed = board.claimed();
    log(&format!(
        "board: {} · {} passed ({} muted) · {} claimed",
        if board.existed { "existing" } else { "new" },
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
        snap.prs.len(),
        snap.issues.len()
    ));
    let my_prs: Vec<u64> = snap
        .prs
        .iter()
        .filter(|item| item.author == login)
        .map(|item| item.number)
        .collect();
    let merge = snapshot::merge_state(config::REPO, &my_prs);
    let mut obs = obligations::collect(&snap, &me, now, &merge);
    enrich_stale_acks(&mut obs, &me);

    // Neither of these may cost the board. A failed source leaves its lane as it
    // was: stale cards are recoverable, deleted ones are not.
    let unshipped_result = snapshot::mine(config::REPO, &login)
        .map_err(anyhow::Error::from)
        .and_then(|mine| unshipped::run(&snap.issues, &mine, &login));
    let (items, unshipped_ok) = match unshipped_result {
        Ok(items) => (items, true),
        Err(err) => {
            log(&format!("  unshipped failed ({err:#})"));
            degraded.push("unshipped");
            (Vec::new(), false)
        }
    };
    let promoted = promote_unshipped(&mut obs, &items, &snap, &me);
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
    let is_claimed = |key: &str| claimed.iter().any(|claim| claim.key == key);
    let shelf: Vec<&Unshipped> = items
        .iter()
        .filter(|u| {
            !promoted.contains(&u.key) && !tombstones.contains(&u.key) && !is_claimed(&u.key)
        })
        .collect();

    let visible: Vec<Obligation<'_>> = obs
        .iter()
        .filter(|ob| !obligations::is_muted(ob, mutes.get(&ob.key)))
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
    let fp = footprint::compute(&unshipped::my_emails(&clones)?, &clones, &branch_refs)?;

    let mut column = HashSet::new();
    if config::PROJECT.enabled {
        match project::needs_review(config::REPO, &config::PROJECT) {
            Ok(numbers) => column = numbers,
            Err(err) => {
                log(&format!("  project board unreadable ({err})"));
                warnings.push(
                    "project board unreadable — does the token have read:project?".to_owned(),
                );
                degraded.push("project");
            }
        }
    }
    let skip: HashSet<String> = obs
        .iter()
        .map(|ob| ob.key.clone())
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
    for o in &mut opps {
        o.merge = opp_merge.get(&o.item.number).cloned();
    }
    opportunities::fallback_rank(&mut opps, &fp);
    log(&format!("  {} opportunity candidates", opps.len()));

    // 3. model. Gists are decoration; the ranking falls back to fallback_rank.
    let (gists, Ranked { picks, judged }) = if args.no_model {
        let picks = opps
            .iter()
            .take(config::OPPORTUNITY_CAP)
            .map(Pick::mechanical)
            .collect();
        (
            BTreeMap::new(),
            Ranked {
                picks,
                judged: false,
            },
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
        if judged { "judged" } else { "mechanical" }
    ));

    // 4. write. An obligation you have dragged into Todo/Doing is yours already: it
    //    stays on that card, as an annotation, instead of appearing twice.
    let active: HashMap<String, &'static str> = claimed
        .iter()
        .filter(|claim| matches!(claim.lane, "Todo" | "Doing"))
        .map(|claim| (claim.key.clone(), claim.lane))
        .collect();
    board.set_agent_lane(
        AgentLane::NeedsMe,
        visible
            .iter()
            .filter(|ob| !active.contains_key(&ob.key))
            .map(|ob| render::ob_card(ob, gists.get(&ob.key).map(String::as_str)))
            .collect(),
    );
    board.set_agent_lane(
        AgentLane::Opportunities,
        picks
            .iter()
            .map(|pick| render::opp_card(pick.opp, &pick.why))
            .collect(),
    );
    if unshipped_ok {
        board.set_agent_lane(
            AgentLane::Unshipped,
            shelf.iter().map(|u| render::unshipped_card(u)).collect(),
        );
    } else {
        log("  Unshipped left untouched: its source failed");
    }

    let github_keys: Vec<String> = claimed
        .iter()
        .filter(|claim| board::is_github_key(&claim.key))
        .map(|claim| claim.key.clone())
        .collect();
    let status = claims::fetch(&github_keys, &login);
    let mut claim_alerts: Vec<String> = Vec::new();
    for claim in &claimed {
        let mut notes = Vec::new();
        if let Some(ob) = visible.iter().find(|ob| ob.key == claim.key) {
            notes.push(render::reasons_text(ob));
        }
        let held = status.get(&claim.key);
        notes.extend(held.iter().flat_map(|s| s.notes.iter().cloned()));
        board.annotate(&claim.key, &notes.join(" · "));
        if let Some(held) = held.filter(|s| s.urgent)
            && let Some((_, number)) = claim.key.rsplit_once('#')
        {
            claim_alerts.push(format!("#{number} — {}", held.notes.join(" · ")));
        }
    }

    let today = local.date_naive().to_string();
    let briefed = if args.no_briefings || args.dry_run {
        Vec::new()
    } else {
        write_briefings(
            &mut board,
            &claimed,
            &snap,
            &login,
            &vault_dir,
            args.max_briefings,
            &today,
        )
    };

    if args.dry_run {
        println!("{}", board.render());
    } else {
        board.save()?;
        log(&format!("wrote {}", board_path.display()));
    }

    // 5. DM
    let last_dm = model::load(&state_path("last_dm.json"));
    let mut marks: BTreeMap<String, String> = visible
        .iter()
        .map(|ob| (ob.key.clone(), obligations::fingerprint(ob).0))
        .collect();
    for pick in &picks {
        let item = pick.opp.item;
        let mark = item
            .head()
            .map_or_else(|| item.created.to_string(), str::to_owned);
        marks.insert(pick.opp.key.clone(), mark);
    }
    let new_keys: HashSet<String> = marks
        .iter()
        .filter(|(key, mark)| last_dm["seen"][key.as_str()].as_str() != Some(mark.as_str()))
        .map(|(key, _)| key.clone())
        .collect();
    let n_gists = gists.len();
    let text = Dm {
        today: &local.format("%a %d %b %H:%M").to_string(),
        obs: &visible,
        gists,
        claimed_lane: active,
        picks: &picks,
        judged,
        unshipped: shelf.len(),
        new_keys,
        warnings: &warnings,
        claim_alerts: &claim_alerts,
        muted: n_muted,
        briefed: &briefed,
        cap: config::DM_OPPORTUNITIES,
    }
    .render();
    fs::write(state_path("summary.txt"), &text)?;
    let last_sent = last_dm["at"].as_str().filter(|at| !at.is_empty());
    let slot = due_slot(local, &config::DM_SLOTS, last_sent)?;
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
            let at = local.to_rfc3339_opts(SecondsFormat::Micros, false);
            let record: Value = json!({"at": at, "seen": marks});
            model::save(&state_path("last_dm.json"), &record)?;
        }
    } else if args.notify {
        log(&format!(
            "DM not due; last sent {}",
            last_dm["at"].as_str().unwrap_or("never")
        ));
    }

    let record = LastRun {
        generated: now.to_rfc3339_opts(SecondsFormat::Micros, false),
        degraded: &degraded,
        counts: Counts {
            obligations: visible.len(),
            muted: n_muted,
            opportunities: picks.len(),
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
        let message = format!(
            "{} need you · {} opp{degraded_note}",
            visible.len(),
            picks.len()
        );
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
    let args = Args::parse();
    let result = match args.command {
        Some(Tool::Survey) => project::survey().map_err(anyhow::Error::from),
        None => run(&args),
    };
    let Err(err) = result else {
        return ExitCode::SUCCESS;
    };
    log(&format!("FAILED: {err:#}"));
    if args.notify {
        let push_url = std::env::var("UPTIME_KUMA_PUSH_URL").ok();
        notify::heartbeat(push_url.as_deref(), "down", &format!("{err:#}"));
    }
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const SLOTS: [(u32, u32); 2] = [(8, 7), (19, 23)];

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<FixedOffset> {
        FixedOffset::west_opt(3 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 10, day, hour, minute, 0)
            .unwrap()
    }

    #[test]
    fn twice_a_day_from_an_hourly_timer() {
        type Clock = (u32, u32, u32);
        let cases: [(&str, Clock, Option<Clock>, Option<Clock>); 8] = [
            (
                "the first run after a slot sends",
                (15, 8, 7),
                Some((14, 19, 23)),
                Some((15, 8, 7)),
            ),
            (
                "the next hourly run does not",
                (15, 9, 7),
                Some((15, 8, 7)),
                None,
            ),
            (
                "a run before the evening slot does not",
                (15, 19, 7),
                Some((15, 8, 7)),
                None,
            ),
            (
                "the evening slot sends",
                (15, 19, 23),
                Some((15, 8, 7)),
                Some((15, 19, 23)),
            ),
            (
                "overnight runs stay quiet",
                (16, 3, 7),
                Some((15, 19, 23)),
                None,
            ),
            (
                "a slot missed while the desktop was off is sent late",
                (15, 11, 30),
                Some((14, 19, 23)),
                Some((15, 8, 7)),
            ),
            (
                "one missed by more than the grace waits for the next",
                (16, 6, 0),
                Some((15, 8, 7)),
                None,
            ),
            ("a first run ever sends", (15, 8, 7), None, Some((15, 8, 7))),
        ];
        let clock = |(day, hour, minute): Clock| at(day, hour, minute);
        for (name, now, last, want) in cases {
            let last = last.map(|last| clock(last).to_rfc3339());
            let got = due_slot(clock(now), &SLOTS, last.as_deref()).unwrap();
            assert_eq!(got, want.map(clock), "{name}");
        }
    }

    #[test]
    fn a_last_sent_with_microseconds_and_an_offset_is_understood() {
        let got = due_slot(
            at(15, 9, 7),
            &SLOTS,
            Some("2026-10-15T08:07:14.123456-03:00"),
        )
        .unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn the_floresta_filter_covers_upstream_and_the_fork_in_any_case() {
        assert!(is_floresta("getfloresta/Floresta#1"));
        assert!(is_floresta("lorenzolfm/Floresta#4"));
        assert!(!is_floresta("bitcoin/bitcoin#1"));
    }
}
