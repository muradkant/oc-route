use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

const MIN_OPENCODE_VERSION: &str = "1.17.7";

#[derive(Clone)]
pub struct OcClient {
    base: String,
    http: Client,
    directory: Option<String>,
    credentials: Option<(String, String)>,
}

#[derive(Deserialize)]
struct Health {
    healthy: bool,
    version: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub time: Option<SessionTime>,
    #[serde(default)]
    pub metadata: Option<Value>,
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

#[derive(Debug)]
pub struct RouterPromptError {
    message: String,
    retryable: bool,
}

impl RouterPromptError {
    fn new(message: String, retryable: bool) -> Self {
        Self { message, retryable }
    }

    pub fn retryable(&self) -> bool {
        self.retryable
    }
}

impl std::fmt::Display for RouterPromptError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RouterPromptError {}

impl OcClient {
    pub fn new(host: &str, port: u16) -> Self {
        let base = format!("http://{}:{}", host, port);
        Self::new_from_url(&base)
    }

    pub fn new_from_url(url: &str) -> Self {
        let base = url.trim_end_matches('/').to_string();
        let http = Client::builder()
            .timeout(Duration::from_secs(180))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build reqwest client");
        Self {
            base,
            http,
            directory: None,
            credentials: None,
        }
    }

    pub fn with_context(
        mut self,
        directory: Option<String>,
        username: Option<String>,
        password: Option<String>,
    ) -> Self {
        self.directory = directory;
        self.credentials =
            password.map(|password| (username.unwrap_or_else(|| "opencode".to_string()), password));
        self
    }

    pub fn with_environment(self, directory: Option<String>) -> Self {
        self.with_context(
            directory,
            std::env::var("OPENCODE_SERVER_USERNAME").ok(),
            std::env::var("OPENCODE_SERVER_PASSWORD").ok(),
        )
    }

    fn request(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let builder = match &self.directory {
            Some(directory) => builder.header(
                "x-opencode-directory",
                utf8_percent_encode(directory, NON_ALPHANUMERIC).to_string(),
            ),
            None => builder,
        };
        match &self.credentials {
            Some((username, password)) => builder.basic_auth(username, Some(password)),
            None => builder,
        }
    }

    pub fn add_context_headers(&self, headers: &mut axum::http::HeaderMap) {
        if !headers.contains_key("x-opencode-directory") {
            if let Some(directory) = &self.directory {
                if let Ok(value) = axum::http::HeaderValue::from_str(
                    &utf8_percent_encode(directory, NON_ALPHANUMERIC).to_string(),
                ) {
                    headers.insert("x-opencode-directory", value);
                }
            }
        }
        if !headers.contains_key("authorization") {
            if let Some((username, password)) = &self.credentials {
                let encoded = BASE64.encode(format!("{username}:{password}"));
                if let Ok(value) = axum::http::HeaderValue::from_str(&format!("Basic {encoded}")) {
                    headers.insert("authorization", value);
                }
            }
        }
    }

