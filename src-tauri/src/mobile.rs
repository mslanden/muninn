//! Embedded mobile companion server.
//!
//! An opt-in axum HTTP + WebSocket server that runs inside the Tauri process so a phone
//! browser on the same LAN can drive Muninn. Off by default; the desktop Settings UI toggles
//! it. Every route (including the page and WS upgrade) is gated by a bearer/query token that
//! is compared in constant time. The only static content served is the embedded `mobile.html`
//! page — there is no filesystem serving.
//!
//! State lives in a module-local static (`SERVER`) so the server can be started/stopped
//! without touching `AppState`. The desktop `get_mobile_server_status` command is allowed to
//! return the raw token because it is invoked only by the trusted desktop webview.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Mutex, OnceLock};

use axum::{
    extract::{ws::Message, ws::WebSocket, Query, State, WebSocketUpgrade},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rusqlite::params;
use serde_json::{json, Value};
use tauri::{AppHandle, Listener, Manager};
use tokio::sync::{broadcast, oneshot};

use crate::db::{get_app_setting_value, with_db};
use crate::AppState;

const DEFAULT_PORT: u16 = 8737;

/// Event names forwarded to connected WebSocket clients. Kept explicit (not a wildcard) so a
/// newly-added desktop-only event never leaks to phones by accident.
const FORWARDED_EVENTS: &[&str] = &[
    "chat-token",
    "chat-done",
    "chat-usage",
    "tool-start",
    "tool-complete",
    "agent-iteration",
    "goal-iteration",
    "goal-met",
    "user-question",
    "user-question-timeout",
    "command-deferred",
    "todos-changed",
];

/// Live server handle. `None` when stopped.
struct MobileServer {
    port: u16,
    shutdown: Option<oneshot::Sender<()>>,
    /// Tauri event-listener handles to unregister on stop.
    listeners: Vec<tauri::EventId>,
    app: AppHandle,
}

fn server_slot() -> &'static Mutex<Option<MobileServer>> {
    static SERVER: OnceLock<Mutex<Option<MobileServer>>> = OnceLock::new();
    SERVER.get_or_init(|| Mutex::new(None))
}

/// Axum shared state passed to every handler.
#[derive(Clone)]
struct AppCtx {
    app: AppHandle,
    token: String,
    broadcast_tx: broadcast::Sender<String>,
}

// ------------------------------------------------------------------------------------------
// Token helpers
// ------------------------------------------------------------------------------------------

/// Constant-time byte comparison. Folds a difference accumulator over the max of the two
/// lengths so neither a length mismatch nor an early differing byte short-circuits. Length
/// difference alone forces a non-zero result.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let max = a.len().max(b.len());
    let mut diff: u8 = (a.len() ^ b.len()) as u8;
    for i in 0..max {
        let av = *a.get(i).unwrap_or(&0);
        let bv = *b.get(i).unwrap_or(&0);
        diff |= av ^ bv;
    }
    diff == 0
}

fn generate_token() -> String {
    let a = uuid::Uuid::new_v4().simple().to_string();
    let b = uuid::Uuid::new_v4().simple().to_string();
    // 32 hex chars = first 16 of each simple (dashless) uuid.
    format!("{}{}", &a[..16], &b[..16])
}

/// Read the persisted token, generating and saving one on first use.
fn ensure_token(state: &AppState) -> Result<String, String> {
    if let Some(tok) = get_app_setting_value(state, "mobile/server_token")? {
        if !tok.trim().is_empty() {
            return Ok(tok);
        }
    }
    let tok = generate_token();
    write_setting(state, "mobile/server_token", &tok)?;
    Ok(tok)
}

/// Direct app_config write, mirroring `set_app_settings`' upsert path.
fn write_setting(state: &AppState, key: &str, value: &str) -> Result<(), String> {
    with_db(state, |db| {
        db.execute(
            r#"
            INSERT INTO app_config (key, value, updated_at)
            VALUES (?1, ?2, CURRENT_TIMESTAMP)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP
            "#,
            params![key, value],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })
}

fn read_port(state: &AppState) -> u16 {
    get_app_setting_value(state, "mobile/server_port")
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<u16>().ok())
        .map(clamp_port)
        .unwrap_or(DEFAULT_PORT)
}

