use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use getrandom::fill;
use ptolemy_storage::{
    CollaborationRole, InvitationTarget, ProjectInvitation, ProjectMember, ProjectWithRole,
    StoreError, WorkspaceInvitation, WorkspaceMember, WorkspaceWithRole,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{AppState, auth::Actor};

pub fn workspace_routes() -> Router<AppState> {
    Router::new()
        .route("/workspaces", get(list_workspaces).post(create_workspace))
        .route(
            "/workspaces/{id}",
            get(get_workspace)
                .put(update_workspace)
                .delete(delete_workspace),
        )
        .route("/workspaces/{id}/members", get(list_workspace_members))
        .route(
            "/workspaces/{workspace_id}/members/{user_id}",
            put(set_workspace_member).delete(delete_workspace_member),
        )
        .route(
            "/workspaces/{id}/projects",
            get(list_workspace_projects).post(create_project),
        )
        .route(
            "/workspaces/{id}/invitations",
            get(list_workspace_invitations).post(create_workspace_invitation),
        )
        .route(
            "/workspaces/{workspace_id}/invitations/{invitation_id}",
            delete(revoke_workspace_invitation),
        )
        .route("/projects", get(list_projects))
        .route(
            "/projects/{id}",
            get(get_project).put(update_project).delete(delete_project),
        )
        .route("/projects/{id}/members", get(list_project_members))
        .route(
            "/projects/{project_id}/members/{user_id}",
            put(set_project_member).delete(delete_project_member),
        )
        .route(
            "/projects/{id}/invitations",
            get(list_project_invitations).post(create_project_invitation),
        )
        .route(
            "/projects/{project_id}/invitations/{invitation_id}",
            delete(revoke_project_invitation),
        )
        .route("/invitations/accept", post(accept_invitation))
}

#[derive(Deserialize)]
struct CreateWorkspaceRequest {
    name: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
struct UpdateMetadataRequest {
    name: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
struct SetMemberRequest {
    role: String,
}

#[derive(Deserialize)]
struct CreateInvitationRequest {
    role: String,
    expires_at: String,
}

#[derive(Deserialize)]
struct AcceptInvitationRequest {
    token: String,
}

#[derive(Serialize)]
struct CreatedInvitationResponse {
    id: Uuid,
    token: String,
}

#[derive(Serialize)]
struct AcceptedInvitationResponse {
    target: &'static str,
    id: Uuid,
}

async fn list_workspaces(
    State(store): State<AppState>,
    actor: Actor,
) -> Result<Json<Vec<WorkspaceWithRole>>, WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    Ok(Json(store.list_workspaces(user_id).await?))
}

async fn create_workspace(
    State(store): State<AppState>,
    actor: Actor,
    Json(request): Json<CreateWorkspaceRequest>,
) -> Result<(StatusCode, Json<WorkspaceWithRole>), WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    let name = validated_name(&request.name)?;
    let workspace = store
        .create_workspace(&name, request.description.as_deref(), user_id)
        .await?;
    Ok((StatusCode::CREATED, Json(workspace)))
}

async fn get_workspace(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    actor: Actor,
) -> Result<Json<WorkspaceWithRole>, WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    Ok(Json(store.get_workspace(id, user_id).await?))
}

async fn update_workspace(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    actor: Actor,
    Json(request): Json<UpdateMetadataRequest>,
) -> Result<Json<WorkspaceWithRole>, WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    let name = validated_name(&request.name)?;
    Ok(Json(
        store
            .update_workspace(id, user_id, &name, request.description.as_deref())
            .await?,
    ))
}

async fn delete_workspace(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    actor: Actor,
) -> Result<StatusCode, WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    store.delete_workspace(id, user_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_workspace_members(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    actor: Actor,
) -> Result<Json<Vec<WorkspaceMember>>, WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    require_workspace_owner(&store, id, user_id).await?;
    Ok(Json(store.list_workspace_members(id).await?))
}

async fn set_workspace_member(
    State(store): State<AppState>,
    Path((workspace_id, user_id)): Path<(Uuid, String)>,
    actor: Actor,
    Json(request): Json<SetMemberRequest>,
) -> Result<Json<WorkspaceMember>, WorkspaceError> {
    let actor_id = authenticated_subject(&actor)?;
    let user_id = validated_subject(&user_id)?;
    let role = validated_role(&request.role)?;
    Ok(Json(
        store
            .set_workspace_member(workspace_id, actor_id, user_id, role)
            .await?,
    ))
}

