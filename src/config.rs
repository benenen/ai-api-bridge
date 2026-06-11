//! Bridge configuration: providers (outbound targets) + model routes.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    /// SQLite database file holding providers + routes (seeded from this config on
    /// first run, then authoritative). Overridable with `--db`.
    #[serde(default = "default_database")]
    pub database: String,
    pub default_provider: Option<String>,
    pub auth_token: Option<String>,
    /// Master switch for cost/usage tracking. When `false` (the default) the bridge
    /// records no cost, writes no `usage_events`, hides `cost_windows` from
    /// `/v1/providers`, and does no usage-based failover — the record path
    /// short-circuits at zero overhead. **Reactive 429 failover is unaffected.**
    /// Sets the startup default; toggle at runtime via `POST /admin/api/usage`.
    #[serde(default)]
    pub cost_tracking: bool,
    #[serde(default)]
    pub providers: HashMap<String, Provider>,
    #[serde(default)]
    pub routes: Vec<Route>,
    /// Global safety-net target, appended as the last candidate of every resolved
    /// chain (and catching aliases that match no route when `default_provider` is
    /// unset). Like `listen`/`default_provider`, always read from the config file.
    #[serde(default)]
    pub fallback_route: Option<RouteTarget>,
}

fn default_listen() -> String {
    "127.0.0.1:8282".to_string()
}

fn default_database() -> String {
    "bridge.db".to_string()
}

/// Whether a probe script is a file path or inline text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeSource {
    #[default]
    Path,
    Text,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Provider {
    pub wire: WireName,
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub model_prefix: Option<String>,
    #[serde(default = "default_max_tokens_field")]
    pub max_tokens_field: String,
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
    /// Path to a Lua quota probe (see `probes/`). Absent = availability-ping only.
    #[serde(default)]
    pub probe_script: Option<String>,
    /// Inline Lua probe script text (used when `probe_source` is `text`).
    #[serde(default)]
    pub probe_script_text: Option<String>,
    /// Whether to use `probe_script` (file path) or `probe_script_text` (inline).
    #[serde(default)]
    pub probe_source: ProbeSource,
    /// Explicit on/off for the watcher (defaults to on when a script source is set).
    #[serde(default)]
    pub probe_enabled: Option<bool>,
    /// Seconds between probes (default 300).
    #[serde(default)]
    pub probe_interval_secs: Option<u64>,
    /// Below this `quota_remaining` the provider counts as exhausted (for failover).
    #[serde(default)]
    pub quota_min: Option<f64>,
    /// Rolling cost windows (e.g. Zen Go: $12/5h, $30/7d, $60/30d). Empty = the
    /// provider is not cost-tracked. Spend is accumulated from each completion's
    /// `cost` field; a window with `remaining <= 0` makes the provider fail over.
    #[serde(default)]
    pub cost_windows: Vec<CostWindow>,
    /// Per-model pay-as-you-go prices, keyed by the upstream model name. Used to
    /// estimate spend (tokens × price) when the upstream reports `cost = $0` — as
    /// subscription plans like Zen Go do. Real non-zero `cost` always wins.
    /// DEPRECATED: folded into a `Billing` [`UsageSpec`] at load (see `normalize_usage`).
    #[serde(default)]
    pub model_prices: HashMap<String, ModelPrice>,
    /// Typed usage specs (billing $, request count, token count …), each a set of
    /// rolling limit windows. Empty = the provider is not usage-tracked. Legacy
    /// `cost_windows`/`model_prices` are folded into a `Billing` spec at load.
    #[serde(default)]
    pub usage: Vec<UsageSpec>,
    /// Supported upstream model names (for autocomplete in the admin UI).
    #[serde(default)]
    pub models: Vec<String>,
}

/// Pay-as-you-go price for one model, in dollars per 1M tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// Input (prompt) price per 1M tokens.
    pub input: f64,
    /// Output (completion) price per 1M tokens.
    pub output: f64,
}

