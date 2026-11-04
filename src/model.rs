//! The two model passes, and the only places a model touches the output.
//!
//!   gists  one line per obligation, and whether anyone is actually asking. Never
//!          touches the board's list: an obligation with no gist stays there, in the
//!          same place, with its computed reasons. The verdict only decides what the
//!          DM may interrupt you with, and no verdict means it may.
//!   rank   the opportunity judge -- filters and orders. It may only point at
//!          candidates it was given; anything else is dropped.
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
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use serde_json::ser::PrettyFormatter;
use sha2::{Digest, Sha256};

use crate::board::CardKey;
use crate::footprint::Footprint;
use crate::obligations::{Obligation, Viewer, fingerprint, last_per_kind, pings, waiting_comments};
use crate::opportunities::{Opportunity, Source};
use crate::proc;
use crate::prompts;
use crate::snapshot::{Ci, Comment, ReviewState};
use crate::text::clip;

const TIMEOUT: Duration = Duration::from_secs(300);
/// Enough that the repo you have fewer commits in still makes the list.
const FOOTPRINT_AREAS: usize = 20;

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

fn short_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    hex[..16].to_owned()
}

/// What has to move for a verdict to be read again: its reasons, or the words it was read
/// from. The hash is of what is sent, so nothing the model reads can change without moving it.
fn gist_fingerprint(ob: &Obligation<'_>, evidence: &Evidence<'_>) -> Result<String> {
    Ok(format!(
        "{}|{}|{}|{}",
        last_per_kind(ob),
        fingerprint(ob).kinds_text(),
        ob.item.head().unwrap_or(""),
        short_hash(&serde_json::to_vec(evidence)?)
    ))
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

/// One item as the model reads it.
#[derive(Serialize)]
struct Excerpt<'a> {
    #[serde(flatten)]
    evidence: Evidence<'a>,
    /// Outside the hash, because "3d ago" moves every day. The key carries what they mean.
    reasons: Vec<&'a str>,
}

/// What a verdict is read from, apart from its reasons.
#[derive(Serialize)]
struct Evidence<'a> {
    key: &'a CardKey,
    kind: &'static str,
    title: &'a str,
    author: &'a str,
    /// He opened it or is assigned it, so a request on it is for him.
    his: bool,
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

fn excerpt<'a>(ob: &'a Obligation<'_>, me: &Viewer, now: DateTime<Utc>) -> Excerpt<'a> {
    let item = ob.item;
    let waiting = waiting_comments(item, me, now);
    // The comment that started the wait and those a mention stands on are what the verdict
    // turns on. On a busy item they would fall out of a plain "last three".
    let pinged = pings(item, &waiting, me);
    let recent: Vec<&Comment> = waiting
        .iter()
        .enumerate()
        .filter(|(at, _)| {
            *at == 0
                || pinged.first() == Some(at)
                || pinged.last() == Some(at)
                || at + 2 >= waiting.len()
        })
        .map(|(_, comment)| *comment)
        .collect();
    let pr = item.pr().map(|pr| PrExcerpt {
        size: format!(
            "+{}/-{} in {} files",
            pr.additions, pr.deletions, pr.changed_files
        ),
        files: &pr.files[..pr.files.len().min(12)],
        // A machine is nobody waiting on you, so it can't push out what a person said, and a
        // thread with only a machine in it takes none of the four places.
        threads: ob
            .threads
            .iter()
            .map(|t| {
                let said: Vec<_> = t.comments.iter().filter(|c| !c.bot).collect();
                ThreadExcerpt {
                    path: &t.path,
                    last: said[said.len().saturating_sub(2)..]
                        .iter()
                        .map(|c| Said {
                            by: &c.author,
                            text: clip(&c.body, 400),
                        })
                        .collect(),
                }
            })
            .filter(|t| !t.last.is_empty())
            .take(4)
            .collect(),
    });
    Excerpt {
        evidence: Evidence {
            key: &item.key,
            kind: item.kind(),
            title: &item.title,
            author: &item.author,
            his: item.author == me.login || item.assignees.contains(&me.login),
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
        },
        reasons: ob.reasons.iter().map(|r| r.text.as_str()).collect(),
    }
}

/// What the gist pass said about an obligation.
#[derive(Debug, PartialEq)]
pub struct Gist {
    pub text: String,
    /// Whether someone is waiting on you specifically.
    pub asks: bool,
}

/// A cache from before `asks` will not read, which costs one model pass.
#[derive(Serialize, Deserialize)]
struct CachedGist {
    fp: String,
    gist: String,
    asks: bool,
}

#[derive(Deserialize)]
struct GistAnswer {
    gists: Vec<Value>,
}

#[derive(Deserialize)]
struct WireGist {
    key: CardKey,
    gist: String,
    asks: bool,
}

