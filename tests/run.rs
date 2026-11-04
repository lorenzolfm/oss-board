//! The whole run, through the real binary, against fake `gh`, `git` and `claude`
//! executables on PATH. These pin what only the orchestration can get wrong: what a
//! failed source costs, what each subprocess is allowed to hold, and what reaches
//! the outside world.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use tempfile::TempDir;

const FAKE_GH: &str = r#"#!/bin/sh
if [ -n "$GH_FAIL_ON" ]; then
  case "$*" in *"$GH_FAIL_ON"*) echo boom >&2; exit 1;; esac
fi
case "$*" in
  *"viewer { login }"*) echo '{"data":{"viewer":{"login":"me"}}}';;
  *"creator=me"*) ;;
  *"first: 50"*) echo '{"data":{"repository":{"pullRequests":{"nodes":[]}}}}';;
  *"pullRequests(states: OPEN"*) cat "$FIXTURE_DIR/prs.json";;
  *"issues(states: OPEN"*) echo '{"data":{"repository":{"issues":{"nodes":[]}}}}';;
  *mergeable*) echo '{"data":{"repository":{}}}';;
  *projectV2*) echo '{"data":{"organization":{"projectV2":{"items":{"nodes":[]}}}}}';;
  *issueOrPullRequest*) echo '{"data":{}}';;
  *) echo "unexpected gh call: $*" >&2; exit 1;;
esac
"#;

/// Records which tokens it was handed, wraps its answers in prose as a real model
/// does, and writes the note a briefing asks for.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
case "$2" in
  "Each item below"*)
    echo "model GH_TOKEN=${GH_TOKEN:-unset} GITHUB_TOKEN=${GITHUB_TOKEN:-unset}" >> "$FIXTURE_DIR/claude.log"
    echo 'Sure, here you go:'
    echo '{"gists":[{"key":"getfloresta/Floresta#7","gist":"wants a yes/no"}]}';;
  "You are picking"*)
    echo "model GH_TOKEN=${GH_TOKEN:-unset} GITHUB_TOKEN=${GITHUB_TOKEN:-unset}" >> "$FIXTURE_DIR/claude.log"
    echo '{"picks":[{"number":8,"why":"small and yours","pitch":"read it first","est":"1h"}]}';;
  "Write an Obsidian"*)
    echo "brief GH_TOKEN=${GH_TOKEN:-unset} GITHUB_TOKEN=${GITHUB_TOKEN:-unset}" >> "$FIXTURE_DIR/claude.log"
    note=$(printf '%s\n' "$2" | sed -n '/exactly this path/{n;s/^ *//;p;}')
    printf '%0400d' 0 > "$note"
    echo DONE;;
esac
"#;

/// #7 mentions you, so it is an obligation. #8 is unreviewed, so an opportunity.
const PRS: &str = r#"{"data":{"repository":{"pullRequests":{"nodes":[
  {"number":7,"title":"waiting on you","url":"https://github.com/getfloresta/Floresta/pull/7",
   "body":"cc @me please","createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z",
   "headRefOid":"aaaaaaa","author":{"__typename":"User","login":"bob"}},
  {"number":8,"title":"nobody reviewed this","url":"https://github.com/getfloresta/Floresta/pull/8",
   "body":"","createdAt":"2026-01-03T00:00:00Z","updatedAt":"2026-01-04T00:00:00Z",
   "headRefOid":"bbbbbbb","author":{"__typename":"User","login":"carol"}}
]}}}}"#;

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let fixture = Self { dir };
        let bin = fixture.path("bin");
        fs::create_dir(&bin).unwrap();
        for (name, script) in [
            ("gh", FAKE_GH),
            ("claude", FAKE_CLAUDE),
            ("git", "#!/bin/sh\nexit 1\n"),
        ] {
            let path = bin.join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(fixture.path("prs.json"), PRS).unwrap();
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

    fn claude_log(&self) -> String {
        fs::read_to_string(self.path("claude.log")).unwrap_or_default()
    }

    /// A command with the fakes first on PATH and nothing of the real user's.
    fn command(&self, args: &[&str]) -> Command {
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_oss-board"));
        cmd.args(["--board", self.board().to_str().unwrap()])
            .args(["--state", self.path("state").to_str().unwrap()])
            .args(args)
            .env("PATH", path)
            .env("HOME", self.dir.path())
            .env("FIXTURE_DIR", self.dir.path())
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
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
         ## Doing\n\n{doing}\n\n## Done\n\n## Passed\n\n"
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
        needs_me[0].contains("[#7](") && needs_me[0].contains("*(wants a yes/no)*"),
        "{board}"
    );
    assert!(
        needs_me[0].contains("<!--t:"),
        "the fingerprint a mute needs: {board}"
    );
    let opportunities = lane(&board, "Opportunities");
    assert_eq!(opportunities.len(), 1, "{board}");
    assert!(
        opportunities[0].contains("[#8](") && opportunities[0].contains("*(small and yours)*"),
        "{board}"
    );
    assert_eq!(lane(&board, "Todo"), ["- [ ] my own words ^abc"]);

    let last_run: serde_json::Value =
        serde_json::from_str(&fixture.read_state("last_run.json")).unwrap();
    assert_eq!(last_run["counts"]["obligations"], 1);
    assert_eq!(last_run["counts"]["judged"], true);
    assert_eq!(last_run["degraded"], serde_json::json!([]));

    let beat = beats.recv().unwrap();
    assert!(
        beat.starts_with("GET /push?token=abc&status=up&msg="),
        "{beat}"
    );
}