impl ModelPrice {
    /// Estimated dollar cost of a request from its token counts.
    pub fn estimate(&self, prompt_tokens: u64, completion_tokens: u64) -> f64 {
        (prompt_tokens as f64 / 1e6) * self.input + (completion_tokens as f64 / 1e6) * self.output
    }
}

/// One rolling cost window: spend over the last `window_secs` is capped at `limit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostWindow {
    /// Display label, e.g. "5h" / "7d" / "30d".
    pub label: String,
    /// Window length in seconds (5h = 18000, 7d = 604800, 30d = 2592000).
    pub window_secs: u64,
    /// Spend cap for the window (dollars).
    pub limit: f64,
}

/// The "currency" a provider is metered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageKind {
    /// Dollars (real `cost` or token × price estimate).
    Billing,
    /// Request count (1 per request).
    Count,
    /// Token count (prompt + completion).
    Token,
}

impl UsageKind {
    /// The display unit for this kind.
    pub fn unit(self) -> &'static str {
        match self {
            UsageKind::Billing => "usd",
            UsageKind::Count => "requests",
            UsageKind::Token => "tokens",
        }
    }

    /// The persisted/exposed tag for this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            UsageKind::Billing => "billing",
            UsageKind::Count => "count",
            UsageKind::Token => "token",
        }
    }

    pub fn parse(s: &str) -> Option<UsageKind> {
        match s {
            "billing" => Some(UsageKind::Billing),
            "count" => Some(UsageKind::Count),
            "token" => Some(UsageKind::Token),
            _ => None,
        }
    }
}

/// One rolling limit window for any usage kind. `limit` is in the kind's unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub label: String,
    pub window_secs: u64,
    pub limit: f64,
}

/// A typed usage spec: a usage kind plus its rolling windows (and, for billing, the
/// per-model prices used to estimate spend). Serializes as `{usage_type, …data}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "usage_type", rename_all = "snake_case")]
pub enum UsageSpec {
    Billing {
        #[serde(default)]
        windows: Vec<UsageWindow>,
        #[serde(default)]
        model_prices: HashMap<String, ModelPrice>,
    },
    Count {
        #[serde(default)]
        windows: Vec<UsageWindow>,
    },
    Token {
        #[serde(default)]
        windows: Vec<UsageWindow>,
    },
}

impl UsageSpec {
    pub fn kind(&self) -> UsageKind {
        match self {
            UsageSpec::Billing { .. } => UsageKind::Billing,
            UsageSpec::Count { .. } => UsageKind::Count,
            UsageSpec::Token { .. } => UsageKind::Token,
        }
    }

    pub fn windows(&self) -> &[UsageWindow] {
        match self {
            UsageSpec::Billing { windows, .. }
            | UsageSpec::Count { windows }
            | UsageSpec::Token { windows } => windows,
        }
    }

    /// Per-model prices (billing specs only) for the token × price estimate.
    pub fn model_prices(&self) -> Option<&HashMap<String, ModelPrice>> {
        match self {
            UsageSpec::Billing { model_prices, .. } => Some(model_prices),
            _ => None,
        }
    }
}

fn default_max_tokens_field() -> String {
    "max_tokens".to_string()
}

