// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! WebSocket real-time notifications for branch updates.

use axum::{
    Router,
    extract::{
        Path, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::get,
};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::broadcast;
use uuid::Uuid;

/// Event broadcast when a branch is updated (commit, merge, etc.)
#[derive(Debug, Clone, Serialize)]
pub struct BranchEvent {
    pub branch_id: Uuid,
    pub changeset_id: Uuid,
    pub author: String,
    pub message: String,
    pub event_type: EventType,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Commit,
    Merge,
}

/// Shared broadcast channel for all branch events.
#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<BranchEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    pub fn publish(&self, event: BranchEvent) {
        let _ = self.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BranchEvent> {
        self.sender.subscribe()
    }
}

/// Create WebSocket routes with their own state (EventBus).
pub fn ws_routes(bus: Arc<EventBus>) -> Router<crate::AppState> {
    Router::new()
        .route("/branches/{branch_id}", get(ws_branch_handler))
        .with_state(bus)
}

async fn ws_branch_handler(
    ws: WebSocketUpgrade,
    Path(branch_id): Path<Uuid>,
    State(bus): State<Arc<EventBus>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, branch_id, bus))
}

async fn handle_socket(mut socket: WebSocket, branch_id: Uuid, bus: Arc<EventBus>) {
    let mut rx = bus.subscribe();

    loop {
        tokio::select! {
            Ok(event) = rx.recv() => {
                if event.branch_id == branch_id {
                    let json = serde_json::to_string(&event).unwrap();
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
