use anyhow::{anyhow, Result};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderName, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{any, post},
    Router,
};
use http_body_util::BodyExt;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

use crate::config::Profile;
use crate::oc_client::{OcClient, RouterPromptError, RouterSessions};
use crate::router;

pub type ProxyClient = Client<HttpConnector, Body>;

#[derive(Clone)]
pub struct AppState {
    pub upstream: String,
    pub oc: OcClient,
    pub router_oc: OcClient,
    pub profile: Arc<Profile>,
    pub proxy_client: ProxyClient,
    pub router_sessions: Arc<RouterSessions>,
}

pub fn build_proxy_client() -> ProxyClient {
    let connector = HttpConnector::new();
    Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Some(Duration::from_secs(90)))
        .build(connector)
}

pub fn router(state: AppState) -> Router {
    Router::new()
        // prompt_async is POST-only; the TUI never GETs it, so an explicit route is safe.
        .route("/session/:id/prompt_async", post(intercept_prompt))
        // NOTE: /session/:id/message is intentionally NOT routed here. The TUI loads
        // session history with GET /session/:id/message, and OpenCode also accepts
        // POST /session/:id/message (a sync prompt). Routing this path with `post`
        // would make axum return 405 for the GET — which silently breaks history
        // loading for every continued session. Both the POST intercept and the GET
        // passthrough are handled in the fallback below by method+path dispatch.
        .fallback(any(passthrough))
        .with_state(state)
}

async fn passthrough(State(state): State<AppState>, req: Request) -> Response {
    // POST /session/:id/message is a synchronous prompt (same shape as prompt_async),
    // so route it through the interceptor just like prompt_async. Everything else —
    // including the GET /session/:id/message the TUI uses to load history — falls
    // through to transparent passthrough. This dispatch is here rather than in an
    // explicit route because axum returns 405 (not fallback) when a path matches a
    // method-router but the method does not, which would block the history GET.
    if req.method() == Method::POST && parse_prompt_path(req.uri().path()).is_some() {
        return intercept_prompt(State(state), req).await;
    }

    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| req.uri().path().to_string());
    let upstream_uri: Uri = match format!("{}{}", state.upstream, path_and_query).parse() {
        Ok(u) => u,
        Err(e) => {
            tracing::error!("passthrough: bad upstream uri: {e}");
            return bad_gateway("invalid upstream uri");
        }
    };

    let (mut parts, body) = req.into_parts();
    parts.uri = upstream_uri.clone();
    sanitize_request_headers(&mut parts.headers);
    state.oc.add_context_headers(&mut parts.headers);
    if let Some(host) = upstream_uri.host() {
        let port = upstream_uri.port_u16();
        let host_value = match port {
            Some(p) => format!("{}:{}", host, p),
            None => host.to_string(),
        };
        if let Ok(val) = HeaderValue::from_str(&host_value) {
            parts.headers.insert(HeaderName::from_static("host"), val);
        }
    }
    let upstream_req = Request::from_parts(parts, body);

    match state.proxy_client.request(upstream_req).await {
        Ok(upstream_resp) => {
            let (mut resp_parts, resp_body) = upstream_resp.into_parts();
            sanitize_hop_by_hop_headers(&mut resp_parts.headers);
            Response::from_parts(resp_parts, Body::new(resp_body))
        }
        Err(e) => {
            tracing::error!("passthrough: upstream request failed: {e}");
            bad_gateway("upstream connection failed")
        }
    }
}

