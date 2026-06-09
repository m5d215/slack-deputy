//! The broker-style CLI: a thin HTTP client for the daemon's API. Every verb —
//! queue and Slack alike — goes through the daemon, so the CLI needs no DB file
//! and no tokens; point `SLACK_DEPUTY_URL` at a remote daemon to drive it from
//! another host. No subcommand → daemon mode (see main).

use crate::db::Row;
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "slack-deputy", about = "Act as yourself on Slack")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Claim the oldest pending event (prints `{pk, thread_ts}` or `null`, no body). Marks it dispatched.
    Next,
    /// Block until a claimable event exists (prints `{ready:true}`), or until
    /// `--timeout` seconds pass with none (`{ready:false}`). Claims nothing — a
    /// pure wake signal so the consumer can park a background wait between drains.
    /// The daemon polls the queue while you block.
    Wait {
        /// Seconds to block before giving up and printing `{ready:false}` (0 / omitted = check once).
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Print the event body JSON for a pk (handler subagent only).
    Body { pk: i64 },
    /// Mark a row done (handled inline).
    Done { pk: i64 },
    /// Mark a row awaiting_human (handed off to confirmation).
    Await { pk: i64 },
    /// Skip pending rows without processing them (pending → skipped). `<pk>` skips
    /// one; `--all` drops the whole backlog (e.g. on consumer recovery, to ignore
    /// messages queued during downtime). Dispatched rows are never touched.
    Skip {
        /// The row to skip (omit when using --all).
        pk: Option<i64>,
        /// Skip every pending row.
        #[arg(long)]
        all: bool,
    },
    /// Post a message as yourself.
    Post {
        #[arg(long)]
        channel: String,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        text: String,
    },
    /// Add a reaction as yourself.
    React {
        #[arg(long)]
        channel: String,
        #[arg(long)]
        ts: String,
        #[arg(long)]
        name: String,
    },
    /// Remove a reaction.
    Unreact {
        #[arg(long)]
        channel: String,
        #[arg(long)]
        ts: String,
        #[arg(long)]
        name: String,
    },
    /// Delete a message.
    Delete {
        #[arg(long)]
        channel: String,
        #[arg(long)]
        ts: String,
    },
    /// Read a thread's replies (context gathering).
    Thread {
        #[arg(long)]
        channel: String,
        #[arg(long)]
        ts: String,
    },
    /// Ask the human to confirm an action (bot DM + buttons). `無視` (terminal
    /// no-op) is always present. Pick one positive button:
    /// `--post` = terminal 投稿 (the daemon posts `--text` to `--channel`/`--thread`
    /// on click, no consumer hop); `--choose a,b,c` = one routed button per choice
    /// (decision = the chosen string); else `--action` JSON = a routed 承認 a
    /// subagent executes. `--danger` = danger styling + a confirm dialog.
    Ask {
        #[arg(long)]
        text: String,
        /// Terminal post: the approve button is 投稿 and the daemon posts on click.
        #[arg(long)]
        post: bool,
        /// Post target channel (required with --post).
        #[arg(long)]
        channel: Option<String>,
        /// Post target thread (optional, with --post).
        #[arg(long)]
        thread: Option<String>,
        /// Routed action JSON (for --choose, or a non-post 承認). Comes back as a
        /// confirmation row on click.
        #[arg(long)]
        action: Option<String>,
        /// Comma-separated choices → routed choose-one buttons.
        #[arg(long)]
        choose: Option<String>,
        /// Danger styling + a confirm dialog on the positive button.
        #[arg(long)]
        danger: bool,
        /// Optional smaller context line (markdown) under the prompt.
        #[arg(long)]
        context: Option<String>,
    },
    /// Read bot-DM messages. `--thread <ts>` reads that ask's thread (draft +
    /// any free-text reply); without, recent history (in-flight confirmations).
    Dm {
        #[arg(long)]
        thread: Option<String>,
    },
    /// Follow captured rows live (read-only, `tail -f` style). Ctrl-C to stop.
    Tail {
        /// Start after this pk (default: last 10, then follow new).
        #[arg(long)]
        from: Option<i64>,
    },
}

/// One-line summary of a captured row for `tail`.
fn fmt_row(r: &Row) -> String {
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap_or(serde_json::Value::Null);
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let chan = r.channel.as_deref().unwrap_or("-");
    let summary = match r.kind.as_str() {
        "message" => {
            let text: String = get("text").chars().take(80).collect();
            format!("<{}> {}", get("user"), text)
        }
        "reaction" => format!("<{}> :{}:", get("user"), get("reaction")),
        "confirmation" => format!("decision={}", get("decision")),
        _ => String::new(),
    };
    format!(
        "[{}] {:<12} {:<14} {:<12} {}",
        r.pk, r.kind, r.status, chan, summary
    )
}

