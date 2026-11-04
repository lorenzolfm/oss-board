//! The whole run, through the real binary, against fake `gh`, `git` and `claude`
//! executables on PATH. These pin what only the orchestration can get wrong: what a
//! failed source costs, what each subprocess is allowed to hold, and what reaches
//! the outside world.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use chrono::{SecondsFormat, TimeDelta, Utc};
use tempfile::TempDir;

const FAKE_GH: &str = r#"#!/bin/sh
if [ -n "$GH_FAIL_ON" ]; then
  case "$*" in *"$GH_FAIL_ON"*) echo boom >&2; exit 1;; esac
fi
case "$*" in
  *"viewer { login }"*) echo '{"data":{"viewer":{"login":"me"}}}';;
  *"creator=me"*) ;;
  *"first: 50"*) echo '{"data":{"repository":{"pullRequests":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}}';;
  *'owner: "cashubtc"'*"pullRequests(states: OPEN"*) cat "$FIXTURE_DIR/cdk_prs.json";;
  *"pullRequests(states: OPEN"*) cat "$FIXTURE_DIR/prs.json";;
  *"repos/cashubtc/cdk/compare/main..."*) echo '[{"filename":"src/a.rs","patch":"same"}]';;
  *"issues(states: OPEN"*) echo '{"data":{"repository":{"issues":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}}';;
  *mergeable*) cat "$FIXTURE_DIR/merge.json";;
  *projectV2*) echo '{"data":{"organization":{"projectV2":{"items":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}}}';;
  *issueOrPullRequest*) cat "$FIXTURE_DIR/claims.json";;
  *) echo "unexpected gh call: $*" >&2; exit 1;;
esac
"#;

/// A repository with nothing unshipped on it, or one that fails the reads named.
const FAKE_GIT: &str = r#"#!/bin/sh
if [ -n "$GIT_FAIL_ON" ]; then
  case "$*" in *"$GIT_FAIL_ON"*) echo boom >&2; exit 128;; esac
fi
case "$*" in
  *"config user.email"*) echo me@example.com;;
esac
"#;

/// Records which tokens it was handed, wraps its answers in prose as a real model
/// does, and writes the note a briefing asks for. Keeps the board it saw at that
/// moment, for what a slow briefing can cost.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
prompt=$2
shift 2
args=$(printf '[%s]' "$@")
case "$prompt" in
  "Each item below"*)
    echo "model GH_TOKEN=${GH_TOKEN:-unset} GITHUB_TOKEN=${GITHUB_TOKEN:-unset} args=$args" >> "$FIXTURE_DIR/claude.log"
    echo 'Sure, here you go:'
    asks=true
    [ -f "$FIXTURE_DIR/nobody_asks" ] && asks=false
    echo '{"gists":[{"key":"getfloresta/Floresta#7","gist":"wants a yes/no","asks":'"$asks"'}]}';;
  "You are picking"*)
    echo "model GH_TOKEN=${GH_TOKEN:-unset} GITHUB_TOKEN=${GITHUB_TOKEN:-unset} args=$args" >> "$FIXTURE_DIR/claude.log"
    [ -f "$FIXTURE_DIR/no_judge" ] && exit 1
    echo '{"picks":[{"key":"getfloresta/Floresta#8","why":"small and yours"}]}';;
  "Write an Obsidian"*)
    echo "brief GH_TOKEN=${GH_TOKEN:-unset} GITHUB_TOKEN=${GITHUB_TOKEN:-unset} args=$args" >> "$FIXTURE_DIR/claude.log"
    cp "$FIXTURE_DIR/vault/Tasks.md" "$FIXTURE_DIR/board_at_brief.md"
    note=$(printf '%s\n' "$prompt" | sed -n '/exactly this path/{n;s/^ *//;p;}')
    printf '%0400d' 0 > "$note"
    echo DONE;;
esac
"#;

/// Writes `TODO` where the note should go, then fails.
const FAILING_AGENT: &str = r#"#!/bin/sh
echo call >> "$FIXTURE_DIR/claude.log"
note=$(printf '%s\n' "$2" | sed -n '/exactly this path/{n;s/^ *//;p;}')
printf 'TODO' > "$note"
exit 1
"#;

/// An open PR, as the bulk query returns one, opened recently enough that a mention in
/// its description is still asking.
fn pr_node(number: u64, title: &str, body: &str, head: &str, author: &str) -> String {
    let opened = (Utc::now() - TimeDelta::days(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
    format!(
        r#"{{"number":{number},"title":"{title}","url":"https://github.com/getfloresta/Floresta/pull/{number}",
        "body":"{body}","isDraft":false,"createdAt":"{opened}","updatedAt":"{opened}",
        "additions":1,"deletions":0,"changedFiles":1,"headRefOid":"{head}",
        "author":{{"__typename":"User","login":"{author}"}},"milestone":null,"labels":{{"nodes":[]}},
        "assignees":{{"nodes":[]}},"reviewDecision":null,"reviewRequests":{{"nodes":[]}},
        "commits":{{"nodes":[{{"commit":{{"committedDate":"2026-01-01T00:00:00Z"}}}}]}},
        "reviews":{{"nodes":[]}},"comments":{{"nodes":[]}},"reviewThreads":{{"nodes":[]}},
        "timelineItems":{{"nodes":[]}},"forcePushes":{{"nodes":[]}},"files":{{"nodes":[]}}}}"#
    )
}

fn prs_page(nodes: &[String]) -> String {
    format!(
        r#"{{"data":{{"repository":{{"pullRequests":{{"pageInfo":{{"hasNextPage":false,"endCursor":null}},
        "nodes":[{}]}}}}}}}}"#,
        nodes.join(",")
    )
}

/// #7 mentions you, so it is an obligation. #8 is unreviewed, so an opportunity.
fn prs() -> String {
    prs_page(&[
        pr_node(7, "waiting on you", "cc @me please", "aaaaaaa", "bob"),
        pr_node(8, "nobody reviewed this", "", "bbbbbbb", "carol"),
    ])
}

/// The same page as it would come from cdk, numbers and all.
fn on_cdk(page: &str) -> String {
    page.replace("getfloresta/Floresta", "cashubtc/cdk")
}

fn merge_node(mergeable: &str, ci: &str) -> String {
    format!(
        r#"{{"mergeable":"{mergeable}","commits":{{"nodes":[{{"commit":{{"statusCheckRollup":{ci}}}}}]}}}}"#
    )
}

/// Your own PR #9, conflicting and red.
const MY_PR_HEAD: &str = "ccccccc";

