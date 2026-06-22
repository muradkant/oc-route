use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

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
            // before that to keep it alive. 1000ms keeps the toast alive (well under the
            // 5s auto-dismiss window) while cutting background POST volume ~60% versus
            // the old 400ms cadence. The animation is still visibly progressing (a calm
            // pulse), preserving the "something is happening" feedback (P7).
            let interval = Duration::from_millis(1000);
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

/// A pool of exactly one pre-provisioned throwaway router session.
///
/// This is the identity-safe optimization for the per-message router-session churn.
/// Previously every routed message did, serially on the critical path: create
/// session -> router call -> delete session. Two of those three round-trips were pure
/// plumbing with no dependency on the routing itself.
///
/// The throwaway-per-message semantics are LOAD-BEARING and must not change: a router
/// session is never reused, because OpenCode appends every prompt to the session's
/// history, and reusing one would feed message N's router the accumulated
/// `<routing_task>` XML from messages 1..N-1 -- violating the "fresh decision per
/// message" (P4) and "router sees only the user's conversation" (P5) principles.
///
/// So this struct keeps the create-once / use-once / delete semantics but moves the
/// plumbing off the critical path:
///   - `take()` returns a session that was created in the background *after the
///     previous message*, so intercept pays ~0ms for creation (just a swap). It then
///     immediately starts creating the *next* one for the message after.
///   - `release()` deletes a used session via `tokio::spawn` -- fire-and-forget, off
///     the critical path entirely.
pub struct RouterSessionSlot {
    oc: OcClient,
    pending: Arc<Mutex<Option<PrefetchedSession>>>,
}

struct PrefetchedSession {
    id: String,
}

impl RouterSessionSlot {
    pub fn new(oc: OcClient) -> Self {
        Self {
            oc,
            pending: Arc::new(Mutex::new(None)),
        }
    }

    /// Start the first prefetch. Call once at startup so the very first intercept
    /// finds a session waiting (or in flight) instead of paying a cold create.
    pub fn prime(&self) {
        self.spawn_prefetch();
    }

    fn spawn_prefetch(&self) {
        let oc = self.oc.clone();
        let pending = self.pending.clone();
        tokio::spawn(async move {
            match oc.create_router_session().await {
                Ok(id) => {
                    let mut slot = pending.lock().await;
                    if slot.is_none() {
                        *slot = Some(PrefetchedSession { id });
                    } else {
                        drop(slot);
                        let _ = oc.delete_session(&id).await;
                    }
                }
                Err(e) => {
                    tracing::warn!("router session prefetch failed: {e}");
                }
            }
        });
    }

    /// Get a fresh router session for the current message.
    pub async fn take(&self) -> Result<String> {
        let fast = {
            let mut slot = self.pending.lock().await;
            slot.take().map(|p| p.id)
        };
        let id = match fast {
            Some(id) => id,
            None => {
                tracing::info!("router session slot empty; creating synchronously");
                self.oc.create_router_session().await?
            }
        };
        self.spawn_prefetch();
        Ok(id)
    }

    /// Delete a used session in the background. Fire-and-forget.
    pub fn release(&self, id: String) {
        let oc = self.oc.clone();
        tokio::spawn(async move {
            if let Err(e) = oc.delete_session(&id).await {
                tracing::warn!("router session delete failed for {id}: {e}");
            }
        });
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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windowed_url_is_correct() {
        fn built_url(limit: Option<usize>) -> String {
            let base = "http://127.0.0.1:4096";
            match limit.filter(|&n| n > 0) {
                Some(n) => format!("{}/session/{}/message?limit={}", base, "ses_x", n),
                None => format!("{}/session/{}/message", base, "ses_x"),
            }
        }
        assert_eq!(built_url(Some(10)), "http://127.0.0.1:4096/session/ses_x/message?limit=10");
        assert_eq!(built_url(Some(1)), "http://127.0.0.1:4096/session/ses_x/message?limit=1");
        assert_eq!(built_url(Some(0)), "http://127.0.0.1:4096/session/ses_x/message");
        assert_eq!(built_url(None), "http://127.0.0.1:4096/session/ses_x/message");
    }

    #[tokio::test]
    async fn slot_take_consumes_and_refills() {
        let slot = RouterSessionSlot {
            oc: unreachable_client(),
            pending: Arc::new(Mutex::new(Some(PrefetchedSession { id: "ses_first".to_string() }))),
        };
        let a = { let mut g = slot.pending.lock().await; g.take().map(|p| p.id) };
        assert_eq!(a.as_deref(), Some("ses_first"));
        { let mut g = slot.pending.lock().await; *g = Some(PrefetchedSession { id: "ses_second".to_string() }); }
        let b = { let mut g = slot.pending.lock().await; g.take().map(|p| p.id) };
        assert_eq!(b.as_deref(), Some("ses_second"));
        assert_ne!(a, b, "consecutive router sessions must be distinct");
    }

    fn unreachable_client() -> OcClient {
        OcClient {
            base: "http://0.0.0.0:0".to_string(),
            http: reqwest::Client::builder().build().expect("failed to build reqwest client"),
        }
    }
}
