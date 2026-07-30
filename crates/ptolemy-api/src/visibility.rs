// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Per-dataset access, enforced in two middlewares: who may read a dataset's
//! content, and who may write to it.
//!
//! A private dataset's content is only served to a caller holding a permission
//! row on it (or one of its branches), or to an instance admin. This runs as a
//! layer rather than a per-handler check because reads resolve a dataset in
//! dozens of places: handlers that build their own SQL, that take a branch id in
//! the path, that take one in a query parameter. What they all share is the id
//! itself, so the layer resolves every uuid the request names.
//!
//! An id may name a dataset or any of the things that belong to one: a branch, a
//! changeset, a merge request, a feature, a raster or point cloud catalog and its
//! tiles or patches, an attachment, a network, an LRS route, a symbology or label
//! rule, a domain, a subtype, an attribute rule, a trajectory, a topology rule, a
//! webhook, a relationship class or record. The store's
//! `private_datasets_for_ids` resolves all of them to a dataset. A request that
//! names none of them has no dataset in scope and passes, and so does a request
//! that names only public ones.
//!
//! A relationship class spans two datasets and resolves to both, which is why
//! `allowed` requires every private dataset in scope to be granted rather than
//! any one of them.
//!
//! Unauthorized reads answer 404, not 403, so a private dataset id cannot be
//! confirmed by probing.
//!
//! [`write_middleware`] is the same idea for mutations. It runs the store's write
//! ladder against whatever the request is aimed at, so a route is guarded by
//! being routed through the layer rather than by its handler remembering to
//! check. It runs inside [`visibility_middleware`], so a caller who cannot read
//! a private dataset still gets 404 rather than a 403 that confirms the id.

use axum::{
    extract::{Request, State},
    http::{StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
};
use ptolemy_storage::StoreError;
use uuid::Uuid;

use crate::{
    AppState, Claims,
    auth::{AuthEnabled, needs_write_grant},
    errors::store_error_status,
};

/// Every uuid the request names, in the path or in a query value.
fn referenced_ids(uri: &axum::http::Uri) -> Vec<Uuid> {
    let path_ids = uri
        .path()
        .split('/')
        .filter_map(|s| Uuid::parse_str(s).ok());
    let query_ids = uri
        .query()
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter_map(|(_, value)| Uuid::parse_str(value).ok());
    let mut ids: Vec<Uuid> = path_ids.chain(query_ids).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({"error": "not found"})),
    )
        .into_response()
}

/// The rule itself: may this caller read the datasets these ids name?
async fn allowed(
    store: &AppState,
    claims: Option<&Claims>,
    ids: &[Uuid],
) -> Result<bool, StoreError> {
    if ids.is_empty() {
        return Ok(true);
    }
    if claims.is_some_and(Claims::can_admin) {
        return Ok(true);
    }
    let private = store.private_datasets_for_ids(ids).await?;
    if private.is_empty() {
        return Ok(true);
    }
    let Some(user_id) = claims.map(|c| c.sub.as_str()) else {
        return Ok(false);
    };
    let readable = store.readable_datasets(&private, user_id).await?;
    // every private dataset the request touches has to be granted, not just one
    Ok(private.iter().all(|id| readable.contains(id)))
}

pub async fn visibility_middleware(
    State(store): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    // dev mode: no verified identity exists, so there is nothing to enforce
    // against and enforcing would lock the dev flow out of its own data
    if request.extensions().get::<AuthEnabled>().is_none() {
        return next.run(request).await;
    }

    let ids = referenced_ids(request.uri());
    match allowed(&store, request.extensions().get::<Claims>(), &ids).await {
        Ok(true) => next.run(request).await,
        Ok(false) => not_found(),
        Err(e) => {
            let (status, message) = store_error_status(&e);
            (status, axum::Json(serde_json::json!({"error": message}))).into_response()
        }
    }
}

/// The same rule for a handler whose dataset scope arrives in the request body,
/// where [`visibility_middleware`] cannot see it. Denial is a `NotFound`, so the
/// handler's own error mapping answers 404 exactly like the layer does.
pub async fn ensure_readable(
    store: &AppState,
    actor: &crate::Actor,
    ids: &[Uuid],
) -> Result<(), StoreError> {
    if !actor.enforces() {
        return Ok(());
    }
    if allowed(store, actor.claims(), ids).await? {
        Ok(())
    } else {
        Err(StoreError::NotFound("not found".into()))
    }
}

