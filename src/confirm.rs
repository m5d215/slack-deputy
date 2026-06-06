//! Human confirmation IF (control channel = bot DM).
//!
//! `ask` posts a bot DM with buttons (approve/reject, or one per `--choose`
//! option, optionally `--danger`); the opaque action rides in each button's value.
//! An answer — a button click or a free-text reply in the ask thread — comes back
//! over the same socket and is normalized into a content-free `confirmation` row
//! (`{decision, action, ask_ts}`); the ask DM is edited to show the outcome. The
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

/// The opaque action embedded in an ask's buttons (each value = {decision, action}).
/// None if the message isn't one of our asks.
fn action_from_blocks(content: &SlackMessageContent) -> Option<Value> {
    for b in content.blocks.as_ref()? {
        let SlackBlock::Actions(a) = b else { continue };
        for el in &a.elements {
            let SlackActionBlockElement::Button(btn) = el else {
                continue;
            };
            if let Some(action) = btn
                .value
                .as_ref()
                .and_then(|v| serde_json::from_str::<Value>(v).ok())
                .and_then(|p| p.get("action").cloned())
            {
                return Some(action);
            }
        }
    }
    None
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

/// Post a confirmation DM and return the DM (channel, ts). The interaction shape
/// is declared by the caller; the daemon owns the Block Kit so the consumer never
/// builds blocks:
/// - default: 承認 / 却下 buttons (decision = "approve" / "reject").
/// - `choices`: one button per choice (decision = the chosen string) + 却下.
/// - `danger`: 承認 gets danger styling + a confirm dialog.
///
/// `action` is opaque to us; it rides in each button's value and comes back on click.
pub async fn ask(
    text: String,
    action: Value,
    choices: Vec<String>,
    danger: bool,
    context: Option<String>,
) -> Result<(String, String), String> {
    let shared = Shared::get();
    let channel = open_dm().await?;
    let token = bot_token();
    let session = shared.slack.open_session(&token);

    let mut buttons: Vec<SlackActionBlockElement> = Vec::new();
    if choices.is_empty() {
        let mut approve = decision_button("approve", "承認", &action)
            .with_style(if danger { "danger" } else { "primary" }.to_string());
        if danger {
            approve = approve.with_confirm(SlackBlockConfirmItem::new(
                SlackBlockPlainTextOnly::from("実行の確認"),
                SlackBlockMarkDownText::new("この操作を実行します。よろしいですか？".into()).into(),
                SlackBlockPlainTextOnly::from("実行する"),
                SlackBlockPlainTextOnly::from("やめる"),
            ));
        }
        buttons.push(approve.into());
    } else {
        for choice in &choices {
            buttons.push(decision_button(choice, choice, &action).into());
        }
    }
    buttons.push(
        decision_button("reject", "却下", &action)
            .with_style("danger".to_string())
            .into(),
    );

    let section = SlackSectionBlock::new().with_text(SlackBlockMarkDownText::new(text).into());
    let mut blocks: Vec<SlackBlock> = vec![section.into()];
    if let Some(ctx) = context {
        blocks.push(
            SlackContextBlock::new(vec![SlackContextBlockElement::MarkDown(
                SlackBlockMarkDownText::new(ctx),
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
    let Some(action) = action_from_blocks(&root.content) else {
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

    // The button value carries {decision, action}. Fall back to action_id for the
    // decision and the raw value as the action if it isn't the expected shape.
    let parsed: Value = serde_json::from_str(&value).unwrap_or(Value::Null);
    let decision = parsed
        .get("decision")
        .and_then(|d| d.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| clicked.map(|a| a.action_id.to_string()).unwrap_or_default());
    let action_json: Value = parsed
        .get("action")
        .cloned()
        .unwrap_or_else(|| Value::String(value.clone()));

    // The container tells us which DM message to edit.
    let (channel, msg_ts) = match &ev.container {
        SlackInteractionActionContainer::Message(m) => {
            (m.channel_id.clone(), Some(m.message_ts.clone()))
        }
        _ => (None, None),
    };

    // Record the decision as a confirmation row. We store only the decision, the
    // opaque action (relayed — the buttons that hold it are about to be edited
    // away), and a pointer to the ask. The handler reads the ask thread back for
    // any content (draft, free-text reply) — the daemon doesn't extract content.
    let ask_ts = msg_ts.as_ref().map(|t| t.to_string());
    let body = json!({ "decision": decision, "action": action_json, "ask_ts": ask_ts }).to_string();
    let ts = msg_ts.as_ref().map(|t| t.to_string()).unwrap_or_else(now);
    let chan_str = channel.as_ref().map(|c| c.to_string());
    if let Err(e) = shared.db.insert(
        "confirmation",
        chan_str.as_deref(),
        None,
        &ts,
        &body,
        &now(),
    ) {
        warn!(kind = "confirm.insert_failed", error = %format!("{e:?}"), "confirmation insert failed");
    }

    // Edit the DM: drop the buttons, show the outcome (so open/answered is
    // legible from the DM alone, for humans and for handler subagents).
    if let (Some(c), Some(t)) = (channel, msg_ts) {
        let label = match decision.as_str() {
            "reject" => "❌ 却下".to_string(),
            "approve" => "✅ 承認済み".to_string(),
            other => format!("✅ {other}"), // a chosen option
        };
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
    info!(kind = "confirm.resolved", decision = %decision, "confirmation resolved");
    Ok(())
}
