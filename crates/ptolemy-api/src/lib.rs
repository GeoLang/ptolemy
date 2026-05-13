// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub mod routes;

use axum::Router;
use ptolemy_storage::postgres::PgStore;
use std::sync::Arc;
use tower_http::trace::TraceLayer;

pub type AppState = Arc<PgStore>;

pub fn app(state: AppState) -> Router {
    Router::new()
        .nest("/api/v1", routes::v1_routes())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
