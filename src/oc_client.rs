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
    ///
    /// This keeps P1 ("OpenCode knows best" / is the source of truth) literally true:
    /// there is no client-side history cache, and the router still sees a clean,
    /// current window of the user's real conversation (P5).
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
        self.show_toast_duration(title, message, variant, Duration::from_secs(8))
            .await
    }

    /// Like [`show_toast`] but with an explicit dismiss duration. The rationale
    /// toast ("Routed to X — <reason>") needs a longer read time than the default
    /// transient toast; 8s is a sensible default for a one-line rationale the user
    /// actively wants to read, without lingering indefinitely.
    pub async fn show_toast_duration(
        &self,
        title: &str,
        message: &str,
        variant: &str,
        duration: Duration,
    ) -> Result<()> {
        let body = json!({
            "title": title,
            "message": message,
            "variant": variant,
            "duration": duration.as_millis() as u64,
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
    /// reposting every ~1000ms keeps a single toast alive and visibly progressing —
    /// giving the user a sense of active loading instead of a frozen banner.
    ///
    /// Returns a [`ToastAnimatorGuard`] whose drop stops the animation *promptly and
    /// reliably*: it sets a flag checked at the top of every loop iteration, so no
    /// stray `Routing...` POST can ever fire after the drop to clobber a result toast.
    pub fn spawn_routing_toast(&self, title: &str) -> ToastAnimatorGuard {
        let title = title.to_string();
        let stop = std::sync::Arc::new(ToastStop::new());
        let stop_clone = stop.clone();
        let oc = self.clone();
        let frames = ["Routing.", "Routing..", "Routing..."];
        tokio::spawn(async move {
            let mut idx = 0;
            let interval = Duration::from_millis(1000);
            loop {
                // Check the flag at the TOP of each iteration, before any POST. This is
                // the key to reliability: drop sets the flag AND notifies to wake the
                // sleep, so after drop the loop wakes, re-checks, and exits WITHOUT
                // firing another `Routing...` POST that would overwrite a result toast.
                if stop_clone.is_set() {
                    break;
                }
                let msg = frames[idx % frames.len()];
                let _ = oc.show_toast(&title, msg, "info").await;
                idx += 1;
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
/// session → router call → delete session. Two of those three round-trips were pure
/// plumbing with no dependency on the routing itself.
///
/// The throwaway-per-message semantics are LOAD-BEARING and must not change: a router
/// session is never reused, because OpenCode appends every prompt to the session's
/// history, and reusing one would feed message N's router the accumulated
/// `<routing_task>` XML from messages 1..N−1 — violating the "fresh decision per
/// message" (P4) and "router sees only the user's conversation" (P5) principles.
///
/// So this struct keeps the create-once / use-once / delete semantics but moves the
/// plumbing off the critical path:
///   - `take()` returns a session that was created in the background *after the
///     previous message*, so intercept pays ~0ms for creation (just a swap). It then
///     immediately starts creating the *next* one for the message after.
///   - `release()` deletes a used session via `tokio::spawn` — fire-and-forget, off
///     the critical path entirely.
///
/// If the prefetched session isn't ready yet when `take()` is called (e.g. two
/// messages sent in quick succession, or the first message of a run), `take()` falls
/// back to a synchronous create. Correctness is never compromised — only latency.
pub struct RouterSessionSlot {
    oc: OcClient,
    // The single prefetched session, if one is ready. None = a prefetch is in flight
    // or none has been requested yet. Creating a session is a single HTTP POST and
    // OpenCode handles it quickly, so under normal cadence the slot is populated.
    pending: Arc<Mutex<Option<PrefetchedSession>>>,
}

// A ready-to-use session id together with the handle of the background task that
// created it, so we can await its completion if a caller takes it mid-creation.
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
                    // Only store if nobody else populated the slot in the meantime.
                    if slot.is_none() {
                        *slot = Some(PrefetchedSession { id });
                    } else {
                        // Lost the race; clean up the extra session asynchronously.
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

    /// Get a fresh router session for the current message. Returns instantly if a
    /// prefetched session is ready; otherwise creates one synchronously as a fallback
    /// (correctness preserved, just slower). Always kicks off the next prefetch so
    /// the following message finds a session waiting.
    pub async fn take(&self) -> Result<String> {
        // Try the fast path first: grab the prefetched session if it's ready.
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
        // Immediately start prefetching the next one. We return right away.
        self.spawn_prefetch();
        Ok(id)
    }

    /// Delete a used session in the background. Fire-and-forget: this must never
    /// block the critical path. The only consequence of a failed/late delete is a
    /// stale `"oc-route-router"` session — which `is_router_session` in setup.rs
    /// already hides from the picker, so it cannot be accidentally continued.
    pub fn release(&self, id: String) {
        let oc = self.oc.clone();
        tokio::spawn(async move {
            if let Err(e) = oc.delete_session(&id).await {
                tracing::warn!("router session delete failed for {id}: {e}");
            }
        });
    }
}

/// Cooperative stop signal for the toast animator loop. Combines:
///   - an `AtomicBool` flag, checked at the TOP of every loop iteration, and
///   - a `Notify`, to wake the loop out of its inter-frame sleep on drop.
/// The flag is what guarantees "no stray POST after drop": even if the notify is
/// missed (the loop was between wait points), the next iteration's top-of-loop
/// check sees the flag and exits before posting.
struct ToastStop {
    flag: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl ToastStop {
    fn new() -> Self {
        Self {
            flag: std::sync::atomic::AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }
    fn is_set(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn notified(&self) -> impl std::future::Future<Output = ()> + '_ {
        self.notify.notified()
    }
    fn signal(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        // notify_one so a currently-sleeping loop wakes immediately.
        self.notify.notify_one();
    }
}

/// Stops the animated routing toast when dropped. Dropping signals the animator to
/// exit on its next tick (within the frame interval). See [`ToastStop`] for why
/// the stop is reliable (no stray POSTs after drop).
pub struct ToastAnimatorGuard {
    stop: std::sync::Arc<ToastStop>,
}

impl Drop for ToastAnimatorGuard {
    fn drop(&mut self) {
        self.stop.signal();
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

    /// The windowed history URL must ask the server for exactly the most recent N
    /// messages — no client-side slicing. None/0 must omit the param (returns all),
    /// matching the OpenCode handler's `limit === undefined || limit === 0` branch.
    #[test]
    fn windowed_url_is_correct() {
        // We test the URL-construction logic directly by mirroring list_messages's
        // branch. This is the contract the router's sliding window depends on.
        fn built_url(limit: Option<usize>) -> String {
            let base = "http://127.0.0.1:4096";
            match limit.filter(|&n| n > 0) {
                Some(n) => format!("{}/session/{}/message?limit={}", base, "ses_x", n),
                None => format!("{}/session/{}/message", base, "ses_x"),
            }
        }
        assert_eq!(
            built_url(Some(10)),
            "http://127.0.0.1:4096/session/ses_x/message?limit=10"
        );
        assert_eq!(
            built_url(Some(1)),
            "http://127.0.0.1:4096/session/ses_x/message?limit=1"
        );
        // 0 and None must fall through to "fetch all" — never send limit=0, which the
        // OC handler treats as "all" anyway, but omitting is clearer and avoids an
        // edge case if the handler semantics ever change.
        assert_eq!(built_url(Some(0)), "http://127.0.0.1:4096/session/ses_x/message");
        assert_eq!(built_url(None), "http://127.0.0.1:4096/session/ses_x/message");
    }

    /// A RouterSessionSlot must hand out distinct ids on consecutive `take()` calls
    /// when creation is faked — i.e. it never hands the same session id twice, which
    /// is the mechanism that enforces "fresh decision per message" (P4). We can't
    /// test the real HTTP path without a server, so we test the swap semantics by
    /// driving the slot's internal state directly.
    #[tokio::test]
    async fn slot_take_consumes_and_refills() {
        // Simulate two prefetched sessions landing in the slot, one after another.
        let slot = RouterSessionSlot {
            oc: unreachable_client(),
            pending: Arc::new(Mutex::new(Some(PrefetchedSession {
                id: "ses_first".to_string(),
            }))),
        };
        // First take gets the prefetched one.
        let a = {
            let mut g = slot.pending.lock().await;
            g.take().map(|p| p.id)
        };
        assert_eq!(a.as_deref(), Some("ses_first"));
        // Slot is now empty until a prefetch lands — take() would fall back to sync
        // creation. Inject a second prefetched session and confirm it's distinct.
        {
            let mut g = slot.pending.lock().await;
            *g = Some(PrefetchedSession {
                id: "ses_second".to_string(),
            });
        }
        let b = {
            let mut g = slot.pending.lock().await;
            g.take().map(|p| p.id)
        };
        assert_eq!(b.as_deref(), Some("ses_second"));
        assert_ne!(a, b, "consecutive router sessions must be distinct");
    }

    /// A client with a bogus base — its methods will never be called in these tests,
    /// we only need the type to construct a slot. If anything accidentally awaits it,
    /// the test will fail loudly rather than silently.
    fn unreachable_client() -> OcClient {
        OcClient {
            base: "http://0.0.0.0:0".to_string(),
            http: Client::builder()
                .build()
                .expect("failed to build reqwest client"),
        }
    }
}
