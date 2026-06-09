//! The daemon's HTTP API — the single window the CLI drives, for both queue verbs
//! (next/body/done/await/tail, backed by SQLite) and Slack verbs (post/react/ask/…
//! under the *user* token). The daemon owns SQLite and all Slack access, so a
//! consumer on another host needs only this address (no DB file, no tokens).
//! Bind is loopback by default; widen to a tailnet via `SLACK_DEPUTY_LISTEN`
//! (Tailscale is the auth boundary). On every post we learn the response's bot_id
//! for echo suppression.

use crate::state::Shared;
use axum::{Json, Router, http::StatusCode, routing::post};
use serde::Deserialize;
use serde_json::{Value, json};
use slack_morphism::prelude::*;
use tokio::net::TcpListener;
use tracing::{error, info};

type ApiResult = Result<Json<Value>, (StatusCode, String)>;

fn user_session() -> SlackApiToken {
    SlackApiToken::new(SlackApiTokenValue::from(
        Shared::get().config.user_token.clone(),
    ))
}

fn fail(e: impl std::fmt::Debug) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:?}"))
}

// --- queue verbs (SQLite-backed; the daemon is the sole owner of the DB) ---

/// Claim the oldest pending row (prints `{pk, thread_ts}` or `null`). No body.
async fn next_handler() -> ApiResult {
    let row = Shared::get().db.claim_next().map_err(fail)?;
    Ok(Json(match row {
        Some((pk, thread_ts)) => json!({ "pk": pk, "thread_ts": thread_ts }),
        None => Value::Null,
    }))
}

/// How often `wait_handler` re-checks the queue while blocking. The queue has no
/// notify channel, so this is a plain poll; 1s is cheap and bounds the extra
/// dispatch latency once a claimable row lands.
const WAIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Deserialize)]
struct WaitReq {
    /// Seconds to block before returning `{ready:false}`. 0 = check once.
    #[serde(default)]
    timeout_secs: u64,
}

/// Block until a claimable row exists (`{ready:true}`) or the timeout elapses
/// (`{ready:false}`). Claims nothing — the consumer parks this (in the background)
/// between drains so it wakes the instant a message lands instead of paying a cron
/// tick's latency floor. Claiming stays in `next` alone, so a wakeup lost across a
/// compaction can never orphan a row: the next drain re-fetches everything pending.
/// `ready` mirrors `claim_next`'s eligibility (a pending row with no in-flight
/// sibling), so a true never decays into a `next` that returns null.
async fn wait_handler(Json(req): Json<WaitReq>) -> ApiResult {
    let db = &Shared::get().db;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(req.timeout_secs);
    loop {
        if db.has_claimable().map_err(fail)? {
            return Ok(Json(json!({ "ready": true })));
        }
        if req.timeout_secs == 0 || tokio::time::Instant::now() >= deadline {
            return Ok(Json(json!({ "ready": false })));
        }
        tokio::time::sleep(WAIT_POLL_INTERVAL).await;
    }
}

#[derive(Deserialize)]
struct PkReq {
    pk: i64,
}

/// The event body JSON for a pk, as raw text (the handler subagent parses it).
async fn body_handler(Json(req): Json<PkReq>) -> Result<String, (StatusCode, String)> {
    match Shared::get().db.get_body(req.pk).map_err(fail)? {
        Some(b) => Ok(b),
        None => Err((StatusCode::NOT_FOUND, format!("no row pk={}", req.pk))),
    }
}

async fn done_handler(Json(req): Json<PkReq>) -> Result<String, (StatusCode, String)> {
    Shared::get().db.set_status(req.pk, "done").map_err(fail)?;
    Ok("ok".to_string())
}

async fn await_handler(Json(req): Json<PkReq>) -> Result<String, (StatusCode, String)> {
    Shared::get()
        .db
        .set_status(req.pk, "awaiting_human")
        .map_err(fail)?;
    Ok("ok".to_string())
}

#[derive(Deserialize)]
struct SkipReq {
    #[serde(default)]
    pk: Option<i64>,
    #[serde(default)]
    all: bool,
}

