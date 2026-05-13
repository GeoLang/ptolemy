// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Server-Sent Events (SSE) endpoint for real-time event streaming.

use axum::{
    Router,
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
    routing::get,
};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::AppState;

/// Shared broadcast channel for SSE events.
#[derive(Clone)]
pub struct SseBroadcast {
    tx: broadcast::Sender<SseEvent>,
}

#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event_type: String,
    pub data: String,
}

impl SseBroadcast {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Send an event to all connected SSE clients.
    pub fn send(&self, event_type: &str, data: &serde_json::Value) {
        let _ = self.tx.send(SseEvent {
            event_type: event_type.to_string(),
            data: data.to_string(),
        });
    }

    fn subscribe(&self) -> broadcast::Receiver<SseEvent> {
        self.tx.subscribe()
    }
}

pub fn sse_routes(broadcast: Arc<SseBroadcast>) -> Router<AppState> {
    Router::new().route(
        "/events/stream",
        get(move |state| sse_handler(state, broadcast.clone())),
    )
}

async fn sse_handler(
    State(_store): State<AppState>,
    broadcast: Arc<SseBroadcast>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = broadcast.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| {
        match result {
            Ok(sse_event) => Some(Ok(Event::default()
                .event(sse_event.event_type)
                .data(sse_event.data))),
            Err(_) => None, // lagged; skip
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}
