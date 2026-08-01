# Open items

## Facade change files: report attachment changes

Change files from `extractChanges` always answer empty attachment arrays, so a
verne delta from the facade silently keeps stale attachments. Closing it needs
three pieces, in one ptolemy task:

- Soft-delete tombstones: migration adds `deleted_at TIMESTAMPTZ NULL` to
  `attachments`, delete becomes an update, every reader filters
  `deleted_at IS NULL`. Needed because the table keeps no history, so deletes
  cannot be diffed today.
- Window mapping: attachment changes do not advance the generation (gens are
  changeset depth, attachments commit no changesets). Approximate the window by
  time: include attachments whose `created_at`/`deleted_at` fall after the
  window-start changeset's `created_at`. Single DB clock, documented as
  approximate.
- Virtual `globalid` field on the facade: layer metadata declares
  `globalIdField`, queries serve the feature uuid in braces under it, and
  `where globalid IN (...)` works (verne resolves attachment parents that did
  not themselves change through exactly that query). Change-file attachment
  records carry `globalId` (attachment uuid) and `parentGlobalId` (feature
  uuid) in the shape verne pairs by.

Acceptance: verne full extract from the facade, add/replace/delete attachments
via the facade routes, `verne extract --since` carries the attachment ops.

## /style passes through translated images

jung-esri (jung commit ef5dc62) now emits `Translation.images`
(`{"<name>": {"data_uri", "width", "height"}}`) for picture markers/fills.
Ptolemy's `/api/v1/datasets/{id}/style` predates that: bump the jung git dep
and include `images` in the response. Viewtopia already consumes the key and
tolerates its absence (viewtopia b21cc01e).

## having: verify against a live Esri client

The `having` grammar follows Esri's REST docs (aggregate functions over source
fields, both `having` and `havingClause` spellings). No live client emission
was ever captured: sampleserver6 advertises `supportsHavingClause` but refuses
every having request, and the docs contradict themselves on whether having
aggregates must appear in `outStatistics` (we allow either). Before advertising
Esri compatibility for dashboards, capture one real request from ArcGIS JS API
or Dashboards and check it parses.

## verne against private ptolemy: partially closed

Ptolemy now accepts `X-Esri-Authorization` on facade paths, so
`VERNE_ARCGIS_TOKEN` with a ptolemy JWT works for extraction. Untested live;
the change-tracking gate ran against a public dataset.
