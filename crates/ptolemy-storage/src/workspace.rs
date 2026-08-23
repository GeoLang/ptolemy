use crate::{PgStore, StoreError};
use serde::Serialize;
use sqlx::{Postgres, Row, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CollaborationRole {
    Owner,
    Editor,
    Viewer,
}

impl CollaborationRole {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Self::Owner),
            "editor" => Some(Self::Editor),
            "viewer" => Some(Self::Viewer),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Editor => "editor",
            Self::Viewer => "viewer",
        }
    }

    pub fn can_edit(self) -> bool {
        matches!(self, Self::Owner | Self::Editor)
    }

    pub fn is_owner(self) -> bool {
        matches!(self, Self::Owner)
    }

    /// The dataset permission this role carries on a dataset attached to the
    /// project. Folded in as the stronger of it and the caller's explicit grant.
    pub fn dataset_permission(self) -> &'static str {
        match self {
            Self::Owner => "admin",
            Self::Editor => "write",
            Self::Viewer => "read",
        }
    }
}

/// SQL for a caller's effective role on a project: their own project membership,
/// the workspace membership they inherit, whichever of the two is higher, and no
/// row when they hold neither. This is the one definition of that inheritance,
/// and every access decision that folds a project role in is built on it.
///
/// `project_expr` is a SQL expression naming the project: a bind, or a column of
/// the calling query, which makes this a correlated subquery. `caller_param` is
/// the 1-based position of the caller-id bind, text and NULL when anonymous. Both
/// come from the calling query, never from request data, so nothing a caller
/// sends is interpolated here.
///
/// `project_expr` resolves in the calling query's scope, so the table aliases
/// here are prefixed: a plain `p` would shadow an outer `p` that the expression
/// meant to name.
pub fn effective_project_role_sql(project_expr: &str, caller_param: usize) -> String {
    format!(
        "(SELECT role FROM (
              SELECT role_workspace.role AS role,
                     CASE role_workspace.role
                         WHEN 'owner' THEN 3 WHEN 'editor' THEN 2 ELSE 1 END AS rank
                FROM projects role_project
                JOIN workspace_members role_workspace
                  ON role_workspace.workspace_id = role_project.workspace_id
               WHERE role_project.id = {project_expr}
                 AND role_workspace.user_id = ${caller_param}
              UNION ALL
              SELECT role_member.role AS role,
                     CASE role_member.role
                         WHEN 'owner' THEN 3 WHEN 'editor' THEN 2 ELSE 1 END AS rank
                FROM project_members role_member
               WHERE role_member.project_id = {project_expr}
                 AND role_member.user_id = ${caller_param}
          ) roles ORDER BY rank DESC LIMIT 1)"
    )
}