async fn delete_workspace_member(
    State(store): State<AppState>,
    Path((workspace_id, user_id)): Path<(Uuid, String)>,
    actor: Actor,
) -> Result<StatusCode, WorkspaceError> {
    let actor_id = authenticated_subject(&actor)?;
    let user_id = validated_subject(&user_id)?;
    store
        .delete_workspace_member(workspace_id, actor_id, user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_workspace_projects(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    actor: Actor,
) -> Result<Json<Vec<ProjectWithRole>>, WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    require_workspace_access(&store, id, user_id).await?;
    Ok(Json(store.list_workspace_projects(id, user_id).await?))
}

async fn create_project(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    actor: Actor,
    Json(request): Json<CreateWorkspaceRequest>,
) -> Result<(StatusCode, Json<ProjectWithRole>), WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    let name = validated_name(&request.name)?;
    let project = store
        .create_project(id, user_id, &name, request.description.as_deref())
        .await?;
    Ok((StatusCode::CREATED, Json(project)))
}

async fn list_projects(
    State(store): State<AppState>,
    actor: Actor,
) -> Result<Json<Vec<ProjectWithRole>>, WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    Ok(Json(store.list_projects(user_id).await?))
}

async fn get_project(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    actor: Actor,
) -> Result<Json<ProjectWithRole>, WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    Ok(Json(store.get_project(id, user_id).await?))
}

async fn update_project(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    actor: Actor,
    Json(request): Json<UpdateMetadataRequest>,
) -> Result<Json<ProjectWithRole>, WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    let name = validated_name(&request.name)?;
    Ok(Json(
        store
            .update_project(id, user_id, &name, request.description.as_deref())
            .await?,
    ))
}

async fn delete_project(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    actor: Actor,
) -> Result<StatusCode, WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    store.delete_project(id, user_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_project_members(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    actor: Actor,
) -> Result<Json<Vec<ProjectMember>>, WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    require_project_owner(&store, id, user_id).await?;
    Ok(Json(store.list_project_members(id).await?))
}

async fn set_project_member(
    State(store): State<AppState>,
    Path((project_id, user_id)): Path<(Uuid, String)>,
    actor: Actor,
    Json(request): Json<SetMemberRequest>,
) -> Result<Json<ProjectMember>, WorkspaceError> {
    let actor_id = authenticated_subject(&actor)?;
    let user_id = validated_subject(&user_id)?;
    let role = validated_role(&request.role)?;
    Ok(Json(
        store
            .set_project_member(project_id, actor_id, user_id, role)
            .await?,
    ))
}

async fn delete_project_member(
    State(store): State<AppState>,
    Path((project_id, user_id)): Path<(Uuid, String)>,
    actor: Actor,
) -> Result<StatusCode, WorkspaceError> {
    let actor_id = authenticated_subject(&actor)?;
    let user_id = validated_subject(&user_id)?;
    store
        .delete_project_member(project_id, actor_id, user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_workspace_invitation(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    actor: Actor,
    Json(request): Json<CreateInvitationRequest>,
) -> Result<(StatusCode, Json<CreatedInvitationResponse>), WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    let role = validated_invitation_role(&request.role)?;
    let expires_at = validated_expiry(&request.expires_at)?;
    let (token, token_hash) = new_invitation_token()?;
    let invitation = store
        .create_workspace_invitation(id, user_id, role, expires_at, &token_hash, token)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedInvitationResponse {
            id: invitation.id,
            token: invitation.token,
        }),
    ))
}

async fn list_workspace_invitations(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    actor: Actor,
) -> Result<Json<Vec<WorkspaceInvitation>>, WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    require_workspace_owner(&store, id, user_id).await?;
    Ok(Json(store.list_workspace_invitations(id).await?))
}

async fn revoke_workspace_invitation(
    State(store): State<AppState>,
    Path((workspace_id, invitation_id)): Path<(Uuid, Uuid)>,
    actor: Actor,
) -> Result<StatusCode, WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    store
        .revoke_workspace_invitation(workspace_id, invitation_id, user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_project_invitation(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    actor: Actor,
    Json(request): Json<CreateInvitationRequest>,
) -> Result<(StatusCode, Json<CreatedInvitationResponse>), WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    let role = validated_invitation_role(&request.role)?;
    let expires_at = validated_expiry(&request.expires_at)?;
    let (token, token_hash) = new_invitation_token()?;
    let invitation = store
        .create_project_invitation(id, user_id, role, expires_at, &token_hash, token)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedInvitationResponse {
            id: invitation.id,
            token: invitation.token,
        }),
    ))
}

async fn list_project_invitations(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    actor: Actor,
) -> Result<Json<Vec<ProjectInvitation>>, WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    require_project_owner(&store, id, user_id).await?;
    Ok(Json(store.list_project_invitations(id).await?))
}