// ─── Writes ─────────────────────────────────────────────────────────

/// The id a mutation is aimed at: the first one in the path.
///
/// This rests on a convention the whole API already follows, and only that one:
/// the resource being written is the first id in the path. Later path ids are
/// either sub-resources of the same dataset (a feature, a lock) or read-only
/// inputs, and query ids are filters. Taking only the first is what keeps
/// `POST /branches/{target}/merge/{source}` from demanding a write grant on the
/// branch it is merging *from*, which the store's own ladder never asked for.
fn write_target_id(uri: &Uri) -> Option<Uuid> {
    uri.path().split('/').find_map(|s| Uuid::parse_str(s).ok())
}

/// Refuse any mutation the caller is not allowed to make against the dataset or
/// branch it names, using the store's ladder so the rule stays in one place.
pub async fn write_middleware(
    State(store): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    // dev mode: no verified identity, so `Writer::Unenforced` would skip every
    // check anyway and running the resolver would only cost a query
    if request.extensions().get::<AuthEnabled>().is_none() {
        return next.run(request).await;
    }
    if !needs_write_grant(request.method(), request.uri().path()) {
        return next.run(request).await;
    }
    let Some(id) = write_target_id(request.uri()) else {
        // nothing named, so nothing to attribute the write to: `POST /datasets`
        // is the case that matters, and it grants its creator on the way out
        return next.run(request).await;
    };

    let actor = crate::Actor::from_extensions(request.extensions());
    match store.ensure_id_writable(id, &actor.writer()).await {
        Ok(()) => next.run(request).await,
        Err(e) => {
            let (status, message) = store_error_status(&e);
            (status, axum::Json(serde_json::json!({"error": message}))).into_response()
        }
    }
}