/// The data goes in inline, so the model passes have no use for GitHub credentials.
#[test]
fn the_model_passes_never_hold_a_github_token() {
    let fixture = Fixture::new();
    run(fixture
        .command(&["--no-briefings"])
        .env("GH_TOKEN", "read-only-token")
        .env("GITHUB_TOKEN", "another-token"));
    let log = fixture.claude_log();
    assert_eq!(log.lines().count(), 2, "{log}");
    assert!(
        log.lines()
            .all(|l| l == "model GH_TOKEN=unset GITHUB_TOKEN=unset"),
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
    let last_run: serde_json::Value =
        serde_json::from_str(&fixture.read_state("last_run.json")).unwrap();
    assert_eq!(last_run["degraded"], serde_json::json!(["unshipped"]));
    let beat = beats.recv().unwrap();
    assert!(beat.contains("degraded"), "{beat}");
}

#[test]
fn a_dry_run_prints_and_leaves_the_vault_alone() {
    let fixture = Fixture::new();
    let before = board_text("", "");
    fs::write(fixture.board(), &before).unwrap();
    let out = run(&mut fixture.command(&["--dry-run", "--no-model"]));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("[#7]("), "{stdout}");
    assert!(stdout.contains("Floresta · "), "the DM too: {stdout}");
    assert_eq!(fixture.read_board(), before);
}

fn claimed_pr_board() -> String {
    board_text(
        "",
        "- [ ] reviewing [#8](https://github.com/getfloresta/Floresta/pull/8)",
    )
}

/// The briefing agent holds Bash and `gh`, so the token is what keeps it read-only.
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
        "brief GH_TOKEN=read-only-token GITHUB_TOKEN=read-only-token"
    );
    let board = fixture.read_board();
    assert!(
        lane(&board, "Doing")[0].contains("[[Floresta-8]]"),
        "{board}"
    );
}

#[test]
fn without_a_read_only_token_a_briefing_is_refused_not_run_on_your_own_auth() {
    let fixture = Fixture::new();
    fs::write(fixture.board(), claimed_pr_board()).unwrap();
    run(&mut fixture.command(&["--no-model"]));
    assert!(!fixture.path("vault/Floresta-8.md").exists());
    assert_eq!(fixture.claude_log(), "");
}

#[test]
fn a_delivered_dm_is_remembered_so_the_next_one_marks_only_what_changed() {
    let fixture = Fixture::new();
    let (url, messages) = listener();
    let notify = |fixture: &Fixture| {
        run(fixture
            .command(&["--notify", "--dm-now", "--no-model", "--no-briefings"])
            .env("OPENCLAW_ENABLED", "1")
            .env("OPENCLAW_URL", &url)
            .env("OPENCLAW_GATEWAY_TOKEN", "gateway-token"));
    };

    notify(&fixture);
    let first = messages.recv().unwrap();
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
    assert!(
        first.contains("🆕"),
        "everything is new the first time: {first}"
    );
    let record: serde_json::Value =
        serde_json::from_str(&fixture.read_state("last_dm.json")).unwrap();
    assert_eq!(record["seen"]["getfloresta/Floresta#8"], "bbbbbbb");

    notify(&fixture);
    let second = messages.recv().unwrap();
    assert!(!second.contains("🆕"), "nothing moved: {second}");
}
