// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

pub mod analytics;
pub mod auth;
pub mod cartography;
pub mod catalog;
pub mod conflicts;
pub mod cql2;
pub mod delivery;
pub mod domains;
pub mod formats;
pub mod grpc;
pub mod h3;
pub mod jobs;
pub mod locks;
pub mod lrs;
pub mod metrics;
pub mod network;
pub mod ogc;
pub mod oidc;
pub mod pointcloud;
pub mod qgis;
pub mod quality;
pub mod raster;
pub mod rate_limit;
pub mod relationships;
pub mod review;
pub mod routes;
pub mod sfcgal;
pub mod sse;
pub mod stac;
pub mod sync;
pub mod telemetry;
pub mod tenant;
pub mod topology;
pub mod trajectory;
pub mod vector_search;
pub mod webhook;
pub mod ws;

use axum::response::Html;
use axum::routing::get;
use axum::{Router, middleware};
use ptolemy_storage::PgStore;
use std::sync::Arc;
use tower_http::trace::TraceLayer;

pub use auth::{AuthConfig, Claims, Role, generate_token, generate_token_from_env};
pub use delivery::{DeliveryJob, DeliverySender, spawn_delivery_worker};
pub use jobs::BackgroundJobs;
pub use metrics::{init_metrics, record_domain_event};
pub use oidc::OidcConfig;
pub use sse::SseBroadcast;
pub use telemetry::init_telemetry;
pub use ws::EventBus;

pub type AppState = Arc<PgStore>;

/// The embedded review UI HTML.
const REVIEW_UI_HTML: &str = include_str!("../../../docs/review.html");
/// The embedded conflict resolution UI HTML.
const CONFLICTS_UI_HTML: &str = include_str!("../../../docs/conflicts.html");

pub fn app(state: AppState) -> Router {
    let event_bus = Arc::new(EventBus::new(1024));
    let sse_broadcast = Arc::new(SseBroadcast::new(4096));
    let prom_handle = init_metrics();

    Router::new()
        .route("/review", get(|| async { Html(REVIEW_UI_HTML) }))
        .route("/conflicts", get(|| async { Html(CONFLICTS_UI_HTML) }))
        .nest("/api/v1", routes::v1_routes())
        .nest("/api/v1", sync::sync_routes())
        .nest("/api/v1", review::review_routes())
        .nest("/api/v1", quality::quality_routes())
        .nest("/api/v1", webhook::webhook_routes())
        .nest("/api/v1", analytics::analytics_routes())
        .nest("/api/v1", ogc::ogc_routes())
        .nest("/api/v1", locks::lock_routes())
        .nest("/api/v1", catalog::catalog_routes())
        .nest("/api/v1", tenant::tenant_routes())
        .nest("/api/v1", conflicts::conflict_routes())
        .nest("/api/v1", network::network_routes())
        .nest("/api/v1", lrs::lrs_routes())
        .nest("/api/v1", raster::raster_routes())
        .nest("/api/v1", domains::domain_routes())
        .nest("/api/v1", relationships::relationship_routes())
        .nest("/api/v1", cartography::cartography_routes())
        .nest("/api/v1", topology::topology_routes())
        .nest("/api/v1", sfcgal::sfcgal_routes())
        .nest("/api/v1", h3::h3_routes())
        .nest("/api/v1", vector_search::vector_routes())
        .nest("/api/v1", pointcloud::pointcloud_routes())
        .nest("/api/v1", trajectory::trajectory_routes())
        .nest("/api/v1", cql2::cql2_routes())
        .nest("/api/v1", stac::stac_routes())
        .nest("/api/v1", formats::format_routes())
        .nest("/api/v1", qgis::qgis_routes())
        .nest("/api/v1", sse::sse_routes(sse_broadcast))
        .merge(oidc::oidc_routes())
        .nest("/ws", ws::ws_routes(event_bus))
        .merge(metrics::metrics_routes(prom_handle))
        .layer(middleware::from_fn(metrics::metrics_middleware))
        .layer(middleware::from_fn(auth::auth_middleware))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
