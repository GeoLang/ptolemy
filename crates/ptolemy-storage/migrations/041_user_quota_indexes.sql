-- the per-user quotas count these rows by creator on every insert they limit
CREATE INDEX IF NOT EXISTS idx_attachments_created_by
    ON attachments(created_by) INCLUDE (size_bytes);
CREATE INDEX IF NOT EXISTS idx_workspaces_created_by ON workspaces(created_by);
CREATE INDEX IF NOT EXISTS idx_projects_created_by ON projects(created_by);
CREATE INDEX IF NOT EXISTS idx_project_invitations_created_by
    ON project_invitations(created_by);
