-- Schema validation, topology rules, and data quality for v0.9

CREATE TABLE IF NOT EXISTS dataset_schemas (
    dataset_id UUID PRIMARY KEY REFERENCES datasets(id),
    fields JSONB NOT NULL DEFAULT '[]',
    geometry_rules JSONB NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS topology_rules (
    id UUID PRIMARY KEY,
    dataset_id UUID NOT NULL REFERENCES datasets(id),
    rule_type JSONB NOT NULL,
    description TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_topology_rules_dataset ON topology_rules(dataset_id);