    pub async fn wait_until_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if std::time::Instant::now() > deadline {
                anyhow::bail!("OpenCode server did not become ready within {:?}", timeout);
            }
            let request = self
                .request(self.http.get(format!("{}/global/health", self.base)))
                .timeout(Duration::from_secs(2));
            match request.send().await {
                Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
                    anyhow::bail!("OpenCode rejected authentication")
                }
                Ok(response) if response.status().is_success() => {
                    let health: Health = response
                        .json()
                        .await
                        .context("OpenCode health response was invalid")?;
                    if !health.healthy {
                        anyhow::bail!("OpenCode reported an unhealthy server")
                    }
                    let installed = semver::Version::parse(health.version.trim_start_matches('v'))
                        .with_context(|| {
                            format!("OpenCode reported invalid version '{}'", health.version)
                        })?;
                    let minimum = semver::Version::parse(MIN_OPENCODE_VERSION).unwrap();
                    if installed < minimum {
                        anyhow::bail!(
                            "OpenCode {} is unsupported; {} or newer is required",
                            installed,
                            minimum
                        )
                    }
                    return Ok(());
                }
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
            .request(self.http.get(format!("{}/config/providers", self.base)))
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
                let name = model
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
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
        let resp = self
            .request(self.http.get(format!("{}/session", self.base)))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("list_sessions: HTTP {}: {}", status, text);
        }
        let sessions: Vec<SessionInfo> = serde_json::from_str(&text)
            .with_context(|| format!("list_sessions: invalid JSON: {}", text))?;
        Ok(sessions)
    }

    /// Confirm that OpenCode accepted the exact private protocol agent supplied by
    /// the process launcher. A matching name alone is not sufficient.
    pub async fn has_router_agent(&self) -> Result<bool> {
        let resp = self
            .request(self.http.get(format!("{}/agent", self.base)))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("list_agents: HTTP {}: {}", status, text);
        }
        let agents: Vec<Value> = serde_json::from_str(&text)
            .with_context(|| format!("list_agents: invalid JSON: {text}"))?;
        Ok(agents.iter().any(|agent| {
            agent.get("name").and_then(Value::as_str) == Some("oc-route")
                && agent.get("prompt").and_then(Value::as_str)
                    == Some(crate::router::ROUTER_SYSTEM_PROMPT)
        }))
    }

    pub async fn verify_router_agent(&self) -> Result<()> {
        if self.has_router_agent().await? {
            Ok(())
        } else {
            anyhow::bail!("OpenCode did not register oc-route's dedicated protocol agent")
        }
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
    /// This keeps OpenCode as the source of truth: there is no client-side history
    /// cache, and the router sees a clean, current window of the real conversation.
    pub async fn list_messages(
        &self,
        session_id: &str,
        limit: Option<usize>,
    ) -> Result<Vec<Value>> {
        let url = match limit.filter(|&n| n > 0) {
            Some(n) => format!("{}/session/{}/message?limit={}", self.base, session_id, n),
            None => format!("{}/session/{}/message", self.base, session_id),
        };
        let resp = self.request(self.http.get(url)).send().await?;
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
        let body = json!({
            "title": "oc-route-router",
            "metadata": { "oc-route.internal": true }
        });
        let resp = self
            .request(self.http.post(format!("{}/session", self.base)))
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
            .request(
                self.http
                    .delete(format!("{}/session/{}", self.base, session_id)),
            )
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("delete_session {}: HTTP {}: {}", session_id, status, text);
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
            "agent": "oc-route",
            // OpenCode applies this only when the model exposes a `none` variant;
            // otherwise it is a no-op. Routing needs a short JSON decision, not
            // hidden chain-of-thought latency.
            "variant": "none",
            "tools": { "*": false },
        });
        let req = self
            .request(
                self.http
                    .post(format!("{}/session/{}/message", self.base, session_id)),
            )
            .json(&body)
            .timeout(timeout);
        let resp = req.send().await.context("prompt_router: request failed")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let retryable = status.is_server_error()
                || status == reqwest::StatusCode::REQUEST_TIMEOUT
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
            return Err(RouterPromptError::new(
                format!("prompt_router: HTTP {status}: {text}"),
                retryable,
            )
            .into());
        }
        let val: Value = serde_json::from_str(&text)
            .with_context(|| format!("prompt_router: invalid JSON: {}", text))?;
        if let Some(error) = val
            .get("info")
            .and_then(|info| info.get("error"))
            .filter(|error| !error.is_null())
        {
            let retryable = error
                .get("data")
                .and_then(|data| data.get("isRetryable"))
                .and_then(Value::as_bool)
                .unwrap_or(true);
            return Err(RouterPromptError::new(
                format!("prompt_router: OpenCode reported an assistant error: {error}"),
                retryable,
            )
            .into());
        }
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
        if out.trim().is_empty() {
            anyhow::bail!("prompt_router: OpenCode returned no router text");
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
            .request(self.http.post(format!("{}/tui/show-toast", self.base)))
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
        let task = tokio::spawn(async move {
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
        ToastAnimatorGuard { stop, task }
    }
}

/// Owns every short-lived router session until OpenCode confirms its deletion.
///
/// Creating a session locally costs only a few milliseconds compared with model
/// inference. Creating it on demand avoids a permanently prefetched session and
/// makes shutdown and cancellation cleanup deterministic.
#[derive(Clone)]
pub struct RouterSessions {
    oc: OcClient,
    active: Arc<Mutex<HashSet<String>>>,
}

impl RouterSessions {
    pub fn new(oc: OcClient) -> Self {
        Self {
            oc,
            active: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub async fn acquire(&self) -> Result<RouterSessionLease> {
        let id = self.oc.create_router_session().await?;
        self.active.lock().unwrap().insert(id.clone());
        Ok(RouterSessionLease {
            owner: self.clone(),
            id: Some(id),
        })
    }

    async fn cleanup(&self, id: String) {
        for attempt in 1..=3 {
            match self.oc.delete_session(&id).await {
                Ok(()) => {
                    self.active.lock().unwrap().remove(&id);
                    return;
                }
                Err(error) if attempt < 3 => {
                    tracing::warn!(
                        "router session delete attempt {attempt} failed for {id}: {error}"
                    );
                    tokio::time::sleep(Duration::from_millis(50 * attempt)).await;
                }
                Err(error) => {
                    tracing::warn!("router session delete failed for {id}: {error}");
                }
            }
        }
    }

    pub async fn cleanup_all(&self) {
        let ids: Vec<String> = self.active.lock().unwrap().iter().cloned().collect();
        for id in ids {
            self.cleanup(id).await;
        }
    }
}

pub struct RouterSessionLease {
    owner: RouterSessions,
    id: Option<String>,
}

impl RouterSessionLease {
    pub fn id(&self) -> &str {
        self.id.as_deref().expect("router session lease is active")
    }
}

impl Drop for RouterSessionLease {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        let owner = self.owner.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                owner.cleanup(id).await;
            });
        }
    }
}

/// Cooperative stop signal for the toast animator loop. Combines:
///   - an `AtomicBool` flag, checked at the TOP of every loop iteration, and
///   - a `Notify`, to wake the loop out of its inter-frame sleep on drop.
///
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
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ToastAnimatorGuard {
    fn drop(&mut self) {
        self.stop.signal();
        self.task.abort();
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
        assert_eq!(
            built_url(Some(0)),
            "http://127.0.0.1:4096/session/ses_x/message"
        );
        assert_eq!(
            built_url(None),
            "http://127.0.0.1:4096/session/ses_x/message"
        );
    }
}
