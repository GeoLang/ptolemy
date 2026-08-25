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
//! rule, a domain, a subtype, an attribute rule, a trajectory, a webhook, a
//! relationship class or record. The store's
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
//!
//! Both the exemption policy and the target id are read off the matched route
//! template, never the raw path. See [`crate::auth::route_template`] for what
//! goes wrong otherwise.

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use ptolemy_storage::{StoreError, WriteGrant};
use uuid::Uuid;

use crate::{
    AppState, Claims,
    auth::{AuthEnabled, needs_write_grant, route_template},
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

/// The id a mutation is aimed at: the request segment sitting under the first
/// `{param}` of the matched route template.
///
/// The position comes from the template rather than from scanning the raw path
/// for something uuid-shaped, so a free-text segment can never pose as the
/// target. In `/api/v1/datasets/{id}/tags/{tag}` only `{id}` is ever consulted,
/// whatever the caller puts in `{tag}`.
///
/// This rests on a convention the route tables follow throughout, and only that
/// one: the resource being written is the first parameter in the template. Later
/// ones are sub-resources of the same dataset (a feature, an attachment) or
/// read-only inputs, which is what keeps `/branches/{target_id}/merge/{source_id}`
/// from demanding a write grant on the branch it is merging *from*.
///
/// [`crate::audit`] reads the same function, so the audited target and the
/// checked target cannot drift apart.
pub(crate) fn write_target_id(template: &str, path: &str) -> Option<Uuid> {
    let at = template
        .split('/')
        .position(|segment| segment.starts_with('{'))?;
    Uuid::parse_str(path.split('/').nth(at)?).ok()
}

/// Refuse any mutation the caller is not allowed to make against the dataset or
/// branch it names, using the store's ladder so the rule stays in one place.
///
/// On the way through it puts the [`WriteGrant`] the ladder minted into the
/// request extensions. A handler that writes takes it with
/// `Extension<WriteGrant>` and hands it to the store method that does the write,
/// which reads the id it writes under off the grant. A route that never reaches
/// the ladder therefore has no grant to hand over and cannot call one of those
/// methods at all.
pub async fn write_middleware(
    State(store): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    // dev mode: no verified identity, so `Writer::Unenforced` would skip every
    // check anyway and running the resolver would only cost a query. The target
    // is still resolved and still granted, so a handler is handed the same grant
    // either way and cannot quietly come to depend on auth being on.
    let enforced = request.extensions().get::<AuthEnabled>().is_some();

    let target = {
        let Some(template) = route_template(request.extensions()) else {
            // No route matched, so the fallback is about to answer 404 and no
            // handler will run. Refusing anyway keeps the gate from ever deciding
            // policy without a template, which is the whole point of keying on one.
            if enforced && needs_write_grant(request.method(), "") {
                return denied("no route matched");
            }
            return next.run(request).await;
        };
        if !needs_write_grant(request.method(), template) {
            return next.run(request).await;
        }
        write_target_id(template, request.uri().path())
    };
    let Some(id) = target else {
        // the template names no id, so there is nothing to attribute the write
        // to: `POST /datasets` is the case that matters, and it grants its
        // creator on the way out
        return next.run(request).await;
    };

    let grant = if enforced {
        let actor = crate::Actor::from_extensions(request.extensions());
        match store.ensure_id_writable(id, &actor.writer()).await {
            Ok(grant) => grant,
            Err(e) => {
                let (status, message) = store_error_status(&e);
                return (status, axum::Json(serde_json::json!({"error": message}))).into_response();
            }
        }
    } else {
        WriteGrant::unenforced(id)
    };
    request.extensions_mut().insert(grant);
    next.run(request).await
}

fn denied(message: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({"error": message})),
    )
        .into_response()
}

