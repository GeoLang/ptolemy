// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Who did what to what, recorded once for every mutation instead of per
//! handler.
//!
//! This is a layer for the same reason [`crate::visibility::write_middleware`]
//! is: a write happens in dozens of handlers, and what they share is the request
//! that reached them. It sits inside the write gate, so a request the ladder
//! refused never gets here and the audited target is the id the ladder checked
//! (both read it with [`crate::visibility::write_target_id`]).
//!
//! Only mutations that answered 2xx are recorded. Recording refusals as well
//! would let an unauthenticated caller fill the table by being refused in a
//! loop, and this log is meant to say what happened to the data.
//!
//! Reads are not recorded at all. A row per GET would bury the writes. What is
//! recorded is every request with a mutating method, which is a little wider
//! than every write: the POSTs that only compute, such as
//! `/branches/{id}/geoprocessing/clip`, get a row too. Over-recording is the
//! safe direction, and the two lists that separate them
//! ([`crate::auth::needs_write_grant`] exempts the compute-only ones and the
//! permission routes alike) cannot tell them apart on their own.
//!
//! An audit write that fails is logged and dropped. The user's write has already
//! happened and its response is already built by the time this runs, so there is
//! nothing left to fail.

use axum::{
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::Response,
};
use tracing::warn;
use uuid::Uuid;

use crate::{Actor, AppState, auth::route_template, visibility::write_target_id};

/// What the `actor` column holds when there is no verified subject: auth is off,
/// or the route is public and the request carried no token.
const UNIDENTIFIED_ACTOR: &str = "unidentified";

/// What `resource_type` holds for a template whose every segment is a parameter.
/// No route is shaped like that today.
const UNKNOWN_RESOURCE: &str = "unknown";

/// Everything the row needs, taken off the request before the handler consumes
/// it.
struct Pending {
    actor: String,
    action: String,
    resource_type: String,
    resource_id: Option<Uuid>,
    /// The request path, no query. Not every target is a uuid: the ArcGIS facade
    /// names its layer by service name and a topology route by topology name, and
    /// `resource_id` is `None` for both. Without this such a row says an edit
    /// happened without saying to what.
    path: String,
}

/// The kind of thing a route acts on: the first named segment of the matched
/// template, so `/api/v1/branches/{id}/commits` is a `branches` write and
/// `/arcgis/rest/services/{layer}/FeatureServer/{id}/applyEdits` is an `arcgis`
/// one. The template, never the raw path, for the reason
/// [`crate::auth::route_template`] gives.
fn resource_type(template: &str) -> &str {
    template
        .split('/')
        .find(|segment| {
            !segment.is_empty()
                && !segment.starts_with('{')
                && *segment != "api"
                && *segment != "v1"
        })
        .unwrap_or(UNKNOWN_RESOURCE)
}

fn is_mutation(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

fn pending(request: &Request) -> Option<Pending> {
    if !is_mutation(request.method()) {
        return None;
    }
    // no template means no route matched, so the fallback 404 is about to answer
    // and nothing was written
    let template = route_template(request.extensions())?;
    let actor = Actor::from_extensions(request.extensions())
        .id()
        .unwrap_or(UNIDENTIFIED_ACTOR)
        .to_owned();
    Some(Pending {
        actor,
        action: format!("{} {}", request.method(), template),
        resource_type: resource_type(template).to_owned(),
        resource_id: write_target_id(template, request.uri().path()),
        path: request.uri().path().to_owned(),
    })
}

pub async fn audit_middleware(
    State(store): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(pending) = pending(&request) else {
        return next.run(request).await;
    };
    let response = next.run(request).await;
    if !response.status().is_success() {
        return response;
    }
    record(&store, &pending, response.status()).await;
    response
}

async fn record(store: &AppState, pending: &Pending, status: StatusCode) {
    // the query string is deliberately left out. On the ArcGIS facade it carries
    // the caller's token (see `auth::request_token`), and a bearer credential
    // must not be copied into a table an admin reads over HTTP. The path is
    // safe: no route takes a credential in a segment.
    let details = serde_json::json!({"status": status.as_u16(), "path": &pending.path});
    if let Err(e) = store
        .audit_log(
            &pending.actor,
            &pending.action,
            &pending.resource_type,
            pending.resource_id,
            &details,
            None,
        )
        .await
    {
        warn!(
            actor = %pending.actor,
            action = %pending.action,
            error = %e,
            "could not record audit entry"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_type_is_the_first_named_segment() {
        assert_eq!(resource_type("/api/v1/datasets/{id}/tags"), "datasets");
        assert_eq!(resource_type("/api/v1/branches/{id}/commits"), "branches");
        assert_eq!(resource_type("/api/v1/datasets"), "datasets");
        assert_eq!(resource_type("/api/v1/sync/push"), "sync");
        assert_eq!(
            resource_type("/arcgis/rest/services/{layer}/FeatureServer/{id}/applyEdits"),
            "arcgis"
        );
    }

    /// A caller-supplied segment can be anything, including the word `datasets`.
    /// It is never the first named one, because a template's named segments are
    /// written by the route table.
    #[test]
    fn a_parameter_is_never_the_resource_type() {
        assert_eq!(resource_type("/api/v1/{a}/{b}"), UNKNOWN_RESOURCE);
        assert_eq!(
            resource_type("/api/v1/projects/{id}/state/{key}"),
            "projects"
        );
    }

    #[test]
    fn only_mutations_are_recorded() {
        for method in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(!is_mutation(&method), "{method}");
        }
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert!(is_mutation(&method), "{method}");
        }
    }
}