async fn intercept_prompt(State(state): State<AppState>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| path.clone());
    let (session_id, endpoint) = match parse_prompt_path(&path) {
        Some(v) => v,
        None => return bad_request("could not parse session id from path"),
    };

    let mut forward_headers = req.headers().clone();
    sanitize_request_headers(&mut forward_headers);
    state.oc.add_context_headers(&mut forward_headers);

    let body_bytes = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            tracing::error!("intercept: failed to read body: {e}");
            return bad_request("failed to read request body");
        }
    };

    let original_body: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("intercept: body is not valid JSON ({e}); forwarding unchanged");
            let _ = state
                .oc
                .show_toast("oc-route", "non-JSON request forwarded", "warning")
                .await;
            return forward_to_upstream(&state, &path_and_query, &forward_headers, &body_bytes)
                .await;
        }
    };

    let new_text = extract_text_from_parts(&original_body);
    let prompt_parts = original_body
        .get("parts")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    tracing::info!(
        "intercept: endpoint={} session={} text_len={} body_len={}",
        endpoint,
        session_id,
        new_text.len(),
        body_bytes.len()
    );

    // Preserve the documented pass-through behavior for prompts without text.
    // Attachments accompany text as bounded routing hints, but an attachment-only
    // submission remains OpenCode's decision exactly as it was before oc-route.
    if new_text.is_empty() {
        return forward_to_upstream(&state, &path_and_query, &forward_headers, &body_bytes).await;
    }

    // Animated loading toast: cycles "Routing." → "Routing.." → "Routing..." every
    // ~1000ms while routing runs. The guard stops the animation when dropped, so it
    // covers exactly the routing window.
    let _routing_toast = state.oc.spawn_routing_toast("oc-route");

    // The profile timeout is one budget for the complete decision, including
    // history lookup, session creation, inference, validation, and any retry. A
    // slow first attempt must never silently double the user's maximum wait.
    let routing_result = enforce_routing_deadline(
        Duration::from_secs(state.profile.router_timeout_secs),
        run_routing(&state, &session_id, prompt_parts),
    )
    .await;

    // Drop the animator BEFORE doing anything else. The guard's stop is reliable
    // (top-of-loop flag check), so no stray `Routing...` POST can fire after this to
    // clobber the rationale toast. This ends the "Routing..." indicator at exactly
    // the right moment: routing is done.
    drop(_routing_toast);

    let (forward_body, title, rationale_text, toast_variant) = match routing_result {
        Ok((full_model_id, rationale)) => {
            let (p, m) = config_split(&full_model_id);
            let mut modified = original_body.clone();
            modified["model"] = json!({ "providerID": p, "modelID": m });
            let bytes = serde_json::to_vec(&modified).unwrap_or_else(|_| body_bytes.to_vec());
            (
                bytes,
                format!("Routed to {full_model_id}"),
                rationale,
                "info",
            )
        }
        Err(e) => {
            tracing::warn!("intercept: routing failed; preserving original prompt: {e}");
            (
                body_bytes.to_vec(),
                "Routing unavailable".to_string(),
                router::clean_rationale(&format!("kept OpenCode's selected model: {e}")),
                "warning",
            )
        }
    };

    let response =
        forward_to_upstream(&state, &path_and_query, &forward_headers, &forward_body).await;

    // Show the rationale after OpenCode accepts the async prompt or returns the sync
    // response. This is when the TUI can move from routing progress to the decision.
    // Fire-and-forget via spawn so we don't delay the response by the toast POST, and
    // so the rationale lives its full ~8s concurrently while the user reads the reply.
    // A sensible read duration: long enough to read a one-line rationale, not forever.
    let oc = state.oc.clone();
    tokio::spawn(async move {
        let _ = oc
            .show_toast_duration(
                &title,
                &rationale_text,
                toast_variant,
                Duration::from_secs(8),
            )
            .await;
    });

    response
}