fn clamp_port(p: u16) -> u16 {
    p.clamp(1024, 65535)
}

// ------------------------------------------------------------------------------------------
// Lifecycle
// ------------------------------------------------------------------------------------------

/// Start the server. Reads port + token from settings (generating a token if absent), binds
/// `0.0.0.0:port`, registers event listeners, and serves with graceful shutdown. Returns the
/// bound address. Errors if already running or the bind fails.
pub fn start(app: AppHandle) -> Result<String, String> {
    {
        let slot = server_slot().lock().map_err(|_| "mobile server lock poisoned".to_string())?;
        if slot.is_some() {
            return Err("mobile server is already running".to_string());
        }
    }

    let state = app.state::<AppState>();
    let port = read_port(&state);
    let token = ensure_token(&state)?;
    drop(state);

    let (broadcast_tx, _rx) = broadcast::channel::<String>(256);

    // Register event forwarders. Each handler wraps the payload in an envelope and broadcasts.
    let mut listeners: Vec<tauri::EventId> = Vec::new();
    for name in FORWARDED_EVENTS {
        let tx = broadcast_tx.clone();
        let ev_name = *name;
        let id = app.listen(ev_name, move |event| {
            // event.payload() is a JSON string (or empty for unit payloads).
            let payload: Value = serde_json::from_str(event.payload()).unwrap_or(Value::Null);
            let envelope = json!({ "event": ev_name, "payload": payload });
            // A lagging/absent receiver is not fatal — drop and continue.
            let _ = tx.send(envelope.to_string());
        });
        listeners.push(id);
    }

    let ctx = AppCtx {
        app: app.clone(),
        token,
        broadcast_tx: broadcast_tx.clone(),
    };

    let router = build_router(ctx);

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let addr = format!("0.0.0.0:{}", port);

    // Bind synchronously so a bind failure surfaces to the caller before we mark the server
    // running. `std::net::TcpListener` -> tokio via `from_std`.
    let std_listener = std::net::TcpListener::bind(&addr)
        .map_err(|e| format!("failed to bind {}: {}", addr, e))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| e.to_string())?;

    let bound = std_listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| addr.clone());

    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(std_listener) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[Mobile] failed to adopt listener: {}", e);
                return;
            }
        };
        let server = axum::serve(listener, router).with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        });
        if let Err(e) = server.await {
            eprintln!("[Mobile] server error: {}", e);
        }
    });

    {
        let mut slot = server_slot().lock().map_err(|_| "mobile server lock poisoned".to_string())?;
        *slot = Some(MobileServer {
            port,
            shutdown: Some(shutdown_tx),
            listeners,
            app: app.clone(),
        });
    }

    // Persist the enabled flag so the state survives restarts.
    let state = app.state::<AppState>();
    let _ = write_setting(&state, "mobile/server_enabled", "1");

    eprintln!("[Mobile] companion server listening on port {}", port);
    Ok(bound)
}

/// Stop the server if running. Unregisters listeners, triggers graceful shutdown. Returns
/// whether a server was actually stopped.
pub fn stop() -> bool {
    let taken = {
        let mut slot = match server_slot().lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        slot.take()
    };
    match taken {
        Some(mut srv) => {
            for id in srv.listeners.drain(..) {
                srv.app.unlisten(id);
            }
            if let Some(tx) = srv.shutdown.take() {
                let _ = tx.send(());
            }
            // Persist disabled flag.
            let state = srv.app.state::<AppState>();
            let _ = write_setting(&state, "mobile/server_enabled", "0");
            true
        }
        None => false,
    }
}

pub fn is_running() -> bool {
    server_slot()
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false)
}

fn running_port() -> Option<u16> {
    server_slot().lock().ok().and_then(|g| g.as_ref().map(|s| s.port))
}

