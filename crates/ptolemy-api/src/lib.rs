// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

pub mod analytics;
pub mod arcgis;
pub mod attachments;
pub mod auth;
pub mod cartography;
pub mod catalog;
pub mod compaction;
pub mod conflicts;
pub mod cql2;
pub mod delivery;
pub mod domains;
pub mod email;
pub mod errors;
pub mod formats;
pub mod geoprocessing;
pub mod h3;
pub mod jobs;
pub mod locks;
pub mod lrs;
pub mod metrics;
pub mod network;
pub mod ogc;
pub mod oidc;
pub mod pointcloud;
pub mod project_state;
pub mod qgis;
pub mod quality;
pub mod raster;
pub mod rate_limit;
pub mod rbac;
pub mod real_estate;
pub mod relationships;
pub mod replication;
pub mod review;
pub mod room_relay;
pub mod routes;
pub mod schema_evolution;
pub mod sfcgal;
pub mod sse;
pub mod stac;
pub mod sync;
pub mod telemetry;
pub mod topology;
pub mod trajectory;
pub mod vector_search;
pub mod verticals;
pub mod visibility;
pub mod webhook;
pub mod workspace;
pub mod ws;

use axum::extract::Request;
use axum::http::Uri;
use axum::response::Html;
use axum::routing::get;
use axum::{Extension, Router, middleware};
use ptolemy_storage::PgStore;
use std::sync::Arc;
use tower_http::trace::TraceLayer;

pub use auth::{
    Access, Actor, AuthConfig, AuthEnabled, Claims, Role, classify, generate_token,
    generate_token_from_env,
};
pub use delivery::{DeliveryJob, DeliverySender, spawn_delivery_worker};
pub use email::EmailConfig;
pub use jobs::BackgroundJobs;
pub use metrics::{init_metrics, record_domain_event};
pub use oidc::OidcConfig;
pub use room_relay::RoomRelay;
pub use sse::SseBroadcast;
pub use telemetry::init_telemetry;
pub use ws::EventBus;

pub type AppState = Arc<PgStore>;

/// What a redacted `token` value reads as in a trace.
const REDACTED: &str = "REDACTED";

/// The uri as the request span records it, with any `token` value taken out.
///
/// The ArcGIS facade takes its credential in the query string, because the
/// Geoservices protocol has no header for one (see `auth::request_token`), and
/// the request span carries the uri, so `tower_http` at debug level would
/// otherwise put live tokens in the log. Redacted here rather than in the
/// facade: this span is what records the uri, and it records it for every route.
///
/// `token` is the name the auth layer reads a credential from, so it is the name
/// taken out here, matched without regard to case and only as the whole name.
/// Everything else in the uri is left alone, so a trace is still worth reading.
fn traced_uri(uri: &Uri) -> String {
    let whole = uri.to_string();
    let Some((before, query)) = whole.split_once('?') else {
        return whole;
    };
    let redacted = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            // token is the arcgis credential, code the oidc authorization code
            Some((name, _))
                if name.eq_ignore_ascii_case("token") || name.eq_ignore_ascii_case("code") =>
            {
                format!("{name}={REDACTED}")
            }
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{before}?{redacted}")
}

/// The embedded review UI HTML.
const REVIEW_UI_HTML: &str = include_str!("../../../docs/review.html");
/// The embedded conflict resolution UI HTML.
const CONFLICTS_UI_HTML: &str = include_str!("../../../docs/conflicts.html");

/// Build the router, reading auth config from the environment. Callers that
/// serve this router must resolve the config with
/// [`AuthConfig::from_env_strict`] and use [`app_with_auth`], so a missing
/// secret refuses to start instead of opening every write endpoint.
pub fn app(state: AppState) -> Router {
    app_with_auth(state, AuthConfig::from_env())
}

pub fn app_with_auth(state: AppState, auth: AuthConfig) -> Router {
    app_with_auth_and_email(state, auth, EmailConfig::from_env())
}

