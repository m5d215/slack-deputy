//! Human confirmation IF (control channel = bot DM).
//!
//! `ask` posts a bot DM with buttons. A button is one of two kinds, told apart by
//! the key its routing rides under in the button value:
//! - **terminal** (`exec`): the daemon runs it the moment the human clicks — `投稿`
//!   posts the ask's draft under the user token, `無視` (always present) just closes
//!   the ask. No consumer hop. The click is still recorded, as a `done` row.
//! - **routed** (`action`): a `--choose` option (or a non-post approval). The click
//!   becomes a `pending` confirmation row a subagent handles on a later tick.
//!
//! So one `status` flag decides whether an answer reaches the consumer at all.
//! A free-text reply in the ask thread is always routed (it needs judgment).
//! `confirmation` rows are content-free (`{decision, action|exec, ask_ts}`); the
//! consumer reads the ask thread back (`dm_history`) for the draft / reply.

use crate::state::Shared;
use serde_json::{Value, json};
use slack_morphism::prelude::*;
use std::sync::Arc;
use tracing::{info, warn};

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn bot_token() -> SlackApiToken {
    SlackApiToken::new(SlackApiTokenValue::from(
        Shared::get().config.bot_token.clone(),
    ))
}

/// Concatenate the text of a message's section blocks. Used by the DM read-back
/// (`dm_history`) to surface an ask's draft, which lives in a section block — so a
/// handler recovers it without the draft ever riding the size-limited button value.
fn section_text(content: &SlackMessageContent) -> Option<String> {
    let parts: Vec<String> = content
        .blocks
        .as_ref()?
        .iter()
        .filter_map(|b| match b {
            SlackBlock::Section(s) => s.text.as_ref().map(|t| match t {
                SlackBlockText::Plain(p) => p.text.clone(),
                SlackBlockText::MarkDown(m) => m.text.clone(),
            }),
            _ => None,
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// The routing embedded in an ask's buttons. A button value carries it under
/// `exec` (terminal) or `action` (routed) — same shape, the key just says who
/// executes. Returns the first non-`ignore` routing, so a free-text reply to a
/// terminal post ask recovers the post target rather than the `無視` no-op. None
/// if the message isn't one of our asks.
fn routing_from_blocks(content: &SlackMessageContent) -> Option<Value> {
    let mut ignore_fallback = None;
    for b in content.blocks.as_ref()? {
        let SlackBlock::Actions(a) = b else { continue };
        for el in &a.elements {
            let SlackActionBlockElement::Button(btn) = el else {
                continue;
            };
            let Some(routing) = btn
                .value
                .as_ref()
                .and_then(|v| serde_json::from_str::<Value>(v).ok())
                .and_then(|p| p.get("exec").or_else(|| p.get("action")).cloned())
            else {
                continue;
            };
            if routing.get("type").and_then(|t| t.as_str()) == Some("ignore") {
                ignore_fallback.get_or_insert(routing);
            } else {
                return Some(routing);
            }
        }
    }
    ignore_fallback
}

/// Rebuild a confirmation message with the buttons (the Actions block) replaced by
/// an outcome line, preserving the original prompt/context blocks — so the DM keeps
/// its content after it's resolved.
fn resolved_content(blocks: Option<Vec<SlackBlock>>, label: &str) -> SlackMessageContent {
    let mut kept: Vec<SlackBlock> = blocks
        .unwrap_or_default()
        .into_iter()
        .filter(|b| !matches!(b, SlackBlock::Actions(_)))
        .collect();
    if kept.is_empty() {
        return SlackMessageContent::new().with_text(label.to_string());
    }
    kept.push(
        SlackContextBlock::new(vec![SlackContextBlockElement::MarkDown(
            SlackBlockMarkDownText::new(label.to_string()),
        )])
        .into(),
    );
    SlackMessageContent::new()
        .with_text(label.to_string())
        .with_blocks(kept)
}

/// The `投稿先:` context line for a post ask: a channel mention, plus a permalink
/// to the target thread when `--thread` was given. The worker can't resolve
/// permalinks (it holds no workspace domain), so the daemon does it here from the
/// post routing. Best effort — a missing channel drops the line, and a
/// `getPermalink` failure degrades to the bare channel mention rather than
/// failing the ask.
async fn post_destination_line(post: &Value) -> Option<String> {
    let channel = post.get("channel").and_then(|c| c.as_str())?;
    let mention = format!("<#{channel}>");
    let Some(thread) = post.get("thread").and_then(|t| t.as_str()) else {
        return Some(format!("投稿先: {mention}"));
    };
    let shared = Shared::get();
    let token = bot_token();
    let session = shared.slack.open_session(&token);
    let req = SlackApiChatGetPermalinkRequest {
        channel: SlackChannelId::from(channel.to_string()),
        message_ts: SlackTs::from(thread.to_string()),
    };
    match session.chat_get_permalink(&req).await {
        Ok(resp) => Some(format!(
            "投稿先: {mention} (<{}|該当スレッド>)",
            resp.permalink
        )),
        Err(e) => {
            warn!(kind = "confirm.permalink_failed", error = %format!("{e:?}"), "getPermalink failed");
            Some(format!("投稿先: {mention}"))
        }
    }
}

/// Open (or fetch) the DM channel with the human, using the bot token.
async fn open_dm() -> Result<SlackChannelId, String> {
    let shared = Shared::get();
    let me = shared
        .my_user_id
        .clone()
        .ok_or_else(|| "my_user_id unknown".to_string())?;
    let token = bot_token();
    let session = shared.slack.open_session(&token);
    let req = SlackApiConversationsOpenRequest::new().with_users(vec![SlackUserId::from(me)]);
    let resp = session
        .conversations_open(&req)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(resp.channel.id)
}

/// One interactive button carrying its decision. The value embeds both the
/// decision (what the human chose) and the opaque `action` (what the consumer
/// executes), so the click round-trips everything the handler needs.
fn decision_button(decision: &str, label: &str, action: &Value) -> SlackBlockButtonElement {
    let value = json!({ "decision": decision, "action": action }).to_string();
    SlackBlockButtonElement::new(
        SlackActionId::from(decision),
        SlackBlockPlainTextOnly::from(label),
    )
    .with_value(value)
}

/// A terminal button: its routing rides under `exec`, so `handle_interaction`
/// runs it at the edge (the daemon posts / no-ops) instead of queuing it for a
/// subagent. `投稿` and the always-present `無視` are built this way.
fn exec_button(decision: &str, label: &str, exec: &Value) -> SlackBlockButtonElement {
    let value = json!({ "decision": decision, "exec": exec }).to_string();
    SlackBlockButtonElement::new(
        SlackActionId::from(decision),
        SlackBlockPlainTextOnly::from(label),
    )
    .with_value(value)
}

/// The confirm dialog for `--danger` (a second click before an irreversible op).
fn confirm_dialog() -> SlackBlockConfirmItem {
    SlackBlockConfirmItem::new(
        SlackBlockPlainTextOnly::from("実行の確認"),
        SlackBlockMarkDownText::new("この操作を実行します。よろしいですか？".into()).into(),
        SlackBlockPlainTextOnly::from("実行する"),
        SlackBlockPlainTextOnly::from("やめる"),
    )
}

/// Post a confirmation DM and return the DM (channel, ts). The interaction shape
/// is declared by the caller; the daemon owns the Block Kit so the consumer never
/// builds blocks. Exactly one positive button is built, then `無視` is appended:
/// - `post`: terminal `投稿` — the daemon posts the ask's draft on click.
/// - `choices`: routed — one button per choice (decision = the chosen string).
/// - `action` (neither of the above): routed `承認` escape hatch for a non-post op.
/// - `danger`: the positive button gets danger styling + a confirm dialog.
/// - `無視`: always present, terminal no-op (just closes the ask).
///
/// `action` rides under `action` (routed, opaque to us); `post` under `exec`
/// (terminal, we read its `channel`/`thread` to post). Both come back on click.
pub async fn ask(
    text: String,
    action: Option<Value>,
    post: Option<Value>,
    choices: Vec<String>,
    danger: bool,
    context: Option<String>,
) -> Result<(String, String), String> {
    let shared = Shared::get();
    let channel = open_dm().await?;
    let token = bot_token();
    let session = shared.slack.open_session(&token);

    let style = if danger { "danger" } else { "primary" }.to_string();
    let mut buttons: Vec<SlackActionBlockElement> = Vec::new();
    if let Some(post) = &post {
        // Terminal: the daemon posts the draft on click, no consumer hop.
        let mut btn = exec_button("post", "投稿", post).with_style(style);
        if danger {
            btn = btn.with_confirm(confirm_dialog());
        }
        buttons.push(btn.into());
    } else if !choices.is_empty() {
        let action = action.clone().unwrap_or(Value::Null);
        for choice in &choices {
            buttons.push(decision_button(choice, choice, &action).into());
        }
    } else if let Some(action) = &action {
        // Routed escape hatch: a non-post approval a subagent executes.
        let mut approve = decision_button("approve", "承認", action).with_style(style);
        if danger {
            approve = approve.with_confirm(confirm_dialog());
        }
        buttons.push(approve.into());
    } else {
        return Err("ask needs one of: post, choices, action".to_string());
    }
    // 無視: always present, terminal no-op (the daemon just closes the ask).
    buttons.push(
        exec_button("ignore", "無視", &json!({ "type": "ignore" }))
            .with_style("danger".to_string())
            .into(),
    );

    let section = SlackSectionBlock::new().with_text(SlackBlockMarkDownText::new(text).into());
    let mut blocks: Vec<SlackBlock> = vec![section.into()];
    // Context block: the worker's `--context` and the auto-resolved post target,
    // newline-joined so both survive when used together.
    let mut ctx_parts: Vec<String> = Vec::new();
    if let Some(ctx) = context {
        ctx_parts.push(ctx);
    }
    if let Some(post) = &post
        && let Some(dest) = post_destination_line(post).await
    {
        ctx_parts.push(dest);
    }
    if !ctx_parts.is_empty() {
        blocks.push(
            SlackContextBlock::new(vec![SlackContextBlockElement::MarkDown(
                SlackBlockMarkDownText::new(ctx_parts.join("\n")),
            )])
            .into(),
        );
    }
    blocks.push(SlackActionsBlock::new(buttons).into());
    let content = SlackMessageContent::new().with_blocks(blocks);

    let req = SlackApiChatPostMessageRequest::new(channel.clone(), content);
    let resp = session
        .chat_post_message(&req)
        .await
        .map_err(|e| format!("{e:?}"))?;
    info!(kind = "confirm.asked", channel = %channel, ts = %resp.ts, "posted confirmation DM");
    Ok((channel.to_string(), resp.ts.to_string()))
}

/// Render a bot-DM message as JSON. `text` prefers the section text (so a
/// block-only ask surfaces its draft, even after the buttons are edited away) and
/// falls back to the plain text (a human's reply). `open` is true while the ask
/// still has its buttons (awaiting an answer) — once resolved the Actions block is
/// removed, so a handler can tell open from answered. The daemon returns content;
/// it doesn't decide what's "the draft".
fn dm_msg_json(m: &SlackHistoryMessage) -> Value {
    let text = section_text(&m.content).or_else(|| m.content.text.clone());
    let open = m
        .content
        .blocks
        .as_ref()
        .is_some_and(|bs| bs.iter().any(|b| matches!(b, SlackBlock::Actions(_))));
    json!({
        "ts": m.origin.ts.to_string(),
        "user": m.sender.user.as_ref().map(|u| u.to_string()),
        "bot_id": m.sender.bot_id.as_ref().map(|b| b.to_string()),
        "text": text,
        "open": open,
    })
}

/// Bot-DM messages, so a handler can read confirmation context. With `thread`,
/// returns that ask's thread (root = draft, replies = any free-text answer);
/// without, recent history (spotting in-flight confirmations).
pub async fn dm_history(thread: Option<String>) -> Result<Vec<Value>, String> {
    let shared = Shared::get();
    let channel = open_dm().await?;
    let token = bot_token();
    let session = shared.slack.open_session(&token);
    let messages = match thread {
        Some(ts) => {
            let req = SlackApiConversationsRepliesRequest::new(channel, SlackTs::from(ts));
            session
                .conversations_replies(&req)
                .await
                .map_err(|e| format!("{e:?}"))?
                .messages
        }
        None => {
            let req = SlackApiConversationsHistoryRequest::new()
                .with_channel(channel)
                .with_limit(20);
            session
                .conversations_history(&req)
                .await
                .map_err(|e| format!("{e:?}"))?
                .messages
        }
    };
    Ok(messages.iter().map(dm_msg_json).collect())
}

/// If `m` is the human's thread reply to one of our open asks in the bot DM,
/// record it as a `text` confirmation (pointing at the ask) and resolve the ask
/// DM. Returns true if handled — the caller then skips normal capture. This is
/// the free-text answer path: a reply in the ask's thread is unambiguously an
/// answer to that ask (thread_ts identifies it), so no server-side correlation.
pub async fn try_free_text_answer(shared: &Shared, m: &SlackMessageEvent) -> bool {
    // Human message in the bot DM, in a thread, with text. (Bot echoes carry a
    // bot_id; our own asks aren't thread replies.)
    if m.subtype.is_some() || m.hidden.unwrap_or(false) || m.sender.bot_id.is_some() {
        return false;
    }
    let (Some(me), Some(bot_dm)) = (&shared.my_user_id, &shared.bot_dm_channel) else {
        return false;
    };
    if m.sender.user.as_ref().map(|u| u.to_string()).as_deref() != Some(me.as_str()) {
        return false;
    }
    let Some(channel) = m.origin.channel.as_ref().map(|c| c.to_string()) else {
        return false;
    };
    if &channel != bot_dm {
        return false;
    }
    let Some(thread_ts) = m.origin.thread_ts.as_ref().map(|t| t.to_string()) else {
        return false;
    };
    let reply_ts = m.origin.ts.to_string();
    let reply_text = m
        .content
        .as_ref()
        .and_then(|c| c.text.clone())
        .unwrap_or_default();
    if reply_text.trim().is_empty() {
        return false;
    }

    // Read the thread root; it must be one of our asks (carries an action in its
    // buttons). If not, this is just a threaded DM — let normal capture have it.
    let token = bot_token();
    let session = shared.slack.open_session(&token);
    let req = SlackApiConversationsRepliesRequest::new(
        SlackChannelId::from(channel.clone()),
        SlackTs::from(thread_ts.clone()),
    );
    let root = match session.conversations_replies(&req).await {
        Ok(r) => r.messages.into_iter().next(),
        Err(e) => {
            warn!(kind = "confirm.text_root_failed", error = %format!("{e:?}"), "read ask root failed");
            return false;
        }
    };
    let Some(root) = root else { return false };
    let Some(action) = routing_from_blocks(&root.content) else {
        return false; // not an ask
    };

    // Record a text confirmation pointing at the ask; the handler reads the thread
    // (root = draft, this reply = the human's text) and decides what to do.
    let body = json!({ "decision": "text", "action": action, "ask_ts": thread_ts }).to_string();
    if let Err(e) = shared.db.insert(
        "confirmation",
        Some(&channel),
        None,
        &reply_ts,
        &body,
        &now(),
    ) {
        warn!(kind = "confirm.text_insert_failed", error = %format!("{e:?}"), "text confirmation insert failed");
        return true; // recorded-or-not, don't also capture it as a plain message
    }

    // Resolve the ask DM: drop the buttons, keep the prompt, mark it answered.
    let content = resolved_content(root.content.blocks.clone(), "✏️ 返信で対応");
    let upd = SlackApiChatUpdateRequest::new(
        SlackChannelId::from(channel),
        content,
        SlackTs::from(thread_ts),
    );
    if let Err(e) = session.chat_update(&upd).await {
        warn!(kind = "confirm.text_edit_failed", error = %format!("{e:?}"), "ask DM resolve failed");
    }
    info!(kind = "confirm.text_reply", "free-text answer recorded");
    true
}

/// Socket callback for Block Kit interactions (button clicks).
pub async fn handle_interaction(
    event: SlackInteractionEvent,
    _client: Arc<SlackHyperClient>,
    _states: SlackClientEventsUserState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let SlackInteractionEvent::BlockActions(ev) = event else {
        return Ok(());
    };
    let shared = Shared::get();

    let clicked = ev.actions.as_ref().and_then(|a| a.first());
    let value = clicked.and_then(|a| a.value.clone()).unwrap_or_default();

    // The button value carries {decision, exec|action}. Fall back to action_id for
    // the decision if it isn't the expected shape.
    let parsed: Value = serde_json::from_str(&value).unwrap_or(Value::Null);
    let decision = parsed
        .get("decision")
        .and_then(|d| d.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| clicked.map(|a| a.action_id.to_string()).unwrap_or_default());

    // The container tells us which DM message to edit (and is the ask pointer).
    let (channel, msg_ts) = match &ev.container {
        SlackInteractionActionContainer::Message(m) => {
            (m.channel_id.clone(), Some(m.message_ts.clone()))
        }
        _ => (None, None),
    };
    let ask_ts = msg_ts.as_ref().map(|t| t.to_string());
    let chan_str = channel.as_ref().map(|c| c.to_string());

    // Terminal (`exec`) vs routed (`action`). A terminal button the daemon runs
    // now and records as `done` (the consumer never sees it); a routed one becomes
    // a `pending` row a subagent handles on a later tick.
    let (status, label, body) = if let Some(exec) = parsed.get("exec") {
        let draft = ev.message.as_ref().and_then(|m| section_text(&m.content));
        let (label, result) = run_terminal(exec, draft).await;
        let body =
            json!({ "decision": decision, "exec": exec, "ask_ts": ask_ts, "result": result })
                .to_string();
        ("done", label, body)
    } else {
        // Relay the opaque action (the buttons holding it are about to be edited
        // away); the subagent reads the ask thread back for content (draft, reply).
        let action = parsed
            .get("action")
            .cloned()
            .unwrap_or_else(|| Value::String(value.clone()));
        let label = if decision == "approve" {
            "✅ 承認済み".to_string()
        } else {
            format!("✅ {decision}") // a chosen option
        };
        let body = json!({ "decision": decision, "action": action, "ask_ts": ask_ts }).to_string();
        ("pending", label, body)
    };

    let ts = msg_ts.as_ref().map(|t| t.to_string()).unwrap_or_else(now);
    if let Err(e) = shared.db.insert_with_status(
        "confirmation",
        chan_str.as_deref(),
        None,
        &ts,
        &body,
        status,
        &now(),
    ) {
        warn!(kind = "confirm.insert_failed", error = %format!("{e:?}"), "confirmation insert failed");
    }

    // Edit the DM: drop the buttons, show the outcome (so open/answered is
    // legible from the DM alone, for humans and for handler subagents).
    if let (Some(c), Some(t)) = (channel, msg_ts) {
        let content = resolved_content(
            ev.message.as_ref().and_then(|m| m.content.blocks.clone()),
            &label,
        );
        let token = bot_token();
        let session = shared.slack.open_session(&token);
        let req = SlackApiChatUpdateRequest::new(c, content, t);
        if let Err(e) = session.chat_update(&req).await {
            warn!(kind = "confirm.dm_edit_failed", error = %format!("{e:?}"), "DM edit failed");
        }
    }
    info!(kind = "confirm.resolved", decision = %decision, status = %status, "confirmation resolved");
    Ok(())
}

/// Carry out a terminal button at the edge. Returns (DM outcome label, a JSON
/// result for the `done` row's audit body). `post` posts the ask's `draft` (its
/// section text) under the user token; `ignore` (and anything else) is a no-op
/// close.
async fn run_terminal(exec: &Value, draft: Option<String>) -> (String, Value) {
    match exec.get("type").and_then(|t| t.as_str()) {
        Some("post") => {
            let channel = exec
                .get("channel")
                .and_then(|c| c.as_str())
                .map(|s| s.to_string());
            let thread = exec
                .get("thread")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string());
            match (draft, channel) {
                (Some(text), Some(channel)) => {
                    match crate::outbound::post_message(channel, thread, text).await {
                        Ok(ts) => ("✅ 投稿済み".to_string(), json!({ "posted_ts": ts })),
                        Err(e) => {
                            warn!(kind = "confirm.post_failed", error = %e, "terminal post failed");
                            ("⚠️ 投稿失敗".to_string(), json!({ "error": e }))
                        }
                    }
                }
                _ => {
                    warn!(
                        kind = "confirm.post_invalid",
                        "terminal post missing draft or channel"
                    );
                    (
                        "⚠️ 投稿失敗".to_string(),
                        json!({ "error": "missing draft or channel" }),
                    )
                }
            }
        }
        _ => ("🚫 無視".to_string(), json!({ "ignored": true })),
    }
}
