//! Model alias -> (provider, upstream model) resolution, with quota/availability
//! failover across a route's fallback chain.

use std::collections::HashMap;

use crate::config::{Config, Provider};
use crate::error::BridgeError;
use crate::store::ProviderStatus;

#[derive(Debug)]
pub struct Resolved<'a> {
    pub provider_name: String,
    pub provider: &'a Provider,
    pub upstream_model: String,
}

pub fn resolve<'a>(
    cfg: &'a Config,
    status: &HashMap<String, ProviderStatus>,
    alias: &str,
) -> Result<Resolved<'a>, BridgeError> {
    if let Some(route) = cfg.routes.iter().find(|r| r.alias == alias) {
        let mut candidates: Vec<(&str, &str)> =
            vec![(route.provider.as_str(), route.model.as_str())];
        for fb in &route.fallback {
            candidates.push((fb.provider.as_str(), fb.model.as_str()));
        }
        return pick(cfg, status, &candidates, alias);
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
    Ok(Resolved {
        provider_name,
        provider,
        upstream_model,
    })
}

/// Pick the first usable candidate; if all are down/exhausted, use the primary.
fn pick<'a>(
    cfg: &'a Config,
    status: &HashMap<String, ProviderStatus>,
    candidates: &[(&str, &str)],
    alias: &str,
) -> Result<Resolved<'a>, BridgeError> {
    for (i, (pname, model)) in candidates.iter().enumerate() {
        if let Some(provider) = cfg.providers.get(*pname)
            && is_usable(provider, status.get(*pname))
        {
            if i > 0 {
                tracing::info!(
                    alias,
                    provider = *pname,
                    "failover: primary degraded, using fallback"
                );
            }
            return Ok(Resolved {
                provider_name: (*pname).to_string(),
                provider,
                upstream_model: (*model).to_string(),
            });
        }
    }
    // All candidates unavailable/exhausted — attempt the primary anyway.
    let (pname, model) = candidates[0];
    let provider = cfg.providers.get(pname).ok_or_else(|| {
        BridgeError::Internal(format!(
            "route '{alias}' references unknown provider {pname}"
        ))
    })?;
    tracing::warn!(
        alias,
        provider = pname,
        "all candidates unavailable/exhausted; using primary anyway"
    );
    Ok(Resolved {
        provider_name: pname.to_string(),
        provider,
        upstream_model: model.to_string(),
    })
}

/// A provider is usable unless its status says unavailable or quota-exhausted.
fn is_usable(provider: &Provider, status: Option<&ProviderStatus>) -> bool {
    match status {
        None => true, // not monitored / never probed -> assume usable
        Some(s) => {
            if !s.available {
                return false;
            }
            match (s.quota_remaining, provider.quota_min) {
                (Some(rem), Some(min)) => rem >= min,
                _ => true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn no_status() -> HashMap<String, ProviderStatus> {
        HashMap::new()
    }

    fn status(available: bool, remaining: Option<f64>) -> ProviderStatus {
        ProviderStatus {
            available,
            quota_remaining: remaining,
            ..Default::default()
        }
    }

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
        let r = resolve(&c, &no_status(), "fast").unwrap();
        assert_eq!(r.upstream_model, "opencode/gpt-5.5-mini");
    }

    #[test]
    fn default_provider_applies_prefix() {
        let c = cfg();
        let r = resolve(&c, &no_status(), "gpt-5.5").unwrap();
        assert_eq!(r.provider_name, "zen");
        assert_eq!(r.upstream_model, "opencode/gpt-5.5");
    }

    #[test]
    fn prefix_skipped_when_alias_already_qualified() {
        let c = cfg();
        let r = resolve(&c, &no_status(), "anthropic/claude").unwrap();
        assert_eq!(r.upstream_model, "anthropic/claude");
    }

    #[test]
    fn unknown_model_without_default_errors() {
        let c = Config::from_toml("[providers.x]\nwire=\"openai-chat\"\nbase_url=\"u\"").unwrap();
        let err = resolve(&c, &no_status(), "whatever").unwrap_err();
        assert!(matches!(err, BridgeError::UnknownModel(_)));
    }

    fn failover_cfg() -> Config {
        Config::from_toml(
            r#"
[providers.go]
wire = "openai-chat"
base_url = "https://go"
quota_min = 1.0
[providers.zen]
wire = "openai-chat"
base_url = "https://zen"
[[routes]]
alias = "gpt-5.5"
provider = "go"
model = "deepseek-v4-pro"
fallback = [{ provider = "zen", model = "gpt-5.5" }]
"#,
        )
        .unwrap()
    }

    #[test]
    fn primary_used_when_healthy() {
        let mut s = HashMap::new();
        s.insert("go".to_string(), status(true, Some(10.0)));
        let c = failover_cfg();
        let r = resolve(&c, &s, "gpt-5.5").unwrap();
        assert_eq!(r.provider_name, "go");
        assert_eq!(r.upstream_model, "deepseek-v4-pro");
    }

    #[test]
    fn fails_over_when_primary_unavailable() {
        let mut s = HashMap::new();
        s.insert("go".to_string(), status(false, None));
        s.insert("zen".to_string(), status(true, None));
        let c = failover_cfg();
        let r = resolve(&c, &s, "gpt-5.5").unwrap();
        assert_eq!(r.provider_name, "zen");
        assert_eq!(r.upstream_model, "gpt-5.5");
    }

    #[test]
    fn fails_over_when_primary_exhausted() {
        let mut s = HashMap::new();
        s.insert("go".to_string(), status(true, Some(0.0))); // below quota_min 1.0
        s.insert("zen".to_string(), status(true, None));
        let c = failover_cfg();
        let r = resolve(&c, &s, "gpt-5.5").unwrap();
        assert_eq!(r.provider_name, "zen");
    }

    #[test]
    fn all_degraded_falls_back_to_primary() {
        let mut s = HashMap::new();
        s.insert("go".to_string(), status(false, None));
        s.insert("zen".to_string(), status(false, None));
        let c = failover_cfg();
        let r = resolve(&c, &s, "gpt-5.5").unwrap();
        assert_eq!(r.provider_name, "go"); // attempted anyway
    }
}