/// Skip pending rows without processing them (pending → skipped). `{all:true}`
/// drops the whole backlog (consumer recovery); `{pk}` skips one. Dispatched
/// rows are never touched.
async fn skip_handler(Json(req): Json<SkipReq>) -> ApiResult {
    let db = &Shared::get().db;
    let skipped = if req.all {
        db.skip_pending().map_err(fail)?
    } else if let Some(pk) = req.pk {
        db.skip(pk).map_err(fail)?
    } else {
        return Err((
            StatusCode::BAD_REQUEST,
            "skip needs pk or --all".to_string(),
        ));
    };
    Ok(Json(json!({ "ok": true, "skipped": skipped })))
}

#[derive(Deserialize)]
struct TailReq {
    #[serde(default)]
    from: Option<i64>,
}

/// Rows after `from` (default: the last ~10), with the new cursor — `tail` polls this.
async fn tail_handler(Json(req): Json<TailReq>) -> ApiResult {
    let db = &Shared::get().db;
    let start = match req.from {
        Some(f) => f,
        None => (db.max_pk().map_err(fail)? - 10).max(0),
    };
    let rows = db.rows_after(start).map_err(fail)?;
    let cursor = rows.last().map(|r| r.pk).unwrap_or(start);
    Ok(Json(json!({ "rows": rows, "cursor": cursor })))
}

#[derive(Deserialize)]
struct PostReq {
    channel: String,
    thread_ts: Option<String>,
    text: String,
}

/// Post a message under the user token and return its ts. Shared by the `/post`
/// verb and the terminal `投稿` confirmation button (`confirm::handle_interaction`),
/// so both learn the response bot_id for echo suppression the same way.
pub async fn post_message(
    channel: String,
    thread_ts: Option<String>,
    text: String,
) -> Result<String, String> {
    let shared = Shared::get();
    let token = user_session();
    let session = shared.slack.open_session(&token);
    let mut api = SlackApiChatPostMessageRequest::new(
        SlackChannelId::from(channel.clone()),
        SlackMessageContent::new().with_text(text),
    );
    if let Some(t) = thread_ts {
        api = api.with_thread_ts(SlackTs::from(t));
    }
    let resp = session
        .chat_post_message(&api)
        .await
        .map_err(|e| format!("{e:?}"))?;

    // Learn the bot_id Slack stamps on our user-token posts → echo suppression.
    if let Some(bot_id) = resp.message.sender.bot_id.as_ref() {
        shared.learn_self_bot(bot_id.as_ref());
    }
    info!(kind = "outbound.posted", channel = %channel, ts = %resp.ts, "posted as user");
    Ok(resp.ts.to_string())
}

async fn post_handler(Json(req): Json<PostReq>) -> ApiResult {
    let ts = post_message(req.channel, req.thread_ts, req.text)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!({ "ok": true, "ts": ts })))
}

#[derive(Deserialize)]
struct ReactReq {
    channel: String,
    ts: String,
    name: String,
}

async fn react_handler(Json(req): Json<ReactReq>) -> ApiResult {
    let shared = Shared::get();
    let token = user_session();
    let session = shared.slack.open_session(&token);
    let key = format!("{}:{}:{}", req.channel, req.ts, req.name);
    let api = SlackApiReactionsAddRequest::new(
        SlackChannelId::from(req.channel),
        SlackReactionName::from(req.name),
        SlackTs::from(req.ts),
    );
    session.reactions_add(&api).await.map_err(fail)?;
    shared.record_reaction(key); // suppress the echo (reaction_added has no bot_id)
    Ok(Json(json!({ "ok": true })))
}

