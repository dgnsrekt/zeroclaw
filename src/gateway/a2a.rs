//! A2A (Agent-to-Agent) protocol server handlers.
//!
//! Exposes two routes when `a2a.server.enabled = true`:
//!
//! - `GET /.well-known/agent.json` — returns a static AgentCard JSON built from config
//! - `POST /a2a` — JSON-RPC 2.0 dispatcher; checks pairing bearer auth (same as `/api/chat`),
//!   then calls `run_gateway_chat_with_tools()` and wraps the reply in an A2A Task response.

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    Json,
};

use super::{run_gateway_chat_with_tools, AppState};

/// GET /.well-known/agent.json — A2A AgentCard (public, no auth required).
pub async fn handle_agent_card(State(state): State<AppState>) -> impl IntoResponse {
    let cfg = state.config.lock();
    let card = serde_json::json!({
        "name": cfg.a2a.server.name,
        "description": cfg.a2a.server.description,
        "url": cfg.a2a.server.url,
        "version": "1.0.0",
        "capabilities": {"streaming": false},
        "skills": [{"id": "chat", "name": "Chat", "description": "General agent chat"}]
    });
    (StatusCode::OK, Json(card))
}

/// POST /a2a — A2A JSON-RPC 2.0 `message/send` handler.
pub async fn handle_a2a_rpc(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(rpc): Json<serde_json::Value>,
) -> impl IntoResponse {
    // ── Auth check (same pattern as /api/chat in openclaw_compat.rs) ──
    let require_auth = state.config.lock().a2a.server.require_auth;
    if require_auth {
        let auth = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let token = auth.strip_prefix("Bearer ").unwrap_or("");
        if !state.pairing.is_authenticated(token) {
            tracing::warn!("/a2a: rejected — not paired / invalid bearer token");
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "error": {"code": -32600, "message": "Unauthorized"}
                })),
            )
                .into_response();
        }
    }

    // ── Extract message text from JSON-RPC params ──
    let message = rpc
        .pointer("/params/message/parts/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let rpc_id = rpc.get("id").cloned().unwrap_or(serde_json::Value::Null);

    if message.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "error": {"code": -32602, "message": "params.message.parts[0].text is required"}
            })),
        )
            .into_response();
    }

    // ── Run agent loop ──
    match run_gateway_chat_with_tools(&state, &message, None).await {
        Ok(reply) => {
            let task = serde_json::json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "result": {
                    "status": "completed",
                    "result": {
                        "artifacts": [{
                            "parts": [{"type": "text", "text": reply}]
                        }]
                    }
                }
            });
            (StatusCode::OK, Json(task)).into_response()
        }
        Err(e) => {
            tracing::error!("/a2a: agent loop error: {e:#}");
            let err = serde_json::json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "error": {"code": -32603, "message": e.to_string()}
            });
            (StatusCode::INTERNAL_SERVER_ERROR, Json(err)).into_response()
        }
    }
}
