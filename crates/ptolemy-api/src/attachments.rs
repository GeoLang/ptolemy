// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Attachment endpoints — binary files linked to a feature, a dataset, or a
//! project.
//!
//! Feature attachments are photos, documents and GPS logs about one feature.
//! Dataset attachments are the files a whole dataset refers to, such as the
//! icon or overlay image a style names. Project attachments are the files a
//! project's shared map refers to, which is how ViewTopia's overlay bitmaps
//! reach every member instead of staying in the browser that dropped them.
//!
//! A project attachment belongs to no dataset, so the per-dataset visibility
//! layer and write ladder have nothing to check it against. Its own routes
//! check the project role instead, and `/attachments/{id}` refuses one so it
//! cannot be reached without that check.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use ptolemy_storage::{Attachment, AttachmentMeta, CollaborationRole, StoreError};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{AppState, auth::Actor, workspace::require_project_role};

/// A draped overlay is a screen-sized bitmap, well past axum's 2 MB default for
/// a JSON body, and base64 adds a third again on top of it. The same ceiling the
/// ArcGIS facade gives an attachment upload.
const MAX_PROJECT_UPLOAD_BODY: usize = 32 * 1024 * 1024;

pub fn attachment_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/branches/{branch_id}/features/{feature_id}/attachments",
            get(list_attachments).post(upload_attachment),
        )
        .route(
            "/datasets/{dataset_id}/attachments",
            get(list_dataset_attachments).post(upload_dataset_attachment),
        )
        .route(
            "/projects/{project_id}/attachments",
            get(list_project_attachments)
                .post(upload_project_attachment)
                .layer(DefaultBodyLimit::max(MAX_PROJECT_UPLOAD_BODY)),
        )
        .route(
            "/projects/{project_id}/attachments/{id}",
            get(download_project_attachment).delete(delete_project_attachment),
        )
        .route("/attachments/{id}", get(download_attachment))
        .route("/attachments/{id}/meta", get(get_attachment_meta))
        .route("/attachments/{id}", delete(delete_attachment))
}