impl Provider {
    /// Whether the watcher should run for this provider.
    pub fn probe_enabled(&self) -> bool {
        self.probe_enabled.unwrap_or(match self.probe_source {
            ProbeSource::Path => self.probe_script.is_some(),
            ProbeSource::Text => self
                .probe_script_text
                .as_ref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false),
        })
    }

    /// Probe interval (default 300s).
    pub fn probe_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.probe_interval_secs.unwrap_or(300).max(1))
    }

    /// Fold legacy `cost_windows`/`model_prices` into a `Billing` [`UsageSpec`] when
    /// `usage` is empty, so old configs/DB rows keep working. Idempotent.
    pub fn normalize_usage(&mut self) {
        if self.usage.is_empty() && (!self.cost_windows.is_empty() || !self.model_prices.is_empty())
        {
            let windows = self
                .cost_windows
                .iter()
                .map(|w| UsageWindow {
                    label: w.label.clone(),
                    window_secs: w.window_secs,
                    limit: w.limit,
                })
                .collect();
            self.usage = vec![UsageSpec::Billing {
                windows,
                model_prices: self.model_prices.clone(),
            }];
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WireName {
    OpenaiChat,
    OpenaiResponses,
    AnthropicMessages,
}

impl WireName {
    /// The kebab-case wire string used in config, the DB, and the admin API.
    pub fn as_str(self) -> &'static str {
        match self {
            WireName::OpenaiChat => "openai-chat",
            WireName::OpenaiResponses => "openai-responses",
            WireName::AnthropicMessages => "anthropic-messages",
        }
    }

    /// Parse the kebab-case wire string (the inverse of [`WireName::as_str`]).
    pub fn parse(s: &str) -> Option<WireName> {
        match s {
            "openai-chat" => Some(WireName::OpenaiChat),
            "openai-responses" => Some(WireName::OpenaiResponses),
            "anthropic-messages" => Some(WireName::AnthropicMessages),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Route {
    pub alias: String,
    pub provider: String,
    pub model: String,
    /// Ordered fallback targets tried (in order) when the primary provider is
    /// unavailable or quota-exhausted.
    #[serde(default)]
    pub fallback: Vec<RouteTarget>,
}

/// A (provider, model) pair — the primary of a route or one of its fallbacks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteTarget {
    pub provider: String,
    pub model: String,
}

impl Config {
    pub fn from_toml(text: &str) -> anyhow::Result<Config> {
        let mut cfg: Config = toml::from_str(text)?;
        for p in cfg.providers.values_mut() {
            p.normalize_usage();
        }
        Ok(cfg)
    }

    pub fn load(path: &std::path::Path) -> anyhow::Result<Config> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading config {}: {e}", path.display()))?;
        Config::from_toml(&text)
    }

    /// Overlay `BRIDGE_PROVIDERS_<NAME>_API_KEY` env vars onto provider keys.
    /// Applied after the DB load so env-only secrets never get persisted.
    pub fn apply_env_overrides(&mut self) {
        for (name, provider) in self.providers.iter_mut() {
            let key = format!("BRIDGE_PROVIDERS_{}_API_KEY", name.to_uppercase());
            if let Ok(val) = std::env::var(&key) {
                provider.api_key = Some(val);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_and_route() {
        let toml = r#"
listen = "127.0.0.1:9000"
default_provider = "zen"

[providers.zen]
wire = "openai-chat"
base_url = "https://opencode.ai/zen/v1"
model_prefix = "opencode/"

[[routes]]
alias = "gpt-5.5"
provider = "zen"
model = "opencode/gpt-5.5"
"#;
        let cfg = Config::from_toml(toml).unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:9000");
        assert_eq!(cfg.default_provider.as_deref(), Some("zen"));
        let zen = cfg.providers.get("zen").unwrap();
        assert_eq!(zen.wire, WireName::OpenaiChat);
        assert_eq!(zen.model_prefix.as_deref(), Some("opencode/"));
        assert_eq!(zen.max_tokens_field, "max_tokens"); // default
        assert_eq!(cfg.routes.len(), 1);
        assert_eq!(cfg.routes[0].model, "opencode/gpt-5.5");
    }

    #[test]
    fn listen_defaults() {
        let cfg = Config::from_toml("").unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:8282");
        assert!(cfg.fallback_route.is_none());
    }

    #[test]
    fn parses_fallback_route() {
        let cfg = Config::from_toml(
            r#"
fallback_route = { provider = "zen", model = "gpt-5.5-mini" }
[providers.zen]
wire = "openai-chat"
base_url = "https://opencode.ai/zen/v1"
"#,
        )
        .unwrap();
        let fb = cfg.fallback_route.unwrap();
        assert_eq!(fb.provider, "zen");
        assert_eq!(fb.model, "gpt-5.5-mini");
    }
}
