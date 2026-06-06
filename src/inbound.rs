//! Socket Mode inbound: triage → capture scope → echo suppression → SQLite.

use crate::state::Shared;
use slack_morphism::prelude::*;
use std::sync::Arc;
use tracing::{error, info, warn};

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub async fn handle_push_event(
    event: SlackPushEventCallback,
    _client: Arc<SlackHyperClient>,
    _states: SlackClientEventsUserState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let shared = Shared::get();
    match event.event {
        SlackEventCallbackBody::Message(m) => {
            // A human's reply in an ask's thread is a free-text answer, not a new
            // message — route it to the confirmation path first.
            if crate::confirm::try_free_text_answer(shared, &m).await {
                return Ok(());
            }
            handle_message(shared, m);
        }
        SlackEventCallbackBody::ReactionAdded(r) => handle_reaction(
            shared,
            "reaction_added",
            &r.user,
            &r.reaction,
            r.item_user.as_ref(),
            &r.item,
            &r.event_ts,
        ),
        SlackEventCallbackBody::ReactionRemoved(r) => handle_reaction(
            shared,
            "reaction_removed",
            &r.user,
            &r.reaction,
            r.item_user.as_ref(),
            &r.item,
            &r.event_ts,
        ),
        other => {
            info!(kind = "slack.other_event", event = %format!("{other:?}"), "non-handled event");
        }
    }
    Ok(())
}

fn handle_message(shared: &Shared, m: SlackMessageEvent) {
    // Triage: whitelist human-equivalent subtypes, drop edits/deletes/system.
    match &m.subtype {
        None
        | Some(SlackMessageEventType::BotMessage)
        | Some(SlackMessageEventType::ThreadBroadcast)
        | Some(SlackMessageEventType::FileShare)
        | Some(SlackMessageEventType::MeMessage) => {}
        Some(other) => {
            info!(kind = "triage.drop", reason = "subtype", subtype = %format!("{other:?}"), "dropped");
            return;
        }
    }
    if m.hidden.unwrap_or(false) {
        return;
    }

    // Echo suppression: our own posts come back carrying a self bot_id (the
    // bot's own, or the learned user-token-post bot_id). Manual posts have no
    // bot_id, so they survive.
    if let Some(sender_bot) = m.sender.bot_id.as_ref() {
        if shared.is_self_bot(&sender_bot.to_string()) {
            info!(kind = "echo.drop", ts = %m.origin.ts, "dropped own echo (self bot_id)");
            return;
        }
    }

    let Some(text) = m.content.as_ref().and_then(|c| c.text.clone()) else {
        return;
    };
    if text.trim().is_empty() {
        return;
    }
    let Some(channel) = m.origin.channel.as_ref().map(|c| c.to_string()) else {
        return;
    };
    let user = m.sender.user.as_ref().map(|u| u.to_string());
    let bot_id = m.sender.bot_id.as_ref().map(|b| b.to_string());
    if user.is_none() && bot_id.is_none() {
        return;
    }
    let ts = m.origin.ts.to_string();
    let thread_ts = m.origin.thread_ts.as_ref().map(|t| t.to_string());
    let channel_type = m.origin.channel_type.as_ref().map(|c| c.to_string());

    // Capture scope: directed events get dispatched (a worker), ambient events
    // are stored as pull-only context, everything else is dropped.
    let status = match classify_message(
        shared,
        channel_type.as_deref(),
        &channel,
        &text,
        user.as_deref(),
        thread_ts.as_deref(),
    ) {
        Capture::Drop => {
            info!(kind = "scope.drop", channel = %channel, ts = %ts, "outside capture scope");
            return;
        }
        Capture::Directed => "pending",
        Capture::Ambient => "ambient",
    };

    let partition = thread_ts.clone().unwrap_or_else(|| ts.clone());
    let body = serde_json::json!({
        "text": text,
        "user": user,
        "bot_id": bot_id,
        "username": m.sender.username,
        "ts": ts,
        "thread_ts": thread_ts,
        "channel": channel,
        "permalink": shared.permalink(&channel, &ts),
    })
    .to_string();

    match shared.db.insert_with_status(
        "message",
        Some(&channel),
        Some(&partition),
        &ts,
        &body,
        status,
        &now(),
    ) {
        Ok(pk) => {
            info!(kind = "store.inserted", pk, row = "message", status, channel = %channel, ts = %ts, "stored message")
        }
        Err(e) => warn!(kind = "store.insert_failed", error = %format!("{e:?}"), "insert failed"),
    }
}

/// How a captured message should enter the queue.
enum Capture {
    /// Outside scope — not stored at all.
    Drop,
    /// Aimed at me — dispatched (a worker handles it).
    Directed,
    /// Observed / participatory but not aimed at me — stored as pull-only
    /// context, never dispatched.
    Ambient,
}

