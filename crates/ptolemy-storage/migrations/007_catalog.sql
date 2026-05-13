-- Data catalog: tags and metadata for datasets
CREATE TABLE IF NOT EXISTS dataset_tags (
    dataset_id UUID NOT NULL REFERENCES datasets(id) ON DELETE CASCADE,
    tag TEXT NOT NULL,
    PRIMARY KEY (dataset_id, tag)
);

CREATE TABLE IF NOT EXISTS dataset_metadata (
    dataset_id UUID PRIMARY KEY REFERENCES datasets(id) ON DELETE CASCADE,
    description TEXT NOT NULL DEFAULT '',
    source TEXT,
    license TEXT,
    attribution TEXT,
    keywords TEXT[] NOT NULL DEFAULT '{}',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_tags_tag ON dataset_tags(tag);
