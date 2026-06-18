use anyhow::Result;
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
use crate::oc_client::OcClient;
use crate::router;

pub type ProxyClient = Client<HttpConnector, Body>;

#[derive(Clone)]
pub struct AppState {
    pub upstream: String,
    pub oc: OcClient,
    pub profile: Arc<Profile>,
    pub proxy_client: ProxyClient,
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
            let (resp_parts, resp_body) = upstream_resp.into_parts();
            Response::from_parts(resp_parts, Body::new(resp_body))
        }
        Err(e) => {
            tracing::error!("passthrough: upstream request failed: {e}");
            bad_gateway("upstream connection failed")
        }
    }
}

async fn intercept_prompt(
    State(state): State<AppState>,
    req: Request,
) -> Response {
    let path = req.uri().path().to_string();
    let (session_id, endpoint) = match parse_prompt_path(&path) {
        Some(v) => v,
        None => return bad_request("could not parse session id from path"),
    };

    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let auth = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let mut forward_headers = req.headers().clone();
    forward_headers.remove("host");
    forward_headers.remove("content-length");

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
            return forward_to_upstream(
                &state,
                &path,
                &forward_headers,
                &body_bytes,
                content_type.as_deref(),
                auth.as_deref(),
            )
            .await;
        }
    };

    let new_text = extract_text_from_parts(&original_body);
    tracing::info!(
        "intercept: endpoint={} session={} text_len={} body_len={}",
        endpoint,
        session_id,
        new_text.len(),
        body_bytes.len()
    );

    // Animated loading toast: cycles "Routing." → "Routing.." → "Routing..." every
    // ~400ms while routing runs. The guard stops the animation when dropped, so it
    // covers exactly the routing window — then the result toast below takes over.
    let _routing_toast = state.oc.spawn_routing_toast("oc-route");

    let routing_result = run_routing(&state, &session_id, &new_text).await;

    // Drop the animator now so the result toast isn't immediately overwritten by
    // the next animation frame. (Explicit for clarity; the block scope would too.)
    drop(_routing_toast);

    let (provider, model, rationale_text, toast_variant) = match routing_result {
        Ok((full_model_id, rationale)) => {
            let (p, m) = config_split(&full_model_id);
            (p, m, rationale, "info")
        }
        Err(e) => {
            tracing::warn!("intercept: routing failed, falling back to pool[0]: {e}");
            let fallback = state.profile.model_pool.first().cloned().unwrap_or_else(|| {
                crate::config::DEFAULT_ROUTER_MODEL.to_string()
            });
            let (p, m) = config_split(&fallback);
            (
                p,
                m,
                format!("routing failed ({}); using fallback", e),
                "warning",
            )
        }
    };

    let display_model = format!("{}/{}", provider, model);
    let title = format!("Routed to {}", display_model);
    let _ = state
        .oc
        .show_toast(&title, &rationale_text, toast_variant)
        .await;

    let mut modified = original_body.clone();
    modified["model"] = json!({ "providerID": provider, "modelID": model });
    let modified_bytes =
        serde_json::to_vec(&modified).unwrap_or_else(|_| body_bytes.to_vec());

    forward_to_upstream(
        &state,
        &path,
        &forward_headers,
        &modified_bytes,
        Some("application/json"),
        auth.as_deref(),
    )
    .await
}

async fn forward_to_upstream(
    state: &AppState,
    path_and_query: &str,
    headers: &axum::http::HeaderMap,
    body: &[u8],
    content_type: Option<&str>,
    _auth: Option<&str>,
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
    if let Some(ct) = content_type {
        if let Ok(v) = HeaderValue::from_str(ct) {
            req_builder = req_builder.header("content-type", v);
        }
    } else {
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
            let (resp_parts, resp_body) = upstream_resp.into_parts();
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
    new_text: &str,
) -> Result<(String, String)> {
    let history = state.oc.list_messages(session_id).await?;
    let xml = router::build_routing_xml(&state.profile, &history, new_text);

    let router_session = state.oc.create_router_session().await?;
    let timeout = Duration::from_secs(state.profile.router_timeout_secs);
    let router_resp = state
        .oc
        .prompt_router(&router_session, &state.profile.router_model, &xml, timeout)
        .await;
    let _ = state.oc.delete_session(&router_session).await;

    let router_resp = router_resp?;
    let decision = router::parse_decision(&router_resp)?;
    let (provider, model) = match router::validate_model(&decision.model, &state.profile.model_pool) {
        Some(pm) => pm,
        None => {
            tracing::warn!(
                "run_routing: model '{}' not in pool; using fallback",
                decision.model
            );
            let fallback = state
                .profile
                .model_pool
                .first()
                .cloned()
                .unwrap_or_else(|| state.profile.router_model.clone());
            let (p, m) = config_split(&fallback);
            return Ok((
                format!("{}/{}", p, m),
                format!(
                    "router chose '{}' (not in pool); using fallback",
                    decision.model
                ),
            ));
        }
    };

    Ok((format!("{}/{}", provider, model), decision.rationale))
}

fn parse_prompt_path(path: &str) -> Option<(String, &'static str)> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 3 || segments[0] != "session" {
        return None;
    }
    let endpoint = match segments[2] {
        "prompt_async" => "prompt_async",
        "message" => "message",
        _ => return None,
    };
    Some((segments[1].to_string(), endpoint))
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
    crate::config::split_model_id(model_id)
        .unwrap_or_else(|| crate::config::split_model_id(crate::config::DEFAULT_ROUTER_MODEL)
            .unwrap_or(("opencode".to_string(), "mimo-v2.5-free".to_string())))
}

fn bad_gateway(msg: &str) -> Response {
    (StatusCode::BAD_GATEWAY, msg.to_string()).into_response()
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}