fn my_broken_pr() -> (String, String) {
    (
        prs_page(&[pr_node(9, "my red pr", "", MY_PR_HEAD, "me")]),
        format!(
            r#"{{"data":{{"repository":{{"p9":{}}}}}}}"#,
            merge_node("CONFLICTING", r#"{"state":"FAILURE"}"#)
        ),
    )
}

/// The claim on #8 is open and nothing has happened to it.
const CLAIMS: &str = r#"{"data":{"n0":{"issueOrPullRequest":{
  "__typename":"PullRequest","state":"OPEN","mergedAt":null,"author":{"login":"carol"}}}}}"#;

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let fixture = Self { dir };
        let bin = fixture.path("bin");
        fs::create_dir(&bin).unwrap();
        for (name, script) in [("gh", FAKE_GH), ("claude", FAKE_CLAUDE), ("git", FAKE_GIT)] {
            let path = bin.join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(fixture.path("prs.json"), prs()).unwrap();
        fs::write(fixture.path("cdk_prs.json"), prs_page(&[])).unwrap();
        let clean = merge_node("MERGEABLE", "null");
        fs::write(
            fixture.path("merge.json"),
            format!(r#"{{"data":{{"repository":{{"p7":{clean},"p8":{clean},"p9":{clean}}}}}}}"#),
        )
        .unwrap();
        fs::write(fixture.path("claims.json"), CLAIMS).unwrap();
        fs::create_dir(fixture.path("vault")).unwrap();
        fixture
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn board(&self) -> PathBuf {
        self.path("vault/Tasks.md")
    }

    fn read_board(&self) -> String {
        fs::read_to_string(self.board()).unwrap()
    }

    fn read_state(&self, name: &str) -> String {
        fs::read_to_string(self.path("state").join(name)).unwrap()
    }

    fn degraded(&self) -> serde_json::Value {
        let last_run: serde_json::Value =
            serde_json::from_str(&self.read_state("last_run.json")).unwrap();
        last_run["degraded"].clone()
    }

    fn claude_log(&self) -> String {
        fs::read_to_string(self.path("claude.log")).unwrap_or_default()
    }

    /// A refresh with the fakes first on PATH and nothing of the real user's.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = self.binary();
        cmd.arg("refresh")
            .args(["--board", self.board().to_str().unwrap()])
            .args(["--state", self.path("state").to_str().unwrap()])
            .args(args);
        cmd
    }

    fn binary(&self) -> Command {
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_oss-board"));
        cmd.env("PATH", path)
            .env("HOME", self.dir.path())
            .env("FIXTURE_DIR", self.dir.path())
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_FAIL_ON")
            .env_remove("GIT_FAIL_ON")
            .env_remove("TELEGRAM_BOT_TOKEN")
            .env_remove("TELEGRAM_BOT_TOKEN_FILE")
            .env_remove("OPENCLAW_URL")
            .env_remove("UPTIME_KUMA_PUSH_URL");
        cmd
    }
}

fn run(cmd: &mut Command) -> Output {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Answers every HTTP request 200 and hands back what it was sent.
fn listener() -> (String, Receiver<String>) {
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", socket.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for stream in socket.incoming().flatten() {
            if tx.send(read_request(stream)).is_err() {
                return;
            }
        }
    });
    (url, rx)
}

fn read_request(mut stream: TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
        request.push_str(&line);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    request.push_str(&String::from_utf8_lossy(&body));
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .unwrap();
    request
}

/// The lane's cards, however the board around them is laid out.
fn lane<'t>(board: &'t str, name: &str) -> Vec<&'t str> {
    let heading = format!("## {name}");
    board
        .lines()
        .skip_while(|line| *line != heading)
        .skip(1)
        .take_while(|line| !line.starts_with("## ") && !line.starts_with("%%"))
        .filter(|line| !line.trim().is_empty())
        .collect()
}

fn board_text(unshipped: &str, doing: &str) -> String {
    format!(
        "---\n\nkanban-plugin: board\n\n---\n\n## Needs me\n\n- [ ] stale card\n\n\
         ## Opportunities\n\n## Unshipped\n\n{unshipped}\n\n## Todo\n\n- [ ] my own words ^abc\n\n\
         ## Doing\n\n{doing}\n\n## Done\n\n## Passed\n\n\n\n\
         %% kanban:settings\n```\n{{\"kanban-plugin\":\"board\",\"list-collapse\":[]}}\n```\n%%\n"
    )
}

#[test]
fn a_run_rewrites_the_agent_lanes_and_leaves_yours() {
    let fixture = Fixture::new();
    fs::write(fixture.board(), board_text("", "")).unwrap();
    let (url, beats) = listener();
    run(fixture
        .command(&["--notify", "--no-briefings"])
        .env("GH_TOKEN", "read-only-token")
        .env("UPTIME_KUMA_PUSH_URL", format!("{url}/push?token=abc")));

    let board = fixture.read_board();
    let needs_me = lane(&board, "Needs me");
    assert_eq!(needs_me.len(), 1, "{board}");
    assert!(
        needs_me[0].contains("[floresta#7](") && needs_me[0].contains("*(wants a yes/no)*"),
        "{board}"
    );
    assert!(
        needs_me[0].contains("<!--t:"),
        "the fingerprint a mute needs: {board}"
    );
    let opportunities = lane(&board, "Opportunities");
    assert_eq!(opportunities.len(), 1, "{board}");
    assert!(
        opportunities[0].contains("[floresta#8](")
            && opportunities[0].contains("*(small and yours)*"),
        "{board}"
    );
    assert_eq!(lane(&board, "Todo"), ["- [ ] my own words ^abc"]);

    let last_run: serde_json::Value =
        serde_json::from_str(&fixture.read_state("last_run.json")).unwrap();
    assert_eq!(last_run["counts"]["obligations"], 1);
    assert_eq!(last_run["counts"]["judged"], true);
    assert_eq!(fixture.degraded(), serde_json::json!([]));

    let beat = beats.recv().unwrap();
    assert!(
        beat.starts_with("GET /push?token=abc&status=up&msg="),
        "{beat}"
    );
}

