//! The two model passes, and the only places a model touches the output.
//!
//!   gists  one line per obligation. Decorative by contract: an obligation with no
//!          gist is still listed, in the same place, with its computed reasons.
//!   rank   the opportunity judge -- filters, orders, estimates, pitches. It may
//!          only point at candidates it was given; anything else is dropped.
//!
//! Both are cached in state/ so the hourly refresh pays for the model only when
//! something it would look at has changed. Neither subprocess gets any tools: the
//! data goes in inline and nothing here needs to fetch, and the prompts carry text
//! from anyone who can open an issue. Removing the GitHub token alone would not be
//! enough, because `gh` falls back to your own login.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use serde_json::ser::PrettyFormatter;
use sha2::{Digest, Sha256};

use crate::board::CardKey;
use crate::footprint::Footprint;
use crate::obligations::{Obligation, Viewer, fingerprint};
use crate::opportunities::{Opportunity, Source};
use crate::proc;
use crate::prompts;
use crate::snapshot::{Ci, ReviewState};
use crate::text::clip;

const TIMEOUT: Duration = Duration::from_secs(300);

fn log(message: &str) {
    eprintln!("[model] {message}");
}

/// The JSON object in `claude -p`'s output, which may be wrapped in prose.
fn json_object_in(stdout: &str) -> Option<Value> {
    let start = stdout.find('{')?;
    let end = stdout.rfind('}')?;
    if end < start {
        return None;
    }
    let parsed = serde_json::from_str(&stdout[start..=end]);
    if parsed.is_err() {
        log("unparseable JSON");
    }
    parsed.ok()
}

/// Parsed JSON object from `claude -p`, or `None` on any failure.
pub fn claude(prompt: &str) -> Option<Value> {
    let mut cmd = Command::new("claude");
    // The prompt goes first: `--tools` takes a list and would swallow it.
    cmd.args(["-p", prompt, "--tools", "", "--strict-mcp-config"])
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN");
    match proc::run(&mut cmd, TIMEOUT) {
        Err(failure) => {
            log(&format!("unavailable ({failure})"));
            None
        }
        Ok(out) if !out.success => {
            log(&format!("failed rc={:?}", out.code));
            None
        }
        Ok(out) => json_object_in(&out.stdout),
    }
}

/// A state file. `None` if it does not exist yet; an error if it will not read.
pub fn read_state<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    match fs::read_to_string(path) {
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.to_string()),
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|err| err.to_string()),
    }
}

/// A cache. One that will not read costs a model pass and nothing else.
fn load_cache<T: DeserializeOwned + Default>(path: &Path) -> T {
    read_state(path).ok().flatten().unwrap_or_default()
}

fn dumps(value: &impl Serialize) -> Result<String> {
    let mut buf = Vec::new();
    let formatter = PrettyFormatter::with_indent(b" ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut buf, formatter);
    value.serialize(&mut serializer)?;
    Ok(String::from_utf8(buf)?)
}

pub fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    fs::write(&tmp, dumps(value)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------- gists

fn gist_fingerprint(ob: &Obligation<'_>) -> String {
    let fp = fingerprint(ob);
    format!(
        "{}|{}|{}",
        fp.last_text(),
        fp.kinds_text(),
        ob.item.head().unwrap_or("")
    )
}

#[derive(Serialize)]
struct Said<'a> {
    by: &'a str,
    text: String,
}

#[derive(Serialize)]
struct ThreadExcerpt<'a> {
    path: &'a str,
    last: Vec<Said<'a>>,
}

#[derive(Serialize)]
struct PrExcerpt<'a> {
    size: String,
    files: &'a [String],
    threads: Vec<ThreadExcerpt<'a>>,
}

#[derive(Serialize)]
struct Excerpt<'a> {
    key: &'a CardKey,
    kind: &'static str,
    title: &'a str,
    reasons: Vec<&'a str>,
    description: String,
    waiting_comments: Vec<Said<'a>>,
    #[serde(flatten)]
    pr: Option<PrExcerpt<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    since_your_ack: Option<AckDiff<'a>>,
}

#[derive(Serialize)]
struct AckDiff<'a> {
    rebase_only: bool,
    changed: &'a [String],
}

