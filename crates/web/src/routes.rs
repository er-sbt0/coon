use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::{
    extract::{ws::WebSocketUpgrade, State},
    response::IntoResponse,
    routing::get,
    Router,
};
use tokio::sync::Mutex;

use ::core::{AppEvent, CoreApp};

use crate::protocol::{ClientMsg, ServerMsg};
use crate::session::WebSession;

pub struct AppState {
    core: Arc<Mutex<CoreApp>>,
}

impl AppState {
    pub fn new(core: CoreApp) -> Self {
        Self {
            core: Arc::new(Mutex::new(core)),
        }
    }

    fn session(&self) -> WebSession {
        WebSession::new(Arc::clone(&self.core))
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/ws", get(ws_handler))
        .nest_service(
            "/",
            tower_http::services::ServeDir::new("crates/web/static"),
        )
        .with_state(Arc::new(state))
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    log::info!("[web] WebSocket upgrade requested");
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    log::info!("[web] WebSocket client connected");
    let session = state.session();

    // Send an initial snapshot so the client has current state immediately.
    log::debug!("[web] sending initial snapshot to new client");
    let msg = session.snapshot_msg().await;
    send_json(&mut socket, &msg).await;

    // Track whether we have already sent the post-load resnapshot triggered by
    // LspProgress(Completed). The status stays Completed forever after loading,
    // so without this gate every tick would resnapshot at 10 Hz indefinitely.
    let mut lsp_completed_sent = false;

    let mut tick_interval = tokio::time::interval(std::time::Duration::from_millis(100));

    loop {
        tokio::select! {
            _ = tick_interval.tick() => {
                let events = { session.core.lock().await.tick() };
                // Only log ticks that produce events beyond routine LspProgress noise.
                let has_meaningful = events.iter().any(|e| !matches!(e, AppEvent::LspProgress(_)));
                if has_meaningful {
                    log::debug!("[web] tick produced {} event(s): {:?}", events.len(), events);
                }
                handle_events(&mut socket, &session, events, &mut lsp_completed_sent).await;
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMsg>(&text) {
                            Ok(ClientMsg::Command { command }) => {
                                log::info!("[web] command received: {:?}", command);
                                let events = { session.core.lock().await.dispatch(command) };
                                log::debug!("[web] dispatch produced {} event(s): {:?}", events.len(), events);
                                handle_events(&mut socket, &session, events, &mut lsp_completed_sent).await;
                            }
                            Err(e) => {
                                log::warn!(
                                    "[web] failed to parse client message: {} | raw: {}",
                                    e,
                                    &text[..text.len().min(200)]
                                );
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) => {
                        log::info!("[web] WebSocket client sent close frame");
                        break;
                    }
                    Some(Err(e)) => {
                        log::warn!("[web] WebSocket receive error: {}", e);
                        break;
                    }
                    None => break,
                    _ => {}
                }
            }
        }
    }
    log::info!("[web] WebSocket client disconnected");
}

async fn handle_events(
    socket: &mut WebSocket,
    session: &WebSession,
    events: Vec<AppEvent>,
    lsp_completed_sent: &mut bool,
) {
    let mut layout_dirty = false;
    let mut resnapshot = false;
    for event in &events {
        match event {
            AppEvent::LayoutDirty => {
                layout_dirty = true;
            }
            // LspProgress(Completed) stays set forever once loading finishes —
            // only resnapshot on the first occurrence (the actual transition).
            AppEvent::LspProgress(model::LspLoadPhase::Completed) => {
                if !*lsp_completed_sent {
                    log::info!("[web] LspProgress(Completed) — scheduling one-time resnapshot");
                    *lsp_completed_sent = true;
                    resnapshot = true;
                }
            }
            // Graph is now fully populated — push a fresh snapshot so the
            // client can show the function picker with all loaded symbols.
            AppEvent::GraphSymbolsUpdated { .. } => {
                log::info!("[web] {:?} — scheduling resnapshot", event);
                resnapshot = true;
                // Do NOT also send an Event frame here — the snapshot that
                // follows already supersedes it and a double-send can cause
                // the client to update state inconsistently mid-flight.
            }
            _ => {
                log::debug!("[web] forwarding event to client: {:?}", event);
                send_json(
                    socket,
                    &ServerMsg::Event {
                        event: event.clone(),
                    },
                )
                .await;
            }
        }
    }
    if resnapshot || layout_dirty {
        log::debug!(
            "[web] sending snapshot (layout_dirty={layout_dirty}, resnapshot={resnapshot})"
        );
        // Always send a full snapshot on layout changes so the client has
        // the updated graph structure (nodes + edges) alongside new positions.
        // A Layout-only message can't tell the client which nodes to add/remove.
        let msg = session.snapshot_msg().await;
        send_json(socket, &msg).await;
    }
}

async fn send_json<T: serde::Serialize>(socket: &mut WebSocket, msg: &T) {
    match serde_json::to_string(msg) {
        Ok(text) => {
            log::debug!("[web] → sending {} bytes to client", text.len());
            if let Err(e) = socket.send(Message::Text(text)).await {
                log::warn!("[web] failed to send message to client: {}", e);
            }
        }
        Err(e) => {
            log::error!("[web] failed to serialise server message: {}", e);
        }
    }
}
