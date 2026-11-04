//! The two model passes, and the only places a model touches the output.
//!
//!   gists  one line per obligation. Decorative by contract: an obligation with no
//!          gist is still listed, in the same place, with its computed reasons.
//!   rank   the opportunity judge -- filters, orders, estimates, pitches. It may
//!          only point at candidates it was given; anything else is dropped.
//!
//! Both are cached in state/ so the hourly refresh pays for the model only when
//! something it would look at has changed. Neither subprocess gets a GitHub token:
//! the data goes in inline, and nothing here needs to fetch.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::ser::PrettyFormatter;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::footprint::Footprint;
use crate::obligations::{Obligation, Viewer};
use crate::opportunities::{Opportunity, Source};
use crate::proc;
use crate::prompts;
use crate::snapshot::{Interdiff, ReviewState};
use crate::text::{clip, is_truthy};

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
    cmd.args(["-p", prompt])
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

/// A state file, or an empty object if it is missing or corrupt.
pub fn load(path: &Path) -> Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}))
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

/// A model-supplied field as text, whatever JSON type it arrived as.
fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(true)) => "True".to_owned(),
        Some(other) if is_truthy(other) => other.to_string(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------- gists

fn gist_fingerprint(ob: &Obligation<'_>) -> String {
    let last = ob
        .reasons
        .iter()
        .filter_map(|reason| reason.last)
        .max()
        .map_or(String::new(), |last| last.to_string());
    let mut kinds: Vec<&str> = ob.reasons.iter().map(|r| r.kind.as_str()).collect();
    kinds.sort_unstable();
    format!(
        "{last}|{}|{}",
        kinds.join(","),
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
    key: &'a str,
    kind: &'static str,
    title: &'a str,
    reasons: Vec<&'a str>,
    description: String,
    waiting_comments: Vec<Said<'a>>,
    #[serde(flatten)]
    pr: Option<PrExcerpt<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    since_your_ack: Option<&'a Interdiff>,
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
        key: &ob.key,
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
        since_your_ack: ob.interdiff.as_ref(),
    }
}

#[derive(Serialize, Deserialize)]
struct CachedGist {
    fp: String,
    gist: String,
}

/// `key -> gist`. Cached per fingerprint; only changed items reach the model.
pub fn gists(
    obs: &[Obligation<'_>],
    me: &Viewer,
    cache_path: &Path,
    mut ask: impl FnMut(&str) -> Option<Value>,
) -> Result<BTreeMap<String, String>> {
    let mut cache: BTreeMap<String, CachedGist> =
        serde_json::from_value(load(cache_path)).unwrap_or_default();
    let want: HashMap<&str, String> = obs
        .iter()
        .map(|ob| (ob.key.as_str(), gist_fingerprint(ob)))
        .collect();
    let todo: Vec<_> = obs
        .iter()
        .filter(|ob| cache.get(&ob.key).map(|cached| &cached.fp) != want.get(ob.key.as_str()))
        .collect();
    if !todo.is_empty() {
        let excerpts: Vec<_> = todo.iter().map(|ob| excerpt(ob, me)).collect();
        let prompt = prompts::GIST.replace("%s", &dumps(&excerpts)?);
        let answer = ask(&prompt).unwrap_or(Value::Null);
        for entry in answer["gists"].as_array().into_iter().flatten() {
            let (Some(key), Some(gist)) = (entry["key"].as_str(), entry.get("gist")) else {
                continue;
            };
            if let Some(fp) = want.get(key)
                && is_truthy(gist)
            {
                let cached = CachedGist {
                    fp: fp.clone(),
                    gist: clip(&text_of(Some(gist)), 200),
                };
                cache.insert(key.to_owned(), cached);
            }
        }
    }
    cache.retain(|key, _| want.contains_key(key.as_str())); // forget what cleared
    save(cache_path, &cache)?;
    Ok(cache
        .into_iter()
        .filter(|(key, cached)| want.get(key.as_str()) == Some(&cached.fp))
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
        since_you: Some(o.note.as_str()).filter(|note| !note.is_empty()),
        pr: item.pr().map(|pr| CandidatePr {
            size: format!("+{}/-{}", pr.additions, pr.deletions),
            files: pr.changed_files,
            draft: pr.draft,
            ci: o.ci(),
            mergeable: o.mergeable(),
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
        .map(|o| format!("{}@{}", o.key, o.item.head().unwrap_or("")))
        .collect();
    signature.sort_unstable();
    let digest = Sha256::digest(signature.join("\n").as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    hex[..16].to_owned()
}

/// The candidate number a pick points at, however the judge wrote it.
fn pick_number(pick: &Value) -> Option<u64> {
    let number = match pick.get("number")? {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64))?,
        Value::String(text) => text.trim().parse().ok()?,
        Value::Bool(flag) => i64::from(*flag),
        _ => return None,
    };
    u64::try_from(number).ok()
}

pub struct Pick<'o, 'a> {
    pub opp: &'o Opportunity<'a>,
    pub why: String,
    pub pitch: String,
    pub est: String,
}

impl<'o, 'a> Pick<'o, 'a> {
    /// A pick with nothing from the judge: ranking order, shown as such.
    pub fn mechanical(opp: &'o Opportunity<'a>) -> Self {
        Self {
            opp,
            why: String::new(),
            pitch: String::new(),
            est: String::new(),
        }
    }
}

pub struct Ranked<'o, 'a> {
    pub picks: Vec<Pick<'o, 'a>>,
    pub judged: bool,
}

/// Falls back to the order given, untouched, with `judged` false.
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
    let cache = load(cache_path);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64());
    let fresh = cache["sig"].as_str() == Some(sig.as_str())
        && now - cache["at"].as_f64().unwrap_or(0.0) < ttl_hours * 3600.0;
    let mut picks = if fresh {
        cache["picks"].as_array().cloned()
    } else {
        None
    };
    if picks.is_none() && !opps.is_empty() {
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
        let answer = ask(&prompt).unwrap_or(Value::Null);
        picks = answer["picks"].as_array().cloned();
        // An empty verdict is retried, not kept.
        if let Some(list) = picks.as_ref().filter(|list| !list.is_empty()) {
            save(cache_path, &json!({"sig": sig, "at": now, "picks": list}))?;
        }
    }
    let mut out: Vec<Pick<'o, 'a>> = Vec::new();
    let mut seen = HashSet::new();
    for pick in picks.iter().flatten() {
        let Some(number) = pick_number(pick) else {
            continue;
        };
        let Some(opp) = by_number.get(&number).copied() else {
            continue; // invented
        };
        if !seen.insert(number) {
            continue; // a duplicate
        }
        out.push(Pick {
            opp,
            why: clip(&text_of(pick.get("why")), 80),
            pitch: clip(&text_of(pick.get("pitch")), 240),
            est: clip(&text_of(pick.get("est")), 8),
        });
        if out.len() >= cap {
            break;
        }
    }
    if !out.is_empty() {
        return Ok(Ranked {
            picks: out,
            judged: true,
        });
    }
    // No usable answer -- or the judge kept nothing, which is distrusted the same
    // way the old board distrusted it.
    Ok(Ranked {
        picks: opps.iter().take(cap).map(Pick::mechanical).collect(),
        judged: false,
    })
}

#[cfg(test)]
mod tests {
    //! How the judge's answer is trusted, and that the hourly refresh does not pay
    //! for the model when nothing moved.

    use super::*;
    use crate::obligations::collect;
    use crate::opportunities::Source;
    use crate::render::Dm;
    use crate::snapshot::Item;
    use crate::testkit::{ME, c, day, me, now, pr, snap};
    use std::cell::Cell;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn opportunities(items: &[Item]) -> Vec<Opportunity<'_>> {
        items
            .iter()
            .map(|item| Opportunity {
                key: item.key.clone(),
                item,
                sources: vec![Source::Unreviewed],
                note: String::new(),
                age_days: 2.0,
                areas: vec![],
                merge: None,
            })
            .collect()
    }

    fn items() -> Vec<Item> {
        vec![pr(1), pr(2), pr(3)]
    }

    fn cache(dir: &TempDir, name: &str) -> PathBuf {
        dir.path().join(name)
    }

    fn numbers(ranked: &Ranked<'_, '_>) -> Vec<u64> {
        ranked.picks.iter().map(|p| p.opp.item.number).collect()
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
        assert!(ranked.judged);
        assert_eq!(numbers(&ranked), [3, 1], "invented and duplicate dropped");
    }

    #[test]
    fn an_empty_verdict_falls_back_to_ranking_order_flagged() {
        let dir = TempDir::new().unwrap();
        let items = items();
        let opps = opportunities(&items);
        let ranked = rank_with(&opps, &cache(&dir, "r.json"), Some(json!({"picks": []})));
        assert!(!ranked.judged);
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
            picks: &ranked.picks,
            judged: ranked.judged,
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
        let key = obs[0].key.clone();

        let calls = Cell::new(0);
        let ask = |_: &str| {
            calls.set(calls.get() + 1);
            Some(json!({"gists": [{"key": key, "gist": "asks for a yes/no"}]}))
        };
        let first = gists(&obs, &viewer, &path, ask).unwrap();
        let second = gists(&obs, &viewer, &path, ask).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(first, second);
        assert_eq!(first[&key], "asks for a yes/no");

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
            "{{\n \"{}\": {{\n  \"fp\": \"2026-10-08T17:14:49Z|review_requested|{}\",\n  \"gist\": \"caf\\u00e9\"\n }}\n}}",
            obs[0].key,
            "f".repeat(40),
        );
        fs::write(&path, cached).unwrap();
        let got = gists(&obs, &viewer, &path, |_| {
            panic!("a cached gist must not ask")
        })
        .unwrap();
        assert_eq!(got[&obs[0].key], "café");
    }
}