/// The data goes in inline, and the prompts carry text from anyone who can open an
/// issue, so the model passes get no tools. An unset token alone would leave them your
/// own login through `gh`. The fake only answers a prompt in `$2`, so this also fails
/// if the prompt ever follows `--tools`, which would swallow it.
#[test]
fn the_model_passes_get_no_tools_and_no_github_token() {
    let fixture = Fixture::new();
    run(fixture
        .command(&["--no-briefings"])
        .env("GH_TOKEN", "read-only-token")
        .env("GITHUB_TOKEN", "another-token"));
    let log = fixture.claude_log();
    assert_eq!(log.lines().count(), 2, "{log}");
    assert!(
        log.lines().all(|l| l
            == "model GH_TOKEN=unset GITHUB_TOKEN=unset args=[--tools][][--strict-mcp-config]"),
        "{log}"
    );
}

/// A lane computed from nothing would delete every card.
#[test]
fn a_failed_snapshot_leaves_the_board_untouched_and_beats_down() {
    let fixture = Fixture::new();
    let before = board_text("- [ ] ⚡ `wip` <!--k:branch:wip-->", "");
    fs::write(fixture.board(), &before).unwrap();
    let (url, beats) = listener();
    let out = fixture
        .command(&["--notify"])
        .env("GH_FAIL_ON", "issues(states: OPEN")
        .env("UPTIME_KUMA_PUSH_URL", format!("{url}/push"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(fixture.read_board(), before);
    let beat = beats.recv().unwrap();
    assert!(beat.starts_with("GET /push?status=down&msg="), "{beat}");
}

/// Stale cards are recoverable, deleted ones are not.
#[test]
fn a_failed_unshipped_source_keeps_its_lane_and_says_so_in_the_beat() {
    let fixture = Fixture::new();
    let wip = "- [ ] ⚡ `wip` <!--k:branch:wip-->";
    fs::write(fixture.board(), board_text(wip, "")).unwrap();
    let (url, beats) = listener();
    run(fixture
        .command(&["--notify", "--no-model"])
        .env("GH_FAIL_ON", "creator=me")
        .env("UPTIME_KUMA_PUSH_URL", format!("{url}/push")));

    let board = fixture.read_board();
    assert_eq!(lane(&board, "Unshipped"), [wip]);
    assert_eq!(
        lane(&board, "Needs me").len(),
        1,
        "the other lanes still refresh"
    );
    assert_eq!(fixture.degraded(), serde_json::json!(["unshipped"]));
    let beat = beats.recv().unwrap();
    assert!(beat.contains("degraded"), "{beat}");
}

/// A failing `git` used to read as a clone with no branches on it, and a footprint
/// that could not be read as one with no areas.
#[test]
fn a_failing_git_keeps_the_unshipped_lane_and_the_rest_still_runs() {
    let wip = "- [ ] ⚡ `wip` <!--k:branch:wip-->";
    for (fails, kept, want) in [
        ("-C", true, serde_json::json!(["unshipped", "footprint"])),
        ("for-each-ref", true, serde_json::json!(["unshipped"])),
        (
            "config user.email",
            true,
            serde_json::json!(["unshipped", "footprint"]),
        ),
        (" log ", false, serde_json::json!(["footprint"])),
    ] {
        let fixture = Fixture::new();
        fs::write(fixture.board(), board_text(wip, "")).unwrap();
        run(fixture
            .command(&["--no-model", "--no-briefings"])
            .env("GIT_FAIL_ON", fails));

        let board = fixture.read_board();
        assert_eq!(
            !lane(&board, "Unshipped").is_empty(),
            kept,
            "{fails}: {board}"
        );
        assert_eq!(lane(&board, "Needs me").len(), 1, "{fails}: {board}");
        assert_eq!(fixture.degraded(), want, "{fails}");
        let dm = fixture.read_state("summary.txt");
        assert_eq!(
            dm.contains("Unshipped not refreshed"),
            kept,
            "{fails}: {dm}"
        );
    }
}

/// A PR list that is missing, or that cannot be followed to its end, would otherwise
/// read as a repo with nothing open.
#[test]
fn an_unreadable_pr_list_fails_the_run_and_leaves_the_board() {
    let bad_url = prs_page(&[pr_node(1, "t", "", "aaaaaaa", "bob")]).replace(
        "https://github.com/getfloresta/Floresta/pull/1",
        "not a url",
    );
    let conversation = |list: &str| {
        prs_page(&[pr_node(1, "t", "", "aaaaaaa", "bob")]).replace(
            r#""comments":{"nodes":[]}"#,
            &format!(r#""comments":{list}"#),
        )
    };
    let (no_nodes, null_nodes, null_list) = (
        conversation("{}"),
        conversation(r#"{"nodes":null}"#),
        conversation("null"),
    );
    for prs in [
        bad_url.as_str(),
        no_nodes.as_str(),
        null_nodes.as_str(),
        null_list.as_str(),
        r#"{"data":{"repository":{"pullRequests":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":null}}}}"#,
        r#"{"data":{"repository":{"pullRequests":{"nodes":[]}}}}"#,
        r#"{"data":{"repository":{"pullRequests":{}}}}"#,
        r#"{"data":{"repository":{"pullRequests":null}}}"#,
        r#"{"data":{"repository":{}}}"#,
        r#"{"data":{"repository":null}}"#,
        r#"{"data":{"repository":{"pullRequests":{"pageInfo":{"hasNextPage":true,"endCursor":null},"nodes":[]}}}}"#,
        r#"{"data":{"repository":{"pullRequests":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"number":1}]}}}}"#,
    ] {
        let fixture = Fixture::new();
        let before = board_text("", "");
        fs::write(fixture.board(), &before).unwrap();
        fs::write(fixture.path("prs.json"), prs).unwrap();
        let out = fixture
            .command(&["--no-model", "--no-briefings"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{prs}");
        assert_eq!(fixture.read_board(), before, "{prs}");
    }
}

/// A failed read must not look like the all-clear: your PR is still broken.
#[test]
fn a_failed_merge_read_keeps_the_card_and_is_not_an_all_clear() {
    let fixture = Fixture::new();
    let (prs, merge) = my_broken_pr();
    fs::write(fixture.path("prs.json"), prs).unwrap();
    fs::write(fixture.path("merge.json"), merge).unwrap();
    run(&mut fixture.command(&["--no-model", "--no-briefings"]));
    let board = fixture.read_board();
    let card = lane(&board, "Needs me")[0].to_owned();
    assert!(card.contains("conflicts with its base"), "{card}");
    assert!(card.contains(MY_PR_HEAD), "the head it was seen on: {card}");

    let (url, beats) = listener();
    run(fixture
        .command(&["--notify", "--no-model", "--no-briefings"])
        .env("GH_FAIL_ON", "mergeable")
        .env("UPTIME_KUMA_PUSH_URL", format!("{url}/push")));
    let board = fixture.read_board();
    assert_eq!(lane(&board, "Needs me"), [card.as_str()]);
    assert_eq!(fixture.degraded(), serde_json::json!(["merge"]));
    let beat = beats.recv().unwrap();
    assert!(beat.contains("degraded"), "{beat}");
    let dm = fixture.read_state("summary.txt");
    assert!(dm.contains("merge state unread"), "{dm}");
}

/// GitHub answers UNKNOWN while it recomputes, as it does after master moves. Your PR
/// may still conflict, so its card waits for an answer.
#[test]
fn a_conflict_still_being_computed_keeps_the_card_and_is_not_an_all_clear() {
    let fixture = Fixture::new();
    let (prs, merge) = my_broken_pr();
    fs::write(fixture.path("prs.json"), prs).unwrap();
    fs::write(fixture.path("merge.json"), merge).unwrap();
    run(&mut fixture.command(&["--no-model", "--no-briefings"]));
    let board = fixture.read_board();
    let card = lane(&board, "Needs me")[0].to_owned();

    let answer_with = |mergeable: &str| {
        let node = merge_node(mergeable, "null");
        let merge = format!(r#"{{"data":{{"repository":{{"p9":{node}}}}}}}"#);
        fs::write(fixture.path("merge.json"), merge).unwrap();
        run(&mut fixture.command(&["--no-model", "--no-briefings"]));
    };

    answer_with("UNKNOWN");
    let board = fixture.read_board();
    assert_eq!(lane(&board, "Needs me"), [card.as_str()]);
    assert_eq!(fixture.degraded(), serde_json::json!(["merge_computing"]));
    let dm = fixture.read_state("summary.txt");
    assert!(dm.contains("still computing conflicts"), "{dm}");

    answer_with("MERGEABLE");
    let board = fixture.read_board();
    assert!(lane(&board, "Needs me").is_empty(), "answered: {board}");
    assert_eq!(fixture.degraded(), serde_json::json!([]));
    assert!(!fixture.read_state("summary.txt").contains('⚠'));
}

/// What you do with the first card in `Needs me` to mute it. Hands back the card.
fn drag_to_passed(fixture: &Fixture) -> String {
    let board = fixture.read_board();
    let card = lane(&board, "Needs me")[0].to_owned();
    let dragged = board
        .replace(&format!("{card}\n"), "")
        .replace("## Passed\n", &format!("## Passed\n\n{card}\n"));
    fs::write(fixture.board(), dragged).unwrap();
    card
}

/// You muted it red. A push is a new trigger, and it comes back only then.
#[test]
fn a_push_rearms_a_muted_red_pr() {
    let fixture = Fixture::new();
    let (prs, merge) = my_broken_pr();
    fs::write(fixture.path("prs.json"), &prs).unwrap();
    fs::write(fixture.path("merge.json"), merge).unwrap();
    run(&mut fixture.command(&["--no-model", "--no-briefings"]));

    let card = drag_to_passed(&fixture);

    run(&mut fixture.command(&["--no-model", "--no-briefings"]));
    let board = fixture.read_board();
    assert!(lane(&board, "Needs me").is_empty(), "still muted: {board}");

    fs::write(fixture.path("prs.json"), prs.replace(MY_PR_HEAD, "ddddddd")).unwrap();
    run(&mut fixture.command(&["--no-model", "--no-briefings"]));
    let board = fixture.read_board();
    assert_eq!(lane(&board, "Needs me").len(), 1, "re-armed: {board}");
    assert_eq!(lane(&board, "Passed"), [card.as_str()], "your card stays");
}

/// GitHub keeps no push time, so the head's date is the later of its commit and the last
/// force-push. A request stays open past your comment only if that is later than it.
#[test]
fn a_push_is_dated_by_its_force_push_or_failing_that_its_commit() {
    let spoke = "2026-02-01T00:00:00Z";
    let before = "2026-01-15T00:00:00Z";
    let after = "2026-03-01T00:00:00Z";
    let cases = [
        (
            "a commit from before you spoke, and no force-push",
            before,
            None,
            false,
        ),
        (
            "the same commit, force-pushed since",
            before,
            Some(after),
            true,
        ),
        ("force-pushed before you spoke", before, Some(before), false),
        (
            "a commit since a force-push from before you spoke",
            after,
            Some(before),
            true,
        ),
    ];
    for (name, committed, forced, open) in cases {
        let forced = forced.map_or(String::new(), |at| format!(r#"{{"createdAt":"{at}"}}"#));
        let node = pr_node(50, "x", "", "ccccccc", "alice")
            .replace("2026-01-01T00:00:00Z", committed)
            .replace(
                r#""reviewRequests":{"nodes":[]}"#,
                r#""reviewRequests":{"nodes":[{"requestedReviewer":{"login":"me"}}]}"#,
            )
            .replace(
                r#""timelineItems":{"nodes":[]}"#,
                r#""timelineItems":{"nodes":[{"createdAt":"2025-12-01T00:00:00Z","requestedReviewer":{"login":"me"}}]}"#,
            )
            .replace(
                r#""comments":{"nodes":[]}"#,
                &format!(
                    r#""comments":{{"nodes":[{{"author":{{"__typename":"User","login":"me"}},
                    "body":"what about the pokes?","createdAt":"{spoke}"}}]}}"#
                ),
            )
            .replace(
                r#""forcePushes":{"nodes":[]}"#,
                &format!(r#""forcePushes":{{"nodes":[{forced}]}}"#),
            );
        let fixture = Fixture::new();
        fs::write(fixture.path("prs.json"), prs_page(&[node])).unwrap();
        run(&mut fixture.command(&["--no-model", "--no-briefings"]));
        let board = fixture.read_board();
        let needs_me = lane(&board, "Needs me");
        assert_eq!(
            needs_me.iter().any(|card| card.contains("[floresta#50](")),
            open,
            "{name}: {board}"
        );
    }
}

/// A failed read says nothing about the card, so it must not erase what it says.
#[test]
fn a_failed_claims_read_keeps_the_warning_on_your_card() {
    let fixture = Fixture::new();
    let card = "- [ ] mine [#8](https://github.com/getfloresta/Floresta/issues/8) <!--a-->PR #20 opened by bob — stop<!--/a-->";
    fs::write(fixture.board(), board_text("", card)).unwrap();
    run(fixture
        .command(&["--no-model", "--no-briefings"])
        .env("GH_FAIL_ON", "issueOrPullRequest"));
    assert_eq!(lane(&fixture.read_board(), "Doing"), [card]);
    assert_eq!(fixture.degraded(), serde_json::json!(["claims"]));
}

/// The same run with the read succeeding: the stale warning goes.
#[test]
fn a_claims_read_replaces_what_the_card_said() {
    let fixture = Fixture::new();
    let card = "- [ ] mine [#8](https://github.com/getfloresta/Floresta/issues/8) <!--a-->PR #20 opened by bob — stop<!--/a-->";
    fs::write(fixture.board(), board_text("", card)).unwrap();
    run(&mut fixture.command(&["--no-model", "--no-briefings"]));
    let board = fixture.read_board();
    assert!(
        !lane(&board, "Doing")[0].contains("opened by bob"),
        "{board}"
    );
    assert_eq!(fixture.degraded(), serde_json::json!([]));
}

#[test]
fn a_file_that_is_not_a_board_fails_the_run_and_is_left_alone() {
    let fixture = Fixture::new();
    let prose = "# Notes\n\nA paragraph I care about.\n";
    fs::write(fixture.board(), prose).unwrap();
    let (url, beats) = listener();
    let out = fixture
        .command(&["--notify", "--no-model", "--no-briefings"])
        .env("UPTIME_KUMA_PUSH_URL", format!("{url}/push"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(fixture.read_board(), prose);
    let beat = beats.recv().unwrap();
    assert!(beat.starts_with("GET /push?status=down&msg="), "{beat}");
}

/// A manual run and the timer's would share every temporary file.
#[test]
fn a_run_finding_another_in_progress_leaves_everything_to_it() {
    let fixture = Fixture::new();
    let before = board_text("", "");
    fs::write(fixture.board(), &before).unwrap();
    fs::create_dir_all(fixture.path("state")).unwrap();
    let held = File::create(fixture.path("state/lock")).unwrap();
    held.lock().unwrap();
    run(&mut fixture.command(&["--no-model", "--no-briefings"]));
    assert_eq!(fixture.read_board(), before);
    assert!(!fixture.path("state/last_run.json").exists());

    held.unlock().unwrap();
    run(&mut fixture.command(&["--no-model", "--no-briefings"]));
    assert_ne!(fixture.read_board(), before, "and is free to go after");
}

/// Each of these used to be accepted, and then did something other than it says.
#[test]
fn flags_that_cannot_mean_what_they_say_are_refused() {
    let fixture = Fixture::new();
    let (url, beats) = listener();
    for flags in [
        ["--dry-run", "--notify"].as_slice(),
        ["--dm-now"].as_slice(),
    ] {
        let out = fixture
            .command(flags)
            .env("UPTIME_KUMA_PUSH_URL", format!("{url}/push"))
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{flags:?}");
    }
    let without_state = Command::new(env!("CARGO_BIN_EXE_oss-board"))
        .args(["refresh", "--dry-run"])
        .output()
        .unwrap();
    assert_eq!(without_state.status.code(), Some(2));
    assert!(
        beats.try_recv().is_err(),
        "nothing ran, so nothing was beaten"
    );
}

#[test]
fn a_dry_run_prints_and_leaves_the_vault_alone() {
    let fixture = Fixture::new();
    let before = board_text("", "");
    fs::write(fixture.board(), &before).unwrap();
    let out = run(&mut fixture.command(&["--dry-run", "--no-model"]));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("[floresta#7]("), "{stdout}");
    assert!(stdout.contains("OSS · "), "the DM too: {stdout}");
    assert_eq!(fixture.read_board(), before);
}

#[test]
fn a_corrupt_last_dm_means_never_sent_and_says_so() {
    for record in [
        "{{{ not json",
        r#"{"at": "yesterday", "seen": {}}"#,
        r#"{"at": 7, "seen": {}}"#,
        r#"{"at": null, "seen": {"planted": "x"}}"#,
    ] {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.path("state")).unwrap();
        fs::write(fixture.path("state/last_dm.json"), record).unwrap();
        run(&mut fixture.command(&["--no-model", "--no-briefings"]));
        assert_eq!(
            fixture.degraded(),
            serde_json::json!(["last_dm"]),
            "{record}"
        );
        let dm = fixture.read_state("summary.txt");
        assert!(dm.contains("1 new"), "{record}: {dm}");
    }
}

fn claimed_pr_board() -> String {
    board_text(
        "",
        "- [ ] reviewing [#8](https://github.com/getfloresta/Floresta/pull/8)",
    )
}

/// The briefing agent needs Bash and `gh`, so the token is what keeps it read-only.
#[test]
fn claiming_a_pr_writes_a_review_brief_with_the_read_only_token() {
    let fixture = Fixture::new();
    fs::write(fixture.board(), claimed_pr_board()).unwrap();
    run(fixture
        .command(&["--no-model"])
        .env("GH_TOKEN", "read-only-token"));

    let note: &Path = &fixture.path("vault/Floresta-8.md");
    assert!(note.exists());
    assert_eq!(
        fixture.claude_log().trim(),
        "brief GH_TOKEN=read-only-token GITHUB_TOKEN=read-only-token args=[]"
    );
    let board = fixture.read_board();
    assert!(
        lane(&board, "Doing")[0].contains("[[Floresta-8]]"),
        "{board}"
    );
    assert_eq!(fixture.degraded(), serde_json::json!([]));
}

/// Briefings can take twenty minutes, which is as long as the unit is allowed.
#[test]
fn the_board_is_written_before_any_briefing_starts() {
    let fixture = Fixture::new();
    fs::write(fixture.board(), claimed_pr_board()).unwrap();
    run(fixture
        .command(&["--no-model"])
        .env("GH_TOKEN", "read-only-token"));
    let seen = fs::read_to_string(fixture.path("board_at_brief.md")).unwrap();
    assert_eq!(lane(&seen, "Needs me").len(), 1, "{seen}");
    assert!(
        lane(&seen, "Needs me")[0].contains("[floresta#7]("),
        "{seen}"
    );
}

/// Under its real name a note is complete, so what it needs is a link, not an agent.
#[test]
fn an_existing_note_is_linked_without_asking_the_agent_again() {
    let fixture = Fixture::new();
    fs::write(fixture.board(), claimed_pr_board()).unwrap();
    fs::write(fixture.path("vault/Floresta-8.md"), "written by hand").unwrap();
    run(fixture
        .command(&["--no-model"])
        .env("GH_TOKEN", "read-only-token"));
    let board = fixture.read_board();
    assert!(
        lane(&board, "Doing")[0].contains("[[Floresta-8]]"),
        "{board}"
    );
    assert_eq!(fixture.claude_log(), "");
    assert_eq!(
        fs::read_to_string(fixture.path("vault/Floresta-8.md")).unwrap(),
        "written by hand"
    );
}

/// A note you deleted stays deleted: the link on the card says it was once written.
#[test]
fn a_linked_card_whose_note_is_gone_is_not_briefed_again() {
    let fixture = Fixture::new();
    let linked = claimed_pr_board().replace("/pull/8)", "/pull/8) [[Floresta-8]]");
    fs::write(fixture.board(), linked).unwrap();
    run(fixture
        .command(&["--no-model"])
        .env("GH_TOKEN", "read-only-token"));
    assert_eq!(fixture.claude_log(), "");
    assert!(!fixture.path("vault/Floresta-8.md").exists());
}

/// What a failed agent left must not pass for a note, or the card is stuck with it.
#[test]
fn a_failed_briefing_leaves_nothing_and_is_tried_again() {
    let fixture = Fixture::new();
    fs::write(fixture.board(), claimed_pr_board()).unwrap();
    fs::write(fixture.path("bin/claude"), FAILING_AGENT).unwrap();
    let briefing_run = || {
        run(fixture
            .command(&["--no-model"])
            .env("GH_TOKEN", "read-only-token"));
    };
    briefing_run();
    briefing_run();
    assert_eq!(fixture.claude_log().lines().count(), 2, "tried both times");
    let left: Vec<_> = fs::read_dir(fixture.path("vault"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(left, ["Tasks.md"], "no note and no draft");
    let board = fixture.read_board();
    assert!(!lane(&board, "Doing")[0].contains("[["), "{board}");
}

/// A run killed mid-briefing leaves its draft, and the next agent can exit 0 having
/// written nothing. What it finds is not that agent's work.
#[test]
fn a_draft_left_by_a_killed_run_does_not_become_the_note() {
    let fixture = Fixture::new();
    fs::write(fixture.board(), claimed_pr_board()).unwrap();
    fs::write(fixture.path("vault/.Floresta-8.md.tmp"), "x".repeat(400)).unwrap();
    fs::write(fixture.path("bin/claude"), "#!/bin/sh\nexit 0\n").unwrap();
    run(fixture
        .command(&["--no-model"])
        .env("GH_TOKEN", "read-only-token"));
    assert!(!fixture.path("vault/Floresta-8.md").exists());
    assert!(!fixture.path("vault/.Floresta-8.md.tmp").exists());
    let board = fixture.read_board();
    assert!(!lane(&board, "Doing")[0].contains("[["), "{board}");
}

/// An agent that succeeds with next to nothing is failing in a way that looks answered.
#[test]
fn a_thin_note_is_discarded() {
    let fixture = Fixture::new();
    fs::write(fixture.board(), claimed_pr_board()).unwrap();
    fs::write(
        fixture.path("bin/claude"),
        FAILING_AGENT.replace("exit 1", "exit 0"),
    )
    .unwrap();
    run(fixture
        .command(&["--no-model"])
        .env("GH_TOKEN", "read-only-token"));
    assert!(!fixture.path("vault/Floresta-8.md").exists());
    let board = fixture.read_board();
    assert!(!lane(&board, "Doing")[0].contains("[["), "{board}");
}

#[test]
fn without_a_read_only_token_a_briefing_is_refused_not_run_on_your_own_auth() {
    let fixture = Fixture::new();
    fs::write(fixture.board(), claimed_pr_board()).unwrap();
    run(&mut fixture.command(&["--no-model"]));
    assert!(!fixture.path("vault/Floresta-8.md").exists());
    assert_eq!(fixture.claude_log(), "");
}

/// One DM through the transport the tests can read, handing back what it sent.
fn deliver(fixture: &Fixture, url: &str, messages: &Receiver<String>, flags: &[&str]) -> String {
    run(fixture
        .command(&[&["--notify", "--dm-now", "--no-briefings"], flags].concat())
        .env("OPENCLAW_ENABLED", "1")
        .env("OPENCLAW_URL", url)
        .env("OPENCLAW_GATEWAY_TOKEN", "gateway-token"));
    messages.recv().unwrap()
}

#[test]
fn a_delivered_dm_is_remembered_so_the_next_one_says_nothing_new() {
    let fixture = Fixture::new();
    let (url, messages) = listener();

    let first = deliver(&fixture, &url, &messages, &["--no-model"]);
    assert!(
        first.contains("POST /api/channels/telegram/send"),
        "{first}"
    );
    assert!(
        first
            .to_lowercase()
            .contains("authorization: bearer gateway-token"),
        "{first}"
    );
    assert!(first.contains("1 new"), "{first}");
    assert!(first.contains("pull/7"), "{first}");

    let second = deliver(&fixture, &url, &messages, &["--no-model"]);
    assert!(second.contains("nothing new"), "{second}");
    assert!(!second.contains("pull/7"), "{second}");
    assert!(second.contains("1 on the board"), "{second}");
}

/// The verdict decides what interrupts you. It never decides what is on the board, so a
/// comment that talks the model out of an ask cannot make it disappear.
#[test]
fn what_the_model_says_asks_nothing_stays_on_the_board_and_out_of_the_dm() {
    let fixture = Fixture::new();
    fs::write(fixture.path("nobody_asks"), "").unwrap();
    let (url, messages) = listener();
    let dm = deliver(&fixture, &url, &messages, &[]);
    assert!(dm.contains("nothing new"), "{dm}");
    assert!(!dm.contains("pull/7"), "{dm}");
    assert!(!dm.contains("not filtered"), "{dm}");

    let board = fixture.read_board();
    assert!(
        lane(&board, "Needs me")[0].contains("[floresta#7]("),
        "{board}"
    );
}

/// GitHub answers UNKNOWN at 08:07 because master just moved. That says nothing about the
/// conflict, so it must not forget that you were told.
#[test]
fn a_conflict_you_were_told_about_is_not_news_after_a_morning_it_went_unread() {
    let fixture = Fixture::new();
    let (prs, _) = my_broken_pr();
    fs::write(fixture.path("prs.json"), prs).unwrap();
    let (url, messages) = listener();
    let morning = |mergeable: &str| {
        let node = merge_node(mergeable, "null");
        let merge = format!(r#"{{"data":{{"repository":{{"p9":{node}}}}}}}"#);
        fs::write(fixture.path("merge.json"), merge).unwrap();
        deliver(&fixture, &url, &messages, &["--no-model"])
    };

    let told = morning("CONFLICTING");
    assert!(told.contains("1 new"), "{told}");
    let computing = morning("UNKNOWN");
    assert!(computing.contains("nothing new"), "{computing}");
    let answered = morning("CONFLICTING");
    assert!(answered.contains("nothing new"), "{answered}");
}

/// Dragging a card to Passed does not clear it. A thank-you re-arms the mute, and only the
/// thank-you is new, so the request you were already told about does not come back with it.
#[test]
fn what_re_arms_a_mute_does_not_resend_what_you_were_told() {
    let fixture = Fixture::new();
    fs::write(fixture.path("nobody_asks"), "").unwrap();
    let requested = pr_node(7, "waiting on you", "", "aaaaaaa", "bob")
        .replace(
            r#""reviewRequests":{"nodes":[]}"#,
            r#""reviewRequests":{"nodes":[{"requestedReviewer":{"login":"me"}}]}"#,
        )
        .replace(
            r#""timelineItems":{"nodes":[]}"#,
            r#""timelineItems":{"nodes":[{"createdAt":"2026-01-01T00:00:00Z","requestedReviewer":{"login":"me"}}]}"#,
        );
    fs::write(
        fixture.path("prs.json"),
        prs_page(std::slice::from_ref(&requested)),
    )
    .unwrap();
    let (url, messages) = listener();

    let told = deliver(&fixture, &url, &messages, &[]);
    assert!(told.contains("1 new"), "{told}");
    drag_to_passed(&fixture);
    let muted = deliver(&fixture, &url, &messages, &[]);
    assert!(muted.contains("nothing new"), "{muted}");

    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let thanked = requested.replace(
        r#""comments":{"nodes":[]}"#,
        &format!(
            r#""comments":{{"nodes":[{{"author":{{"__typename":"User","login":"bob"}},
            "body":"thanks @me","createdAt":"{now}"}}]}}"#
        ),
    );
    fs::write(fixture.path("prs.json"), prs_page(&[thanked])).unwrap();
    let rearmed = deliver(&fixture, &url, &messages, &[]);
    assert!(
        lane(&fixture.read_board(), "Needs me")[0].contains("[floresta#7]("),
        "the mute re-armed"
    );
    assert!(rearmed.contains("nothing new"), "{rearmed}");
}

/// Opportunities stay on the board.
#[test]
fn the_dm_carries_no_opportunities() {
    let fixture = Fixture::new();
    let (url, messages) = listener();
    let dm = deliver(&fixture, &url, &messages, &[]);
    assert!(!dm.contains("pull/8"), "{dm}");
    let board = fixture.read_board();
    assert!(
        lane(&board, "Opportunities")[0].contains("[floresta#8]("),
        "{board}"
    );
}

const CLAIMED_ISSUE: &str = "- [ ] mine [#98](https://github.com/getfloresta/Floresta/issues/98)";

/// Issue #98, as the claims read finds it.
fn claimed_issue(state: &str, assignee: &str) -> String {
    format!(
        r#"{{"data":{{"n0":{{"issueOrPullRequest":{{"__typename":"Issue","state":"{state}",
        "closedAt":"2026-01-01T00:00:00Z","assignees":{{"nodes":[{{"login":"{assignee}"}}]}},
        "timelineItems":{{"nodes":[]}}}}}}}}}}"#
    )
}

