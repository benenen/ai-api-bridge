//! Provider quota/availability probing.
//!
//! Quota is read by a per-provider **Lua script** (`probe_script`) so the bridge
//! never hardcodes any vendor's quota API: the script drives the HTTP call (via
//! an injected `http{}` helper) and returns `{ ok, remaining, used, limit, note }`.
//! Providers without a script get a lightweight connectivity ping instead.

use std::time::Duration;

use mlua::{Lua, LuaSerdeExt};

use crate::config::{ProbeSource, Provider, WireName};
use crate::store::ProviderStatus;

/// Probe one provider, returning its fresh status. Lua probe when a script
/// source is set, otherwise a connectivity ping.
pub async fn run_probe(name: &str, provider: &Provider) -> ProviderStatus {
    let now = epoch_secs();
    let script: Option<String> = match provider.probe_source {
        ProbeSource::Path => match &provider.probe_script {
            Some(path) => match tokio::fs::read_to_string(path).await {
                Ok(s) => Some(s),
                Err(e) => return err_status(now, format!("read {path}: {e}")),
            },
            None => None,
        },
        ProbeSource::Text => provider.probe_script_text.clone(),
    };

    match script {
        Some(script) => {
            let ctx = serde_json::json!({
                "name": name,
                "base_url": provider.base_url,
                "api_key": provider.api_key,
                "extra_headers": provider.extra_headers,
                "wire": wire_str(provider.wire),
            });
            match tokio::task::spawn_blocking(move || run_lua(&script, ctx)).await {
                Ok(Ok(ret)) => ProviderStatus {
                    available: ret.ok,
                    quota_remaining: ret.remaining,
                    quota_used: ret.used,
                    quota_limit: ret.limit,
                    last_checked: Some(now),
                    last_ok: ret.ok,
                    error: None,
                    note: ret.note,
                },
                Ok(Err(e)) => err_status(now, e),
                Err(join) => err_status(now, format!("probe task failed: {join}")),
            }
        }
        None => ping(provider, now).await,
    }
}

/// The table a probe script returns.
#[derive(Debug, Default, serde::Deserialize)]
struct ProbeReturn {
    #[serde(default)]
    ok: bool,
    remaining: Option<f64>,
    used: Option<f64>,
    limit: Option<f64>,
    note: Option<String>,
}

/// Run a probe script (blocking). Injects `ctx`, `http{}`, `json_decode/encode`.
fn run_lua(script: &str, ctx: serde_json::Value) -> Result<ProbeReturn, String> {
    let lua = Lua::new();
    // Probes are operator-authored, but keep file/process access out of reach.
    lua.globals()
        .set("io", mlua::Value::Nil)
        .map_err(|e| e.to_string())?;
    lua.globals()
        .set("os", mlua::Value::Nil)
        .map_err(|e| e.to_string())?;

    inject_helpers(&lua).map_err(|e| e.to_string())?;
    let ctx_val = lua.to_value(&ctx).map_err(|e| e.to_string())?;
    lua.globals()
        .set("ctx", ctx_val)
        .map_err(|e| e.to_string())?;

    // Load the script as a function and run it; it returns the result table.
    let func = lua
        .load(script)
        .set_name("probe")
        .into_function()
        .map_err(|e| e.to_string())?;
    let value: mlua::Value = func.call(()).map_err(|e| e.to_string())?;
    lua.from_value(value).map_err(|e| e.to_string())
}

fn inject_helpers(lua: &Lua) -> mlua::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(mlua::Error::external)?;

    // http{ url=, method="GET", headers={}, body=nil } -> { status=, body= }
    let http = lua.create_function(move |lua, req: mlua::Table| {
        let url: String = req.get("url")?;
        let method: Option<String> = req.get("method")?;
        let method = method.unwrap_or_else(|| "GET".to_string());
        let m = reqwest::Method::from_bytes(method.to_uppercase().as_bytes())
            .map_err(mlua::Error::external)?;
        let mut rb = client.request(m, &url);
        let headers: Option<mlua::Table> = req.get("headers")?;
        if let Some(h) = headers {
            for pair in h.pairs::<String, String>() {
                let (k, v) = pair?;
                rb = rb.header(k, v);
            }
        }
        let body: Option<String> = req.get("body")?;
        if let Some(b) = body {
            rb = rb.body(b);
        }
        let resp = rb.send().map_err(mlua::Error::external)?;
        let status = resp.status().as_u16();
        let text = resp.text().map_err(mlua::Error::external)?;
        let out = lua.create_table()?;
        out.set("status", status)?;
        out.set("body", text)?;
        Ok(out)
    })?;
    lua.globals().set("http", http)?;

    let json_decode = lua.create_function(|lua, s: String| {
        let v: serde_json::Value = serde_json::from_str(&s).map_err(mlua::Error::external)?;
        lua.to_value(&v)
    })?;
    lua.globals().set("json_decode", json_decode)?;

    let json_encode = lua.create_function(|lua, v: mlua::Value| {
        let j: serde_json::Value = lua.from_value(v)?;
        serde_json::to_string(&j).map_err(mlua::Error::external)
    })?;
    lua.globals().set("json_encode", json_encode)?;

    Ok(())
}