async fn list_attachments(
    State(store): State<AppState>,
    Path((branch_id, feature_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<AttachmentMeta>>, AttachmentError> {
    let attachments = store.list_attachments(feature_id, branch_id).await?;
    Ok(Json(attachments))
}

async fn upload_attachment(
    State(store): State<AppState>,
    Path((branch_id, feature_id)): Path<(Uuid, Uuid)>,
    actor: Actor,
    Json(req): Json<UploadAttachmentRequest>,
) -> Result<(StatusCode, Json<AttachmentMeta>), AttachmentError> {
    store
        .ensure_branch_writable(branch_id, &actor.writer())
        .await?;
    let data = base64_decode(&req.data)?;
    let size = data.len() as i64;
    let now = OffsetDateTime::now_utc();

    let attachment = Attachment {
        id: Uuid::now_v7(),
        feature_id: Some(feature_id),
        branch_id: Some(branch_id),
        dataset_id: None,
        project_id: None,
        name: req.name.clone(),
        content_type: req
            .content_type
            .unwrap_or_else(|| "application/octet-stream".into()),
        size_bytes: size,
        data,
        thumbnail: None,
        metadata: req.metadata.unwrap_or(serde_json::json!({})),
        created_by: actor.or_body(&req.created_by).to_string(),
        created_at: now,
    };

    store.create_attachment(&attachment).await?;

    let meta = AttachmentMeta {
        id: attachment.id,
        feature_id: Some(feature_id),
        branch_id: Some(branch_id),
        dataset_id: None,
        project_id: None,
        name: attachment.name,
        content_type: attachment.content_type,
        size_bytes: size,
        metadata: attachment.metadata,
        created_by: attachment.created_by,
        created_at: now,
    };

    Ok((StatusCode::CREATED, Json(meta)))
}

async fn list_dataset_attachments(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<AttachmentMeta>>, AttachmentError> {
    let attachments = store.list_dataset_attachments(dataset_id).await?;
    Ok(Json(attachments))
}

async fn upload_dataset_attachment(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    actor: Actor,
    Json(req): Json<UploadAttachmentRequest>,
) -> Result<(StatusCode, Json<AttachmentMeta>), AttachmentError> {
    store
        .ensure_dataset_writable(dataset_id, &actor.writer())
        .await?;
    let data = base64_decode(&req.data)?;
    let size = data.len() as i64;
    let now = OffsetDateTime::now_utc();

    let attachment = Attachment {
        id: Uuid::now_v7(),
        feature_id: None,
        branch_id: None,
        dataset_id: Some(dataset_id),
        project_id: None,
        name: req.name.clone(),
        content_type: req
            .content_type
            .unwrap_or_else(|| "application/octet-stream".into()),
        size_bytes: size,
        data,
        thumbnail: None,
        metadata: req.metadata.unwrap_or(serde_json::json!({})),
        created_by: actor.or_body(&req.created_by).to_string(),
        created_at: now,
    };

    store.create_attachment(&attachment).await?;

    let meta = AttachmentMeta {
        id: attachment.id,
        feature_id: None,
        branch_id: None,
        dataset_id: Some(dataset_id),
        project_id: None,
        name: attachment.name,
        content_type: attachment.content_type,
        size_bytes: size,
        metadata: attachment.metadata,
        created_by: attachment.created_by,
        created_at: now,
    };

    Ok((StatusCode::CREATED, Json(meta)))
}

async fn list_project_attachments(
    State(store): State<AppState>,
    Path(project_id): Path<Uuid>,
    actor: Actor,
) -> Result<Json<Vec<AttachmentMeta>>, AttachmentError> {
    let user_id = authenticated_subject(&actor)?;
    require_project_role(&store, project_id, user_id, CollaborationRole::Viewer).await?;
    Ok(Json(store.list_project_attachments(project_id).await?))
}

async fn upload_project_attachment(
    State(store): State<AppState>,
    Path(project_id): Path<Uuid>,
    actor: Actor,
    Json(req): Json<UploadAttachmentRequest>,
) -> Result<(StatusCode, Json<AttachmentMeta>), AttachmentError> {
    let user_id = authenticated_subject(&actor)?.to_string();
    require_project_role(&store, project_id, &user_id, CollaborationRole::Editor).await?;
    let data = base64_decode(&req.data)?;
    let size = data.len() as i64;
    let now = OffsetDateTime::now_utc();

    let attachment = Attachment {
        id: Uuid::now_v7(),
        feature_id: None,
        branch_id: None,
        dataset_id: None,
        project_id: Some(project_id),
        name: req.name.clone(),
        content_type: req
            .content_type
            .unwrap_or_else(|| "application/octet-stream".into()),
        size_bytes: size,
        data,
        thumbnail: None,
        metadata: req.metadata.unwrap_or(serde_json::json!({})),
        created_by: actor.or_body(&req.created_by).to_string(),
        created_at: now,
    };

    store.create_attachment(&attachment).await?;

    let meta = AttachmentMeta {
        id: attachment.id,
        feature_id: None,
        branch_id: None,
        dataset_id: None,
        project_id: Some(project_id),
        name: attachment.name,
        content_type: attachment.content_type,
        size_bytes: size,
        metadata: attachment.metadata,
        created_by: attachment.created_by,
        created_at: now,
    };

    Ok((StatusCode::CREATED, Json(meta)))
}

async fn download_project_attachment(
    State(store): State<AppState>,
    Path((project_id, id)): Path<(Uuid, Uuid)>,
    actor: Actor,
) -> Result<Response, AttachmentError> {
    let user_id = authenticated_subject(&actor)?;
    require_project_role(&store, project_id, user_id, CollaborationRole::Viewer).await?;
    Ok(as_download(
        store.get_project_attachment(project_id, id).await?,
    ))
}

async fn delete_project_attachment(
    State(store): State<AppState>,
    Path((project_id, id)): Path<(Uuid, Uuid)>,
    actor: Actor,
) -> Result<StatusCode, AttachmentError> {
    let user_id = authenticated_subject(&actor)?;
    require_project_role(&store, project_id, user_id, CollaborationRole::Editor).await?;
    store.delete_project_attachment(project_id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct UploadAttachmentRequest {
    name: String,
    #[serde(default)]
    content_type: Option<String>,
    /// Base64-encoded file data
    data: String,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    created_by: String,
}

async fn download_attachment(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Response, AttachmentError> {
    let attachment = store.get_attachment(id).await?;
    outside_a_project(&attachment)?;
    Ok(as_download(attachment))
}

async fn get_attachment_meta(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<AttachmentMeta>, AttachmentError> {
    let a = store.get_attachment(id).await?;
    outside_a_project(&a)?;
    Ok(Json(AttachmentMeta {
        id: a.id,
        feature_id: a.feature_id,
        branch_id: a.branch_id,
        dataset_id: a.dataset_id,
        project_id: a.project_id,
        name: a.name,
        content_type: a.content_type,
        size_bytes: a.size_bytes,
        metadata: a.metadata,
        created_by: a.created_by,
        created_at: a.created_at,
    }))
}

async fn delete_attachment(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    actor: Actor,
) -> Result<StatusCode, AttachmentError> {
    store
        .ensure_attachment_writable(id, &actor.writer())
        .await?;
    store.delete_attachment(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── Helpers ────────────────────────────────────────────────────────

fn as_download(attachment: Attachment) -> Response {
    (
        StatusCode::OK,
        [
            ("content-type", attachment.content_type.as_str().to_string()),
            (
                "content-disposition",
                format!("attachment; filename=\"{}\"", attachment.name),
            ),
            ("content-length", attachment.size_bytes.to_string()),
        ],
        Bytes::from(attachment.data),
    )
        .into_response()
}

/// The dataset-scoped routes serve anything the visibility layer let through,
/// and a project attachment names no dataset for that layer to weigh. It is
/// absent here, so reaching one always goes through the project role check.
fn outside_a_project(attachment: &Attachment) -> Result<(), AttachmentError> {
    if attachment.project_id.is_some() {
        return Err(AttachmentError::Store(StoreError::NotFound(format!(
            "attachment {}",
            attachment.id
        ))));
    }
    Ok(())
}

fn authenticated_subject(actor: &Actor) -> Result<&str, AttachmentError> {
    actor.id().ok_or_else(|| {
        AttachmentError::Store(StoreError::Forbidden(
            "project attachments require authentication".into(),
        ))
    })
}

fn base64_decode(input: &str) -> Result<Vec<u8>, AttachmentError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input)
        .map_err(|e| AttachmentError::BadRequest(format!("invalid base64: {e}")))
}

// ─── Error type ─────────────────────────────────────────────────────

#[derive(Debug)]
enum AttachmentError {
    Store(ptolemy_storage::StoreError),
    BadRequest(String),
}

impl From<ptolemy_storage::StoreError> for AttachmentError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        Self::Store(e)
    }
}

impl IntoResponse for AttachmentError {
    fn into_response(self) -> Response {
        match self {
            Self::Store(e) => {
                let (status, message) = crate::errors::store_error_status(&e);
                (status, message).into_response()
            }
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
        }
    }
}