/// An alert is news once, and the card goes on saying it. Only saying something else, or
/// clearing and coming back, sends it again.
#[test]
fn a_claim_alert_goes_out_once_and_stays_on_the_card() {
    let fixture = Fixture::new();
    fs::write(fixture.board(), board_text("", CLAIMED_ISSUE)).unwrap();
    let (url, messages) = listener();
    let closed = claimed_issue("CLOSED", "me");
    let taken = claimed_issue("CLOSED", "bob");
    let open = claimed_issue("OPEN", "me");
    for (name, claims, sent) in [
        ("the first morning", closed.as_str(), true),
        ("the next one", &closed, false),
        ("it says something else", &taken, true),
        ("a read that failed says nothing", "not json", false),
        ("and is no reason to say it again", &taken, false),
        ("it cleared", &open, false),
        ("it came back", &taken, true),
    ] {
        fs::write(fixture.path("claims.json"), claims).unwrap();
        let dm = deliver(&fixture, &url, &messages, &["--no-model"]);
        assert_eq!(dm.contains("floresta#98 —"), sent, "{name}: {dm}");
    }
    let board = fixture.read_board();
    assert!(
        lane(&board, "Doing")[0].contains("assigned to bob"),
        "{board}"
    );
}

/// Finished with, so a closed issue there is how it should be.
#[test]
fn a_card_in_done_raises_no_alert() {
    let fixture = Fixture::new();
    let board =
        board_text("", "").replace("## Done\n\n", &format!("## Done\n\n{CLAIMED_ISSUE}\n\n"));
    fs::write(fixture.board(), board).unwrap();
    fs::write(fixture.path("claims.json"), claimed_issue("CLOSED", "me")).unwrap();
    run(&mut fixture.command(&["--no-model", "--no-briefings"]));
    let dm = fixture.read_state("summary.txt");
    assert!(!dm.contains("floresta#98 —"), "{dm}");
    let board = fixture.read_board();
    assert!(
        lane(&board, "Done")[0].contains("closed upstream"),
        "the card still says so: {board}"
    );
}