async fn unreact_handler(Json(req): Json<ReactReq>) -> ApiResult {
    let shared = Shared::get();
    let token = user_session();
    let session = shared.slack.open_session(&token);
    let key = format!("{}:{}:{}", req.channel, req.ts, req.name);
    let api = SlackApiReactionsRemoveRequest::new(SlackReactionName::from(req.name))
        .with_channel(SlackChannelId::from(req.channel))
        .with_timestamp(SlackTs::from(req.ts));
    session.reactions_remove(&api).await.map_err(fail)?;
    shared.record_reaction(key);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct DeleteReq {
    channel: String,
    ts: String,
}

async fn delete_handler(Json(req): Json<DeleteReq>) -> ApiResult {
    let token = user_session();
    let session = Shared::get().slack.open_session(&token);
    let api =
        SlackApiChatDeleteRequest::new(SlackChannelId::from(req.channel), SlackTs::from(req.ts));
    session.chat_delete(&api).await.map_err(fail)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct ThreadReq {
    channel: String,
    ts: String,
}

/// Read a thread (conversations.replies) as yourself, for context gathering.
async fn thread_handler(Json(req): Json<ThreadReq>) -> ApiResult {
    let token = user_session();
    let session = Shared::get().slack.open_session(&token);
    let api = SlackApiConversationsRepliesRequest::new(
        SlackChannelId::from(req.channel),
        SlackTs::from(req.ts),
    );
    let resp = session.conversations_replies(&api).await.map_err(fail)?;
    let messages: Vec<Value> = resp
        .messages
        .iter()
        .map(|m| {
            json!({
                "ts": m.origin.ts.to_string(),
                "user": m.sender.user.as_ref().map(|u| u.to_string()),
                "bot_id": m.sender.bot_id.as_ref().map(|b| b.to_string()),
                "text": m.content.text,
            })
        })
        .collect();
    Ok(Json(json!({ "ok": true, "messages": messages })))
}

#[derive(Deserialize)]
struct AskReq {
    text: String,
    /// Routed action JSON (for `--choose` or a non-post approval). A subagent runs
    /// it on a later tick. `null` when the ask is a terminal post.
    #[serde(default)]
    action: Option<Value>,
    /// Terminal post routing `{type:"post", channel, thread}`. When set, the
    /// approve button is `投稿` and the daemon posts the draft on click.
    #[serde(default)]
    post: Option<Value>,
    #[serde(default)]
    choices: Vec<String>,
    #[serde(default)]
    danger: bool,
    #[serde(default)]
    context: Option<String>,
}

/// Post a confirmation DM with buttons (control channel).
async fn ask_handler(Json(req): Json<AskReq>) -> ApiResult {
    let (channel, ts) = crate::confirm::ask(
        req.text,
        req.action,
        req.post,
        req.choices,
        req.danger,
        req.context,
    )
    .await
    .map_err(fail)?;
    Ok(Json(json!({ "ok": true, "channel": channel, "ts": ts })))
}

#[derive(Deserialize)]
struct DmReq {
    #[serde(default)]
    thread: Option<String>,
}

/// Read bot-DM messages. With `thread`, that ask's thread (draft + replies);
/// without, recent history (in-flight confirmations).
async fn dm_handler(Json(req): Json<DmReq>) -> ApiResult {
    let messages = crate::confirm::dm_history(req.thread).await.map_err(fail)?;
    Ok(Json(json!({ "ok": true, "messages": messages })))
}

pub async fn run_http_server() {
    let shared = Shared::get();
    let listen = shared.config.listen.clone();
    let app = Router::new()
        // queue verbs (SQLite, daemon-owned)
        .route("/next", post(next_handler))
        .route("/wait", post(wait_handler))
        .route("/body", post(body_handler))
        .route("/done", post(done_handler))
        .route("/await", post(await_handler))
        .route("/skip", post(skip_handler))
        .route("/tail", post(tail_handler))
        // slack verbs (user/bot token)
        .route("/post", post(post_handler))
        .route("/react", post(react_handler))
        .route("/unreact", post(unreact_handler))
        .route("/delete", post(delete_handler))
        .route("/thread", post(thread_handler))
        .route("/ask", post(ask_handler))
        .route("/dm", post(dm_handler));
    let listener = TcpListener::bind(listen.as_str()).await.expect("bind http");
    info!(kind = "startup.http_listening", addr = %listen, "http server listening");
    if let Err(e) = axum::serve(listener, app).await {
        error!(kind = "http.serve_failed", error = %format!("{e:?}"), "http serve ended");
    }
}
