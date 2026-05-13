// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Merge request / review API endpoints.
//!
//! Provides a pull-request-style workflow for geodata changes:
//! - Create merge requests proposing branch merges
//! - Add review comments (optionally per-feature)
//! - Approve/close/merge requests
//! - View diff for a merge request

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, put},
};
use ptolemy_core::review::{MergeRequest, MergeRequestStatus, ReviewComment};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::AppState;

pub fn review_routes() -> Router<AppState> {
    Router::new()
        .route("/reviews", get(list_reviews).post(create_review))
        .route("/reviews/{id}", get(get_review))
        .route("/reviews/{id}/approve", put(approve_review))
        .route("/reviews/{id}/close", put(close_review))
        .route("/reviews/{id}/merge", post(merge_review))
        .route("/reviews/{id}/diff", get(review_diff))
        .route(
            "/reviews/{id}/comments",
            get(list_comments).post(add_comment),
        )
}

// ─── List / Create ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct ListReviewsParams {
    dataset_id: Uuid,
    #[serde(default)]
    status: Option<String>,
}

async fn list_reviews(
    State(store): State<AppState>,
    Query(params): Query<ListReviewsParams>,
) -> Result<Json<Vec<MergeRequest>>, ReviewError> {
    let reviews = store
        .list_merge_requests(params.dataset_id, params.status.as_deref())
        .await?;
    Ok(Json(reviews))
}

#[derive(Deserialize)]
struct CreateReviewRequest {
    dataset_id: Uuid,
    source_branch_id: Uuid,
    target_branch_id: Uuid,
    title: String,
    #[serde(default)]
    description: String,
    author: String,
}

async fn create_review(
    State(store): State<AppState>,
    Json(req): Json<CreateReviewRequest>,
) -> Result<(StatusCode, Json<MergeRequest>), ReviewError> {
    let now = OffsetDateTime::now_utc();
    let mr = MergeRequest {
        id: Uuid::now_v7(),
        dataset_id: req.dataset_id,
        source_branch_id: req.source_branch_id,
        target_branch_id: req.target_branch_id,
        title: req.title,
        description: req.description,
        author: req.author,
        status: MergeRequestStatus::Open,
        created_at: now,
        updated_at: now,
    };
    store.create_merge_request(&mr).await?;
    Ok((StatusCode::CREATED, Json(mr)))
}

async fn get_review(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<MergeRequest>, ReviewError> {
    let mr = store.get_merge_request(id).await?;
    Ok(Json(mr))
}

// ─── Status transitions ─────────────────────────────────────────────

async fn approve_review(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ReviewError> {
    store
        .update_merge_request_status(id, &MergeRequestStatus::Approved)
        .await?;
    Ok(StatusCode::OK)
}

async fn close_review(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ReviewError> {
    store
        .update_merge_request_status(id, &MergeRequestStatus::Closed)
        .await?;
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
struct MergeReviewRequest {
    author: String,
}

#[derive(Serialize)]
struct MergeReviewResponse {
    changeset_id: Option<Uuid>,
    conflicts: Vec<String>,
}

async fn merge_review(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<MergeReviewRequest>,
) -> Result<Json<MergeReviewResponse>, ReviewError> {
    let mr = store.get_merge_request(id).await?;

    if mr.status == MergeRequestStatus::Merged {
        return Err(ReviewError::BadRequest("already merged".into()));
    }
    if mr.status == MergeRequestStatus::Closed {
        return Err(ReviewError::BadRequest("merge request is closed".into()));
    }

    let result = store
        .merge(mr.source_branch_id, mr.target_branch_id, &req.author)
        .await?;

    match result {
        ptolemy_storage::MergeResult::Success(cs) => {
            store
                .update_merge_request_status(id, &MergeRequestStatus::Merged)
                .await?;
            Ok(Json(MergeReviewResponse {
                changeset_id: Some(cs.id),
                conflicts: vec![],
            }))
        }
        ptolemy_storage::MergeResult::Conflicts(conflicts) => Ok(Json(MergeReviewResponse {
            changeset_id: None,
            conflicts: conflicts
                .iter()
                .map(|c| format!("feature {} conflict", c.feature_id))
                .collect(),
        })),
    }
}

// ─── Diff ───────────────────────────────────────────────────────────

async fn review_diff(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<ptolemy_core::diff::Diff>, ReviewError> {
    let mr = store.get_merge_request(id).await?;
    let source = store.get_branch(mr.source_branch_id).await?;
    let target = store.get_branch(mr.target_branch_id).await?;

    let source_head = source
        .head
        .ok_or_else(|| ReviewError::BadRequest("source branch has no commits".into()))?;
    let target_head = target.head;

    let diff = store.diff(target_head, source_head).await?;
    Ok(Json(diff))
}

// ─── Comments ───────────────────────────────────────────────────────

async fn list_comments(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<ReviewComment>>, ReviewError> {
    let comments = store.list_review_comments(id).await?;
    Ok(Json(comments))
}

#[derive(Deserialize)]
struct AddCommentRequest {
    author: String,
    body: String,
    #[serde(default)]
    feature_id: Option<Uuid>,
}

async fn add_comment(
    State(store): State<AppState>,
    Path(mr_id): Path<Uuid>,
    Json(req): Json<AddCommentRequest>,
) -> Result<(StatusCode, Json<ReviewComment>), ReviewError> {
    let comment = ReviewComment {
        id: Uuid::now_v7(),
        merge_request_id: mr_id,
        feature_id: req.feature_id,
        author: req.author,
        body: req.body,
        created_at: OffsetDateTime::now_utc(),
    };
    store.add_review_comment(&comment).await?;
    Ok((StatusCode::CREATED, Json(comment)))
}

// ─── Error Handling ─────────────────────────────────────────────────

enum ReviewError {
    Store(ptolemy_storage::StoreError),
    BadRequest(String),
}

impl From<ptolemy_storage::StoreError> for ReviewError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        ReviewError::Store(e)
    }
}

impl IntoResponse for ReviewError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            ReviewError::Store(ptolemy_storage::StoreError::NotFound(msg)) => {
                (StatusCode::NOT_FOUND, msg)
            }
            ReviewError::Store(ptolemy_storage::StoreError::Conflict(msg)) => {
                (StatusCode::CONFLICT, msg)
            }
            ReviewError::Store(ptolemy_storage::StoreError::Db(e)) => {
                tracing::error!("Database error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
            ReviewError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}