/// With the opportunities gone from the DM, nothing else would say so on a morning with
/// no ask for the "not filtered" line to go on.
#[test]
fn a_model_that_does_not_answer_is_said_on_a_quiet_morning_too() {
    let fixture = Fixture::new();
    fs::write(fixture.path("bin/claude"), "#!/bin/sh\nexit 1\n").unwrap();
    let (url, messages) = listener();
    let first = deliver(&fixture, &url, &messages, &[]);
    assert!(first.contains("did not answer"), "{first}");
    let quiet = deliver(&fixture, &url, &messages, &[]);
    assert!(quiet.contains("nothing new"), "{quiet}");
    assert!(quiet.contains("did not answer"), "{quiet}");
    assert_eq!(fixture.degraded(), serde_json::json!(["model"]));
}

#[test]
fn what_counts_as_the_model_not_answering() {
    let waiting = pr_node(7, "waiting on you", "cc @me please", "aaaaaaa", "bob");
    let also = pr_node(50, "also waiting", "cc @me too", "ddddddd", "bob");
    let answered = serde_json::json!([]);
    let unanswered = serde_json::json!(["model"]);
    let asks_the_model: &[&str] = &["--no-briefings"];
    for (name, prs, flags, judge_fails, want) in [
        (
            "told not to ask",
            prs(),
            &["--no-briefings", "--no-model"][..],
            false,
            &answered,
        ),
        (
            "nothing to rank is not a failed judge",
            prs_page(std::slice::from_ref(&waiting)),
            asks_the_model,
            false,
            &answered,
        ),
        (
            "the judge alone failed",
            prs(),
            asks_the_model,
            true,
            &unanswered,
        ),
        (
            "one item got no gist",
            prs_page(&[waiting.clone(), also.clone()]),
            asks_the_model,
            false,
            &unanswered,
        ),
    ] {
        let fixture = Fixture::new();
        fs::write(fixture.path("prs.json"), prs).unwrap();
        if judge_fails {
            fs::write(fixture.path("no_judge"), "").unwrap();
        }
        run(&mut fixture.command(flags));
        assert_eq!(&fixture.degraded(), want, "{name}");
    }
}

