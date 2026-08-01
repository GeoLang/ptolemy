-- ─── Every dataset that can name an owner gets one ───────────────────
-- A dataset with no permission rows used to accept writes from any editor the
-- role gate let through. That rule is gone: the write ladder now denies an
-- enforced caller unless a grant allows them. Datasets that predate the creator
-- auto-grant would be left writable by instance admins only, so give each of
-- them an admin grant for its creator.
--
-- datasets.created_by is only a verified token subject when the dataset was
-- created with auth on. With auth off it is whatever the request body or the
-- CLI flag said, and the connectors write a machine label there. Those are not
-- identities anyone can present a token for, so they are skipped: such a
-- dataset stays writable by instance admins only until one of them grants.
-- 'unknown' is in the list because logins have been recorded under it, so an
-- owner by that name would be shared by whoever they were.
--
-- Only the dataset scope is filled in. A branch that already has its own rows
-- keeps deciding its own writes, so this cannot widen access to one.

INSERT INTO dataset_permissions (id, dataset_id, user_id, permission, granted_by)
SELECT gen_random_uuid(), d.id, btrim(d.created_by), 'admin', btrim(d.created_by)
  FROM datasets d
 WHERE NOT EXISTS (
           SELECT 1 FROM dataset_permissions p WHERE p.dataset_id = d.id
       )
   AND btrim(d.created_by) <> ''
   AND lower(btrim(d.created_by)) NOT IN (
           'anonymous',
           'unknown',
           'system',
           'cli',
           'arcgis',
           'elasticsearch',
           'geopackage',
           'mongodb'
       );
