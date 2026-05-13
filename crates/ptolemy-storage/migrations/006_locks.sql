-- Feature locking for pessimistic concurrency control
CREATE TABLE IF NOT EXISTS feature_locks (
    feature_id UUID NOT NULL,
    branch_id UUID NOT NULL REFERENCES branches(id),
    locked_by TEXT NOT NULL,
    locked_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL DEFAULT (now() + interval '1 hour'),
    reason TEXT,
    PRIMARY KEY (feature_id, branch_id)
);

CREATE INDEX IF NOT EXISTS idx_locks_branch ON feature_locks(branch_id);
CREATE INDEX IF NOT EXISTS idx_locks_expires ON feature_locks(expires_at);