/// The shortlist only feeds the ranking, so losing it says nothing about your own PRs.
#[test]
fn a_failed_read_of_the_shortlist_does_not_warn_about_your_prs() {
    let fixture = Fixture::new();
    run(fixture
        .command(&["--no-model", "--no-briefings"])
        .env("GH_FAIL_ON", "mergeable"));
    assert_eq!(fixture.degraded(), serde_json::json!(["opportunity_merge"]));
    let dm = fixture.read_state("summary.txt");
    assert!(!dm.contains("merge state unread"), "{dm}");
}

// ---------------------------------------------------------------- two repos

/// Both repos have a #7 and a #8. The model only spoke about Floresta's, so what it said
/// must not reach cdk's.
#[test]
fn the_same_number_in_two_repos_is_two_items() {
    let fixture = Fixture::new();
    fs::write(fixture.path("cdk_prs.json"), on_cdk(&prs())).unwrap();
    run(&mut fixture.command(&[]));
    let board = fixture.read_board();

    let needs_me = lane(&board, "Needs me");
    assert_eq!(needs_me.len(), 2, "{board}");
    let of = |label: &str| needs_me.iter().find(|card| card.contains(label)).unwrap();
    assert!(
        of("[floresta#7](").contains("*(wants a yes/no)*"),
        "{board}"
    );
    assert!(!of("[cdk#7](").contains("wants a yes/no"), "{board}");
    assert!(of("[cdk#7](").contains("https://github.com/cashubtc/cdk/pull/7"));

    let opportunities = lane(&board, "Opportunities");
    assert_eq!(opportunities.len(), 1, "the judge kept one: {board}");
    assert!(opportunities[0].contains("[floresta#8]("), "{board}");
}

