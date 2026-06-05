//! Model alias -> (provider, upstream model) resolution.

use crate::config::{Config, Provider};
use crate::error::BridgeError;

#[derive(Debug)]
pub struct Resolved<'a> {
    pub provider_name: String,
    pub provider: &'a Provider,
    pub upstream_model: String,
}

pub fn resolve<'a>(cfg: &'a Config, alias: &str) -> Result<Resolved<'a>, BridgeError> {
    if let Some(route) = cfg.routes.iter().find(|r| r.alias == alias) {
        let provider = cfg.providers.get(&route.provider).ok_or_else(|| {
            BridgeError::Internal(format!("route references unknown provider {}", route.provider))
        })?;
        return Ok(Resolved {
            provider_name: route.provider.clone(),
            provider,
            upstream_model: route.model.clone(),
        });
    }

    let provider_name = cfg
        .default_provider
        .clone()
        .ok_or_else(|| BridgeError::UnknownModel(alias.to_string()))?;
    let provider = cfg.providers.get(&provider_name).ok_or_else(|| {
        BridgeError::Internal(format!("default provider {provider_name} not configured"))
    })?;
    let upstream_model = match &provider.model_prefix {
        Some(prefix) if !alias.contains('/') => format!("{prefix}{alias}"),
        _ => alias.to_string(),
    };
    Ok(Resolved { provider_name, provider, upstream_model })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg() -> Config {
        Config::from_toml(
            r#"
default_provider = "zen"
[providers.zen]
wire = "openai-chat"
base_url = "https://opencode.ai/zen/v1"
model_prefix = "opencode/"
[[routes]]
alias = "fast"
provider = "zen"
model = "opencode/gpt-5.5-mini"
"#,
        )
        .unwrap()
    }

    #[test]
    fn explicit_route_wins() {
        let c = cfg();
        let r = resolve(&c, "fast").unwrap();
        assert_eq!(r.upstream_model, "opencode/gpt-5.5-mini");
    }

    #[test]
    fn default_provider_applies_prefix() {
        let c = cfg();
        let r = resolve(&c, "gpt-5.5").unwrap();
        assert_eq!(r.provider_name, "zen");
        assert_eq!(r.upstream_model, "opencode/gpt-5.5");
    }

    #[test]
    fn prefix_skipped_when_alias_already_qualified() {
        let c = cfg();
        let r = resolve(&c, "anthropic/claude").unwrap();
        assert_eq!(r.upstream_model, "anthropic/claude");
    }

    #[test]
    fn unknown_model_without_default_errors() {
        let c = Config::from_toml("[providers.x]\nwire=\"openai-chat\"\nbase_url=\"u\"").unwrap();
        let err = resolve(&c, "whatever").unwrap_err();
        assert!(matches!(err, BridgeError::UnknownModel(_)));
    }
}