/// Connectivity ping for probe-less providers: any HTTP response = reachable.
async fn ping(provider: &Provider, now: i64) -> ProviderStatus {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return err_status(now, e.to_string()),
    };
    match client.get(&provider.base_url).send().await {
        Ok(resp) => ProviderStatus {
            available: true,
            last_checked: Some(now),
            last_ok: true,
            note: Some(format!(
                "ping {} -> {}",
                provider.base_url,
                resp.status().as_u16()
            )),
            ..Default::default()
        },
        Err(e) => ProviderStatus {
            available: false,
            last_checked: Some(now),
            last_ok: false,
            error: Some(format!("ping failed: {e}")),
            ..Default::default()
        },
    }
}

fn err_status(now: i64, msg: String) -> ProviderStatus {
    ProviderStatus {
        available: false,
        last_checked: Some(now),
        last_ok: false,
        error: Some(msg),
        ..Default::default()
    }
}

fn epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn wire_str(w: WireName) -> &'static str {
    match w {
        WireName::OpenaiChat => "openai-chat",
        WireName::OpenaiResponses => "openai-responses",
        WireName::AnthropicMessages => "anthropic-messages",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Provider;
    use std::collections::HashMap;

    fn provider_with_script(path: Option<String>) -> Provider {
        Provider {
            wire: WireName::OpenaiChat,
            base_url: "https://example.invalid".into(),
            api_key: Some("sk-test".into()),
            model_prefix: None,
            max_tokens_field: "max_tokens".into(),
            extra_headers: HashMap::new(),
            probe_script: path,
            probe_script_text: None,
            probe_source: ProbeSource::Path,
            probe_enabled: None,
            probe_interval_secs: None,
            quota_min: None,
            cost_windows: Vec::new(),
            model_prices: HashMap::new(),
            usage: Vec::new(),
            models: Vec::new(),
        }
    }

    async fn write_script(body: &str) -> String {
        let path = std::env::temp_dir().join(format!("probe-test-{}.lua", uuid::Uuid::new_v4()));
        tokio::fs::write(&path, body).await.unwrap();
        path.to_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn script_quota_result_maps_to_status() {
        let path =
            write_script("return { ok = true, remaining = 42.5, used = 7, note = \"hi\" }").await;
        let p = provider_with_script(Some(path));
        let s = run_probe("go", &p).await;
        assert!(s.available);
        assert!(s.last_ok);
        assert_eq!(s.quota_remaining, Some(42.5));
        assert_eq!(s.quota_used, Some(7.0));
        assert_eq!(s.note.as_deref(), Some("hi"));
        assert!(s.error.is_none());
    }

    #[tokio::test]
    async fn script_receives_ctx() {
        let path =
            write_script("return { ok = true, note = ctx.name .. \":\" .. ctx.base_url }").await;
        let p = provider_with_script(Some(path));
        let s = run_probe("zen", &p).await;
        assert_eq!(s.note.as_deref(), Some("zen:https://example.invalid"));
    }

    #[tokio::test]
    async fn json_decode_is_available_to_scripts() {
        let path = write_script(
            "local t = json_decode('{\"credits\": 9}'); return { ok = true, remaining = t.credits }",
        )
        .await;
        let p = provider_with_script(Some(path));
        let s = run_probe("go", &p).await;
        assert_eq!(s.quota_remaining, Some(9.0));
    }

    #[tokio::test]
    async fn script_error_becomes_unavailable() {
        let path = write_script("error(\"boom\")").await;
        let p = provider_with_script(Some(path));
        let s = run_probe("go", &p).await;
        assert!(!s.available);
        assert!(!s.last_ok);
        assert!(s.error.as_deref().unwrap().contains("boom"));
    }

    #[tokio::test]
    async fn missing_script_file_is_an_error() {
        let p = provider_with_script(Some("/no/such/probe.lua".into()));
        let s = run_probe("go", &p).await;
        assert!(!s.available);
        assert!(s.error.as_deref().unwrap().contains("/no/such/probe.lua"));
    }
}