/// A role column produced by [`effective_project_role_sql`]. Both member tables
/// CHECK the value, so anything unparseable means the schema drifted and is
/// refused rather than read as the weakest role.
pub(crate) fn parse_effective_role(
    role: Option<String>,
) -> Result<Option<CollaborationRole>, StoreError> {
    match role {
        Some(role) => CollaborationRole::parse(&role)
            .map(Some)
            .ok_or_else(|| StoreError::Conflict("project membership role is invalid".into())),
        None => Ok(None),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Workspace {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub created_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceWithRole {
    #[serde(flatten)]
    pub workspace: Workspace,
    pub role: CollaborationRole,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceMember {
    pub workspace_id: Uuid,
    pub user_id: String,
    pub role: CollaborationRole,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub created_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectWithRole {
    #[serde(flatten)]
    pub project: Project,
    pub role: CollaborationRole,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectMember {
    pub project_id: Uuid,
    pub user_id: String,
    pub role: CollaborationRole,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy)]
pub enum InvitationTarget {
    Workspace(Uuid),
    Project(Uuid),
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceInvitation {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub role: CollaborationRole,
    pub created_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectInvitation {
    pub id: Uuid,
    pub project_id: Uuid,
    pub role: CollaborationRole,
    pub created_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub struct CreatedInvitation {
    pub id: Uuid,
    pub token: String,
}

impl PgStore {
    pub async fn create_workspace(
        &self,
        name: &str,
        description: Option<&str>,
        creator: &str,
    ) -> Result<WorkspaceWithRole, StoreError> {
        let mut tx = self.pool.begin().await?;
        let id = Uuid::now_v7();
        let row = sqlx::query(
            "INSERT INTO workspaces (id, name, description, created_by)
             VALUES ($1, $2, $3, $4)
             RETURNING id, name, description, created_by, created_at, updated_at",
        )
        .bind(id)
        .bind(name)
        .bind(description)
        .bind(creator)
        .fetch_one(&mut *tx)
        .await?;
        insert_workspace_member(&mut tx, id, creator, CollaborationRole::Owner).await?;
        tx.commit().await?;
        Ok(WorkspaceWithRole {
            workspace: workspace_from_row(&row),
            role: CollaborationRole::Owner,
        })
    }

    pub async fn list_workspaces(
        &self,
        user_id: &str,
    ) -> Result<Vec<WorkspaceWithRole>, StoreError> {
        let rows = sqlx::query(
            "SELECT w.id, w.name, w.description, w.created_by, w.created_at, w.updated_at, m.role
             FROM workspaces w
             JOIN workspace_members m ON m.workspace_id = w.id
             WHERE m.user_id = $1
             ORDER BY w.created_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(workspace_with_role_from_row).collect())
    }

    pub async fn get_workspace(
        &self,
        id: Uuid,
        user_id: &str,
    ) -> Result<WorkspaceWithRole, StoreError> {
        let row = sqlx::query(
            "SELECT w.id, w.name, w.description, w.created_by, w.created_at, w.updated_at, m.role
             FROM workspaces w
             JOIN workspace_members m ON m.workspace_id = w.id
             WHERE w.id = $1 AND m.user_id = $2",
        )
        .bind(id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| workspace_with_role_from_row(&row))
            .ok_or_else(|| StoreError::NotFound("workspace not found".into()))
    }

    pub async fn workspace_role(
        &self,
        workspace_id: Uuid,
        user_id: &str,
    ) -> Result<Option<CollaborationRole>, StoreError> {
        role_for_workspace(&self.pool, workspace_id, user_id).await
    }

    pub async fn update_workspace(
        &self,
        id: Uuid,
        user_id: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<WorkspaceWithRole, StoreError> {
        let mut tx = self.pool.begin().await?;
        lock_workspace(&mut tx, id).await?;
        let role = require_workspace_role(&mut tx, id, user_id, CollaborationRole::Editor).await?;
        let row = sqlx::query(
            "UPDATE workspaces
             SET name = $2, description = $3, updated_at = now()
             WHERE id = $1
             RETURNING id, name, description, created_by, created_at, updated_at",
        )
        .bind(id)
        .bind(name)
        .bind(description)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(WorkspaceWithRole {
            workspace: workspace_from_row(&row),
            role,
        })
    }

    pub async fn delete_workspace(&self, id: Uuid, user_id: &str) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        lock_workspace(&mut tx, id).await?;
        require_workspace_role(&mut tx, id, user_id, CollaborationRole::Owner).await?;
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn list_workspace_members(
        &self,
        workspace_id: Uuid,
    ) -> Result<Vec<WorkspaceMember>, StoreError> {
        let rows = sqlx::query(
            "SELECT workspace_id, user_id, role, created_at
             FROM workspace_members WHERE workspace_id = $1 ORDER BY created_at, user_id",
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(workspace_member_from_row).collect())
    }

    pub async fn set_workspace_member(
        &self,
        workspace_id: Uuid,
        actor: &str,
        user_id: &str,
        role: CollaborationRole,
    ) -> Result<WorkspaceMember, StoreError> {
        let mut tx = self.pool.begin().await?;
        lock_workspace(&mut tx, workspace_id).await?;
        require_workspace_role(&mut tx, workspace_id, actor, CollaborationRole::Owner).await?;
        let row = sqlx::query(
            "INSERT INTO workspace_members (workspace_id, user_id, role)
             VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, user_id)
             DO UPDATE SET role = EXCLUDED.role
             RETURNING workspace_id, user_id, role, created_at",
        )
        .bind(workspace_id)
        .bind(user_id)
        .bind(role.as_str())
        .fetch_one(&mut *tx)
        .await?;
        ensure_workspace_owner(&mut tx, workspace_id).await?;
        tx.commit().await?;
        Ok(workspace_member_from_row(&row))
    }

    pub async fn delete_workspace_member(
        &self,
        workspace_id: Uuid,
        actor: &str,
        user_id: &str,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        lock_workspace(&mut tx, workspace_id).await?;
        require_workspace_role(&mut tx, workspace_id, actor, CollaborationRole::Owner).await?;
        let result =
            sqlx::query("DELETE FROM workspace_members WHERE workspace_id = $1 AND user_id = $2")
                .bind(workspace_id)
                .bind(user_id)
                .execute(&mut *tx)
                .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound("workspace member not found".into()));
        }
        ensure_workspace_owner(&mut tx, workspace_id).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn create_project(
        &self,
        workspace_id: Uuid,
        actor: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<ProjectWithRole, StoreError> {
        let mut tx = self.pool.begin().await?;
        lock_workspace(&mut tx, workspace_id).await?;
        require_workspace_role(&mut tx, workspace_id, actor, CollaborationRole::Editor).await?;
        let id = Uuid::now_v7();
        let row = sqlx::query(
            "INSERT INTO projects (id, workspace_id, name, description, created_by)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING id, workspace_id, name, description, created_by, created_at, updated_at",
        )
        .bind(id)
        .bind(workspace_id)
        .bind(name)
        .bind(description)
        .bind(actor)
        .fetch_one(&mut *tx)
        .await?;
        insert_project_member(&mut tx, id, actor, CollaborationRole::Owner).await?;
        tx.commit().await?;
        Ok(ProjectWithRole {
            project: project_from_row(&row),
            role: CollaborationRole::Owner,
        })
    }

    pub async fn list_projects(&self, user_id: &str) -> Result<Vec<ProjectWithRole>, StoreError> {
        let rows = sqlx::query(
            "SELECT p.id, p.workspace_id, p.name, p.description, p.created_by,
                    p.created_at, p.updated_at, access.role
             FROM projects p
             JOIN LATERAL (
                 SELECT role
                 FROM (
                     SELECT wm.role,
                            CASE wm.role WHEN 'owner' THEN 3 WHEN 'editor' THEN 2 ELSE 1 END AS rank
                     FROM workspace_members wm
                     WHERE wm.workspace_id = p.workspace_id AND wm.user_id = $1
                     UNION ALL
                     SELECT pm.role,
                            CASE pm.role WHEN 'owner' THEN 3 WHEN 'editor' THEN 2 ELSE 1 END AS rank
                     FROM project_members pm
                     WHERE pm.project_id = p.id AND pm.user_id = $1
                 ) roles
                 ORDER BY rank DESC
                 LIMIT 1
             ) access ON true
             ORDER BY p.created_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(project_with_role_from_row).collect())
    }

    pub async fn list_workspace_projects(
        &self,
        workspace_id: Uuid,
        user_id: &str,
    ) -> Result<Vec<ProjectWithRole>, StoreError> {
        let rows = sqlx::query(
            "SELECT p.id, p.workspace_id, p.name, p.description, p.created_by, p.created_at,
                    p.updated_at, access.role
             FROM projects p
             JOIN LATERAL (
                 SELECT role
                 FROM (
                     SELECT wm.role,
                            CASE wm.role WHEN 'owner' THEN 3 WHEN 'editor' THEN 2 ELSE 1 END AS rank
                     FROM workspace_members wm
                     WHERE wm.workspace_id = p.workspace_id AND wm.user_id = $2
                     UNION ALL
                     SELECT pm.role,
                            CASE pm.role WHEN 'owner' THEN 3 WHEN 'editor' THEN 2 ELSE 1 END AS rank
                     FROM project_members pm
                     WHERE pm.project_id = p.id AND pm.user_id = $2
                 ) roles
                 ORDER BY rank DESC
                 LIMIT 1
             ) access ON true
             WHERE p.workspace_id = $1
             ORDER BY p.created_at DESC",
        )
        .bind(workspace_id)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(project_with_role_from_row).collect())
    }

    pub async fn get_project(
        &self,
        id: Uuid,
        user_id: &str,
    ) -> Result<ProjectWithRole, StoreError> {
        let row = sqlx::query(
            "SELECT p.id, p.workspace_id, p.name, p.description, p.created_by,
                    p.created_at, p.updated_at, access.role
             FROM projects p
             JOIN LATERAL (
                 SELECT role
                 FROM (
                     SELECT wm.role,
                            CASE wm.role WHEN 'owner' THEN 3 WHEN 'editor' THEN 2 ELSE 1 END AS rank
                     FROM workspace_members wm
                     WHERE wm.workspace_id = p.workspace_id AND wm.user_id = $2
                     UNION ALL
                     SELECT pm.role,
                            CASE pm.role WHEN 'owner' THEN 3 WHEN 'editor' THEN 2 ELSE 1 END AS rank
                     FROM project_members pm
                     WHERE pm.project_id = p.id AND pm.user_id = $2
                 ) roles
                 ORDER BY rank DESC
                 LIMIT 1
             ) access ON true
             WHERE p.id = $1",
        )
        .bind(id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| project_with_role_from_row(&row))
            .ok_or_else(|| StoreError::NotFound("project not found".into()))
    }

    pub async fn project_role(
        &self,
        project_id: Uuid,
        user_id: &str,
    ) -> Result<Option<CollaborationRole>, StoreError> {
        role_for_project(&self.pool, project_id, user_id).await
    }

    pub async fn update_project(
        &self,
        id: Uuid,
        user_id: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<ProjectWithRole, StoreError> {
        let mut tx = self.pool.begin().await?;
        let workspace_id = lock_project_and_workspace(&mut tx, id).await?;
        let role = require_project_role(
            &mut tx,
            id,
            workspace_id,
            user_id,
            CollaborationRole::Editor,
        )
        .await?;
        let row = sqlx::query(
            "UPDATE projects
             SET name = $2, description = $3, updated_at = now()
             WHERE id = $1
             RETURNING id, workspace_id, name, description, created_by, created_at, updated_at",
        )
        .bind(id)
        .bind(name)
        .bind(description)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ProjectWithRole {
            project: project_from_row(&row),
            role,
        })
    }

    pub async fn delete_project(&self, id: Uuid, user_id: &str) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let workspace_id = lock_project_and_workspace(&mut tx, id).await?;
        require_project_role(&mut tx, id, workspace_id, user_id, CollaborationRole::Owner).await?;
        sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn list_project_members(
        &self,
        project_id: Uuid,
    ) -> Result<Vec<ProjectMember>, StoreError> {
        let rows = sqlx::query(
            "SELECT project_id, user_id, role, created_at
             FROM project_members WHERE project_id = $1 ORDER BY created_at, user_id",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(project_member_from_row).collect())
    }

    pub async fn set_project_member(
        &self,
        project_id: Uuid,
        actor: &str,
        user_id: &str,
        role: CollaborationRole,
    ) -> Result<ProjectMember, StoreError> {
        let mut tx = self.pool.begin().await?;
        let workspace_id = lock_project_and_workspace(&mut tx, project_id).await?;
        require_project_role(
            &mut tx,
            project_id,
            workspace_id,
            actor,
            CollaborationRole::Owner,
        )
        .await?;
        let row = sqlx::query(
            "INSERT INTO project_members (project_id, user_id, role)
             VALUES ($1, $2, $3)
             ON CONFLICT (project_id, user_id)
             DO UPDATE SET role = EXCLUDED.role
             RETURNING project_id, user_id, role, created_at",
        )
        .bind(project_id)
        .bind(user_id)
        .bind(role.as_str())
        .fetch_one(&mut *tx)
        .await?;
        ensure_project_owner(&mut tx, project_id, workspace_id).await?;
        tx.commit().await?;
        Ok(project_member_from_row(&row))
    }

    pub async fn delete_project_member(
        &self,
        project_id: Uuid,
        actor: &str,
        user_id: &str,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let workspace_id = lock_project_and_workspace(&mut tx, project_id).await?;
        require_project_role(
            &mut tx,
            project_id,
            workspace_id,
            actor,
            CollaborationRole::Owner,
        )
        .await?;
        let result =
            sqlx::query("DELETE FROM project_members WHERE project_id = $1 AND user_id = $2")
                .bind(project_id)
                .bind(user_id)
                .execute(&mut *tx)
                .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound("project member not found".into()));
        }
        ensure_project_owner(&mut tx, project_id, workspace_id).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn create_workspace_invitation(
        &self,
        workspace_id: Uuid,
        actor: &str,
        role: CollaborationRole,
        expires_at: OffsetDateTime,
        token_hash: &[u8],
        token: String,
    ) -> Result<CreatedInvitation, StoreError> {
        ensure_invitation_role(role)?;
        let mut tx = self.pool.begin().await?;
        lock_workspace(&mut tx, workspace_id).await?;
        require_workspace_role(&mut tx, workspace_id, actor, CollaborationRole::Owner).await?;
        let id = insert_invitation(
            &mut tx,
            InvitationTarget::Workspace(workspace_id),
            actor,
            role,
            expires_at,
            token_hash,
        )
        .await?;
        tx.commit().await?;
        Ok(CreatedInvitation { id, token })
    }

    pub async fn create_project_invitation(
        &self,
        project_id: Uuid,
        actor: &str,
        role: CollaborationRole,
        expires_at: OffsetDateTime,
        token_hash: &[u8],
        token: String,
    ) -> Result<CreatedInvitation, StoreError> {
        ensure_invitation_role(role)?;
        let mut tx = self.pool.begin().await?;
        let workspace_id = lock_project_and_workspace(&mut tx, project_id).await?;
        require_project_role(
            &mut tx,
            project_id,
            workspace_id,
            actor,
            CollaborationRole::Owner,
        )
        .await?;
        let id = insert_invitation(
            &mut tx,
            InvitationTarget::Project(project_id),
            actor,
            role,
            expires_at,
            token_hash,
        )
        .await?;
        tx.commit().await?;
        Ok(CreatedInvitation { id, token })
    }

    pub async fn list_workspace_invitations(
        &self,
        workspace_id: Uuid,
    ) -> Result<Vec<WorkspaceInvitation>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, workspace_id, role, created_by, created_at, expires_at
             FROM project_invitations
             WHERE workspace_id = $1 AND revoked_at IS NULL AND accepted_at IS NULL
                   AND expires_at > now()
             ORDER BY created_at DESC",
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(workspace_invitation_from_row).collect())
    }

    pub async fn list_project_invitations(
        &self,
        project_id: Uuid,
    ) -> Result<Vec<ProjectInvitation>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, project_id, role, created_by, created_at, expires_at
             FROM project_invitations
             WHERE project_id = $1 AND revoked_at IS NULL AND accepted_at IS NULL
                   AND expires_at > now()
             ORDER BY created_at DESC",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(project_invitation_from_row).collect())
    }

    pub async fn revoke_workspace_invitation(
        &self,
        workspace_id: Uuid,
        invitation_id: Uuid,
        actor: &str,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        lock_workspace(&mut tx, workspace_id).await?;
        require_workspace_role(&mut tx, workspace_id, actor, CollaborationRole::Owner).await?;
        revoke_invitation(
            &mut tx,
            invitation_id,
            InvitationTarget::Workspace(workspace_id),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn revoke_project_invitation(
        &self,
        project_id: Uuid,
        invitation_id: Uuid,
        actor: &str,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let workspace_id = lock_project_and_workspace(&mut tx, project_id).await?;
        require_project_role(
            &mut tx,
            project_id,
            workspace_id,
            actor,
            CollaborationRole::Owner,
        )
        .await?;
        revoke_invitation(
            &mut tx,
            invitation_id,
            InvitationTarget::Project(project_id),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn accept_invitation(
        &self,
        token_hash: &[u8],
        user_id: &str,
    ) -> Result<InvitationTarget, StoreError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT workspace_id, project_id, role, expires_at, revoked_at, accepted_at
             FROM project_invitations WHERE token_hash = $1 FOR UPDATE",
        )
        .bind(token_hash)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StoreError::NotFound("invitation not found".into()))?;
        let expires_at: OffsetDateTime = row.get("expires_at");
        let revoked_at: Option<OffsetDateTime> = row.get("revoked_at");
        let accepted_at: Option<OffsetDateTime> = row.get("accepted_at");
        if revoked_at.is_some() || accepted_at.is_some() {
            return Err(StoreError::NotFound("invitation not found".into()));
        }
        if expires_at <= OffsetDateTime::now_utc() {
            return Err(StoreError::Conflict("invitation has expired".into()));
        }
        let role = role_from_row(&row)?;
        let target = match (
            row.get::<Option<Uuid>, _>("workspace_id"),
            row.get::<Option<Uuid>, _>("project_id"),
        ) {
            (Some(workspace_id), None) => InvitationTarget::Workspace(workspace_id),
            (None, Some(project_id)) => InvitationTarget::Project(project_id),
            _ => return Err(StoreError::Conflict("invitation target is invalid".into())),
        };
        match target {
            InvitationTarget::Workspace(workspace_id) => {
                lock_workspace(&mut tx, workspace_id).await?;
                upsert_workspace_member(&mut tx, workspace_id, user_id, role).await?;
            }
            InvitationTarget::Project(project_id) => {
                lock_project_and_workspace(&mut tx, project_id).await?;
                upsert_project_member(&mut tx, project_id, user_id, role).await?;
            }
        }
        sqlx::query(
            "UPDATE project_invitations
             SET accepted_by = $2, accepted_at = now()
             WHERE token_hash = $1",
        )
        .bind(token_hash)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(target)
    }
}

async fn role_for_workspace<'e, E>(
    executor: E,
    workspace_id: Uuid,
    user_id: &str,
) -> Result<Option<CollaborationRole>, StoreError>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let role = sqlx::query_scalar::<_, String>(
        "SELECT role FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_optional(executor)
    .await?;
    match role {
        Some(role) => CollaborationRole::parse(&role)
            .map(Some)
            .ok_or_else(|| StoreError::Conflict("workspace membership role is invalid".into())),
        None => Ok(None),
    }
}

async fn role_for_project<'e, E>(
    executor: E,
    project_id: Uuid,
    user_id: &str,
) -> Result<Option<CollaborationRole>, StoreError>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let role = sqlx::query_scalar::<_, Option<String>>(&format!(
        "SELECT {}",
        effective_project_role_sql("$1", 2)
    ))
    .bind(project_id)
    .bind(user_id)
    .fetch_one(executor)
    .await?;
    parse_effective_role(role)
}

async fn lock_workspace(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
) -> Result<(), StoreError> {
    let row = sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace_id)
        .fetch_optional(&mut **tx)
        .await?;
    if row.is_none() {
        return Err(StoreError::NotFound("workspace not found".into()));
    }
    Ok(())
}

async fn lock_project_and_workspace(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
) -> Result<Uuid, StoreError> {
    let workspace_id =
        sqlx::query_scalar::<_, Uuid>("SELECT workspace_id FROM projects WHERE id = $1 FOR UPDATE")
            .bind(project_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| StoreError::NotFound("project not found".into()))?;
    lock_workspace(tx, workspace_id).await?;
    Ok(workspace_id)
}

async fn require_workspace_role(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    user_id: &str,
    required: CollaborationRole,
) -> Result<CollaborationRole, StoreError> {
    let role = role_for_workspace(&mut **tx, workspace_id, user_id)
        .await?
        .ok_or_else(|| StoreError::NotFound("workspace not found".into()))?;
    if has_role(role, required) {
        return Ok(role);
    }
    Err(StoreError::Forbidden(
        "workspace role is insufficient".into(),
    ))
}

async fn require_project_role(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    _workspace_id: Uuid,
    user_id: &str,
    required: CollaborationRole,
) -> Result<CollaborationRole, StoreError> {
    let role = role_for_project(&mut **tx, project_id, user_id)
        .await?
        .ok_or_else(|| StoreError::NotFound("project not found".into()))?;
    if has_role(role, required) {
        return Ok(role);
    }
    Err(StoreError::Forbidden("project role is insufficient".into()))
}

fn has_role(actual: CollaborationRole, required: CollaborationRole) -> bool {
    match required {
        CollaborationRole::Owner => actual.is_owner(),
        CollaborationRole::Editor => actual.can_edit(),
        CollaborationRole::Viewer => true,
    }
}

fn ensure_invitation_role(role: CollaborationRole) -> Result<(), StoreError> {
    if role == CollaborationRole::Owner {
        return Err(StoreError::Conflict(
            "invitations may grant editor or viewer only".into(),
        ));
    }
    Ok(())
}

async fn ensure_workspace_owner(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
) -> Result<(), StoreError> {
    let owner_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
             SELECT 1 FROM workspace_members WHERE workspace_id = $1 AND role = 'owner'
         )",
    )
    .bind(workspace_id)
    .fetch_one(&mut **tx)
    .await?;
    if owner_exists {
        return Ok(());
    }
    Err(StoreError::Conflict(
        "removing or demoting the last workspace owner is not allowed".into(),
    ))
}

