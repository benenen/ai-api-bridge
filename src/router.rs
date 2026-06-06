//! Model alias -> ordered provider candidate chain, for proactive (watcher
//! status) + reactive (per-request error) failover.

use std::collections::{HashMap, HashSet};

use crate::config::{Config, Provider};
use crate::error::BridgeError;
use crate::store::ProviderStatus;

#[derive(Debug)]
pub struct Resolved<'a> {
    pub provider_name: String,
    pub provider: &'a Provider,
    pub upstream_model: String,
}

/// Resolve an alias to its ordered candidate chain — the route's primary plus
/// its `fallback` list (or the default provider) — reordered so watcher-usable
/// providers come first. The caller tries them in order; reactive failure
/// advances to the next. Degraded providers stay in the chain as a last resort.
pub fn resolve_candidates<'a>(
    cfg: &'a Config,
    status: &HashMap<String, ProviderStatus>,
    cost_exhausted: &HashSet<String>,
    alias: &str,
) -> Result<Vec<Resolved<'a>>, BridgeError> {
    let raw: Vec<(String, String)> =
        if let Some(route) = cfg.routes.iter().find(|r| r.alias == alias) {
            let mut v = vec![(route.provider.clone(), route.model.clone())];
            for fb in &route.fallback {
                v.push((fb.provider.clone(), fb.model.clone()));
            }
            v
        } else {
            let pname = cfg
                .default_provider
                .clone()
                .ok_or_else(|| BridgeError::UnknownModel(alias.to_string()))?;
            let provider = cfg.providers.get(&pname).ok_or_else(|| {
                BridgeError::Internal(format!("default provider {pname} not configured"))
            })?;
            let model = match &provider.model_prefix {
                Some(prefix) if !alias.contains('/') => format!("{prefix}{alias}"),
                _ => alias.to_string(),
            };
            vec![(pname, model)]
        };

    let mut usable = Vec::new();
    let mut degraded = Vec::new();
    for (pname, model) in raw {
        let Some(provider) = cfg.providers.get(&pname) else {
            continue; // drop unknown providers from the chain
        };
        let resolved = Resolved {
            provider_name: pname.clone(),
            provider,
            upstream_model: model,
        };
        // Cost-exhausted providers are demoted like any other degraded provider:
        // tried only as a last resort (so a healthy fallback wins) but not dropped.
        if is_usable(provider, status.get(&pname)) && !cost_exhausted.contains(&pname) {
            usable.push(resolved);
        } else {
            degraded.push(resolved);
        }
    }
    usable.append(&mut degraded);
    if usable.is_empty() {
        return Err(BridgeError::UnknownModel(alias.to_string()));
    }
    Ok(usable)
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
        let cands = resolve_candidates(&c, &no_status(), &HashSet::new(), "fast").unwrap();
        assert_eq!(cands[0].upstream_model, "opencode/gpt-5.5-mini");
    }

    #[test]
    fn default_provider_applies_prefix() {
        let c = cfg();
        let cands = resolve_candidates(&c, &no_status(), &HashSet::new(), "gpt-5.5").unwrap();
        assert_eq!(cands[0].provider_name, "zen");
        assert_eq!(cands[0].upstream_model, "opencode/gpt-5.5");
    }

    #[test]
    fn prefix_skipped_when_alias_already_qualified() {
        let c = cfg();
        let cands =
            resolve_candidates(&c, &no_status(), &HashSet::new(), "anthropic/claude").unwrap();
        assert_eq!(cands[0].upstream_model, "anthropic/claude");
    }

    #[test]
    fn unknown_model_without_default_errors() {
        let c = Config::from_toml("[providers.x]\nwire=\"openai-chat\"\nbase_url=\"u\"").unwrap();
        let err = resolve_candidates(&c, &no_status(), &HashSet::new(), "whatever").unwrap_err();
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
    fn healthy_primary_first_fallback_still_in_chain() {
        let mut s = HashMap::new();
        s.insert("go".to_string(), status(true, Some(10.0)));
        let c = failover_cfg();
        let cands = resolve_candidates(&c, &s, &HashSet::new(), "gpt-5.5").unwrap();
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].provider_name, "go");
        assert_eq!(cands[0].upstream_model, "deepseek-v4-pro");
        assert_eq!(cands[1].provider_name, "zen");
    }

    #[test]
    fn unavailable_primary_ordered_last() {
        let mut s = HashMap::new();
        s.insert("go".to_string(), status(false, None));
        s.insert("zen".to_string(), status(true, None));
        let c = failover_cfg();
        let cands = resolve_candidates(&c, &s, &HashSet::new(), "gpt-5.5").unwrap();
        assert_eq!(cands[0].provider_name, "zen"); // usable first
        assert_eq!(cands[1].provider_name, "go"); // degraded last (last resort)
    }

    #[test]
    fn exhausted_primary_ordered_last() {
        let mut s = HashMap::new();
        s.insert("go".to_string(), status(true, Some(0.0))); // below quota_min 1.0
        s.insert("zen".to_string(), status(true, None));
        let c = failover_cfg();
        let cands = resolve_candidates(&c, &s, &HashSet::new(), "gpt-5.5").unwrap();
        assert_eq!(cands[0].provider_name, "zen");
    }

    #[test]
    fn all_degraded_keeps_original_order() {
        let mut s = HashMap::new();
        s.insert("go".to_string(), status(false, None));
        s.insert("zen".to_string(), status(false, None));
        let c = failover_cfg();
        let cands = resolve_candidates(&c, &s, &HashSet::new(), "gpt-5.5").unwrap();
        assert_eq!(cands[0].provider_name, "go"); // primary tried first
        assert_eq!(cands[1].provider_name, "zen");
    }

    #[test]
    fn cost_exhausted_primary_ordered_last() {
        let mut s = HashMap::new();
        s.insert("go".to_string(), status(true, None));
        s.insert("zen".to_string(), status(true, None));
        let c = failover_cfg();
        let ex = HashSet::from(["go".to_string()]); // go's cost window is spent
        let cands = resolve_candidates(&c, &s, &ex, "gpt-5.5").unwrap();
        assert_eq!(cands[0].provider_name, "zen"); // healthy fallback first
        assert_eq!(cands[1].provider_name, "go"); // exhausted primary demoted
    }
}