async fn forward_to_upstream(
    state: &AppState,
    path_and_query: &str,
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> Response {
    let upstream_uri: Uri = match format!("{}{}", state.upstream, path_and_query).parse() {
        Ok(u) => u,
        Err(e) => {
            tracing::error!("forward: bad upstream uri: {e}");
            return bad_gateway("invalid upstream uri");
        }
    };

    let mut req_builder = Request::builder()
        .method(Method::POST)
        .uri(upstream_uri.clone());
    for (name, value) in headers.iter() {
        if let Ok(v) = HeaderValue::from_bytes(value.as_bytes()) {
            req_builder = req_builder.header(name.clone(), v);
        }
    }
    if !headers.contains_key("content-type") {
        req_builder = req_builder.header("content-type", "application/json");
    }
    if let Ok(v) = HeaderValue::from_str(&body.len().to_string()) {
        req_builder = req_builder.header("content-length", v);
    }
    if let Some(host) = upstream_uri.host() {
        let port = upstream_uri.port_u16();
        let host_value = match port {
            Some(p) => format!("{}:{}", host, p),
            None => host.to_string(),
        };
        if let Ok(v) = HeaderValue::from_str(&host_value) {
            req_builder = req_builder.header("host", v);
        }
    }

    let upstream_req = req_builder.body(Body::from(body.to_vec())).unwrap();

    match state.proxy_client.request(upstream_req).await {
        Ok(upstream_resp) => {
            let (mut resp_parts, resp_body) = upstream_resp.into_parts();
            sanitize_hop_by_hop_headers(&mut resp_parts.headers);
            Response::from_parts(resp_parts, Body::new(resp_body))
        }
        Err(e) => {
            tracing::error!("forward: upstream request failed: {e}");
            bad_gateway("upstream connection failed")
        }
    }
}

async fn run_routing(
    state: &AppState,
    session_id: &str,
    new_parts: &[Value],
) -> Result<(String, String)> {
    // Ask OpenCode (the source of truth) to do the windowing server-side: it returns
    // exactly the most recent N messages in chronological order (verified in OC
    // source, MessageV2.page). This avoids fetching the whole history every message.
    let history = state
        .oc
        .list_messages(session_id, Some(state.profile.sliding_window))
        .await?;
    let xml = router::build_routing_xml(&state.profile, &history, new_parts);

    let timeout = Duration::from_secs(state.profile.router_timeout_secs);

    // One bounded retry covers transient free-tier failures and invalid decisions.
    // Each attempt uses a fresh throwaway session, so it never sees a prior routing
    // request. Session deletion is scheduled independently of request forwarding.
    let mut last_err: Option<anyhow::Error> = None;
    let mut attempt = 0;
    while attempt < 2 {
        attempt += 1;
        let router_session = match state.router_sessions.acquire().await {
            Ok(session) => session,
            Err(e) => {
                last_err = Some(e.context("take router session"));
                continue;
            }
        };
        let router_resp = state
            .router_oc
            .prompt_router(
                router_session.id(),
                &state.profile.router_model,
                &xml,
                timeout,
            )
            .await;
        // Deletion is independent of the decision. Dropping the lease schedules
        // bounded cleanup without adding a session-delete round trip to the user's
        // critical path; shutdown still drains any tracked sessions.
        drop(router_session);

        match router_resp {
            Ok(text) => {
                let decision = match router::parse_decision(&text) {
                    Ok(decision) => decision,
                    Err(error) => {
                        tracing::warn!(
                            "run_routing: attempt {attempt} returned invalid JSON: {error}"
                        );
                        last_err = Some(error);
                        continue;
                    }
                };
                let Some((provider, model)) =
                    router::validate_model(&decision.model, &state.profile.model_pool)
                else {
                    let error = anyhow!("router chose '{}' outside the model pool", decision.model);
                    tracing::warn!("run_routing: attempt {attempt} failed validation: {error}");
                    last_err = Some(error);
                    continue;
                };

                return Ok((
                    format!("{provider}/{model}"),
                    router::clean_rationale(&decision.rationale),
                ));
            }
            Err(e) => {
                let retryable = e
                    .downcast_ref::<RouterPromptError>()
                    .map(RouterPromptError::retryable)
                    .unwrap_or(true);
                tracing::warn!("run_routing: attempt {attempt} failed: {e}");
                last_err = Some(e);
                if !retryable {
                    break;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("routing failed with no error captured")))
}

async fn enforce_routing_deadline<T>(
    budget: Duration,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(budget, future)
        .await
        .map_err(|_| anyhow!("routing exceeded the total {budget:?} deadline"))?
}

fn parse_prompt_path(path: &str) -> Option<(String, &'static str)> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() != 3 || segments[0] != "session" {
        return None;
    }
    let endpoint = match segments[2] {
        "prompt_async" => "prompt_async",
        "message" => "message",
        _ => return None,
    };
    Some((segments[1].to_string(), endpoint))
}

fn sanitize_request_headers(headers: &mut axum::http::HeaderMap) {
    sanitize_hop_by_hop_headers(headers);
    headers.remove("host");
    headers.remove("content-length");
}

fn sanitize_hop_by_hop_headers(headers: &mut axum::http::HeaderMap) {
    let nominated: Vec<HeaderName> = headers
        .get_all("connection")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|value| HeaderName::from_bytes(value.trim().as_bytes()).ok())
        .collect();
    for name in nominated {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

fn extract_text_from_parts(body: &Value) -> String {
    let mut text = String::new();
    if let Some(parts) = body.get("parts").and_then(|p| p.as_array()) {
        for part in parts {
            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                    text.push('\n');
                }
            }
        }
    }
    text.trim().to_string()
}

fn config_split(model_id: &str) -> (String, String) {
    crate::config::split_model_id(model_id).unwrap_or_else(|| {
        crate::config::split_model_id(crate::config::DEFAULT_ROUTER_MODEL)
            .unwrap_or(("opencode".to_string(), "mimo-v2.5-free".to_string()))
    })
}

fn bad_gateway(msg: &str) -> Response {
    (StatusCode::BAD_GATEWAY, msg.to_string()).into_response()
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State as AxumState;
    use axum::routing::any;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tower::ServiceExt;

    #[tokio::test]
    async fn routing_deadline_bounds_the_complete_operation() {
        let result = enforce_routing_deadline(Duration::from_millis(5), async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok::<_, anyhow::Error>(())
        })
        .await;

        assert!(result
            .unwrap_err()
            .to_string()
            .contains("total 5ms deadline"));
    }

    #[derive(Clone, Debug)]
    struct SeenRequest {
        method: Method,
        path_and_query: String,
        headers: axum::http::HeaderMap,
        body: Vec<u8>,
    }

    #[derive(Clone, Default)]
    struct MockState {
        seen: Arc<Mutex<Vec<SeenRequest>>>,
        sessions: Arc<AtomicUsize>,
        router_responses: Arc<Mutex<VecDeque<Value>>>,
    }

    async fn mock_upstream(AxumState(state): AxumState<MockState>, request: Request) -> Response {
        let method = request.method().clone();
        let path_and_query = request
            .uri()
            .path_and_query()
            .map(|value| value.as_str().to_string())
            .unwrap_or_else(|| request.uri().path().to_string());
        let headers = request.headers().clone();
        let body = request
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        state.seen.lock().unwrap().push(SeenRequest {
            method: method.clone(),
            path_and_query: path_and_query.clone(),
            headers,
            body,
        });

        let path = path_and_query.split('?').next().unwrap();
        if method == Method::GET && path.ends_with("/message") {
            return axum::Json(json!([])).into_response();
        }
        if method == Method::POST && path == "/session" {
            let id = state.sessions.fetch_add(1, Ordering::SeqCst) + 1;
            return axum::Json(json!({ "id": format!("router-{id}") })).into_response();
        }
        if method == Method::POST
            && path.starts_with("/session/router-")
            && path.ends_with("/message")
        {
            if let Some(response) = state.router_responses.lock().unwrap().pop_front() {
                return axum::Json(response).into_response();
            }
            return (StatusCode::INTERNAL_SERVER_ERROR, "injected failure").into_response();
        }
        if method == Method::DELETE && path.starts_with("/session/router-") {
            return axum::Json(json!(true)).into_response();
        }
        axum::Json(json!({ "forwarded": true })).into_response()
    }

    async fn test_app() -> (
        Router,
        MockState,
        tokio::task::JoinHandle<()>,
        Arc<RouterSessions>,
    ) {
        let mock = MockState::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mock_app = Router::new()
            .fallback(any(mock_upstream))
            .with_state(mock.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, mock_app).await.unwrap();
        });
        let oc = OcClient::new_from_url(&format!("http://{address}")).with_context(
            Some("/configured project".to_string()),
            Some("router-user".to_string()),
            Some("router-password".to_string()),
        );
        let sessions = Arc::new(RouterSessions::new(oc.clone()));
        let app = router(AppState {
            upstream: format!("http://{address}"),
            oc: oc.clone(),
            router_oc: oc,
            profile: Arc::new(Profile {
                name: "test".into(),
                router_model: "opencode/router".into(),
                sliding_window: 5,
                router_timeout_secs: 2,
                model_pool: vec!["opencode/fallback".into()],
                routing_prompt: "rules".into(),
            }),
            proxy_client: build_proxy_client(),
            router_sessions: sessions.clone(),
        });
        (app, mock, server, sessions)
    }

    #[tokio::test]
    async fn routing_failure_preserves_request_and_query() {
        let (app, mock, server, sessions) = test_app().await;
        let original = json!({
            "parts": [{ "type": "text", "text": "hello" }],
            "model": { "providerID": "original", "modelID": "chosen" }
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/session/user/message?directory=%2Ftmp%2Fquery")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer incoming")
                    .header("x-opencode-directory", "%2Ftmp%2Fincoming")
                    .header("connection", "x-remove-me")
                    .header("x-remove-me", "hop value")
                    .body(Body::from(serde_json::to_vec(&original).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        {
            let seen = mock.seen.lock().unwrap();
            let forwarded = seen
                .iter()
                .find(|request| {
                    request.method == Method::POST
                        && request.path_and_query.starts_with("/session/user/message?")
                })
                .unwrap();
            assert_eq!(
                forwarded.path_and_query,
                "/session/user/message?directory=%2Ftmp%2Fquery"
            );
            assert_eq!(
                serde_json::from_slice::<Value>(&forwarded.body).unwrap(),
                original
            );
            assert_eq!(forwarded.headers["authorization"], "Bearer incoming");
            assert!(!forwarded.headers.contains_key("connection"));
            assert!(!forwarded.headers.contains_key("x-remove-me"));
            assert_eq!(
                forwarded.headers["x-opencode-directory"],
                "%2Ftmp%2Fincoming"
            );
        }
        sessions.cleanup_all().await;
        server.abort();
    }

    #[tokio::test]
    async fn assistant_error_is_not_mistaken_for_a_routing_decision() {
        let (app, mock, server, sessions) = test_app().await;
        let assistant_error = json!({
            "info": {
                "error": {
                    "name": "ProviderAuthError",
                    "data": {
                        "message": "injected provider failure",
                        "isRetryable": false
                    }
                }
            },
            "parts": []
        });
        // A non-retryable assistant error must fail open immediately rather than
        // spending the user's time on a knowingly futile second request.
        mock.router_responses
            .lock()
            .unwrap()
            .push_back(assistant_error);
        let original = json!({
            "parts": [{ "type": "text", "text": "keep this request intact" }],
            "model": { "providerID": "original", "modelID": "chosen" }
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/session/user/prompt_async")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&original).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(mock.sessions.load(Ordering::SeqCst), 1);

        sessions.cleanup_all().await;
        let seen = mock.seen.lock().unwrap();
        let forwarded = seen
            .iter()
            .find(|request| request.path_and_query == "/session/user/prompt_async")
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&forwarded.body).unwrap(),
            original
        );
        drop(seen);
        server.abort();
    }

    #[tokio::test]
    async fn successful_routing_uses_dedicated_agent_and_injects_only_pool_member() {
        let (app, mock, server, sessions) = test_app().await;
        mock.router_responses.lock().unwrap().push_back(json!({
            "parts": [{
                "type": "text",
                "text": "{\"model\":\"opencode/fallback\",\"rationale\":\"policy match\"}"
            }]
        }));
        let original = json!({
            "parts": [{ "type": "text", "text": "route this" }],
            "model": { "providerID": "original", "modelID": "chosen" }
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/session/user/prompt_async")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&original).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        sessions.cleanup_all().await;
        {
            let seen = mock.seen.lock().unwrap();
            let router_prompt = seen
                .iter()
                .find(|request| {
                    request.method == Method::POST
                        && request.path_and_query.starts_with("/session/router-")
                        && request.path_and_query.ends_with("/message")
                })
                .unwrap();
            let router_body: Value = serde_json::from_slice(&router_prompt.body).unwrap();
            assert_eq!(router_body["agent"], "oc-route");
            assert_eq!(router_body["variant"], "none");
            assert_eq!(router_body["tools"]["*"], false);
            assert!(router_body.get("system").is_none());
            assert!(router_body["parts"][0]["text"]
                .as_str()
                .unwrap()
                .contains("<routing_rules>"));

            let forwarded = seen
                .iter()
                .find(|request| {
                    request.method == Method::POST
                        && request.path_and_query == "/session/user/prompt_async"
                })
                .unwrap();
            let forwarded_body: Value = serde_json::from_slice(&forwarded.body).unwrap();
            assert_eq!(forwarded_body["model"]["providerID"], "opencode");
            assert_eq!(forwarded_body["model"]["modelID"], "fallback");
            assert_eq!(forwarded_body["parts"], original["parts"]);
        }
        server.abort();
    }

    #[tokio::test]
    async fn non_prompt_and_textless_posts_are_transparent() {
        for (uri, body) in [
            (
                "/session/user/message/extra",
                json!({ "parts": [{ "type": "text", "text": "not a prompt path" }] }),
            ),
            (
                "/session/user/prompt_async",
                json!({
                    "parts": [{
                        "type": "file",
                        "mime": "image/png",
                        "filename": "diagram.png",
                        "url": "data:image/png;base64,AA=="
                    }],
                    "model": { "providerID": "original", "modelID": "vision" }
                }),
            ),
        ] {
            let (app, mock, server, sessions) = test_app().await;
            let response = app
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            {
                let seen = mock.seen.lock().unwrap();
                assert_eq!(
                    seen.iter()
                        .filter(|request| request.path_and_query == "/session")
                        .count(),
                    0,
                    "transparent requests must not create router sessions"
                );
                let forwarded = seen
                    .iter()
                    .find(|request| request.path_and_query == uri)
                    .unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&forwarded.body).unwrap(),
                    body
                );
            }
            sessions.cleanup_all().await;
            server.abort();
        }
    }
}
