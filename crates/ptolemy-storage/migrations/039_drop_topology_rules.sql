-- drop the Esri-style rule table from 003: the rules were stored and never read
-- back, and the only validator queried columns feature_versions does not have.
-- PostGIS Topology proper is unaffected, it lives in its own schemas.
DROP INDEX IF EXISTS idx_topology_rules_dataset;
DROP TABLE IF EXISTS topology_rules;