/// The write ladder for a handler whose target arrives in the request body,
/// where [`write_middleware`] cannot see it. The mirror of [`ensure_readable`].
pub async fn ensure_writable(
    store: &AppState,
    actor: &crate::Actor,
    ids: &[Uuid],
) -> Result<(), StoreError> {
    let writer = actor.writer();
    for id in ids {
        store.ensure_id_writable(*id, &writer).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    fn ids_of(uri: &str) -> Vec<Uuid> {
        referenced_ids(&uri.parse().unwrap())
    }

    #[test]
    fn finds_path_and_query_ids() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        assert_eq!(ids_of(&format!("/api/v1/branches/{a}/features")), vec![a]);
        assert_eq!(
            ids_of(&format!("/api/v1/branches/{a}/tiles/1/2/3")),
            vec![a]
        );
        // sync and the vertical listings take the branch as a query parameter
        let mut expected = vec![a, b];
        expected.sort_unstable();
        assert_eq!(
            ids_of(&format!(
                "/api/v1/sync/pull?branch_id={a}&since_changeset={b}"
            )),
            expected
        );
    }

    #[test]
    fn ignores_paths_without_ids() {
        assert!(ids_of("/api/v1/datasets").is_empty());
        assert!(ids_of("/api/v1/health").is_empty());
        assert!(ids_of("/api/v1/topologies/roads/edges?limit=10").is_empty());
    }

    #[test]
    fn dedups_repeated_ids() {
        let a = Uuid::now_v7();
        assert_eq!(ids_of(&format!("/api/v1/diff/{a}/{a}")), vec![a]);
    }

    fn target_of(uri: &str) -> Option<Uuid> {
        write_target_id(&uri.parse().unwrap())
    }

    /// The convention the write gate rests on, stated as a test: the resource
    /// being written is the first id in the path.
    #[test]
    fn write_target_is_the_first_path_id() {
        let target = Uuid::now_v7();
        let source = Uuid::now_v7();
        assert_eq!(
            target_of(&format!("/api/v1/branches/{target}/merge/{source}")),
            Some(target)
        );
        assert_eq!(
            target_of(&format!(
                "/api/v1/branches/{target}/conflicts/{source}/resolve-merge"
            )),
            Some(target)
        );
        let branch = Uuid::now_v7();
        let feature = Uuid::now_v7();
        assert_eq!(
            target_of(&format!(
                "/api/v1/branches/{branch}/features/{feature}/attachments"
            )),
            Some(branch)
        );
    }

    #[test]
    fn write_target_ignores_query_ids() {
        let a = Uuid::now_v7();
        // sync push takes its branch in the body, so nothing in the path or
        // query is the target and store.commit stays the only guard
        assert_eq!(target_of(&format!("/api/v1/sync/push?trace={a}")), None);
        assert_eq!(target_of("/api/v1/datasets"), None);
    }

    fn gated(method: &str, path: &str) -> bool {
        needs_write_grant(&Method::from_bytes(method.as_bytes()).unwrap(), path)
    }

    #[test]
    fn reads_are_never_write_gated() {
        assert!(!gated("GET", "/api/v1/datasets/x/tags"));
        assert!(!gated("HEAD", "/api/v1/datasets/x/tags"));
    }

    #[test]
    fn mutations_are_gated_by_default() {
        for (method, path) in [
            ("POST", "/api/v1/datasets/x/tags"),
            ("DELETE", "/api/v1/datasets/x/tags/y"),
            ("PUT", "/api/v1/datasets/x/metadata"),
            ("PUT", "/api/v1/datasets/x/schema"),
            ("POST", "/api/v1/datasets/x/schema/migrations"),
            ("PUT", "/api/v1/reviews/x/approve"),
            ("POST", "/api/v1/reviews/x/comments"),
            ("PUT", "/api/v1/symbology/x"),
            ("DELETE", "/api/v1/labels/x"),
            ("DELETE", "/api/v1/domains/x"),
            ("PUT", "/api/v1/attribute-rules/x"),
            ("POST", "/api/v1/networks/x/edges"),
            ("POST", "/api/v1/routes/x/events"),
            ("POST", "/api/v1/datasets/x/events"),
            ("POST", "/api/v1/branches/x/h3/index"),
            ("POST", "/api/v1/branches/x/similarity/embed"),
            ("POST", "/api/v1/branches/x/locks"),
            ("POST", "/api/v1/datasets/x/trajectories"),
            ("POST", "/api/v1/qgis/branches/x/conflicts/resolve"),
        ] {
            assert!(gated(method, path), "{method} {path} should be gated");
        }
    }

    #[test]
    fn compute_only_posts_are_exempt() {
        for path in [
            "/api/v1/branches/x/geoprocessing/clip",
            "/api/v1/branches/x/geoprocessing/voronoi",
            "/api/v1/branches/x/3d/extrude",
            "/api/v1/branches/x/3d/minkowski-sum",
            "/api/v1/networks/x/trace",
            "/api/v1/networks/x/shortest-path",
            "/api/v1/networks/x/astar",
            "/api/v1/networks/x/isochrone",
            "/api/v1/networks/x/tsp",
            "/api/v1/pointclouds/x/query",
            "/api/v1/pointclouds/x/profile",
            "/api/v1/attribute-rules/x/validate",
            "/api/v1/topologies/roads/validate",
            "/api/v1/branches/x/similarity/search",
            "/api/v1/branches/x/similarity/cluster",
            "/api/v1/branches/x/h3/compact",
            "/api/v1/branches/x/transform",
            "/api/v1/branches/x/features/intersects",
            "/api/v1/branches/x/features/within",
            "/api/v1/branches/x/features/filter",
            "/api/v1/trajectories/x/simplify",
            "/api/v1/datasets/x/trajectories/nearest",
            "/api/v1/parcels/split",
            "/api/v1/parcels/merge",
            "/api/v1/surveys/compare",
            "/api/v1/coverage/simulate",
            "/api/v1/incidents/evacuate",
        ] {
            assert!(!gated("POST", path), "{path} writes nothing");
        }
    }

    /// The near-miss pairs, so a broadened suffix cannot quietly exempt a write.
    #[test]
    fn lookalike_write_routes_stay_gated() {
        assert!(gated("POST", "/api/v1/branches/x/merge/y"));
        assert!(gated("POST", "/api/v1/incidents"));
        assert!(gated("POST", "/api/v1/branches/x/reproject"));
        assert!(gated("POST", "/api/v1/branches/x/import/geojson"));
    }

    /// Grant management is gated harder elsewhere, and the ladder would deny the
    /// dataset admin who needs it most.
    #[test]
    fn permission_routes_are_left_to_rbac() {
        assert!(!gated("POST", "/api/v1/datasets/x/permissions"));
        assert!(!gated("POST", "/api/v1/branches/x/permissions"));
        assert!(!gated("DELETE", "/api/v1/branches/x/permissions/bob"));
    }
}