/// Same, with invitation mail configured by the caller rather than the
/// environment. A test points this at an SMTP server whose port it only learns
/// once that server is listening.
pub fn app_with_auth_and_email(state: AppState, auth: AuthConfig, email: EmailConfig) -> Router {
    let event_bus = Arc::new(EventBus::new(1024));
    let sse_broadcast = Arc::new(SseBroadcast::new(4096));
    let room_relay = Arc::new(RoomRelay::new());
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
        .nest("/api/v1", geoprocessing::geoprocessing_routes())
        .nest("/api/v1", ogc::ogc_routes())
        .nest("/api/v1", locks::lock_routes())
        .nest("/api/v1", catalog::catalog_routes())
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
        .nest("/api/v1", attachments::attachment_routes())
        .nest("/api/v1", schema_evolution::schema_routes())
        .nest("/api/v1", replication::replication_routes())
        .nest("/api/v1", rbac::rbac_routes())
        .nest("/api/v1", workspace::workspace_routes())
        .nest("/api/v1", project_state::project_state_routes())
        .nest("/api/v1", real_estate::real_estate_routes())
        .nest("/api/v1", verticals::vertical_routes())
        .nest("/api/v1", compaction::compaction_routes())
        .nest("/api/v1", sse::sse_routes(sse_broadcast))
        // not under /api/v1: an Esri client builds every URL from
        // /arcgis/rest/services, which is where it expects to find it
        .merge(arcgis::arcgis_routes())
        .merge(oidc::oidc_routes())
        .nest("/ws", ws::ws_routes(event_bus))
        .nest("/ws/rooms", room_relay::room_routes(room_relay))
        .merge(metrics::metrics_routes(prom_handle))
        .layer(Extension(email))
        .layer(middleware::from_fn(metrics::metrics_middleware))
        // inside visibility, so a caller who cannot read a private dataset gets
        // its 404 rather than a 403 that would confirm the id exists
        .layer(middleware::from_fn_with_state(
            state.clone(),
            visibility::write_middleware,
        ))
        // inside the auth layer, so the token is already decoded and a bad one
        // is a 401 before visibility ever runs
        .layer(middleware::from_fn_with_state(
            state.clone(),
            visibility::visibility_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            auth::AuthState::new(auth, state.clone()),
            auth::auth_middleware,
        ))
        // the default span records the uri as it arrived, which on the arcgis
        // facade carries the caller's token: see traced_uri
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request| {
                tracing::debug_span!(
                    "request",
                    method = %request.method(),
                    uri = %traced_uri(request.uri()),
                    version = ?request.version(),
                )
            }),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn traced(uri: &str) -> String {
        traced_uri(&uri.parse::<Uri>().unwrap())
    }

    /// The one thing a trace must not carry. A token is a bearer credential, so
    /// a log line holding one is a log line anyone who reads it can write with.
    #[test]
    fn the_token_value_never_reaches_the_span() {
        let secret = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhIn0.sig";
        for uri in [
            &format!("/arcgis/rest/services/x/FeatureServer/0/query?token={secret}"),
            &format!("/arcgis/rest/services?f=json&token={secret}"),
            &format!("/arcgis/rest/services?token={secret}&f=json"),
            &format!("/arcgis/rest/services?TOKEN={secret}"),
            &format!("/arcgis/rest/services?token={secret}&token={secret}"),
            &format!("https://host/arcgis/rest/services?token={secret}"),
            &format!("/auth/callback?code={secret}&state=xyz"),
            &format!("/auth/callback?CODE={secret}"),
        ] {
            let out = traced(uri);
            assert!(!out.contains(secret), "{uri} traced as {out}");
            assert!(out.contains(REDACTED), "{uri} traced as {out}");
        }
    }

    /// Everything else in the uri is untouched: a trace nobody can read a route
    /// off is a trace that costs more than it is worth.
    #[test]
    fn the_rest_of_the_uri_is_left_alone() {
        assert_eq!(traced("/api/v1/datasets"), "/api/v1/datasets");
        assert_eq!(
            traced("/arcgis/rest/services/x/FeatureServer/0/query?f=json&where=pop%3D1&token=abc"),
            "/arcgis/rest/services/x/FeatureServer/0/query?f=json&where=pop%3D1&token=REDACTED"
        );
        assert_eq!(
            traced("https://host:8080/api/v1/datasets?limit=5"),
            "https://host:8080/api/v1/datasets?limit=5"
        );
        // a parameter that only ends in the word is not the credential, and the
        // auth layer does not read it as one either
        assert_eq!(
            traced("/api/v1/datasets?sessiontoken=abc"),
            "/api/v1/datasets?sessiontoken=abc"
        );
        // nothing to redact, and nothing to break on
        assert_eq!(traced("/x?token"), "/x?token");
        assert_eq!(traced("/x?"), "/x?");
        assert_eq!(traced("/x?token="), "/x?token=REDACTED");
    }
}
