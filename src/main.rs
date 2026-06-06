//! slack-deputy — a personal Slack agent.
//!
//! One binary, broker-style. With no subcommand it runs as the daemon: Socket
//! Mode inbound → SQLite, plus an HTTP API that serves every verb (queue + Slack).
//! With a subcommand it's the CLI — a thin HTTP client the consumer drives, which
//! can run on another host (`SLACK_DEPUTY_URL`).

mod cli;
mod config;
mod confirm;
mod db;
mod inbound;
mod outbound;
mod state;

use clap::Parser;
use config::Config;
use db::Db;
use slack_morphism::prelude::*;
use state::Shared;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

fn main() {
    let cli = cli::Cli::parse();
    match cli.command {
        None => run_daemon(),
        Some(cmd) => {
            if let Err(e) = cli::run(cmd) {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }
}

fn run_daemon() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "slack_deputy=info,slack_morphism=info".into()),
        )
        .init();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(daemon_main());
}

async fn daemon_main() {
    let config = Arc::new(Config::from_env().unwrap_or_else(|e| {
        error!(kind = "startup.config", error = %e, "config error");
        std::process::exit(1);
    }));
    let db = Arc::new(Db::open(&config.db_path).expect("open sqlite"));
    info!(kind = "startup.db_open", path = %config.db_path, "sqlite opened (WAL)");

    let client = Arc::new(SlackClient::new(
        SlackClientHyperConnector::new().expect("hyper connector"),
    ));
    let bot_token = SlackApiToken::new(SlackApiTokenValue::from(config.bot_token.clone()));
    let user_token = SlackApiToken::new(SlackApiTokenValue::from(config.user_token.clone()));

    // Echo suppression self-ids: the bot's own bot_id (control-DM echoes) +
    // the learned user-token-post bot_id (persisted across restarts).
    let mut self_bot_ids = HashSet::new();
    match client.open_session(&bot_token).auth_test().await {
        Ok(auth) => {
            if let Some(b) = &auth.bot_id {
                self_bot_ids.insert(b.to_string());
            }
            info!(
                kind = "startup.auth_bot",
                bot_user = %auth.user_id,
                bot_id = auth.bot_id.as_ref().map(|b| b.to_string()).unwrap_or_default(),
                team = %auth.team,
                "bot auth ok"
            );
        }
        Err(e) => {
            error!(kind = "startup.auth_bot_failed", error = %format!("{e:?}"), "bot auth failed")
        }
    }
    if let Ok(Some(b)) = db.meta_get("self_post_bot_id") {
        self_bot_ids.insert(b);
    }

    // Human user id (mention / own-post detection) + workspace URL (for building
    // message permalinks at capture) — both from the *user* token auth.test.
    let (my_user_id, workspace_url) = match client.open_session(&user_token).auth_test().await {
        Ok(auth) => {
            let url = auth.url.0.to_string();
            info!(kind = "startup.auth_user", user_id = %auth.user_id, url = %url, "user auth ok");
            (Some(auth.user_id.to_string()), Some(url))
        }
        Err(e) => {
            error!(kind = "startup.auth_user_failed", error = %format!("{e:?}"), "user auth failed");
            (None, None)
        }
    };

    // Bot-DM channel (control channel), for recognizing free-text answers to asks.
    let bot_dm_channel = match &my_user_id {
        Some(me) => {
            let req = SlackApiConversationsOpenRequest::new()
                .with_users(vec![SlackUserId::from(me.clone())]);
            match client
                .open_session(&bot_token)
                .conversations_open(&req)
                .await
            {
                Ok(r) => Some(r.channel.id.to_string()),
                Err(e) => {
                    error!(kind = "startup.bot_dm_failed", error = %format!("{e:?}"), "open bot DM failed");
                    None
                }
            }
        }
        None => None,
    };

    Shared {
        db,
        config: config.clone(),
        slack: client.clone(),
        my_user_id,
        workspace_url,
        bot_dm_channel,
        self_bot_ids: Mutex::new(self_bot_ids),
        react_ledger: Mutex::new(std::collections::HashMap::new()),
    }
    .install();

    tokio::spawn(outbound::run_http_server());

    let callbacks = SlackSocketModeListenerCallbacks::new()
        .with_push_events(inbound::handle_push_event)
        .with_interaction_events(confirm::handle_interaction);
    let env = Arc::new(
        SlackClientEventsListenerEnvironment::new(client.clone())
            .with_error_handler(inbound::on_error),
    );
    let listener =
        SlackClientSocketModeListener::new(&SlackClientSocketModeConfig::new(), env, callbacks);
    let app_token = SlackApiToken::new(SlackApiTokenValue::from(config.app_token.clone()));

    info!(
        kind = "startup.connecting",
        watch_channels = config.watch_channels.len(),
        "connecting via socket mode"
    );
    listener
        .listen_for(&app_token)
        .await
        .expect("listen_for failed");
    listener.serve().await;
}