/// Classify a message for the queue. Directed signals win over ambient ones, so
/// a mention inside a watched channel (or a reply in a thread a directed event
/// already touched) is dispatched, not merely observed.
///
/// - `Directed`: DM/group-DM, a mention of me, or a reply in a thread that
///   already holds a directed row.
/// - `Ambient`: a watched channel, my own manual post (participation + thread
///   seed), or a reply that only continues an ambient thread.
/// - `Drop`: everything else.
fn classify_message(
    shared: &Shared,
    channel_type: Option<&str>,
    channel: &str,
    text: &str,
    user: Option<&str>,
    thread_ts: Option<&str>,
) -> Capture {
    if matches!(channel_type, Some("im") | Some("mpim")) {
        return Capture::Directed;
    }
    if let Some(me) = &shared.my_user_id {
        if text.contains(&format!("<@{me}>")) {
            return Capture::Directed;
        }
    }
    if let Some(t) = thread_ts {
        if shared.db.thread_has_directed(t).unwrap_or(false) {
            return Capture::Directed;
        }
    }
    if shared.config.watch_channels.contains(channel) {
        return Capture::Ambient;
    }
    if let Some(me) = &shared.my_user_id {
        if user == Some(me.as_str()) {
            return Capture::Ambient;
        }
    }
    if let Some(t) = thread_ts {
        if shared.db.thread_tracked(t).unwrap_or(false) {
            return Capture::Ambient;
        }
    }
    Capture::Drop
}

fn handle_reaction(
    shared: &Shared,
    event: &str,
    user: &SlackUserId,
    reaction: &SlackReactionName,
    item_user: Option<&SlackUserId>,
    item: &SlackReactionsItem,
    event_ts: &SlackTs,
) {
    // Our own reactions ARE captured (reaction-as-command: reactor == me is a
    // signal). Their programmatic echoes are dropped below via the key ledger
    // (reaction_added has no bot_id), leaving only manual reactions.
    let (channel, item_type, item_ts) = match item {
        SlackReactionsItem::Message(msg) => (
            msg.origin.channel.as_ref().map(|c| c.to_string()),
            "message",
            Some(msg.origin.ts.to_string()),
        ),
        SlackReactionsItem::File(_) => (None, "file", None),
    };

    // Echo suppression: reaction_added has no bot_id, so we can't use the
    // bot_id trick. Match the key we recorded when we emitted a reaction, and
    // require reactor == me (so we never drop someone else's reaction).
    if let (Some(me), Some(it)) = (&shared.my_user_id, &item_ts) {
        if user.to_string() == *me {
            let key = format!("{}:{}:{}", channel.as_deref().unwrap_or(""), it, reaction.to_string());
            if shared.take_self_reaction(&key) {
                info!(kind = "echo.drop", row = "reaction", "dropped own reaction echo (key ledger)");
                return;
            }
        }
    }

    // Capture scope for reactions:
    //  - by me (reaction-as-command: 👀 = look at this, 🎫 = file a ticket, …;
    //    the reaction→action mapping lives consumer-side). My own *programmatic*
    //    reactions were already dropped above by the echo ledger, so a `by_me`
    //    reaction reaching here is a manual one.
    //  - on my own message, or in a tracked thread.
    let by_me = matches!(&shared.my_user_id, Some(me) if user.to_string() == *me);
    let on_my_message = match (&shared.my_user_id, item_user) {
        (Some(me), Some(iu)) => iu.to_string() == *me,
        _ => false,
    };
    let in_tracked = item_ts
        .as_deref()
        .map(|t| shared.db.thread_tracked(t).unwrap_or(false))
        .unwrap_or(false);
    if !(by_me || on_my_message || in_tracked) {
        info!(kind = "scope.drop", row = "reaction", reaction = %reaction.to_string(), "outside capture scope");
        return;
    }

    let ts = event_ts.to_string();
    let partition = item_ts.clone();
    // permalink points at the reacted message (channel + item_ts), not the reaction.
    let permalink = match (&channel, &item_ts) {
        (Some(c), Some(it)) => shared.permalink(c, it),
        _ => None,
    };
    let body = serde_json::json!({
        "event": event,
        "user": user.to_string(),
        "reaction": reaction.to_string(),
        "item_type": item_type,
        "item_user": item_user.map(|u| u.to_string()),
        "item_ts": item_ts,
        "event_ts": ts,
        "channel": channel,
        "permalink": permalink,
    })
    .to_string();

    match shared.db.insert(
        "reaction",
        channel.as_deref(),
        partition.as_deref(),
        &ts,
        &body,
        &now(),
    ) {
        Ok(pk) => {
            info!(kind = "store.inserted", pk, row = "reaction", reaction = %reaction.to_string(), "stored reaction")
        }
        Err(e) => warn!(kind = "store.insert_failed", error = %format!("{e:?}"), "insert failed"),
    }
}

pub fn on_error(
    err: Box<dyn std::error::Error + Send + Sync>,
    _client: Arc<SlackHyperClient>,
    _states: SlackClientEventsUserState,
) -> HttpStatusCode {
    error!(kind = "callback.error", error = %format!("{err:?}"), "slack callback error");
    HttpStatusCode::OK
}