async fn ensure_project_owner(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    workspace_id: Uuid,
) -> Result<(), StoreError> {
    let owner_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
             SELECT 1 FROM workspace_members WHERE workspace_id = $1 AND role = 'owner'
             UNION ALL
             SELECT 1 FROM project_members WHERE project_id = $2 AND role = 'owner'
         )",
    )
    .bind(workspace_id)
    .bind(project_id)
    .fetch_one(&mut **tx)
    .await?;
    if owner_exists {
        return Ok(());
    }
    Err(StoreError::Conflict(
        "removing or demoting the last project owner is not allowed".into(),
    ))
}

async fn insert_workspace_member(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    user_id: &str,
    role: CollaborationRole,
) -> Result<(), StoreError> {
    sqlx::query("INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, $3)")
        .bind(workspace_id)
        .bind(user_id)
        .bind(role.as_str())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn insert_project_member(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    user_id: &str,
    role: CollaborationRole,
) -> Result<(), StoreError> {
    sqlx::query("INSERT INTO project_members (project_id, user_id, role) VALUES ($1, $2, $3)")
        .bind(project_id)
        .bind(user_id)
        .bind(role.as_str())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn upsert_workspace_member(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    user_id: &str,
    role: CollaborationRole,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, user_id, role)
         VALUES ($1, $2, $3)
         ON CONFLICT (workspace_id, user_id)
         DO UPDATE SET role = CASE
             WHEN workspace_members.role = 'owner' THEN workspace_members.role
             WHEN EXCLUDED.role = 'owner' THEN EXCLUDED.role
             WHEN workspace_members.role = 'editor' THEN workspace_members.role
             ELSE EXCLUDED.role
         END",
    )
    .bind(workspace_id)
    .bind(user_id)
    .bind(role.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn upsert_project_member(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    user_id: &str,
    role: CollaborationRole,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO project_members (project_id, user_id, role)
         VALUES ($1, $2, $3)
         ON CONFLICT (project_id, user_id)
         DO UPDATE SET role = CASE
             WHEN project_members.role = 'owner' THEN project_members.role
             WHEN EXCLUDED.role = 'owner' THEN EXCLUDED.role
             WHEN project_members.role = 'editor' THEN project_members.role
             ELSE EXCLUDED.role
         END",
    )
    .bind(project_id)
    .bind(user_id)
    .bind(role.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_invitation(
    tx: &mut Transaction<'_, Postgres>,
    target: InvitationTarget,
    created_by: &str,
    role: CollaborationRole,
    expires_at: OffsetDateTime,
    token_hash: &[u8],
) -> Result<Uuid, StoreError> {
    let id = Uuid::now_v7();
    let (workspace_id, project_id) = match target {
        InvitationTarget::Workspace(workspace_id) => (Some(workspace_id), None),
        InvitationTarget::Project(project_id) => (None, Some(project_id)),
    };
    sqlx::query(
        "INSERT INTO project_invitations (
             id, workspace_id, project_id, token_hash, role, created_by, expires_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(workspace_id)
    .bind(project_id)
    .bind(token_hash)
    .bind(role.as_str())
    .bind(created_by)
    .bind(expires_at)
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

async fn revoke_invitation(
    tx: &mut Transaction<'_, Postgres>,
    invitation_id: Uuid,
    target: InvitationTarget,
) -> Result<(), StoreError> {
    let (workspace_id, project_id) = match target {
        InvitationTarget::Workspace(workspace_id) => (Some(workspace_id), None),
        InvitationTarget::Project(project_id) => (None, Some(project_id)),
    };
    let result = sqlx::query(
        "UPDATE project_invitations SET revoked_at = now()
         WHERE id = $1
           AND workspace_id IS NOT DISTINCT FROM $2
           AND project_id IS NOT DISTINCT FROM $3
           AND revoked_at IS NULL
           AND accepted_at IS NULL
           AND expires_at > now()",
    )
    .bind(invitation_id)
    .bind(workspace_id)
    .bind(project_id)
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(StoreError::NotFound("pending invitation not found".into()));
    }
    Ok(())
}

fn workspace_from_row(row: &sqlx::postgres::PgRow) -> Workspace {
    Workspace {
        id: row.get("id"),
        name: row.get("name"),
        description: row.get("description"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn workspace_with_role_from_row(row: &sqlx::postgres::PgRow) -> WorkspaceWithRole {
    WorkspaceWithRole {
        workspace: workspace_from_row(row),
        role: role_from_row(row).expect("workspace role is constrained"),
    }
}

fn workspace_member_from_row(row: &sqlx::postgres::PgRow) -> WorkspaceMember {
    WorkspaceMember {
        workspace_id: row.get("workspace_id"),
        user_id: row.get("user_id"),
        role: role_from_row(row).expect("workspace membership role is constrained"),
        created_at: row.get("created_at"),
    }
}

fn project_from_row(row: &sqlx::postgres::PgRow) -> Project {
    Project {
        id: row.get("id"),
        workspace_id: row.get("workspace_id"),
        name: row.get("name"),
        description: row.get("description"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn project_with_role_from_row(row: &sqlx::postgres::PgRow) -> ProjectWithRole {
    ProjectWithRole {
        project: project_from_row(row),
        role: role_from_row(row).expect("project role is constrained"),
    }
}

fn project_member_from_row(row: &sqlx::postgres::PgRow) -> ProjectMember {
    ProjectMember {
        project_id: row.get("project_id"),
        user_id: row.get("user_id"),
        role: role_from_row(row).expect("project membership role is constrained"),
        created_at: row.get("created_at"),
    }
}

fn workspace_invitation_from_row(row: &sqlx::postgres::PgRow) -> WorkspaceInvitation {
    WorkspaceInvitation {
        id: row.get("id"),
        workspace_id: row.get("workspace_id"),
        role: role_from_row(row).expect("invitation role is constrained"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
        expires_at: row.get("expires_at"),
    }
}

fn project_invitation_from_row(row: &sqlx::postgres::PgRow) -> ProjectInvitation {
    ProjectInvitation {
        id: row.get("id"),
        project_id: row.get("project_id"),
        role: role_from_row(row).expect("invitation role is constrained"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
        expires_at: row.get("expires_at"),
    }
}

fn role_from_row(row: &sqlx::postgres::PgRow) -> Result<CollaborationRole, StoreError> {
    let role: String = row.get("role");
    CollaborationRole::parse(&role)
        .ok_or_else(|| StoreError::Conflict("membership role is invalid".into()))
}