// ------------------------------------------------------------------------------------------
// Router + auth
// ------------------------------------------------------------------------------------------

fn build_router(ctx: AppCtx) -> Router {
    Router::new()
        .route("/", get(serve_page))
        .route("/ws", get(ws_upgrade))
        .route("/api/sessions", get(api_get_sessions).post(api_create_session))
        .route("/api/messages", get(api_get_messages))
        .route("/api/chat", post(api_chat))
        .route("/api/stop", post(api_stop))
        .route("/api/goal", get(api_get_goal).post(api_set_goal))
        .route("/api/todos", get(api_get_todos))
        .route("/api/deferred", get(api_get_deferred))
        .route("/api/deferred/approve", post(api_deferred_approve))
        .route("/api/deferred/dismiss", post(api_deferred_dismiss))
        .route("/api/answer", post(api_answer))
        .with_state(ctx)
}

/// Extract the presented token from `Authorization: Bearer <t>` or `?token=<t>`.
fn presented_token(headers: &HeaderMap, q: &std::collections::HashMap<String, String>) -> Option<String> {
    if let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(rest) = auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer ")) {
            let t = rest.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    q.get("token").filter(|t| !t.is_empty()).cloned()
}

/// Returns true if the request carries the correct token.
fn authed(
    ctx: &AppCtx,
    headers: &HeaderMap,
    q: &std::collections::HashMap<String, String>,
) -> bool {
    match presented_token(headers, q) {
        Some(t) => constant_time_eq(t.as_bytes(), ctx.token.as_bytes()),
        None => false,
    }
}

fn unauthorized_json() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized" })),
    )
        .into_response()
}

const UNAUTHORIZED_PAGE: &str = "<!doctype html><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>Muninn</title><body style=\"font-family:system-ui;padding:2rem;color:#333\">Unauthorized — scan the QR code in Muninn Settings.</body>";

// ------------------------------------------------------------------------------------------
// Route handlers
// ------------------------------------------------------------------------------------------

async fn serve_page(
    State(ctx): State<AppCtx>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if !authed(&ctx, &headers, &q) {
        return (StatusCode::UNAUTHORIZED, Html(UNAUTHORIZED_PAGE)).into_response();
    }
    Html(include_str!("mobile.html")).into_response()
}

async fn ws_upgrade(
    State(ctx): State<AppCtx>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    if !authed(&ctx, &headers, &q) {
        return unauthorized_json();
    }
    let rx = ctx.broadcast_tx.subscribe();
    ws.on_upgrade(move |socket| ws_task(socket, rx))
}