fn excerpt<'a>(ob: &'a Obligation<'_>, me: &Viewer) -> Excerpt<'a> {
    let item = ob.item;
    let since = item
        .comments
        .iter()
        .filter(|c| c.author == me.login)
        .map(|c| c.at)
        .max();
    let waiting: Vec<_> = item
        .comments
        .iter()
        .filter(|c| c.author != me.login && !c.bot && Some(c.at) > since)
        .collect();
    // The comment that started the wait is the one the gist is about; on a busy
    // thread it would fall out of a plain "last three".
    let recent: Vec<_> = if waiting.len() <= 3 {
        waiting
    } else {
        let mut kept = vec![waiting[0]];
        kept.extend_from_slice(&waiting[waiting.len() - 2..]);
        kept
    };
    let pr = item.pr().map(|pr| PrExcerpt {
        size: format!(
            "+{}/-{} in {} files",
            pr.additions, pr.deletions, pr.changed_files
        ),
        files: &pr.files[..pr.files.len().min(12)],
        threads: pr
            .threads
            .iter()
            .filter(|t| {
                !t.resolved
                    && (item.author == me.login || t.comments.iter().any(|c| c.author == me.login))
            })
            .take(4)
            .map(|t| ThreadExcerpt {
                path: &t.path,
                last: t.comments[t.comments.len().saturating_sub(2)..]
                    .iter()
                    .map(|c| Said {
                        by: &c.author,
                        text: clip(&c.body, 400),
                    })
                    .collect(),
            })
            .collect(),
    });
    Excerpt {
        key: &item.key,
        kind: item.kind(),
        title: &item.title,
        reasons: ob.reasons.iter().map(|r| r.text.as_str()).collect(),
        description: clip(&item.body, 500),
        waiting_comments: recent
            .iter()
            .map(|c| Said {
                by: &c.author,
                text: clip(&c.body, 700),
            })
            .collect(),
        pr,
        since_your_ack: ob.interdiff.as_ref().map(|diff| AckDiff {
            rebase_only: diff.is_rebase_only(),
            changed: &diff.changed,
        }),
    }
}

#[derive(Serialize, Deserialize)]
struct CachedGist {
    fp: String,
    gist: String,
}

#[derive(Deserialize)]
struct GistAnswer {
    gists: Vec<Value>,
}

#[derive(Deserialize)]
struct WireGist {
    key: CardKey,
    gist: String,
}

/// `key -> gist`. Cached per fingerprint; only changed items reach the model. An entry
/// of another shape costs that one gist, which is decoration anyway.
pub fn gists(
    obs: &[Obligation<'_>],
    me: &Viewer,
    cache_path: &Path,
    mut ask: impl FnMut(&str) -> Option<Value>,
) -> Result<BTreeMap<CardKey, String>> {
    let mut cache: BTreeMap<CardKey, CachedGist> = load_cache(cache_path);
    let want: HashMap<&CardKey, String> = obs
        .iter()
        .map(|ob| (&ob.item.key, gist_fingerprint(ob)))
        .collect();
    let todo: Vec<_> = obs
        .iter()
        .filter(|ob| cache.get(&ob.item.key).map(|cached| &cached.fp) != want.get(&ob.item.key))
        .collect();
    if !todo.is_empty() {
        let excerpts: Vec<_> = todo.iter().map(|ob| excerpt(ob, me)).collect();
        let prompt = prompts::GIST.replace("%s", &dumps(&excerpts)?);
        let answer = ask(&prompt).and_then(|answer| serde_json::from_value(answer).ok());
        let entries = answer.map_or(Vec::new(), |answer: GistAnswer| answer.gists);
        for entry in entries {
            let Ok(WireGist { key, gist }) = serde_json::from_value(entry) else {
                continue;
            };
            let gist = clip(&gist, 200);
            if let Some(fp) = want.get(&key).filter(|_| !gist.is_empty()) {
                let fp = fp.clone();
                cache.insert(key, CachedGist { fp, gist });
            }
        }
    }
    cache.retain(|key, _| want.contains_key(key)); // forget what cleared
    save(cache_path, &cache)?;
    Ok(cache
        .into_iter()
        .filter(|(key, cached)| want.get(key) == Some(&cached.fp))
        .map(|(key, cached)| (key, cached.gist))
        .collect())
}

// ---------------------------------------------------------------- rank

#[derive(Serialize)]
struct CandidatePr<'a> {
    size: String,
    files: u64,
    draft: bool,
    ci: Option<&'a str>,
    mergeable: Option<&'a str>,
    reviews: usize,
}

