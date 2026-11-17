//! `jira-review serve --socket <path> --data <dir>`: the Tracon plugin
//! contract v1. MCP over HTTP on a Unix socket the node names; every
//! `tools/call` carries a signed caller context and the credentials lent to
//! it, and nothing is read from the start environment but the contract's
//! own variables.

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::contract::{ContractEnv, NonceCache};
use crate::mcp::{self, Plugin};
use crate::store::Store;

/// The contract version this build serves.
pub const CONTRACT: &str = "1";

/// Check the start environment and open the store: everything `serve` needs
/// before it listens.
pub fn plugin(get: impl Fn(&str) -> Option<String>, data: &Path) -> Result<Plugin, String> {
    let token = get("TRACON_PLUGIN_TOKEN")
        .filter(|t| !t.is_empty())
        .ok_or("TRACON_PLUGIN_TOKEN is not set; serve is started by a Tracon node")?;
    let key = get("TRACON_NODE_KEY")
        .filter(|k| !k.is_empty())
        .ok_or("TRACON_NODE_KEY is not set; serve is started by a Tracon node")?;
    if let Some(v) = get("TRACON_CONTRACT").filter(|v| v != CONTRACT) {
        return Err(format!(
            "TRACON_CONTRACT is {v}; this build serves contract {CONTRACT}"
        ));
    }
    Ok(Plugin {
        env: ContractEnv::new(&token, &key)?,
        nonces: NonceCache::default(),
        store: Store::open(data.join("proposals"))?,
    })
}

pub async fn run(socket: &Path, data: &Path) -> Result<(), String> {
    let host = Arc::new(plugin(|k| std::env::var(k).ok(), data)?);
    match std::fs::symlink_metadata(socket) {
        Ok(m) if m.file_type().is_socket() => {
            std::fs::remove_file(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
        }
        Ok(_) => {
            return Err(format!(
                "{} exists and is not a socket; not replacing it",
                socket.display()
            ));
        }
        Err(_) => {}
    }
    let listener =
        tokio::net::UnixListener::bind(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("{}: {e}", socket.display()))?;
    let app = Router::new().fallback(endpoint).with_state(host);
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
        .map_err(|e| e.to_string());
    let _ = std::fs::remove_file(socket);
    served
}

async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("a SIGTERM handler installs");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// Every path answers MCP: a POST carries one JSON-RPC message and gets its
/// answer as JSON, or 202 for a notification. There is no event stream, so
/// GET is refused.
async fn endpoint(State(host): State<Arc<Plugin>>, method: Method, body: Bytes) -> Response {
    if method != Method::POST {
        return (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")]).into_response();
    }
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(msg) => msg,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(mcp::parse_error(&e.to_string())),
            )
                .into_response();
        }
    };
    match mcp::handle(&*host, msg).await {
        Some(answer) => axum::Json(answer).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}