/// Forward every broadcast message to the socket as a text frame. Also drains incoming client
/// frames (answer pings, ignore other content) so the socket doesn't stall.
async fn ws_task(mut socket: WebSocket, mut rx: broadcast::Receiver<String>) {
    loop {
        tokio::select! {
            broadcast_msg = rx.recv() => {
                match broadcast_msg {
                    Ok(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Skip dropped messages and keep going.
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            client_msg = socket.recv() => {
                match client_msg {
                    Some(Ok(Message::Ping(p))) => {
                        if socket.send(Message::Pong(p)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    // Ignore any other client-sent frames.
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }
}

fn err_response(e: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": e })),
    )
        .into_response()
}

// ---- Guarded handler macro helper -------------------------------------------------------

macro_rules! guard {
    ($ctx:expr, $headers:expr, $q:expr) => {
        if !authed(&$ctx, &$headers, &$q) {
            return unauthorized_json();
        }
    };
}

async fn api_get_sessions(
    State(ctx): State<AppCtx>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    guard!(ctx, headers, q);
    let state = ctx.app.state::<AppState>();
    let req = crate::models::SessionQueryRequest {
        limit: Some(40),
        query: q.get("query").filter(|s| !s.is_empty()).cloned(),
        project_path: None,
    };
    match crate::commands::get_sessions_core(&state, req) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}

async fn api_create_session(
    State(ctx): State<AppCtx>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    guard!(ctx, headers, q);
    let state = ctx.app.state::<AppState>();
    match crate::commands::create_session_core(&state, None) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}

async fn api_get_messages(
    State(ctx): State<AppCtx>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    guard!(ctx, headers, q);
    let session_id = match q.get("session_id") {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return err_response("session_id is required".to_string()),
    };
    let state = ctx.app.state::<AppState>();
    match crate::commands::get_messages_core(&state, &session_id) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}

async fn api_chat(
    State(ctx): State<AppCtx>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Response {
    guard!(ctx, headers, q);
    let session_id = body.get("session_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let message = body.get("message").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if session_id.is_empty() {
        return err_response("session_id is required".to_string());
    }
    let req = crate::models::ChatRequest {
        session_id,
        message,
        provider: None,
        model: None,
        agent_id: None,
        attachments: None,
    };
    // Long-running; the phone streams tokens over WS. HTTP returns the final reply.
    let app = ctx.app.clone();
    let state = app.state::<AppState>();
    match crate::commands::chat_core(&app, &state, req).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}

async fn api_stop(
    State(ctx): State<AppCtx>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Response {
    guard!(ctx, headers, q);
    let session_id = body.get("session_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let state = ctx.app.state::<AppState>();
    match crate::commands::stop_chat_core(&state, &session_id) {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn api_get_goal(
    State(ctx): State<AppCtx>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    guard!(ctx, headers, q);
    let session_id = match q.get("session_id") {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return err_response("session_id is required".to_string()),
    };
    let state = ctx.app.state::<AppState>();
    match crate::commands::get_session_goal_core(&state, &session_id) {
        Ok(goal) => Json(json!({ "goal": goal })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn api_set_goal(
    State(ctx): State<AppCtx>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Response {
    guard!(ctx, headers, q);
    let session_id = body.get("session_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let goal = body.get("goal").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if session_id.is_empty() {
        return err_response("session_id is required".to_string());
    }
    let state = ctx.app.state::<AppState>();
    match crate::commands::set_session_goal_core(&state, &session_id, &goal) {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn api_get_todos(
    State(ctx): State<AppCtx>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    guard!(ctx, headers, q);
    let session_id = match q.get("session_id") {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return err_response("session_id is required".to_string()),
    };
    let state = ctx.app.state::<AppState>();
    match crate::commands::get_session_todos_core(&state, &session_id) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}

async fn api_get_deferred(
    State(ctx): State<AppCtx>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    guard!(ctx, headers, q);
    let session_id = q.get("session_id").filter(|s| !s.is_empty()).cloned();
    let state = ctx.app.state::<AppState>();
    match crate::commands::list_deferred_commands_core(&state, session_id, None) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}

async fn api_deferred_approve(
    State(ctx): State<AppCtx>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Response {
    guard!(ctx, headers, q);
    let id = match body.get("id").and_then(|v| v.as_i64()) {
        Some(i) => i,
        None => return err_response("id is required".to_string()),
    };
    let state = ctx.app.state::<AppState>();
    match crate::commands::approve_deferred_command_core(&state, id) {
        Ok(output) => Json(json!({ "output": output })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn api_deferred_dismiss(
    State(ctx): State<AppCtx>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Response {
    guard!(ctx, headers, q);
    let id = match body.get("id").and_then(|v| v.as_i64()) {
        Some(i) => i,
        None => return err_response("id is required".to_string()),
    };
    let state = ctx.app.state::<AppState>();
    match crate::commands::dismiss_deferred_command_core(&state, id) {
        Ok(ok) => Json(json!({ "ok": ok })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn api_answer(
    State(ctx): State<AppCtx>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Response {
    guard!(ctx, headers, q);
    let session_id = body.get("session_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let answer = body.get("answer").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let state = ctx.app.state::<AppState>();
    match crate::interactive::deliver_answer(&state, &session_id, &answer) {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err_response(e),
    }
}

// ------------------------------------------------------------------------------------------
// LAN address discovery (for the desktop status command)
// ------------------------------------------------------------------------------------------

fn is_private_v4(ip: &Ipv4Addr) -> bool {
    ip.is_private() && !ip.is_loopback() && !ip.is_link_local()
}

/// Collect private LAN IPv4 addresses, loopback excluded, deduplicated.
pub fn lan_ips() -> Vec<String> {
    use local_ip_address::{list_afinet_netifas, local_ip};
    let mut out: Vec<String> = Vec::new();

    if let Ok(IpAddr::V4(v4)) = local_ip() {
        if is_private_v4(&v4) {
            out.push(v4.to_string());
        }
    }
    if let Ok(list) = list_afinet_netifas() {
        for (_name, ip) in list {
            if let IpAddr::V4(v4) = ip {
                if is_private_v4(&v4) {
                    let s = v4.to_string();
                    if !out.contains(&s) {
                        out.push(s);
                    }
                }
            }
        }
    }
    out
}

/// Assemble the desktop status payload. Reads the port (running port if live, else settings)
/// and the token; builds `http://<ip>:<port>/?token=<token>` URLs. Returning the raw token is
/// intentional — this is invoked only by the trusted desktop webview.
pub fn status(app: &AppHandle) -> Value {
    let state = app.state::<AppState>();
    let running = is_running();
    let port = running_port().unwrap_or_else(|| read_port(&state));
    let token = ensure_token(&state).unwrap_or_default();
    let ips = lan_ips();
    let urls: Vec<String> = ips
        .iter()
        .map(|ip| format!("http://{}:{}/?token={}", ip, port, token))
        .collect();
    json!({
        "running": running,
        "port": port,
        "token": token,
        "lan_ips": ips,
        "urls": urls,
    })
}

/// Generate and persist a fresh token, restarting the server if it is running so the new
/// token takes effect. Returns the new token.
pub fn regenerate_token(app: &AppHandle) -> Result<String, String> {
    let state = app.state::<AppState>();
    let tok = generate_token();
    write_setting(&state, "mobile/server_token", &tok)?;
    drop(state);
    if is_running() {
        stop();
        // Re-start with the new token; enabled flag is re-persisted by start().
        start(app.clone())?;
    }
    Ok(tok)
}

// ------------------------------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq(b"abcdef", b"abcdef"));
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(
            b"0123456789abcdef0123456789abcdef",
            b"0123456789abcdef0123456789abcdef"
        ));
    }

    #[test]
    fn constant_time_eq_rejects_differences() {
        assert!(!constant_time_eq(b"abcdef", b"abcdeg"));
        assert!(!constant_time_eq(b"abc", b"abcd")); // length mismatch
        assert!(!constant_time_eq(b"abcd", b"abc"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(!constant_time_eq(b"Xbcdef", b"abcdef")); // first byte differs
    }

    #[test]
    fn clamp_port_bounds() {
        assert_eq!(clamp_port(0), 1024);
        assert_eq!(clamp_port(1023), 1024);
        assert_eq!(clamp_port(1024), 1024);
        assert_eq!(clamp_port(8737), 8737);
        assert_eq!(clamp_port(65535), 65535);
    }

    #[test]
    fn generated_token_is_32_hex() {
        let t = generate_token();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        // Two consecutive tokens should differ.
        assert_ne!(t, generate_token());
    }

    #[test]
    fn private_v4_classification() {
        assert!(is_private_v4(&Ipv4Addr::new(192, 168, 1, 5)));
        assert!(is_private_v4(&Ipv4Addr::new(10, 0, 0, 1)));
        assert!(is_private_v4(&Ipv4Addr::new(172, 16, 3, 4)));
        assert!(!is_private_v4(&Ipv4Addr::new(127, 0, 0, 1))); // loopback
        assert!(!is_private_v4(&Ipv4Addr::new(8, 8, 8, 8))); // public
        assert!(!is_private_v4(&Ipv4Addr::new(169, 254, 1, 1))); // link-local
    }
}