/// The write ladder for a handler whose target arrives in the request body,
/// where [`write_middleware`] cannot see it. The mirror of [`ensure_readable`],
/// and the other source of a [`WriteGrant`]: a handler that names two targets
/// calls it once per target and holds a grant for each.
///
/// Unlike [`ensure_readable`] this runs even with auth off, because the ladder
/// also refuses a target that does not exist or is an external read-only table.
pub async fn ensure_writable(
    store: &AppState,
    actor: &crate::Actor,
    id: Uuid,
) -> Result<WriteGrant, StoreError> {
    store.ensure_id_writable(id, &actor.writer()).await
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

    /// The convention the write gate rests on, stated as a test: the resource
    /// being written is the first parameter in the route template.
    #[test]
    fn write_target_is_the_first_template_parameter() {
        let target = Uuid::now_v7();
        let source = Uuid::now_v7();
        assert_eq!(
            write_target_id(
                "/api/v1/branches/{target_id}/merge/{source_id}",
                &format!("/api/v1/branches/{target}/merge/{source}"),
            ),
            Some(target)
        );
        assert_eq!(
            write_target_id(
                "/api/v1/branches/{target_id}/conflicts/{source_id}/resolve-merge",
                &format!("/api/v1/branches/{target}/conflicts/{source}/resolve-merge"),
            ),
            Some(target)
        );
        let branch = Uuid::now_v7();
        let feature = Uuid::now_v7();
        assert_eq!(
            write_target_id(
                "/api/v1/branches/{branch_id}/features/{feature_id}/attachments",
                &format!("/api/v1/branches/{branch}/features/{feature}/attachments"),
            ),
            Some(branch)
        );
    }

    /// The exploit that refuted the raw-path version: a caller-supplied segment
    /// cannot become the target, whatever it is made to look like.
    #[test]
    fn a_free_text_segment_is_never_the_target() {
        let dataset = Uuid::now_v7();
        let planted = Uuid::now_v7();
        assert_eq!(
            write_target_id(
                "/api/v1/datasets/{id}/tags/{tag}",
                &format!("/api/v1/datasets/{dataset}/tags/{planted}"),
            ),
            Some(dataset)
        );
    }

    #[test]
    fn a_template_with_no_parameter_has_no_target() {
        let a = Uuid::now_v7();
        // sync push takes its branch in the body, so nothing in the path is the
        // target and store.commit stays the only guard
        assert_eq!(
            write_target_id("/api/v1/sync/push", &format!("/api/v1/sync/push?trace={a}")),
            None
        );
        assert_eq!(
            write_target_id("/api/v1/datasets", "/api/v1/datasets"),
            None
        );
    }

    /// These take route templates, the same strings the route tables register.
    /// They document intent; the authority on which mounted routes are gated is
    /// `test_every_mounted_mutating_route_is_gated_or_listed` in the integration
    /// tests, which walks the route tables instead of a hand-picked list.
    fn gated(method: &str, route: &str) -> bool {
        needs_write_grant(&Method::from_bytes(method.as_bytes()).unwrap(), route)
    }

    #[test]
    fn reads_are_never_write_gated() {
        assert!(!gated("GET", "/api/v1/datasets/{id}/tags"));
        assert!(!gated("HEAD", "/api/v1/datasets/{id}/tags"));
    }

    #[test]
    fn mutations_are_gated_by_default() {
        for (method, route) in [
            ("POST", "/api/v1/datasets/{id}/tags"),
            ("DELETE", "/api/v1/datasets/{id}/tags/{tag}"),
            ("PUT", "/api/v1/datasets/{id}/metadata"),
            ("PUT", "/api/v1/datasets/{id}/schema"),
            ("POST", "/api/v1/datasets/{id}/schema/migrations"),
            ("PUT", "/api/v1/reviews/{id}/approve"),
            ("POST", "/api/v1/reviews/{id}/comments"),
            ("PUT", "/api/v1/symbology/{id}"),
            ("DELETE", "/api/v1/labels/{id}"),
            ("DELETE", "/api/v1/domains/{id}"),
            ("PUT", "/api/v1/attribute-rules/{id}"),
            ("POST", "/api/v1/networks/{id}/edges"),
            ("POST", "/api/v1/routes/{id}/events"),
            ("POST", "/api/v1/datasets/{id}/events"),
            ("POST", "/api/v1/branches/{id}/h3/index"),
            ("POST", "/api/v1/branches/{id}/similarity/embed"),
            ("POST", "/api/v1/datasets/{id}/trajectories"),
            (
                "POST",
                "/api/v1/qgis/branches/{branch_id}/conflicts/resolve",
            ),
        ] {
            assert!(gated(method, route), "{method} {route} should be gated");
        }
    }

    #[test]
    fn compute_only_posts_are_exempt() {
        for route in [
            "/api/v1/branches/{id}/geoprocessing/clip",
            "/api/v1/branches/{id}/geoprocessing/voronoi",
            "/api/v1/branches/{id}/3d/extrude",
            "/api/v1/branches/{id}/3d/minkowski-sum",
            "/api/v1/networks/{id}/trace",
            "/api/v1/networks/{id}/shortest-path",
            "/api/v1/networks/{id}/astar",
            "/api/v1/networks/{id}/isochrone",
            "/api/v1/networks/{id}/tsp",
            "/api/v1/pointclouds/{id}/query",
            "/api/v1/pointclouds/{id}/profile",
            "/api/v1/attribute-rules/{id}/validate",
            "/api/v1/topologies/{name}/validate",
            "/api/v1/branches/{id}/similarity/search",
            "/api/v1/branches/{id}/similarity/cluster",
            "/api/v1/branches/{id}/h3/compact",
            "/api/v1/branches/{id}/transform",
            "/api/v1/branches/{id}/features/intersects",
            "/api/v1/branches/{id}/features/within",
            "/api/v1/branches/{id}/features/filter",
            "/api/v1/trajectories/{id}/simplify",
            "/api/v1/datasets/{id}/trajectories/nearest",
            "/api/v1/parcels/split",
            "/api/v1/parcels/merge",
            "/api/v1/surveys/compare",
            "/api/v1/coverage/simulate",
            "/api/v1/incidents/evacuate",
        ] {
            assert!(!gated("POST", route), "{route} writes nothing");
        }
    }

    /// The near-miss pairs, so a broadened suffix cannot quietly exempt a write.
    #[test]
    fn lookalike_write_routes_stay_gated() {
        assert!(gated("POST", "/api/v1/branches/{t}/merge/{s}"));
        assert!(gated("POST", "/api/v1/incidents"));
        assert!(gated("POST", "/api/v1/branches/{id}/import/geojson"));
    }

    /// A caller can put any single segment in `{tag}`, including the name of a
    /// policy rule. The refuted version matched its lists against the raw path,
    /// so `.../tags/trace` read as the compute-only `/trace` endpoint and
    /// `.../tags/permissions` dropped the required role to any. On a template
    /// those strings are inert: they can only ever appear as a parameter name,
    /// and no policy entry is one. The live exploit runs in
    /// `test_a_planted_tag_cannot_opt_out_of_the_write_ladder`.
    #[test]
    fn the_template_is_what_policy_reads() {
        let template = "/api/v1/datasets/{id}/tags/{tag}";
        assert!(gated("DELETE", template));
        assert_eq!(
            crate::auth::classify(&Method::DELETE, template),
            crate::Access::Write
        );
    }

    /// Grant management is gated harder elsewhere, and the ladder would deny the
    /// dataset admin who needs it most.
    #[test]
    fn permission_routes_are_left_to_rbac() {
        assert!(!gated("POST", "/api/v1/datasets/{id}/permissions"));
        assert!(!gated("POST", "/api/v1/branches/{id}/permissions"));
        assert!(!gated(
            "DELETE",
            "/api/v1/branches/{id}/permissions/{user_id}"
        ));
    }

    /// With no template there is no policy to read, so a mutation is refused.
    #[test]
    fn a_missing_template_refuses_mutations_and_allows_reads() {
        assert!(gated("POST", ""));
        assert!(gated("DELETE", ""));
        assert!(!gated("GET", ""));
    }
}