/// `key -> gist`. Cached per fingerprint; only changed items reach the model. An entry
/// of another shape costs that one gist, and one for a card it was not sent is dropped: it
/// can only have come from the text it was sent.
pub fn gists(
    obs: &[Obligation<'_>],
    me: &Viewer,
    now: DateTime<Utc>,
    cache_path: &Path,
    mut ask: impl FnMut(&str) -> Option<Value>,
) -> Result<BTreeMap<CardKey, Gist>> {
    let mut cache: BTreeMap<CardKey, CachedGist> = load_cache(cache_path);
    let excerpts: Vec<Excerpt<'_>> = obs.iter().map(|ob| excerpt(ob, me, now)).collect();
    let want = obs
        .iter()
        .zip(&excerpts)
        .map(|(ob, read)| Ok((&ob.item.key, gist_fingerprint(ob, &read.evidence)?)))
        .collect::<Result<HashMap<&CardKey, String>>>()?;
    let todo: Vec<&Excerpt<'_>> = excerpts
        .iter()
        .filter(|read| {
            let key = read.evidence.key;
            cache.get(key).map(|cached| &cached.fp) != want.get(key)
        })
        .collect();
    if !todo.is_empty() {
        let asked: HashSet<&CardKey> = todo.iter().map(|read| read.evidence.key).collect();
        let prompt = prompts::GIST.replace("%s", &dumps(&todo)?);
        let answer = ask(&prompt).and_then(|answer| serde_json::from_value(answer).ok());
        let entries = answer.map_or(Vec::new(), |answer: GistAnswer| answer.gists);
        for entry in entries {
            let Ok(WireGist { key, gist, asks }) = serde_json::from_value(entry) else {
                continue;
            };
            let gist = clip(&gist, 120);
            if let Some(fp) = want
                .get(&key)
                .filter(|_| asked.contains(&key) && !gist.is_empty())
            {
                let fp = fp.clone();
                cache.insert(key, CachedGist { fp, gist, asks });
            }
        }
    }
    cache.retain(|key, _| want.contains_key(key)); // forget what cleared
    save(cache_path, &cache)?;
    Ok(cache
        .into_iter()
        .filter(|(key, cached)| want.get(key) == Some(&cached.fp))
        .map(|(key, cached)| {
            let gist = Gist {
                text: cached.gist,
                asks: cached.asks,
            };
            (key, gist)
        })
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
    key: &'a CardKey,
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
        key: &item.key,
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
    short_hash(signature.join("\n").as_bytes())
}

/// One that will not read costs a model pass, as `load_cache` says. That covers a cache
/// from before picks had keys.
#[derive(Default, Deserialize, Serialize)]
struct RankCache {
    sig: String,
    at: f64,
    /// Only what could be used, so a fresh cache always holds an answer.
    picks: Vec<WirePick>,
}

#[derive(Deserialize)]
struct RankAnswer {
    picks: Vec<Value>,
}

/// One pick as the judge wrote it. Each is parsed on its own, so one that is malformed
/// costs only itself.
#[derive(Clone, Deserialize, Serialize)]
struct WirePick {
    key: CardKey,
    #[serde(default, deserialize_with = "text_or_nothing")]
    why: Option<String>,
}

/// What can be used of the judge's answer: parsed, one of the candidates, once each.
fn usable(
    answer: Vec<Value>,
    by_key: &HashMap<&CardKey, &Opportunity<'_>>,
    cap: usize,
) -> Vec<WirePick> {
    let mut seen = HashSet::new();
    answer
        .into_iter()
        .filter_map(|pick| serde_json::from_value::<WirePick>(pick).ok())
        .filter(|pick| by_key.contains_key(&pick.key))
        .filter(|pick| seen.insert(pick.key.clone()))
        .take(cap)
        .collect()
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
}

/// The opportunities to show, best first.
#[derive(Debug)]
pub enum Ranked<'o, 'a> {
    Judged(Vec<(&'o Opportunity<'a>, Verdict)>),
    /// Ranking order with nothing from the judge. The run reports it as `Degraded::Model`.
    Mechanical(Vec<&'o Opportunity<'a>>),
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

/// Falls back to the order given, untouched, and `is_judged` says so.
pub fn rank<'o, 'a>(
    opps: &'o [Opportunity<'a>],
    fp: &Footprint,
    cap: usize,
    cache_path: &Path,
    ttl_hours: f64,
    mut ask: impl FnMut(&str) -> Option<Value>,
) -> Result<Ranked<'o, 'a>> {
    let by_key: HashMap<&CardKey, &'o Opportunity<'a>> =
        opps.iter().map(|o| (&o.item.key, o)).collect();
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
            .take(FOOTPRINT_AREAS)
            .map(|area| Value::from(area).to_string())
            .collect();
        // The payload goes in last: it holds text from GitHub, and nothing after
        // it may be mistaken for a placeholder.
        let prompt = prompts::RANK
            .replace("%(cap)d", &cap.to_string())
            .replace("%(footprint)s", &format!("[{}]", areas.join(", ")))
            .replace("%(payload)s", &dumps(&candidates)?);
        let answer = ask(&prompt).and_then(|answer| serde_json::from_value(answer).ok());
        picks = answer.map_or(Vec::new(), |answer: RankAnswer| {
            usable(answer.picks, &by_key, cap)
        });
        // An answer with nothing usable in it is retried, not kept.
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
    let out: Vec<_> = picks
        .into_iter()
        .filter_map(|pick| {
            let opp = by_key.get(&pick.key).copied()?;
            let verdict = Verdict {
                why: clipped(pick.why, 80),
            };
            Some((opp, verdict))
        })
        .take(cap)
        .collect();
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
    use crate::snapshot::{Interdiff, Item};
    use crate::testkit::{ME, bot, c, day, issue, me, now, opportunity, pr, review, snap, thread};
    use chrono::TimeDelta;
    use serde_json::json;
    use std::cell::{Cell, RefCell};
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
            .map(|(opp, _)| opp.item.number())
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
            {"key": "getfloresta/Floresta#3", "why": "w"},
            {"key": "getfloresta/Floresta#99", "why": "invented"},
            {"key": "getfloresta/floresta#3"},
            {"bogus": 1},
            {"key": "GetFloresta/floresta#1", "why": "x"},
        ]});
        let ranked = rank_with(&opps, &cache(&dir, "r.json"), Some(answer));
        assert!(ranked.is_judged());
        assert_eq!(numbers(&ranked), [3, 1], "invented and duplicate dropped");
    }

    #[test]
    fn the_judge_tells_the_same_number_in_two_repos_apart() {
        let dir = TempDir::new().unwrap();
        let items = vec![pr(7), pr(7).in_repo("cashubtc/cdk")];
        let opps = opportunities(&items);
        let answer = json!({"picks": [
            {"key": "cashubtc/cdk#7", "why": "the cdk one"},
            {"key": "getfloresta/Floresta#7", "why": "the floresta one"},
        ]});
        let ranked = rank_with(&opps, &cache(&dir, "r.json"), Some(answer));
        let picks = ranked.picks();
        let got: Vec<_> = picks
            .iter()
            .map(|(opp, verdict)| {
                let why = verdict.and_then(|v| v.why.as_deref());
                (opp.item.key.to_string(), why)
            })
            .collect();
        assert_eq!(
            got,
            [
                ("cashubtc/cdk#7".to_owned(), Some("the cdk one")),
                (
                    "getfloresta/floresta#7".to_owned(),
                    Some("the floresta one")
                ),
            ]
        );
    }

    #[test]
    fn a_pick_that_points_at_nothing_is_dropped_and_a_bad_field_costs_only_itself() {
        let dir = TempDir::new().unwrap();
        let items = items();
        let opps = opportunities(&items);
        let answer = json!({"picks": [
            {"key": true, "why": "a boolean is not a key"},
            {"key": 1, "why": "a bare number could be any repo's"},
            {"key": "1"},
            {"key": "#1"},
            {"key": [1]},
            {"key": null},
            "not an object",
            {"key": "getfloresta/Floresta#2", "why": true},
        ]});
        let ranked = rank_with(&opps, &cache(&dir, "r.json"), Some(answer));
        assert_eq!(numbers(&ranked), [2]);
        let picks = ranked.picks();
        assert_eq!(picks[0].1.unwrap().why, None);
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

    /// The name a card shows is not a key. Kept, this answer would hold the lane at ranking
    /// order until the TTL, saying the model did not answer while nothing asked it.
    #[test]
    fn an_answer_with_nothing_usable_in_it_is_asked_again_not_kept() {
        let dir = TempDir::new().unwrap();
        let path = cache(&dir, "r.json");
        let items = items();
        let opps = opportunities(&items);
        let calls = Cell::new(0);
        let ask = |_: &str| {
            calls.set(calls.get() + 1);
            Some(json!({"picks": [
                {"key": "floresta#2"},
                {"key": "getfloresta/Floresta#99"},
            ]}))
        };
        for run in 1..=2 {
            let ranked = rank(&opps, &Footprint::default(), 8, &path, 12.0, ask).unwrap();
            assert!(!ranked.is_judged());
            assert_eq!(calls.get(), run);
        }
    }

    #[test]
    fn only_what_could_be_used_of_an_answer_is_kept() {
        let dir = TempDir::new().unwrap();
        let path = cache(&dir, "r.json");
        let items = items();
        let opps = opportunities(&items);
        let answer = json!({"picks": [
            {"key": "floresta#3"},
            {"key": "getfloresta/Floresta#2", "why": "w"},
        ]});
        rank_with(&opps, &path, Some(answer));
        let ranked = rank(&opps, &Footprint::default(), 8, &path, 12.0, |_| {
            panic!("a fresh ranking must not ask")
        })
        .unwrap();
        assert_eq!(numbers(&ranked), [2]);
        assert_eq!(ranked.picks()[0].1.unwrap().why.as_deref(), Some("w"));
    }

    /// The `rank.json` of every run before picks had keys.
    #[test]
    fn a_rank_cache_from_before_keys_costs_one_model_pass() {
        let dir = TempDir::new().unwrap();
        let path = cache(&dir, "r.json");
        let items = items();
        let opps = opportunities(&items);
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let cached = json!({
            "sig": rank_fingerprint(&opps),
            "at": at,
            "picks": [{"number": 2, "why": "w"}],
        });
        fs::write(&path, cached.to_string()).unwrap();
        let answer = json!({"picks": [{"key": "getfloresta/Floresta#3"}]});
        let ranked = rank_with(&opps, &path, Some(answer));
        assert_eq!(numbers(&ranked), [3]);
    }

    #[test]
    fn a_failed_judge_falls_back_to_ranking_order_flagged() {
        let dir = TempDir::new().unwrap();
        let items = items();
        let opps = opportunities(&items);
        let ranked = rank_with(&opps, &cache(&dir, "r.json"), None);
        assert!(!ranked.is_judged());
        assert_eq!(numbers(&ranked), [1, 2, 3]);
    }

    #[test]
    fn the_hourly_refresh_reranks_only_when_a_head_moves() {
        let dir = TempDir::new().unwrap();
        let path = cache(&dir, "r.json");
        let calls = Cell::new(0);
        let ask = |_: &str| {
            calls.set(calls.get() + 1);
            Some(json!({"picks": [{"key": "getfloresta/Floresta#2"}]}))
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
            Some(json!({"gists": [
                {"key": item_key.to_string(), "gist": "asks for a yes/no", "asks": true}
            ]}))
        };
        let first = gists(&obs, &viewer, now(), &path, ask).unwrap();
        let second = gists(&obs, &viewer, now(), &path, ask).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(first, second);
        assert_eq!(
            first[&item_key],
            Gist {
                text: "asks for a yes/no".to_owned(),
                asks: true
            }
        );

        let newer = snap(
            vec![
                base.clone()
                    .comments(vec![c("bob", 0, &format!("@{ME} and now?"))]),
            ],
            vec![],
        );
        let newer_obs = collect(&newer, &viewer, now(), &empty);
        gists(&newer_obs, &viewer, now(), &path, ask).unwrap();
        assert_eq!(calls.get(), 2, "a new trigger asks again");

        let changed = snap(vec![pr(100).requested(ME, &day(0).to_string())], vec![]);
        let changed_obs = collect(&changed, &viewer, now(), &empty);
        let after_failure = gists(&changed_obs, &viewer, now(), &path, |_| None).unwrap();
        assert!(
            after_failure.is_empty(),
            "a failed gist pass leaves no stale gist on a changed item"
        );
    }

    /// One time across every kind would let this head hide the ping, and a verdict on the
    /// thank-you would decide a question the model never read.
    #[test]
    fn a_newer_ping_on_a_head_dated_ahead_is_read_again() {
        let dir = TempDir::new().unwrap();
        let path = cache(&dir, "g.json");
        let viewer = me();
        let empty = HashMap::new();
        let acked = |comments| {
            pr(98)
                .with_head(&"a".repeat(40))
                .pushed_at(day(-30))
                .comments(comments)
        };
        let ack = c(ME, 9, "ACK 1234567");
        let thanks = c("bob", 5, &format!("@{ME} thanks"));
        let question = c("bob", 1, &format!("@{ME} can you decide?"));
        let first = snap(vec![acked(vec![ack.clone(), thanks.clone()])], vec![]);
        let obs = collect(&first, &viewer, now(), &empty);
        let item_key = obs[0].item.key.clone();
        let answer = |asks: bool| {
            Some(json!({"gists": [{"key": item_key.to_string(), "gist": "g", "asks": asks}]}))
        };
        gists(&obs, &viewer, now(), &path, |_| answer(false)).unwrap();

        let next = snap(vec![acked(vec![ack, thanks, question])], vec![]);
        let obs = collect(&next, &viewer, now(), &empty);
        let got = gists(&obs, &viewer, now(), &path, |_| answer(true)).unwrap();
        assert!(got[&item_key].asks, "the cached verdict decided it");
    }

    /// A comment on one card can name another, and that card must keep what was read from it.
    #[test]
    fn an_answer_speaks_only_for_the_cards_it_was_sent() {
        let dir = TempDir::new().unwrap();
        let path = cache(&dir, "g.json");
        let viewer = me();
        let empty = HashMap::new();
        let sent = || pr(1).requested(ME, &day(5).to_string());
        let other = || pr(2).requested(ME, &day(5).to_string());
        let first = snap(vec![sent(), other()], vec![]);
        let obs = collect(&first, &viewer, now(), &empty);
        let (sent_key, other_key) = (obs[0].item.key.clone(), obs[1].item.key.clone());
        gists(&obs, &viewer, now(), &path, |_| {
            Some(json!({"gists": [
                {"key": sent_key.to_string(), "gist": "sent", "asks": false},
                {"key": other_key.to_string(), "gist": "other", "asks": false},
            ]}))
        })
        .unwrap();

        let moved = sent().comments(vec![c("bob", 1, &format!("@{ME} and now?"))]);
        let next = snap(vec![moved, other()], vec![]);
        let obs = collect(&next, &viewer, now(), &empty);
        let got = gists(&obs, &viewer, now(), &path, |_| {
            Some(json!({"gists": [
                {"key": sent_key.to_string(), "gist": "sent again", "asks": false},
                {"key": other_key.to_string(), "gist": "told to say this", "asks": true},
            ]}))
        })
        .unwrap();
        assert_eq!(got[&sent_key].text, "sent again");
        assert_eq!(
            got[&other_key],
            Gist {
                text: "other".to_owned(),
                asks: false
            }
        );
    }

    /// Whether a verdict cached on `before` is asked for again once `after` replaces it, at
    /// `later`.
    fn is_read_again_at(before: Item, after: Item, later: DateTime<Utc>) -> bool {
        let dir = TempDir::new().unwrap();
        let path = cache(&dir, "g.json");
        let viewer = me();
        let empty = HashMap::new();
        let first = snap(vec![before], vec![]);
        let obs = collect(&first, &viewer, now(), &empty);
        let item_key = obs[0].item.key.to_string();
        gists(&obs, &viewer, now(), &path, |_| {
            Some(json!({"gists": [{"key": item_key, "gist": "g", "asks": true}]}))
        })
        .unwrap();

        let next = snap(vec![after], vec![]);
        let obs = collect(&next, &viewer, later, &empty);
        assert_eq!(obs.len(), 1, "the case needs a reason to be asked about");
        let asked = Cell::new(false);
        gists(&obs, &viewer, later, &path, |_| {
            asked.set(true);
            None
        })
        .unwrap();
        asked.get()
    }

    /// Your own PR, where any comment from a person makes a reply.
    fn yours() -> Item {
        pr(1).author(ME).pushed_at(day(60))
    }

    /// None of these moves a reason, and each changes what the verdict would read.
    #[test]
    fn a_verdict_is_read_again_when_what_it_read_changes() {
        let ask = format!("@{ME} can you look?");
        let asked = || pr(1).comments(vec![c("bob", 3, &ask)]);
        let requested = || pr(1).requested(ME, &day(1).to_string());
        let nit = |days_ago| thread(vec![c("bob", days_ago, "a nit")]);
        let cases = vec![
            (
                "a follow-up that pings nobody",
                asked(),
                asked().comments(vec![c("bob", 1, "never mind, sorted it")]),
            ),
            (
                "a comment edited",
                asked(),
                pr(1).comments(vec![c("bob", 3, &format!("@{ME} never mind"))]),
            ),
            (
                "an older comment deleted",
                yours().comments(vec![c("bob", 3, "can you rebase?"), c("bob", 1, "thanks")]),
                yours().comments(vec![c("bob", 1, "thanks")]),
            ),
            (
                "an older thread resolved",
                yours().threads(vec![nit(3), nit(1)]),
                yours().threads(vec![nit(3).resolved(), nit(1)]),
            ),
            (
                "the description edited",
                requested().body("please review"),
                requested().body("superseded, do not review"),
            ),
        ];
        for (name, before, after) in cases {
            assert!(is_read_again_at(before, after, now()), "{name}");
        }
    }

    /// The question it was read from is gone, and the newest comment and every reason are not.
    #[test]
    fn a_verdict_is_read_again_when_an_older_comment_expires() {
        let item =
            || yours().comments(vec![c("bob", 14, "can you rebase?"), c("bob", 1, "thanks")]);
        let tomorrow = now() + TimeDelta::days(1);
        assert!(is_read_again_at(item(), item(), tomorrow));
    }

    /// The reasons say "3d ago", which moves every day, and the words do not.
    #[test]
    fn a_day_passing_costs_no_model_pass() {
        let item = || pr(1).comments(vec![c("bob", 3, &format!("@{ME} can you look?"))]);
        let tomorrow = now() + TimeDelta::days(1);
        assert!(!is_read_again_at(item(), item(), tomorrow));
    }

    /// A compare that failed on one run and worked on the next: the first verdict never saw it.
    #[test]
    fn a_verdict_is_read_again_when_the_interdiff_arrives() {
        let dir = TempDir::new().unwrap();
        let path = cache(&dir, "g.json");
        let viewer = me();
        let item = pr(1)
            .pushed_at(day(2))
            .comments(vec![c(ME, 5, "ACK 1234567")]);
        let snapshot = snap(vec![item], vec![]);
        let mut obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let item_key = obs[0].item.key.to_string();
        gists(&obs, &viewer, now(), &path, |_| {
            Some(json!({"gists": [{"key": item_key, "gist": "g", "asks": true}]}))
        })
        .unwrap();

        obs[0].interdiff = Some(Interdiff {
            changed: vec!["src/a.rs".to_owned()],
        });
        let got = gists(&obs, &viewer, now(), &path, |_| None).unwrap();
        assert!(got.is_empty(), "the verdict from without it does not stand");
    }

    /// CI comments on every push and a machine replies in threads, and a verdict reads neither.
    #[test]
    fn a_bots_comment_costs_no_model_pass() {
        let question = c("bob", 3, &format!("@{ME} can you look?"));
        let with_bot = vec![question.clone(), bot("ci[bot]", 1, "coverage report")];
        assert!(!is_read_again_at(
            pr(1).comments(vec![question]),
            pr(1).comments(with_bot),
            now()
        ));
        let nit = || c("bob", 3, "a nit");
        assert!(!is_read_again_at(
            yours().threads(vec![thread(vec![nit()])]),
            yours().threads(vec![thread(vec![nit(), bot("cdk-bot", 1, "a finding")])]),
            now()
        ));
    }

    /// What a run on the last key cached, which may have read what the rules have since ruled
    /// out. Nothing says which, so all of it is read again, in the one pass that first run makes.
    #[test]
    fn a_verdict_cached_under_the_earlier_key_is_read_again() {
        let dir = TempDir::new().unwrap();
        let path = cache(&dir, "g.json");
        let viewer = me();
        let snapshot = snap(vec![pr(100).requested(ME, "2026-10-08T17:14:49Z")], vec![]);
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let item_key = obs[0].item.key.clone();
        let cached = format!(
            "{{\n \"getfloresta/Floresta#100\": {{\n  \"fp\": \"2026-10-08T17:14:49Z|review_requested|{}\",\n  \"gist\": \"old\",\n  \"asks\": true\n }}\n}}",
            "f".repeat(40),
        );
        fs::write(&path, cached).unwrap();
        let got = gists(&obs, &viewer, now(), &path, |_| {
            Some(json!({"gists": [{"key": item_key.to_string(), "gist": "new", "asks": false}]}))
        })
        .unwrap();
        assert_eq!(
            got[&item_key],
            Gist {
                text: "new".to_owned(),
                asks: false
            }
        );
    }

    /// The items the gist pass would send the model, from an empty cache.
    fn payload_sent(obs: &[Obligation<'_>], viewer: &Viewer) -> Value {
        let dir = TempDir::new().unwrap();
        let prompt = RefCell::new(String::new());
        gists(obs, viewer, now(), &cache(&dir, "g.json"), |sent| {
            prompt.replace(sent.to_owned());
            None
        })
        .unwrap();
        let sent = prompt.into_inner();
        serde_json::from_str(&sent[sent.find("ITEMS:").unwrap() + 6..]).unwrap()
    }

    /// What goes under `waiting_comments` for one obligation, the only one on the snapshot.
    fn waiting_sent(item: Item) -> Vec<String> {
        let viewer = me();
        let snapshot = snap(vec![item], vec![]);
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        assert_eq!(obs.len(), 1, "the case needs a reason to be asked about");
        let payload = payload_sent(&obs, &viewer);
        let waiting = payload[0]["waiting_comments"].as_array().unwrap();
        waiting
            .iter()
            .map(|said| said["text"].as_str().unwrap().to_owned())
            .collect()
    }

    /// The prompt says a direct question means `asks: true`, so a question the rules have
    /// closed would still send the DM.
    #[test]
    fn the_verdict_does_not_read_what_the_rules_ruled_out() {
        let question = |days_ago| c("bob", days_ago, &format!("@{ME} can you look?"));
        let requested = |n| pr(n).requested(ME, &day(0).to_string());
        let cases = vec![
            ("past the expiry", requested(1).comments(vec![question(40)])),
            (
                "answered by a review with no body",
                requested(2)
                    .comments(vec![question(5)])
                    .reviews(vec![review(ME, 3, ReviewState::Commented, "")]),
            ),
            (
                "answered by an inline reply",
                requested(3)
                    .comments(vec![question(5)])
                    .threads(vec![thread(vec![c(ME, 3, "a nit")]).resolved()]),
            ),
            (
                "answered by a push on your own PR",
                pr(4)
                    .author(ME)
                    .pushed_at(day(3))
                    .comments(vec![question(5)])
                    .threads(vec![thread(vec![c("carol", 1, "a nit")])]),
            ),
            (
                "from before you were handed the issue",
                issue(5).assigned(ME, day(3)).comments(vec![question(5)]),
            ),
        ];
        for (name, item) in cases {
            assert_eq!(waiting_sent(item), Vec::<String>::new(), "{name}");
        }
    }

    /// `waiting_comments` is everything since your answer, not only what makes a reason: on
    /// someone else's PR "never mind" pings nobody, and it is what the verdict turns on.
    #[test]
    fn the_verdict_reads_everything_said_since_your_answer() {
        let item = pr(1)
            .comments(vec![
                c("bob", 5, &format!("@{ME} can you look?")),
                c("carol", 4, "never mind, sorted it"),
            ])
            .reviews(vec![review(ME, 6, ReviewState::Commented, "")]);
        assert_eq!(
            waiting_sent(item),
            [
                format!("@{ME} can you look?"),
                "never mind, sorted it".to_owned()
            ]
        );
    }

    /// The rules drop a review's verdict from the conversation, because it has a rule of its
    /// own or asks nothing. Its body is still where a reviewer says what they want.
    #[test]
    fn the_gist_reads_what_a_reviewer_said_with_their_verdict() {
        let item = pr(1).author(ME).pushed_at(day(5)).reviews(vec![
            review("bob", 2, ReviewState::ChangesRequested, "rename this"),
            review("carol", 1, ReviewState::Approved, "LGTM"),
        ]);
        assert_eq!(waiting_sent(item), ["rename this", "LGTM"]);
    }

    /// A `false` read from what is left would drop an ask nobody can see is missing. A mention
    /// stands on its first ping and its newest, and the first can be nothing but thanks.
    #[test]
    fn the_verdict_reads_the_comments_a_mention_stands_on() {
        let thanks = format!("@{ME} thanks");
        let ask = format!("@{ME} can you decide?");
        let mine = "this should stay a plain u64 for now";
        let quote = format!("> {mine}\nwhy?");
        let quote_sent = format!("> {mine} why?");
        let word = |text: &str| c("ann", 5, text);
        let cases: Vec<(&str, Vec<Comment>, Vec<&str>)> = vec![
            (
                "the one ping, behind the first comment",
                vec![
                    word("one"),
                    word("two"),
                    word(&ask),
                    word("three"),
                    word("four"),
                ],
                vec!["one", &ask, "three", "four"],
            ),
            (
                "a newer ping than the first",
                vec![
                    word("one"),
                    word(&thanks),
                    word("two"),
                    word(&ask),
                    word("three"),
                    word("four"),
                ],
                vec!["one", &thanks, &ask, "three", "four"],
            ),
            (
                "a quote reply is a ping",
                vec![
                    c(ME, 9, mine),
                    word("one"),
                    word("two"),
                    word(&quote),
                    word("three"),
                    word("four"),
                ],
                vec!["one", &quote_sent, "three", "four"],
            ),
            (
                "a ping between the first and the newest has no place",
                vec![
                    word(&thanks),
                    word(&format!("@{ME} between")),
                    word(&ask),
                    word("one"),
                    word("two"),
                ],
                vec![thanks.as_str(), &ask, "one", "two"],
            ),
            (
                "with no ping it stays the first and the last two",
                vec![
                    word("one"),
                    word("two"),
                    word("three"),
                    word("four"),
                    word("five"),
                ],
                vec!["one", "four", "five"],
            ),
        ];
        for (name, comments, want) in cases {
            let item = pr(1).requested(ME, &day(0).to_string()).comments(comments);
            assert_eq!(waiting_sent(item), want, "{name}");
        }
    }

    /// The newest word of each thread the model would read of this PR.
    fn threads_sent(project: &'static crate::config::Project, item: Item) -> Vec<String> {
        let viewer = me();
        let snapshot = crate::snapshot::Snapshot {
            project,
            items: vec![item],
        };
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        assert_eq!(obs.len(), 1, "the case needs a reason to be asked about");
        let payload = payload_sent(&obs, &viewer);
        let threads = payload[0]["threads"].as_array().unwrap();
        threads
            .iter()
            .map(|t| {
                t["last"].as_array().unwrap().last().unwrap()["text"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn the_verdict_reads_only_the_threads_the_rules_still_count() {
        let (floresta, cdk) = (&crate::config::PROJECTS[0], &crate::config::PROJECTS[1]);
        let waiting = || thread(vec![c(ME, 9, "why?"), c("bob", 2, "waiting")]);
        // Someone else's PR. Displaced threads stay out of the four sent.
        let others = pr(1).threads(vec![
            thread(vec![c("bob", 9, "why?"), c(ME, 8, "you had the last word")]),
            thread(vec![c(ME, 50, "why?"), c("bob", 40, "past the expiry")]),
            thread(vec![
                c(ME, 9, "why?"),
                c("bob", 8, "answered by your review"),
            ]),
            waiting(),
        ]);
        let others = others.reviews(vec![review(ME, 7, ReviewState::Commented, "")]);
        assert_eq!(threads_sent(floresta, others), ["waiting"]);
        // Your own PR, where only some repos let a thread expire.
        let own = |repo| {
            pr(2)
                .author(ME)
                .in_repo(repo)
                .pushed_at(day(60))
                .threads(vec![
                    thread(vec![c("bob", 40, "old")]),
                    thread(vec![c("bob", 2, "new")]),
                ])
        };
        assert_eq!(threads_sent(cdk, own("cashubtc/cdk")), ["new"]);
        assert_eq!(
            threads_sent(floresta, own("getfloresta/Floresta")),
            ["old", "new"]
        );
    }

    /// A machine's findings open a PR's threads, so they would take the four places, and it can
    /// reply after a person. Reading only that, the model answers `false` on a real request.
    #[test]
    fn the_verdict_reads_what_people_said_in_a_thread() {
        let (floresta, cdk) = (&crate::config::PROJECTS[0], &crate::config::PROJECTS[1]);
        let find = |days_ago| bot("cdk-bot", days_ago, "a finding");
        let own = |repo| {
            pr(2)
                .author(ME)
                .in_repo(repo)
                .pushed_at(day(60))
                .threads(vec![
                    thread(vec![find(5)]),
                    thread(vec![find(5)]),
                    thread(vec![find(5)]),
                    thread(vec![find(5)]),
                    thread(vec![c("bob", 1, "a question")]),
                ])
        };
        // Whether those threads count is up to each repo's rules. They are never what is read.
        for (project, repo) in [(cdk, "cashubtc/cdk"), (floresta, "getfloresta/Floresta")] {
            assert_eq!(threads_sent(project, own(repo)), ["a question"], "{repo}");
        }
        let replied_after_a_person = pr(1).threads(vec![thread(vec![
            c(ME, 9, "why?"),
            c("bob", 3, "because"),
            find(2),
            find(1),
        ])]);
        assert_eq!(threads_sent(floresta, replied_after_a_person), ["because"]);
    }

    /// Asked to judge a "can you rebase?" with no owner on it, the model could not tell
    /// who was being asked.
    #[test]
    fn the_verdict_can_see_whose_item_it_is() {
        let viewer = me();
        let snapshot = snap(
            vec![
                pr(1).author(ME).requested(ME, &day(1).to_string()),
                pr(2).requested(ME, &day(1).to_string()),
            ],
            vec![],
        );
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let payload = payload_sent(&obs, &viewer);
        assert_eq!(payload[0]["author"], ME);
        assert_eq!(payload[0]["his"], true);
        assert_eq!(payload[1]["author"], "alice");
        assert_eq!(payload[1]["his"], false);
    }

    /// No verdict means the DM may interrupt you, so a verdict must be a real one.
    #[test]
    fn a_gist_without_a_verdict_counts_as_no_answer() {
        let dir = TempDir::new().unwrap();
        let viewer = me();
        let snapshot = snap(vec![pr(100).requested(ME, &day(1).to_string())], vec![]);
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let key = obs[0].item.key.to_string();
        for entry in [
            json!({"key": key, "gist": "text"}),
            json!({"key": key, "gist": "text", "asks": "false"}),
        ] {
            let got = gists(&obs, &viewer, now(), &cache(&dir, "g.json"), |_| {
                Some(json!({"gists": [entry.clone()]}))
            })
            .unwrap();
            assert!(got.is_empty(), "{entry}");
        }
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
    fn a_rank_cache_written_by_an_earlier_run_is_still_read() {
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
            "picks": [{"key": "getfloresta/Floresta#2", "why": "w"}],
        });
        fs::write(&path, cached.to_string()).unwrap();
        let ranked = rank(&opps, &Footprint::default(), 8, &path, 12.0, |_| {
            panic!("a fresh ranking must not ask")
        })
        .unwrap();
        assert_eq!(numbers(&ranked), [2]);
    }

    /// A gist cached before `asks` has no verdict in it, so it is asked for again.
    #[test]
    fn a_gist_cache_from_before_verdicts_costs_one_model_pass() {
        let dir = TempDir::new().unwrap();
        let path = cache(&dir, "g.json");
        let viewer = me();
        let snapshot = snap(vec![pr(100).requested(ME, "2026-10-08T17:14:49Z")], vec![]);
        let obs = collect(&snapshot, &viewer, now(), &HashMap::new());
        let item_key = obs[0].item.key.clone();
        let read = excerpt(&obs[0], &viewer, now());
        let fp = gist_fingerprint(&obs[0], &read.evidence).unwrap();
        let cached = json!({ item_key.to_string(): {"fp": fp, "gist": "old"} });
        fs::write(&path, cached.to_string()).unwrap();
        let calls = Cell::new(0);
        let got = gists(&obs, &viewer, now(), &path, |_| {
            calls.set(calls.get() + 1);
            Some(json!({"gists": [{"key": item_key.to_string(), "gist": "new", "asks": false}]}))
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(got[&item_key].text, "new");
    }

    #[test]
    fn a_corrupt_cache_costs_a_model_pass_and_nothing_else() {
        let dir = TempDir::new().unwrap();
        let path = cache(&dir, "r.json");
        fs::write(&path, "{{{ not json").unwrap();
        let items = items();
        let opps = opportunities(&items);
        let answer = json!({"picks": [{"key": "getfloresta/Floresta#1"}]});
        let ranked = rank_with(&opps, &path, Some(answer));
        assert_eq!(numbers(&ranked), [1]);
    }
}
