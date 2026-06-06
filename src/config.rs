//! Daemon configuration, read from the environment.

use std::collections::HashSet;

#[derive(Clone)]
pub struct Config {
    pub app_token: String,  // xapp- (Socket Mode connection)
    pub bot_token: String,  // xoxb- (socket auth + control DM)
    pub user_token: String, // xoxp- (act as yourself)
    pub db_path: String,
    /// HTTP API bind address `host:port`. Default loopback; set to `0.0.0.0:8799`
    /// to serve a LAN/tailnet consumer (Tailscale is the auth boundary — don't
    /// expose it beyond the tailnet).
    pub listen: String,
    /// Channel IDs to capture even without a mention (capture scope, config-driven).
    pub watch_channels: HashSet<String>,
}

/// Load `KEY=VALUE` lines from the config env file (`$SLACK_DEPUTY_CONFIG`, else
/// `$XDG_CONFIG_HOME/slack-deputy/.env`, falling back to `~/.config/...`) into the
/// process environment, without overriding vars already set. Lets `brew services`
/// run the daemon with its tokens in a file instead of an exported environment.
fn load_env_file() {
    let path = std::env::var("SLACK_DEPUTY_CONFIG").unwrap_or_else(|_| {
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.config")))
            .unwrap_or_default();
        format!("{base}/slack-deputy/.env")
    });
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let (k, v) = (k.trim(), v.trim().trim_matches('"'));
            if !k.is_empty() && std::env::var_os(k).is_none() {
                // Safe: called once at startup, before any threads are spawned.
                unsafe { std::env::set_var(k, v) };
            }
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        load_env_file();
        let req = |k: &str| std::env::var(k).map_err(|_| format!("{k}: not set"));
        let opt = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());

        let watch_channels = opt("SLACK_DEPUTY_WATCH_CHANNELS", "")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(Self {
            app_token: req("SLACK_APP_TOKEN")?,
            bot_token: req("SLACK_BOT_TOKEN")?,
            user_token: req("SLACK_USER_TOKEN")?,
            db_path: crate::db::resolve_path(),
            listen: opt("SLACK_DEPUTY_LISTEN", "127.0.0.1:8799"),
            watch_channels,
        })
    }
}