#[test]
fn the_dm_says_which_repo_an_ask_is_in() {
    let fixture = Fixture::new();
    fs::write(fixture.path("prs.json"), prs_page(&[])).unwrap();
    fs::write(fixture.path("cdk_prs.json"), on_cdk(&prs())).unwrap();
    let out = run(&mut fixture.command(&["--dry-run", "--no-model"]));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\ncdk#7 waiting on you"), "{stdout}");
    assert!(
        stdout.contains("\nhttps://github.com/cashubtc/cdk/pull/7"),
        "{stdout}"
    );
}

/// A snapshot is what every lane is computed from, and an answer from one repo is not an
/// answer about the other's cards.
#[test]
fn a_failed_snapshot_of_either_repo_leaves_the_board_untouched() {
    for failing in ["getfloresta", "cashubtc"] {
        let fixture = Fixture::new();
        fs::write(fixture.board(), board_text("", "")).unwrap();
        let before = fixture.read_board();
        let out = fixture
            .command(&[])
            .env("GH_FAIL_ON", format!("owner: \"{failing}\""))
            .output()
            .unwrap();
        assert!(!out.status.success(), "{failing}");
        assert_eq!(fixture.read_board(), before, "{failing}");
    }
}

/// cdk's default branch is `main`. Comparing against `master` fails, and a failed compare
/// only costs the note, so nothing else would notice.
#[test]
fn a_stale_ack_on_cdk_is_diffed_against_its_own_base() {
    let fixture = Fixture::new();
    let acked = pr_node(7, "acked", "", "bbbbbbb", "bob").replace(
        r#""comments":{"nodes":[]}"#,
        r#""comments":{"nodes":[{"author":{"__typename":"User","login":"me"},
            "body":"ACK aaaaaaa","createdAt":"2025-12-01T00:00:00Z"}]}"#,
    );
    fs::write(fixture.path("prs.json"), prs_page(&[])).unwrap();
    fs::write(fixture.path("cdk_prs.json"), on_cdk(&prs_page(&[acked]))).unwrap();
    run(&mut fixture.command(&["--no-model"]));
    let board = fixture.read_board();
    assert!(
        lane(&board, "Needs me")[0].contains("rebase only"),
        "{board}"
    );
}