fn run_tail(from: Option<i64>) -> Result<(), String> {
    let mut cursor = from;
    loop {
        let v = http_post_value("/tail", json!({ "from": cursor }))?;
        if let Some(rows) = v.get("rows").and_then(|r| r.as_array()) {
            for rv in rows {
                if let Ok(row) = serde_json::from_value::<Row>(rv.clone()) {
                    println!("{}", fmt_row(&row));
                }
            }
        }
        cursor = v.get("cursor").and_then(|c| c.as_i64());
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

fn daemon_url(path: &str) -> String {
    let base =
        std::env::var("SLACK_DEPUTY_URL").unwrap_or_else(|_| "http://127.0.0.1:8799".to_string());
    format!("{}{}", base.trim_end_matches('/'), path)
}

/// POST to the daemon and return its response text (error on non-2xx). `timeout`
/// overrides the request timeout; the reqwest **blocking** client defaults to 30s
/// (unlike the async client, which has none), which would cut long-poll verbs like
/// `wait` short — pass a longer one there, `None` keeps the 30s default.
fn request(path: &str, body: Value, timeout: Option<Duration>) -> Result<String, String> {
    let mut rb = reqwest::blocking::Client::new()
        .post(daemon_url(path))
        .json(&body);
    if let Some(t) = timeout {
        rb = rb.timeout(t);
    }
    let resp = rb.send().map_err(|e| {
        format!(
            "request to daemon failed (is it running? SLACK_DEPUTY_URL={}): {e}",
            daemon_url("")
        )
    })?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("daemon {status}: {text}"));
    }
    Ok(text)
}

/// POST and print the daemon's response verbatim (what most verbs emit).
fn http_post(path: &str, body: Value) -> Result<(), String> {
    println!("{}", request(path, body, None)?);
    Ok(())
}

/// Like `http_post`, but with an explicit client timeout — for long-poll verbs
/// that block in the daemon longer than the 30s blocking-client default.
fn http_post_timeout(path: &str, body: Value, timeout: Duration) -> Result<(), String> {
    println!("{}", request(path, body, Some(timeout))?);
    Ok(())
}

/// POST and parse the daemon's response as JSON (for `tail`).
fn http_post_value(path: &str, body: Value) -> Result<Value, String> {
    serde_json::from_str(&request(path, body, None)?)
        .map_err(|e| format!("bad daemon response: {e}"))
}

pub fn run(cmd: Command) -> Result<(), String> {
    match cmd {
        Command::Next => http_post("/next", json!({}))?,
        Command::Wait { timeout } => {
            let secs = timeout.unwrap_or(0);
            // The daemon blocks up to `secs`; give the client headroom over that so
            // its own (30s default) timeout doesn't cut the long-poll short.
            http_post_timeout(
                "/wait",
                json!({ "timeout_secs": secs }),
                Duration::from_secs(secs + 30),
            )?
        }
        Command::Body { pk } => http_post("/body", json!({ "pk": pk }))?,
        Command::Done { pk } => http_post("/done", json!({ "pk": pk }))?,
        Command::Await { pk } => http_post("/await", json!({ "pk": pk }))?,
        Command::Skip { pk, all } => {
            if pk.is_none() && !all {
                return Err("skip needs a <pk> or --all".to_string());
            }
            http_post("/skip", json!({ "pk": pk, "all": all }))?
        }
        Command::Post {
            channel,
            thread,
            text,
        } => http_post(
            "/post",
            json!({ "channel": channel, "thread_ts": thread, "text": text }),
        )?,
        Command::React { channel, ts, name } => http_post(
            "/react",
            json!({ "channel": channel, "ts": ts, "name": name }),
        )?,
        Command::Unreact { channel, ts, name } => http_post(
            "/unreact",
            json!({ "channel": channel, "ts": ts, "name": name }),
        )?,
        Command::Delete { channel, ts } => {
            http_post("/delete", json!({ "channel": channel, "ts": ts }))?
        }
        Command::Thread { channel, ts } => {
            http_post("/thread", json!({ "channel": channel, "ts": ts }))?
        }
        Command::Ask {
            text,
            post,
            channel,
            thread,
            action,
            choose,
            danger,
            context,
        } => {
            let choices: Vec<String> = choose
                .map(|s| {
                    s.split(',')
                        .map(|x| x.trim().to_string())
                        .filter(|x| !x.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let post_routing = if post {
                let channel = channel.ok_or("--post needs --channel")?;
                json!({ "type": "post", "channel": channel, "thread": thread })
            } else {
                Value::Null
            };
            let action: Value = match action {
                Some(a) => {
                    serde_json::from_str(&a).map_err(|e| format!("--action must be JSON: {e}"))?
                }
                None => Value::Null,
            };
            if !post && choices.is_empty() && action.is_null() {
                return Err("ask needs one of: --post, --choose, --action".to_string());
            }
            http_post(
                "/ask",
                json!({ "text": text, "action": action, "post": post_routing, "choices": choices, "danger": danger, "context": context }),
            )?
        }
        Command::Dm { thread } => http_post("/dm", json!({ "thread": thread }))?,
        Command::Tail { from } => run_tail(from)?,
    }
    Ok(())
}
