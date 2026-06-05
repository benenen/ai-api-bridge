//! Bridge configuration: providers (outbound targets) + model routes.

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    pub default_provider: Option<String>,
    pub auth_token: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, Provider>,
    #[serde(default)]
    pub routes: Vec<Route>,
}

fn default_listen() -> String {
    "127.0.0.1:8282".to_string()
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
}

fn default_max_tokens_field() -> String {
    "max_tokens".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WireName {
    OpenaiChat,
    OpenaiResponses,
    AnthropicMessages,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Route {
    pub alias: String,
    pub provider: String,
    pub model: String,
}

impl Config {
    pub fn from_toml(text: &str) -> anyhow::Result<Config> {
        let mut cfg: Config = toml::from_str(text)?;
        cfg.apply_env_overrides();
        Ok(cfg)
    }

    pub fn load(path: &std::path::Path) -> anyhow::Result<Config> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading config {}: {e}", path.display()))?;
        Config::from_toml(&text)
    }

    fn apply_env_overrides(&mut self) {
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
    }
}