/// cdk's board stays off until `survey` shows it is curated, so `survey` has to be able to
/// read a board a run does not.
#[test]
fn survey_reads_a_board_a_run_does_not() {
    let cdk_board = r#"organization(login: "cashubtc")"#;
    let fixture = Fixture::new();
    run(fixture
        .command(&["--no-model", "--no-briefings"])
        .env("GH_FAIL_ON", cdk_board));
    assert_eq!(fixture.degraded(), serde_json::json!([]));

    let out = run(fixture.binary().arg("survey"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("cashubtc/cdk: cashubtc's board #2"),
        "{stdout}"
    );
    let failed = fixture
        .binary()
        .arg("survey")
        .env("GH_FAIL_ON", cdk_board)
        .output()
        .unwrap();
    assert!(!failed.status.success());
}

#[test]
fn claiming_a_cdk_pr_briefs_that_pr_not_florestas_with_the_same_number() {
    let fixture = Fixture::new();
    fs::write(fixture.path("cdk_prs.json"), on_cdk(&prs())).unwrap();
    fs::write(fixture.board(), on_cdk(&claimed_pr_board())).unwrap();
    run(fixture
        .command(&["--no-model"])
        .env("GH_TOKEN", "read-only"));
    assert!(fixture.path("vault/cdk-8.md").exists());
    assert!(!fixture.path("vault/Floresta-8.md").exists());
    let board = fixture.read_board();
    assert!(lane(&board, "Doing")[0].contains("[[cdk-8]]"), "{board}");
    assert!(
        lane(&board, "Opportunities")
            .iter()
            .any(|card| card.contains("[floresta#8](")),
        "Floresta's #8 was not the one claimed: {board}"
    );
}
