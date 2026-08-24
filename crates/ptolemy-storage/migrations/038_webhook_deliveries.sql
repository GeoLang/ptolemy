-- The webhook outbox: one row per (subscription, event).
--
-- Rows are inserted in the same transaction as the change that produced the
-- event, so a delivery exists exactly when the commit it describes does. The
-- worker claims a due row by bumping `attempts` and pushing `next_attempt_at`
-- forward, then does the HTTP call outside any transaction, so a crash mid
-- delivery costs one retry rather than a lost event.
--
-- A row with `delivered_at IS NULL` and `attempts` at the worker's bound is a
-- dead letter: nothing will pick it up again and `last_error` says why.
CREATE TABLE IF NOT EXISTS webhook_deliveries (
    webhook_id UUID NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
    event_id UUID NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    attempts INT NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    delivered_at TIMESTAMPTZ,
    last_error TEXT,
    PRIMARY KEY (webhook_id, event_id)
);

CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_due
    ON webhook_deliveries (next_attempt_at)
    WHERE delivered_at IS NULL;
