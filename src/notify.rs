//! Send the run summary to Telegram, and beat the uptime-kuma heartbeat.
//!
//! Two transports:
//!
//!   telegram  -- the Telegram Bot API directly. Fully specified: the bot token
//!                already exists in the homelab sealed secret as TELEGRAM_BOT_TOKEN
//!                and the chat id is the openclaw allowlist entry. Works even when
//!                openclaw is down.
//!   openclaw  -- POST through the openclaw gateway. Its /api/channels/telegram
//!                endpoint exists (it answers 401, not 404) but the request body
//!                shape is undocumented, so this stays opt-in until confirmed.
//!
//! Notification failure must never fail the run: the board is the deliverable.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use ureq::Agent;

use crate::text::prefix;

const TIMEOUT: Duration = Duration::from_secs(20);
/// Telegram refuses anything over 4096; chunks leave room.
const MESSAGE_LIMIT: usize = 4000;
const CHUNK_LIMIT: usize = 3900;

/// A transport's verdict: `(ok, detail)`.
pub type Outcome = (bool, String);

pub struct Delivery {
    pub transport: &'static str,
    pub ok: bool,
    pub detail: String,
}

fn agent() -> Agent {
    Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into()
}

/// Secrets travel in these URLs. An error message can quote the URL, and these
/// details end up in the journal, so nothing leaves here with the secret still in it.
fn describe(err: &ureq::Error, secret: &str) -> String {
    err.to_string().replace(secret, "<secret>")
}

fn response_text(response: &mut ureq::http::Response<ureq::Body>, limit: u64) -> String {
    let mut buf = Vec::new();
    // A body that will not read still leaves the status to report.
    let _ = response
        .body_mut()
        .as_reader()
        .take(limit)
        .read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

fn outcome(
    result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    secret: &str,
    is_ok: impl Fn(u16) -> bool,
) -> Outcome {
    match result {
        Err(err) => (false, describe(&err, secret)),
        Ok(mut response) => {
            let status = response.status().as_u16();
            let body = response_text(&mut response, 400);
            if is_ok(status) {
                (true, format!("HTTP {status} {}", prefix(&body, 120)))
            } else {
                (false, format!("HTTP {status} {}", prefix(&body, 200)))
            }
        }
    }
}

fn send_telegram(text: &str, token: &str, chat_id: &str) -> Outcome {
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let result = agent().post(&url).send_form([
        ("chat_id", chat_id),
        ("text", prefix(text, MESSAGE_LIMIT)),
        ("disable_web_page_preview", "true"),
    ]);
    outcome(result, token, |status| status == 200)
}

/// Opt-in: confirm the body shape against your openclaw build before use.
fn send_openclaw(text: &str, base_url: &str, token: Option<&str>, path: &str) -> Outcome {
    let Some(token) = token else {
        return (false, "missing gateway token".to_owned());
    };
    let url = format!("{}{path}", base_url.trim_end_matches('/'));
    let body = json!({ "text": prefix(text, MESSAGE_LIMIT) }).to_string();
    let result = agent()
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .send(body);
    outcome(result, token, |status| (200..300).contains(&status))
}

/// uptime-kuma push monitor, mirroring the borgmatic hooks.
pub fn heartbeat(push_url: Option<&str>, status: &str, message: &str) -> Outcome {
    let Some(push_url) = push_url.filter(|url| !url.is_empty()) else {
        return (false, "no push url".to_owned());
    };
    let result = agent()
        .get(push_url)
        .query("status", status)
        .query("msg", prefix(message, 200))
        .call();
    match result {
        Ok(response) => {
            let status = response.status().as_u16();
            (status == 200, format!("HTTP {status}"))
        }
        Err(err) => (false, describe(&err, push_url)),
    }
}

/// Split at line boundaries so no part exceeds Telegram's limit. The asks are capped
/// but the alerts are not, so a message that somehow runs long is several messages
/// rather than a truncated one.
pub fn chunks(text: &str, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_len = 0;
    for mut line in text.split('\n') {
        let mut line_len = line.chars().count();
        while line_len > limit {
            // A single absurd line.
            let head = prefix(line, limit);
            out.push(head.to_owned());
            line = &line[head.len()..];
            line_len -= limit;
        }
        if !current.is_empty() && current_len + 1 + line_len > limit {
            out.push(std::mem::take(&mut current));
            current.push_str(line);
            current_len = line_len;
        } else if current.is_empty() {
            current.push_str(line);
            current_len = line_len;
        } else {
            current.push('\n');
            current.push_str(line);
            current_len += 1 + line_len;
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn expand_user(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

fn read_secret(path: Option<String>) -> Option<String> {
    let text = fs::read_to_string(expand_user(&path?)).ok()?;
    Some(text.trim().to_owned()).filter(|secret| !secret.is_empty())
}

fn dispatch_one(text: &str) -> Vec<Delivery> {
    let mut results = Vec::new();
    let token = env("TELEGRAM_BOT_TOKEN").or_else(|| read_secret(env("TELEGRAM_BOT_TOKEN_FILE")));
    if let (Some(token), Some(chat_id)) = (token, env("TELEGRAM_CHAT_ID")) {
        let (ok, detail) = send_telegram(text, &token, &chat_id);
        results.push(Delivery {
            transport: "telegram",
            ok,
            detail,
        });
    }
    if let Some(url) = env("OPENCLAW_URL")
        && env("OPENCLAW_ENABLED").as_deref() == Some("1")
    {
        let token = env("OPENCLAW_GATEWAY_TOKEN")
            .or_else(|| read_secret(env("OPENCLAW_GATEWAY_TOKEN_FILE")));
        let path =
            env("OPENCLAW_SEND_PATH").unwrap_or_else(|| "/api/channels/telegram/send".to_owned());
        let (ok, detail) = send_openclaw(text, &url, token.as_deref(), &path);
        results.push(Delivery {
            transport: "openclaw",
            ok,
            detail,
        });
    }
    if results.is_empty() {
        results.push(Delivery {
            transport: "none",
            ok: false,
            detail: "no transport configured".to_owned(),
        });
    }
    results
}

/// Fire whichever transports are configured, one message per chunk.
pub fn dispatch(text: &str) -> Vec<Delivery> {
    chunks(text, CHUNK_LIMIT)
        .iter()
        .flat_map(|part| dispatch_one(part))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_message_is_split_at_lines_and_nothing_is_lost() {
        let message = vec!["t".repeat(120); 60].join("\n");
        let parts = chunks(&message, CHUNK_LIMIT);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|part| part.chars().count() <= CHUNK_LIMIT));
        assert_eq!(parts.join("\n"), message);
    }

    #[test]
    fn a_single_absurd_line_is_cut_by_characters_not_bytes() {
        let line = "é".repeat(25);
        let parts = chunks(&line, 10);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|part| part.chars().count() <= 10));
        assert_eq!(parts.concat(), line);
    }

    #[test]
    fn an_error_never_carries_the_secret_in_the_url() {
        // A push URL with no scheme: the one mistake the client quotes back whole.
        let secret = "/api/push/SECRET";
        let err = agent().get(secret).call().unwrap_err();
        assert!(
            err.to_string().contains(secret),
            "this error must quote the URL, or the test proves nothing: {err}"
        );
        assert!(!describe(&err, secret).contains(secret));
    }
}
