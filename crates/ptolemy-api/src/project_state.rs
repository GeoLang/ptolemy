// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! A project's shared state, one JSON value per key.
//!
//! This is where a client keeps what every member of a project should see
//! rather than what one browser happens to hold. ViewTopia writes its map
//! snapshot under `map` and its dashboards under `dashboards`. The value is
//! opaque to the server: a viewer that changes its snapshot shape needs nothing
//! here.

use axum::{
    Json, Router,
    body::Bytes,
    extract::rejection::{BytesRejection, FailedToBufferBody},
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use ptolemy_storage::{CollaborationRole, ProjectStateEntry, StoreError};
use uuid::Uuid;

use crate::{AppState, auth::Actor, workspace::require_project_role};

/// The largest value a project state key may hold. A ViewTopia map snapshot
/// with a few hundred layers is well under a megabyte; this is the point at
/// which the client is storing something that belongs in a dataset.
pub const MAX_PROJECT_STATE_BYTES: usize = 5 * 1024 * 1024;

pub fn project_state_routes() -> Router<AppState> {
    Router::new().route(
        "/projects/{project_id}/state/{key}",
        get(get_project_state)
            .put(put_project_state)
            .layer(DefaultBodyLimit::max(MAX_PROJECT_STATE_BYTES)),
    )
}

async fn get_project_state(
    State(store): State<AppState>,
    Path((project_id, key)): Path<(Uuid, String)>,
    actor: Actor,
) -> Result<Json<ProjectStateEntry>, ProjectStateError> {
    let user_id = authenticated_subject(&actor)?;
    require_project_role(&store, project_id, user_id, CollaborationRole::Viewer).await?;
    Ok(Json(store.project_state(project_id, &key).await?))
}

async fn put_project_state(
    State(store): State<AppState>,
    Path((project_id, key)): Path<(Uuid, String)>,
    actor: Actor,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<ProjectStateEntry>, ProjectStateError> {
    let user_id = authenticated_subject(&actor)?.to_string();
    require_project_role(&store, project_id, &user_id, CollaborationRole::Editor).await?;

    let body = match body {
        Ok(bytes) => bytes,
        Err(BytesRejection::FailedToBufferBody(FailedToBufferBody::LengthLimitError(_))) => {
            return Err(ProjectStateError::TooLarge);
        }
        Err(rejection) => return Err(ProjectStateError::BadRequest(rejection.body_text())),
    };
    let value: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| ProjectStateError::BadRequest(format!("value must be JSON: {e}")))?;

    Ok(Json(
        store
            .set_project_state(project_id, &key, &value, &user_id)
            .await?,
    ))
}

fn authenticated_subject(actor: &Actor) -> Result<&str, ProjectStateError> {
    actor
        .id()
        .ok_or_else(|| ProjectStateError::Unauthorized("project state requires authentication"))
}

#[derive(Debug)]
enum ProjectStateError {
    Store(StoreError),
    BadRequest(String),
    Unauthorized(&'static str),
    TooLarge,
}

impl From<StoreError> for ProjectStateError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl IntoResponse for ProjectStateError {
    fn into_response(self) -> Response {
        match self {
            Self::Store(error) => {
                let (status, message) = crate::errors::store_error_status(&error);
                (status, message).into_response()
            }
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message).into_response(),
            Self::Unauthorized(message) => (StatusCode::UNAUTHORIZED, message).into_response(),
            Self::TooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("project state value must be at most {MAX_PROJECT_STATE_BYTES} bytes"),
            )
                .into_response(),
        }
    }
}
