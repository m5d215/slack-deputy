//! Shared daemon state, accessible from both the socket callbacks and the HTTP
//! handlers via a process-global.

use crate::config::Config;
use crate::db::Db;
use slack_morphism::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long an emitted-reaction key stays in the echo ledger. Echoes return in
/// seconds; a few minutes is plenty.
const REACT_TTL: Duration = Duration::from_secs(300);

pub struct Shared {
    pub db: Arc<Db>,
    pub config: Arc<Config>,
    pub slack: Arc<SlackHyperClient>,
    /// The human's user id (from the user-token auth.test). Used for mention /
    /// own-post detection in capture scope.
    pub my_user_id: Option<String>,
    /// Workspace base URL from the user-token auth.test (e.g.
    /// `https://team.slack.com/`). Used to build message permalinks at capture so
    /// handlers get a URL without holding Slack creds or hardcoding the host.
    pub workspace_url: Option<String>,
    /// The bot-DM channel id (control channel). Used to recognize the human's
    /// free-text replies to asks (thread replies in this channel).
    pub bot_dm_channel: Option<String>,
    /// Known "self" bot_ids whose inbound events are our own echoes:
    /// the bot's own bot_id (auth.test) + the bot_id learned from our own
    /// user-token post responses. See db meta key `self_post_bot_id`.
    pub self_bot_ids: Mutex<HashSet<String>>,
    /// Reactions we emitted, keyed `channel:item_ts:name`. reaction_added has no
    /// bot_id, so the bot_id trick can't suppress reaction echoes — we match the
    /// key instead (TTL'd, one-shot).
    pub react_ledger: Mutex<HashMap<String, Instant>>,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

impl Shared {
    pub fn install(self) {
        if SHARED.set(Arc::new(self)).is_err() {
            panic!("shared state already set");
        }
    }

    pub fn get() -> &'static Arc<Shared> {
        SHARED.get().expect("shared state not initialized")
    }

    /// Build a Slack message permalink from the workspace URL + channel + ts
    /// (`<base>/archives/<channel>/p<ts without dot>`). None if the workspace URL
    /// is unknown (auth.test failed).
    pub fn permalink(&self, channel: &str, ts: &str) -> Option<String> {
        let base = self.workspace_url.as_ref()?.trim_end_matches('/');
        let ts_compact = ts.replace('.', "");
        Some(format!("{base}/archives/{channel}/p{ts_compact}"))
    }

    pub fn is_self_bot(&self, bot_id: &str) -> bool {
        self.self_bot_ids.lock().expect("self_bot_ids poisoned").contains(bot_id)
    }

    /// Learn a self-post bot_id (from a chat.postMessage response) and persist
    /// it so echo suppression survives restarts.
    pub fn learn_self_bot(&self, bot_id: &str) {
        let mut set = self.self_bot_ids.lock().expect("self_bot_ids poisoned");
        if set.insert(bot_id.to_string()) {
            let _ = self.db.meta_set("self_post_bot_id", bot_id);
        }
    }

    /// Record a reaction we just emitted, so its echo can be dropped.
    pub fn record_reaction(&self, key: String) {
        self.react_ledger
            .lock()
            .expect("react_ledger poisoned")
            .insert(key, Instant::now());
    }

    /// One-shot: true if `key` matches a reaction we emitted (and not expired).
    /// Prunes expired entries and removes the matched one (each echo fires once).
    pub fn take_self_reaction(&self, key: &str) -> bool {
        let mut m = self.react_ledger.lock().expect("react_ledger poisoned");
        let now = Instant::now();
        m.retain(|_, t| now.duration_since(*t) < REACT_TTL);
        m.remove(key).is_some()
    }
}