#[derive(Serialize)]
struct Candidate<'a> {
    number: u64,
    kind: &'static str,
    title: &'a str,
    author: &'a str,
    sources: &'a [Source],
    age_days: f64,
    labels: &'a [String],
    milestone: Option<&'a str>,
    comments: usize,
    areas: &'a [String],
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    since_you: Option<&'a str>,
    #[serde(flatten)]
    pr: Option<CandidatePr<'a>>,
}

fn candidate<'a>(o: &'a Opportunity<'_>) -> Candidate<'a> {
    let item = o.item;
    let merge = o.merge;
    Candidate {
        number: item.number,
        kind: item.kind(),
        title: &item.title,
        author: &item.author,
        sources: &o.sources,
        age_days: o.age_days,
        labels: &item.labels,
        milestone: item.milestone.as_ref().map(|m| m.title.as_str()),
        comments: item.comments.len(),
        areas: &o.areas,
        description: clip(&item.body, 300),
        since_you: o.note.as_deref(),
        pr: item.pr().map(|pr| CandidatePr {
            size: format!("+{}/-{}", pr.additions, pr.deletions),
            files: pr.changed_files,
            draft: pr.draft,
            // "NONE" tells the judge there are no checks; null, that nobody looked.
            ci: merge.map(|merge| merge.ci.map_or("NONE", Ci::wire)),
            mergeable: merge.map(|merge| merge.mergeable.wire()),
            reviews: pr
                .reviews
                .iter()
                .filter(|r| r.state != ReviewState::Pending)
                .map(|r| r.author.as_str())
                .collect::<HashSet<_>>()
                .len(),
        }),
    }
}

/// Re-rank when the candidate set or a PR's head moves, not on every comment --
/// comments are most of the churn and rarely change the order.
fn rank_fingerprint(opps: &[Opportunity<'_>]) -> String {
    let mut signature: Vec<String> = opps
        .iter()
        .map(|o| format!("{}@{}", o.item.key, o.item.head().unwrap_or("")))
        .collect();
    signature.sort_unstable();
    let digest = Sha256::digest(signature.join("\n").as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    hex[..16].to_owned()
}

#[derive(Default, Deserialize, Serialize)]
struct RankCache {
    sig: String,
    at: f64,
    picks: Vec<Value>,
}

#[derive(Deserialize)]
struct RankAnswer {
    picks: Vec<Value>,
}

/// One pick as the judge wrote it. Each is parsed on its own, so one that is malformed
/// costs only itself.
#[derive(Deserialize)]
struct WirePick {
    #[serde(deserialize_with = "number_or_numeric_text")]
    number: u64,
    #[serde(default, deserialize_with = "text_or_nothing")]
    why: Option<String>,
    #[serde(default, deserialize_with = "text_or_nothing")]
    pitch: Option<String>,
    #[serde(default, deserialize_with = "text_or_nothing")]
    est: Option<String>,
}

/// The judge writes `"number": "1"` as readily as `"number": 1`. Anything else points
/// at nothing.
fn number_or_numeric_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Number {
        Number(u64),
        Text(String),
    }
    match Number::deserialize(deserializer)? {
        Number::Number(number) => Ok(number),
        Number::Text(text) => text.trim().parse().map_err(serde::de::Error::custom),
    }
}

/// A field of the wrong type costs that field, not the pick.
fn text_or_nothing<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Ok(Value::deserialize(deserializer)?
        .as_str()
        .map(str::to_owned))
}

fn clipped(field: Option<String>, n: usize) -> Option<String> {
    field
        .map(|text| clip(&text, n))
        .filter(|text| !text.is_empty())
}

