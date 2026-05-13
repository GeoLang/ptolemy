// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Feature attachment endpoints — binary files linked to features.
//!
//! Supports photos, documents, GPS logs, and other files associated with
//! individual features in a branch.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use ptolemy_storage::{Attachment, AttachmentMeta};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::AppState;

pub fn attachment_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/branches/{branch_id}/features/{feature_id}/attachments",
            get(list_attachments).post(upload_attachment),
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
    Json(req): Json<UploadAttachmentRequest>,
) -> Result<(StatusCode, Json<AttachmentMeta>), AttachmentError> {
    let data = base64_decode(&req.data)?;
    let size = data.len() as i64;
    let now = OffsetDateTime::now_utc();

    let attachment = Attachment {
        id: Uuid::now_v7(),
        feature_id,
        branch_id,
        name: req.name.clone(),
        content_type: req
            .content_type
            .unwrap_or_else(|| "application/octet-stream".into()),
        size_bytes: size,
        data,
        thumbnail: None,
        metadata: req.metadata.unwrap_or(serde_json::json!({})),
        created_by: req.created_by.clone(),
        created_at: now,
    };

    store.create_attachment(&attachment).await?;

    let meta = AttachmentMeta {
        id: attachment.id,
        feature_id,
        branch_id,
        name: attachment.name,
        content_type: attachment.content_type,
        size_bytes: size,
        metadata: attachment.metadata,
        created_by: attachment.created_by,
        created_at: now,
    };

    Ok((StatusCode::CREATED, Json(meta)))
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
    Ok((
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
        .into_response())
}

async fn get_attachment_meta(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<AttachmentMeta>, AttachmentError> {
    let a = store.get_attachment(id).await?;
    Ok(Json(AttachmentMeta {
        id: a.id,
        feature_id: a.feature_id,
        branch_id: a.branch_id,
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
) -> Result<StatusCode, AttachmentError> {
    store.delete_attachment(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── Helpers ────────────────────────────────────────────────────────

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
            Self::Store(ptolemy_storage::StoreError::NotFound(msg)) => {
                (StatusCode::NOT_FOUND, msg).into_response()
            }
            Self::Store(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
        }
    }
}
