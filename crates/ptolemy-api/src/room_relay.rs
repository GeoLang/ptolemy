// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Ephemeral room-based relay for real-time collaboration.
//!
//! Clients connect to `/ws/rooms/{room_id}` and every JSON message sent by one
//! participant is broadcast to all other participants in the same room.  No
//! messages are persisted — this is a pure in-memory pub/sub relay designed for
//! cursor sharing, camera-state synchronisation (view sync / "follow mode"),
//! presence, and chat.
//!
//! Rooms are created on first connection and dropped when the last client
//! disconnects.

use axum::{
    Router,
    extract::{
        Path, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::get,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast};

/// Capacity of the per-room broadcast channel.
const ROOM_CAPACITY: usize = 256;

/// Shared state holding all active rooms.
#[derive(Clone, Default)]
pub struct RoomRelay {
    rooms: Arc<Mutex<HashMap<String, broadcast::Sender<String>>>>,
}

impl RoomRelay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get or create the broadcast sender for `room_id`.
    async fn get_or_create(&self, room_id: &str) -> broadcast::Sender<String> {
        let mut map = self.rooms.lock().await;
        if let Some(tx) = map.get(room_id) {
            // If all receivers have been dropped the channel is dead; recreate.
            if tx.receiver_count() > 0 {
                return tx.clone();
            }
        }
        let (tx, _) = broadcast::channel(ROOM_CAPACITY);
        map.insert(room_id.to_owned(), tx.clone());
        tx
    }

    /// Remove a room if it has no remaining receivers.
    async fn maybe_cleanup(&self, room_id: &str) {
        let mut map = self.rooms.lock().await;
        if let Some(tx) = map.get(room_id)
            && tx.receiver_count() == 0
        {
            map.remove(room_id);
        }
    }

    /// Number of active rooms (for metrics / tests).
    pub async fn room_count(&self) -> usize {
        let map = self.rooms.lock().await;
        map.len()
    }
}

/// Build the room relay router.  Mount it under `/ws/rooms`.
pub fn room_routes(relay: Arc<RoomRelay>) -> Router<crate::AppState> {
    Router::new()
        .route("/{room_id}", get(ws_room_handler))
        .with_state(relay)
}

async fn ws_room_handler(
    ws: WebSocketUpgrade,
    Path(room_id): Path<String>,
    State(relay): State<Arc<RoomRelay>>,
) -> impl IntoResponse {
    // auth_middleware already rejected an anonymous handshake. echoing the
    // marker is required for a browser to accept the 101, and the token itself
    // is never echoed
    ws.protocols([crate::auth::BEARER_SUBPROTOCOL])
        .on_upgrade(move |socket| handle_room_socket(socket, room_id, relay))
}

async fn handle_room_socket(socket: WebSocket, room_id: String, relay: Arc<RoomRelay>) {
    let tx = relay.get_or_create(&room_id).await;
    let mut rx = tx.subscribe();

    let (mut sink, mut stream) = socket.split();

    use futures::SinkExt;
    use tokio::sync::broadcast::error::RecvError;

    // Spawn a task to forward broadcast messages → WebSocket.
    let fwd_tx = tx.clone();
    let fwd_room = room_id.clone();
    let fwd_relay = relay.clone();
    let send_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    if sink.send(Message::Text(msg.into())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
        let _ = fwd_relay;
        let _ = fwd_tx;
        let _ = fwd_room;
    });

    // Read from WebSocket and broadcast to the room.
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(text) => {
                // Broadcast to all other subscribers.
                let _ = tx.send(text.to_string());
            }
            Message::Binary(data) => {
                if let Ok(text) = String::from_utf8(data.to_vec()) {
                    let _ = tx.send(text);
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    send_task.abort();
    // Drop our reference and clean up empty rooms.
    drop(tx);
    relay.maybe_cleanup(&room_id).await;
}

// We need `StreamExt` for `stream.next()` and `SinkExt` for `sink.send()`.
use futures::StreamExt;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relay_creates_and_cleans_rooms() {
        let relay = RoomRelay::new();
        assert_eq!(relay.room_count().await, 0);

        // Creating a sender for a room makes it exist.
        let tx = relay.get_or_create("room-1").await;
        assert_eq!(relay.room_count().await, 1);

        // A second call for the same room reuses it.
        let tx2 = relay.get_or_create("room-1").await;
        assert_eq!(relay.room_count().await, 1);

        // Different room.
        let _tx3 = relay.get_or_create("room-2").await;
        assert_eq!(relay.room_count().await, 2);

        // Subscribe, then drop; cleanup should remove empty rooms.
        let _rx = tx.subscribe();
        drop(tx);
        drop(tx2);
        // room-1 still has _rx as a receiver proxy through the stored sender.
        // But since we dropped our tx handles, the stored sender is the only
        // one keeping the channel alive.  After _rx drops, cleanup removes it.
        drop(_rx);
        relay.maybe_cleanup("room-1").await;
        assert_eq!(relay.room_count().await, 1);
    }

    #[tokio::test]
    async fn broadcast_reaches_subscribers() {
        let relay = RoomRelay::new();
        let tx = relay.get_or_create("chat").await;
        let mut rx1 = tx.subscribe();
        let mut rx2 = tx.subscribe();

        tx.send("hello".to_string()).unwrap();

        assert_eq!(rx1.recv().await.unwrap(), "hello");
        assert_eq!(rx2.recv().await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn separate_rooms_are_isolated() {
        let relay = RoomRelay::new();
        let tx_a = relay.get_or_create("room-a").await;
        let tx_b = relay.get_or_create("room-b").await;
        let mut rx_a = tx_a.subscribe();
        let mut rx_b = tx_b.subscribe();

        tx_a.send("only-a".to_string()).unwrap();

        assert_eq!(rx_a.recv().await.unwrap(), "only-a");
        // rx_b should have nothing.
        assert!(rx_b.try_recv().is_err());
    }
}