/// What the judge said about a pick.
#[derive(Debug)]
pub struct Verdict {
    pub why: Option<String>,
    pub pitch: Option<String>,
    pub est: Option<String>,
}

/// The opportunities to show, best first.
#[derive(Debug)]
pub enum Ranked<'o, 'a> {
    Judged(Vec<(&'o Opportunity<'a>, Verdict)>),
    /// Ranking order with nothing from the judge, and shown as such.
    Mechanical(Vec<&'o Opportunity<'a>>),
}

impl Default for Ranked<'_, '_> {
    fn default() -> Self {
        Self::Mechanical(Vec::new())
    }
}

impl<'o, 'a> Ranked<'o, 'a> {
    pub fn mechanical(opps: &'o [Opportunity<'a>], cap: usize) -> Self {
        Self::Mechanical(opps.iter().take(cap).collect())
    }

    pub fn is_judged(&self) -> bool {
        matches!(self, Self::Judged(_))
    }

    pub fn picks(&self) -> Vec<(&'o Opportunity<'a>, Option<&Verdict>)> {
        match self {
            Self::Judged(picks) => picks.iter().map(|(opp, v)| (*opp, Some(v))).collect(),
            Self::Mechanical(opps) => opps.iter().map(|opp| (*opp, None)).collect(),
        }
    }
}

