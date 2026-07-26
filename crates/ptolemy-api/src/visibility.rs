// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Per-dataset read visibility, enforced in one middleware.
//!
//! A private dataset's content is only served to a caller holding a permission
//! row on it (or one of its branches), or to an instance admin. This runs as a
//! layer rather than a per-handler check because reads resolve a dataset in
//! dozens of places: handlers that build their own SQL, that take a branch id in
//! the path, that take one in a query parameter. What they all share is the id
//! itself, so the layer resolves every uuid the request names.
//!
//! An id may name a dataset, a branch, a changeset, a merge request or a
//! feature: [`PgStore::private_datasets_for_ids`] resolves all five to a
//! dataset. A request that names none of them has no dataset in scope and
//! passes, and so does a request that names only public ones.
//!
//! Unauthorized reads answer 404, not 403, so a private dataset id cannot be
//! confirmed by probing.

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use uuid::Uuid;

use crate::{AppState, auth::AuthEnabled, errors::store_error_status};

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
    if ids.is_empty() {
        return next.run(request).await;
    }

    let private = match store.private_datasets_for_ids(&ids).await {
        Ok(private) => private,
        Err(e) => {
            let (status, message) = store_error_status(&e);
            return (status, axum::Json(serde_json::json!({"error": message}))).into_response();
        }
    };
    if private.is_empty() {
        return next.run(request).await;
    }

    let claims = request.extensions().get::<crate::Claims>();
    if claims.is_some_and(crate::Claims::can_admin) {
        return next.run(request).await;
    }
    let Some(user_id) = claims.map(|c| c.sub.clone()) else {
        return not_found();
    };

    // every private dataset the request touches has to be granted, not just one
    match store.readable_datasets(&private, &user_id).await {
        Ok(readable) => {
            if private.iter().all(|id| readable.contains(id)) {
                next.run(request).await
            } else {
                not_found()
            }
        }
        Err(e) => {
            let (status, message) = store_error_status(&e);
            (status, axum::Json(serde_json::json!({"error": message}))).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
