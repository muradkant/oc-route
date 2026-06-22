use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct OcClient {
    base: String,
    http: Client,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub time: Option<SessionTime>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SessionTime {
    #[serde(default)]
    pub created: Option<u64>,
    #[serde(default)]
    pub updated: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(default)]
    pub name: Option<String>,
}

impl OcClient {
    pub fn new(host: &str, port: u16) -> Self {
        let base = format!("http://{}:{}", host, port);
        let http = Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .expect("failed to build reqwest client");
        Self { base, http }
    }

    pub fn new_from_url(url: &str) -> Self {
        let base = url.trim_end_matches('/').to_string();
        let http = Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .expect("failed to build reqwest client");
        Self { base, http }
    }

    pub async fn wait_until_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if std::time::Instant::now() > deadline {
                anyhow::bail!("OpenCode server did not become ready within {:?}", timeout);
            }
            match self.http.get(format!("{}/session", self.base)).send().await {
                Ok(r) if r.status().is_success() => return Ok(()),
                _ => tokio::time::sleep(Duration::from_millis(150)).await,
            }
        }
    }

    /// List models the user can actually use.
    ///
    /// This hits `GET /config/providers`, NOT `GET /api/model`. The two return
    /// very different things:
    /// - `/api/model` returns the models.dev **catalog** (`catalog.model.available()`) —
    ///   advertising for every model that *could* be offered, including ones the
    ///   user's providers don't actually serve (e.g. `opencode/minimax-m3-free`).
    ///   Routing to one of those fails at runtime with `ProviderModelNotFoundError`.
    /// - `/config/providers` returns each configured provider's **actually-served**
    ///   models (`providers[].models`). A model here is real and runnable.
    ///
    /// So the model picker and pool must only ever offer `/config/providers` models;
    /// otherwise oc-route would happily route to phantom catalog entries.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let resp = self
            .http
            .get(format!("{}/config/providers", self.base))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("list_models: HTTP {}: {}", status, text);
        }
        let val: Value = serde_json::from_str(&text)
            .with_context(|| format!("list_models: invalid JSON: {}", text))?;

        let providers = val
            .get("providers")
            .and_then(|p| p.as_array())
            .ok_or_else(|| anyhow!("list_models: response missing 'providers' array"))?;

        let mut models = Vec::new();
        for provider in providers {
            let provider_id = provider
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let provider_models = match provider.get("models").and_then(|m| m.as_object()) {
                Some(m) => m,
                None => continue,
            };
            for (_model_key, model) in provider_models {
                let id = match model.get("id").and_then(|v| v.as_str()) {
                    Some(id) => id.to_string(),
                    None => continue,
                };
                // Prefer the providerID embedded in the model entry; fall back to the
                // enclosing provider's id. They normally match, but the entry is
                // authoritative for cross-provider mappings.
                let pid = model
                    .get("providerID")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&provider_id)
                    .to_string();
                let name = model.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
                models.push(ModelInfo {
                    id,
                    provider_id: pid,
                    name,
                });
            }
        }

        // Stable, readable ordering: by provider then model id.
        models.sort_by(|a, b| {
            a.provider_id
                .cmp(&b.provider_id)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(models)
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let resp = self.http.get(format!("{}/session", self.base)).send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("list_sessions: HTTP {}: {}", status, text);
        }
        let sessions: Vec<SessionInfo> = serde_json::from_str(&text)
            .with_context(|| format!("list_sessions: invalid JSON: {}", text))?;
        Ok(sessions)
    }

    /// Fetch the messages for a session, optionally capped to the most recent `limit`.
    ///
    /// The `?limit=N` query is honored server-side by `MessageV2.page` (verified in
    /// OpenCode source: `session/message-v2.ts` — `orderBy(desc(time_created))`, take
    /// newest N, then `.reverse()`). It returns the **most recent N messages in
    /// chronological order** — i.e. exactly the sliding window the router wants, so
    /// the proxy can ask the source of truth (OpenCode) to do the windowing instead
    /// of fetching the whole history every message. `limit=None` or `0` returns all.
    pub async fn list_messages(
        &self,
        session_id: &str,
        limit: Option<usize>,
    ) -> Result<Vec<Value>> {
        let url = match limit.filter(|&n| n > 0) {
            Some(n) => format!("{}/session/{}/message?limit={}", self.base, session_id, n),
            None => format!("{}/session/{}/message", self.base, session_id),
        };
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("list_messages: HTTP {}: {}", status, text);
        }
        let msgs: Vec<Value> = serde_json::from_str(&text)
            .with_context(|| format!("list_messages: invalid JSON: {}", text))?;
        Ok(msgs)
    }

    pub async fn create_router_session(&self) -> Result<String> {
        let body = json!({ "title": "oc-route-router" });
        let resp = self
            .http
            .post(format!("{}/session", self.base))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("create_router_session: HTTP {}: {}", status, text);
        }
        let val: Value = serde_json::from_str(&text)
            .with_context(|| format!("create_router_session: invalid JSON: {}", text))?;
        let id = val
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("create_router_session: response missing 'id': {}", text))?
            .to_string();
        Ok(id)
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        let resp = self
            .http
            .delete(format!("{}/session/{}", self.base, session_id))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!("delete_session {}: HTTP {}: {}", session_id, status, text);
        }
        Ok(())
    }

    pub async fn prompt_router(
        &self,
        session_id: &str,
        router_model: &str,
        routing_xml: &str,
        timeout: Duration,
    ) -> Result<String> {
        let (provider, model) = config::split_model_id(router_model)
            .ok_or_else(|| anyhow!("invalid router_model '{}'", router_model))?;
        let body = json!({
            "parts": [{ "type": "text", "text": routing_xml }],
            "model": { "providerID": provider, "modelID": model },
            "tools": {},
        });
        let req = self
            .http
            .post(format!("{}/session/{}/message", self.base, session_id))
            .json(&body)
            .timeout(timeout);
        let resp = req.send().await.context("prompt_router: request failed")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("prompt_router: HTTP {}: {}", status, text);
        }
        let val: Value = serde_json::from_str(&text)
            .with_context(|| format!("prompt_router: invalid JSON: {}", text))?;
        let parts = val
            .get("parts")
            .and_then(|p| p.as_array())
            .ok_or_else(|| anyhow!("prompt_router: response missing 'parts'"))?;
        let mut out = String::new();
        for part in parts {
            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    out.push_str(t);
                    out.push('\n');
                }
            }
        }
        Ok(out)
    }

    pub async fn show_toast(&self, title: &str, message: &str, variant: &str) -> Result<()> {
        let body = json!({
            "title": title,
            "message": message,
            "variant": variant,
            "duration": 5000,
        });
        let resp = self
            .http
            .post(format!("{}/tui/show-toast", self.base))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!("show_toast: HTTP {}: {}", status, text);
        }
        Ok(())
    }

    /// Show an animated "Routing…" toast that cycles its dots (`.`, `..`, `...`)
    /// until the returned guard is dropped. OpenCode's toast `show()` replaces the
    /// current toast in place and resets its auto-dismiss timer on each call, so
    /// reposting every ~400ms keeps a single toast alive and visibly progressing —
    /// giving the user a sense of active loading instead of a frozen 5s banner.
    ///
    /// Returns a [`ToastAnimatorGuard`] whose drop stops the animation. Call
    /// `show_toast` (or another animator) afterward to display the result.
    pub fn spawn_routing_toast(&self, title: &str) -> ToastAnimatorGuard {
        let title = title.to_string();
        let stop = std::sync::Arc::new(tokio::sync::Notify::new());
        let stop_clone = stop.clone();
        let oc = self.clone();
        let frames = ["Routing.", "Routing..", "Routing..."];
        tokio::spawn(async move {
            let mut idx = 0;
            // The toast auto-dismisses after `duration` if never refreshed; repost well
            // before that to keep it alive. ~400ms gives a calm, readable animation.
            let interval = Duration::from_millis(400);
            loop {
                let msg = frames[idx % frames.len()];
                let _ = oc.show_toast(&title, msg, "info").await;
                idx += 1;
                // Race the sleep against a stop notification so drop cancels promptly.
                tokio::select! {
                    _ = stop_clone.notified() => break,
                    _ = tokio::time::sleep(interval) => {}
                }
            }
        });
        ToastAnimatorGuard { stop }
    }
}

/// Stops the animated routing toast when dropped. Dropping signals the background
/// task to exit on its next tick (within the frame interval).
pub struct ToastAnimatorGuard {
    stop: std::sync::Arc<tokio::sync::Notify>,
}

impl Drop for ToastAnimatorGuard {
    fn drop(&mut self) {
        self.stop.notify_waiters();
    }
}

mod config {
    pub fn split_model_id(model_id: &str) -> Option<(String, String)> {
        let mut iter = model_id.splitn(2, '/');
        let provider = iter.next()?.trim();
        let model = iter.next()?.trim();
        if provider.is_empty() || model.is_empty() {
            return None;
        }
        Some((provider.to_string(), model.to_string()))
    }
}
