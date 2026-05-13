// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use axum::{
    Router,
    routing::get,
};
use crate::AppState;

pub fn v1_routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/datasets", get(list_datasets))
}

async fn health() -> &'static str {
    "ok"
}

async fn list_datasets() -> &'static str {
    "[]"
}
