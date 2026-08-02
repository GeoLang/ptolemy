-- drop the unused org tenancy layer from 008: nothing enforced it, and the
-- org_members fallback in the permission checks could report access the write
-- ladder would refuse. tenancy is per-user dataset grants and visibility.
DROP INDEX IF EXISTS idx_datasets_org;
ALTER TABLE datasets DROP COLUMN IF EXISTS org_id;
DROP TABLE IF EXISTS org_members;
DROP TABLE IF EXISTS organizations;