/// Falls back to the order given, untouched, and says so.
pub fn rank<'o, 'a>(
    opps: &'o [Opportunity<'a>],
    fp: &Footprint,
    cap: usize,
    cache_path: &Path,
    ttl_hours: f64,
    mut ask: impl FnMut(&str) -> Option<Value>,
) -> Result<Ranked<'o, 'a>> {
    let by_number: HashMap<u64, &'o Opportunity<'a>> =
        opps.iter().map(|o| (o.item.number, o)).collect();
    let sig = rank_fingerprint(opps);
    let cache: RankCache = load_cache(cache_path);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64());
    let fresh = cache.sig == sig && now - cache.at < ttl_hours * 3600.0;
    let mut picks = if fresh { cache.picks } else { Vec::new() };
    if picks.is_empty() && !opps.is_empty() {
        let candidates: Vec<_> = opps.iter().map(candidate).collect();
        let areas: Vec<String> = fp
            .areas()
            .take(10)
            .map(|area| Value::from(area).to_string())
            .collect();
        // The payload goes in last: it holds text from GitHub, and nothing after
        // it may be mistaken for a placeholder.
        let prompt = prompts::RANK
            .replace("%(cap)d", &cap.to_string())
            .replace("%(footprint)s", &format!("[{}]", areas.join(", ")))
            .replace("%(payload)s", &dumps(&candidates)?);
        let answer = ask(&prompt).and_then(|answer| serde_json::from_value(answer).ok());
        picks = answer.map_or(Vec::new(), |answer: RankAnswer| answer.picks);
        // An empty verdict is retried, not kept.
        if !picks.is_empty() {
            save(
                cache_path,
                &RankCache {
                    sig,
                    at: now,
                    picks: picks.clone(),
                },
            )?;
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for pick in picks {
        let Ok(pick) = serde_json::from_value::<WirePick>(pick) else {
            continue;
        };
        let Some(opp) = by_number.get(&pick.number).copied() else {
            continue; // invented
        };
        if !seen.insert(pick.number) {
            continue; // a duplicate
        }
        let verdict = Verdict {
            why: clipped(pick.why, 80),
            pitch: clipped(pick.pitch, 240),
            est: clipped(pick.est, 8),
        };
        out.push((opp, verdict));
        if out.len() >= cap {
            break;
        }
    }
    if out.is_empty() {
        // No usable answer -- or the judge kept nothing, which is distrusted the same
        // way the old board distrusted it.
        return Ok(Ranked::mechanical(opps, cap));
    }
    Ok(Ranked::Judged(out))
}

#[cfg(test)]
mod tests {
    //! How the judge's answer is trusted, and that the hourly refresh does not pay
    //! for the model when nothing moved.

    use super::*;
    use crate::obligations::collect;
    use crate::render::Dm;
    use crate::snapshot::Item;
    use crate::testkit::{ME, c, day, key, me, now, opportunity, pr, snap};
    use serde_json::json;
    use std::cell::Cell;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn opportunities(items: &[Item]) -> Vec<Opportunity<'_>> {
        items.iter().map(opportunity).collect()
    }

    fn items() -> Vec<Item> {
        vec![pr(1), pr(2), pr(3)]
    }

    fn cache(dir: &TempDir, name: &str) -> PathBuf {
        dir.path().join(name)
    }

    fn numbers(ranked: &Ranked<'_, '_>) -> Vec<u64> {
        ranked
            .picks()
            .iter()
            .map(|(opp, _)| opp.item.number)
            .collect()
    }

    fn rank_with<'o, 'a>(
        opps: &'o [Opportunity<'a>],
        path: &Path,
        answer: Option<Value>,
    ) -> Ranked<'o, 'a> {
        rank(opps, &Footprint::default(), 8, path, 12.0, |_| {
            answer.clone()
        })
        .unwrap()
    }

    #[test]
    fn the_judge_may_only_reorder_what_it_was_given() {
        let dir = TempDir::new().unwrap();
        let items = items();
        let opps = opportunities(&items);
        let answer = json!({"picks": [
            {"number": 3, "why": "w", "pitch": "p", "est": "1h"},
            {"number": 99, "why": "invented"},
            {"number": 3},
            {"bogus": 1},
            {"number": "1", "why": "x"},
        ]});
        let ranked = rank_with(&opps, &cache(&dir, "r.json"), Some(answer));
        assert!(ranked.is_judged());
        assert_eq!(numbers(&ranked), [3, 1], "invented and duplicate dropped");
    }

    #[test]
    fn a_pick_that_points_at_nothing_is_dropped_and_a_bad_field_costs_only_itself() {
        let dir = TempDir::new().unwrap();
        let items = items();
        let opps = opportunities(&items);
        let answer = json!({"picks": [
            {"number": true, "why": "a boolean is not number 1"},
            {"number": 1.5},
            {"number": -1},
            {"number": [1]},
            {"number": null},
            "not an object",
            {"number": 2, "why": true, "pitch": 7, "est": ""},
        ]});
        let ranked = rank_with(&opps, &cache(&dir, "r.json"), Some(answer));
        assert_eq!(numbers(&ranked), [2]);
        let picks = ranked.picks();
        let verdict = picks[0].1.unwrap();
        assert_eq!(
            (&verdict.why, &verdict.pitch, &verdict.est),
            (&None, &None, &None)
        );
    }

    #[test]
    fn an_empty_verdict_falls_back_to_ranking_order_flagged() {
        let dir = TempDir::new().unwrap();
        let items = items();
        let opps = opportunities(&items);
        let ranked = rank_with(&opps, &cache(&dir, "r.json"), Some(json!({"picks": []})));
        assert!(!ranked.is_judged());
        assert_eq!(numbers(&ranked), [1, 2, 3]);
    }

    #[test]
    fn a_failed_judge_is_said_out_loud() {
        let dir = TempDir::new().unwrap();
        let items = items();
        let opps = opportunities(&items);
        let ranked = rank_with(&opps, &cache(&dir, "r.json"), None);
        let message = Dm {
            today: "d",
            ranked,
            cap: 3,
            ..Dm::default()
        }
        .render();
        assert!(message.contains("judge unavailable"), "{message}");
    }

    #[test]
    fn the_hourly_refresh_reranks_only_when_a_head_moves() {
        let dir = TempDir::new().unwrap();
        let path = cache(&dir, "r.json");
        let calls = Cell::new(0);
        let ask = |_: &str| {
            calls.set(calls.get() + 1);
            Some(json!({"picks": [{"number": 2}]}))
        };
        let items = items();
        let opps = opportunities(&items);
        rank(&opps, &Footprint::default(), 8, &path, 12.0, ask).unwrap();
        rank(&opps, &Footprint::default(), 8, &path, 12.0, ask).unwrap();
        assert_eq!(calls.get(), 1, "same candidates, one judge call");

        let moved = vec![pr(1), pr(2).with_head(&"e".repeat(40)), pr(3)];
        let opps = opportunities(&moved);
        rank(&opps, &Footprint::default(), 8, &path, 12.0, ask).unwrap();
        assert_eq!(calls.get(), 2, "a push re-ranks");
    }

    #[test]
    fn a_gist_is_written_once_per_fingerprint() {
        let dir = TempDir::new().unwrap();
        let path = cache(&dir, "g.json");
        let viewer = me();
        let empty = HashMap::new();
        let base = pr(100).requested(ME, &day(1).to_string());
        let snapshot = snap(vec![base.clone()], vec![]);
        let obs = collect(&snapshot, &viewer, now(), &empty);
        let item_key = obs[0].item.key.clone();

        let calls = Cell::new(0);
        let ask = |_: &str| {
            calls.set(calls.get() + 1);
            Some(json!({"gists": [{"key": item_key.to_string(), "gist": "asks for a yes/no"}]}))
        };
        let first = gists(&obs, &viewer, &path, ask).unwrap();
        let second = gists(&obs, &viewer, &path, ask).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(first, second);
        assert_eq!(first[&item_key], "asks for a yes/no");

        let newer = snap(
            vec![
                base.clone()
                    .comments(vec![c("bob", 0, &format!("@{ME} and now?"))]),
            ],
            vec![],
        );
        let newer_obs = collect(&newer, &viewer, now(), &empty);
        gists(&newer_obs, &viewer, &path, ask).unwrap();
        assert_eq!(calls.get(), 2, "a new trigger asks again");

        let changed = snap(vec![pr(100).requested(ME, &day(0).to_string())], vec![]);
        let changed_obs = collect(&changed, &viewer, now(), &empty);
        let after_failure = gists(&changed_obs, &viewer, &path, |_| None).unwrap();
        assert!(
            after_failure.is_empty(),
            "a failed gist pass leaves no stale gist on a changed item"
        );
    }

    #[test]
    fn prose_around_the_answer_is_tolerated_and_garbage_is_not_an_answer() {
        assert_eq!(
            json_object_in("Sure, here you go:\n{\"picks\": []}\nAnything else?"),
            Some(json!({"picks": []}))
        );
        assert_eq!(json_object_in("no braces here"), None);
        assert_eq!(json_object_in("} backwards {"), None);
        assert_eq!(json_object_in("{not json}"), None);
    }

    #[test]
    fn state_files_written_by_earlier_runs_are_still_read() {
        let dir = TempDir::new().unwrap();
        let path = cache(&dir, "g.json");
        let viewer = me();
        let snapshot = snap(vec![pr(100).requested(ME, "2026-10-08T17:14:49Z")], vec![]);
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let cached = format!(
            "{{\n \"getfloresta/Floresta#100\": {{\n  \"fp\": \"2026-10-08T17:14:49Z|review_requested|{}\",\n  \"gist\": \"caf\\u00e9\"\n }}\n}}",
            "f".repeat(40),
        );
        fs::write(&path, cached).unwrap();
        let got = gists(&obs, &viewer, &path, |_| {
            panic!("a cached gist must not ask")
        })
        .unwrap();
        assert_eq!(got[&key("getfloresta/floresta#100")], "café");

        let dir = TempDir::new().unwrap();
        let path = cache(&dir, "r.json");
        let items = items();
        let opps = opportunities(&items);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let cached = json!({
            "sig": rank_fingerprint(&opps),
            "at": now,
            "picks": [{"est": "1h", "number": 2, "pitch": "p", "why": "w"}],
        });
        fs::write(&path, cached.to_string()).unwrap();
        let ranked = rank(&opps, &Footprint::default(), 8, &path, 12.0, |_| {
            panic!("a fresh ranking must not ask")
        })
        .unwrap();
        assert_eq!(numbers(&ranked), [2]);
    }

    #[test]
    fn a_corrupt_cache_costs_a_model_pass_and_nothing_else() {
        let dir = TempDir::new().unwrap();
        let path = cache(&dir, "r.json");
        fs::write(&path, "{{{ not json").unwrap();
        let items = items();
        let opps = opportunities(&items);
        let ranked = rank_with(&opps, &path, Some(json!({"picks": [{"number": 1}]})));
        assert_eq!(numbers(&ranked), [1]);
    }
}