async fn revoke_project_invitation(
    State(store): State<AppState>,
    Path((project_id, invitation_id)): Path<(Uuid, Uuid)>,
    actor: Actor,
) -> Result<StatusCode, WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    store
        .revoke_project_invitation(project_id, invitation_id, user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn accept_invitation(
    State(store): State<AppState>,
    actor: Actor,
    Json(request): Json<AcceptInvitationRequest>,
) -> Result<Json<AcceptedInvitationResponse>, WorkspaceError> {
    let user_id = authenticated_subject(&actor)?;
    if request.token.is_empty() {
        return Err(WorkspaceError::BadRequest("token must not be blank".into()));
    }
    let token_hash = Sha256::digest(request.token.as_bytes());
    let target = store.accept_invitation(&token_hash, user_id).await?;
    let (target, id) = match target {
        InvitationTarget::Workspace(id) => ("workspace", id),
        InvitationTarget::Project(id) => ("project", id),
    };
    Ok(Json(AcceptedInvitationResponse { target, id }))
}

fn authenticated_subject(actor: &Actor) -> Result<&str, WorkspaceError> {
    actor.id().ok_or_else(|| {
        WorkspaceError::Unauthorized("workspace routes require authentication".into())
    })
}

fn validated_name(name: &str) -> Result<String, WorkspaceError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(WorkspaceError::BadRequest("name must not be blank".into()));
    }
    Ok(name.to_string())
}

fn validated_subject(user_id: &str) -> Result<&str, WorkspaceError> {
    if user_id.trim().is_empty() {
        return Err(WorkspaceError::BadRequest(
            "user_id must not be blank".into(),
        ));
    }
    Ok(user_id)
}

fn validated_role(role: &str) -> Result<CollaborationRole, WorkspaceError> {
    CollaborationRole::parse(role)
        .ok_or_else(|| WorkspaceError::BadRequest("role must be owner, editor, or viewer".into()))
}

fn validated_invitation_role(role: &str) -> Result<CollaborationRole, WorkspaceError> {
    let role = validated_role(role)?;
    if role == CollaborationRole::Owner {
        return Err(WorkspaceError::BadRequest(
            "invitations may grant editor or viewer only".into(),
        ));
    }
    Ok(role)
}

fn validated_expiry(value: &str) -> Result<OffsetDateTime, WorkspaceError> {
    let expires_at = OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| WorkspaceError::BadRequest("expires_at must be RFC 3339".into()))?;
    if expires_at <= OffsetDateTime::now_utc() {
        return Err(WorkspaceError::BadRequest(
            "expires_at must be in the future".into(),
        ));
    }
    Ok(expires_at)
}

fn new_invitation_token() -> Result<(String, [u8; 32]), WorkspaceError> {
    let mut bytes = [0_u8; 32];
    fill(&mut bytes).map_err(|_| WorkspaceError::Internal)?;
    let token = URL_SAFE_NO_PAD.encode(bytes);
    Ok((token.clone(), Sha256::digest(token.as_bytes()).into()))
}

async fn require_workspace_access(
    store: &AppState,
    workspace_id: Uuid,
    user_id: &str,
) -> Result<(), WorkspaceError> {
    if store.workspace_role(workspace_id, user_id).await?.is_some() {
        return Ok(());
    }
    Err(WorkspaceError::Store(StoreError::NotFound(
        "workspace not found".into(),
    )))
}

async fn require_workspace_owner(
    store: &AppState,
    workspace_id: Uuid,
    user_id: &str,
) -> Result<(), WorkspaceError> {
    match store.workspace_role(workspace_id, user_id).await? {
        Some(CollaborationRole::Owner) => Ok(()),
        Some(_) => Err(WorkspaceError::Store(StoreError::Forbidden(
            "workspace role is insufficient".into(),
        ))),
        None => Err(WorkspaceError::Store(StoreError::NotFound(
            "workspace not found".into(),
        ))),
    }
}

async fn require_project_owner(
    store: &AppState,
    project_id: Uuid,
    user_id: &str,
) -> Result<(), WorkspaceError> {
    match store.project_role(project_id, user_id).await? {
        Some(CollaborationRole::Owner) => Ok(()),
        Some(_) => Err(WorkspaceError::Store(StoreError::Forbidden(
            "project role is insufficient".into(),
        ))),
        None => Err(WorkspaceError::Store(StoreError::NotFound(
            "project not found".into(),
        ))),
    }
}

#[derive(Debug)]
enum WorkspaceError {
    Store(StoreError),
    BadRequest(String),
    Unauthorized(String),
    Internal,
}

impl From<StoreError> for WorkspaceError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl IntoResponse for WorkspaceError {
    fn into_response(self) -> Response {
        match self {
            Self::Store(error) => {
                let (status, message) = crate::errors::store_error_status(&error);
                (status, message).into_response()
            }
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message).into_response(),
            Self::Unauthorized(message) => (StatusCode::UNAUTHORIZED, message).into_response(),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response(),
        }
    }
}
