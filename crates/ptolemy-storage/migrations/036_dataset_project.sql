-- A dataset may belong to one project. Project membership then carries access to
-- it: viewer reads, editor writes, owner administers, folded in as the stronger
-- of that and any explicit grant row.
--
-- NULL means no project, which is every dataset that existed before this column
-- and every dataset nobody attaches: only explicit dataset and branch grants
-- decide, exactly as before.
--
-- ON DELETE SET NULL rather than CASCADE: dropping a project must not drop
-- anyone's data. The dataset was made private when it was attached and nothing
-- here changes that, so losing the project closes access rather than opening it.

ALTER TABLE datasets
    ADD COLUMN project_id UUID REFERENCES projects(id) ON DELETE SET NULL;

CREATE INDEX idx_datasets_project ON datasets(project_id) WHERE project_id IS NOT NULL;
