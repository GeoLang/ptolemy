# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Changed

- 2026-08-15: disjoint attribute edits on the same feature auto-merge. Two
  edits of the same key, or both sides moving geometry, still conflict. OGC
  API Features is described as Part 1 only. The v0.7 "QGIS plugin" line is
  HTTP endpoints.

### Added

- 2026-08-13: the Helm chart takes an external database.
  `externalDatabase.existingSecret` names a secret holding the whole
  `DATABASE_URL`, and setting it points the deployment there instead of at the
  in-cluster postgres. The URL was previously built from chart values alone,
  with no field for `sslmode`, so a chart deployed against a managed database
  connected under whatever the default `prefer` negotiated. Keeping the URL in a
  secret keeps the password out of `values.yaml` as well. Leaving the value
  empty renders exactly the in-cluster URL it rendered before.

- 2026-08-13: sqlx is built with the `tls-rustls-ring` backend, so Ptolemy can
  connect to a PostgreSQL server that refuses plaintext, such as an RDS instance
  with `rds.force_ssl`. Without it sqlx has no TLS at all: under the default
  `sslmode=prefer` it hands back a plaintext socket and the server closes the
  connection, and under `require` it fails outright. rustls, webpki-roots and ring
  were already in the dependency graph through mongodb, reqwest,
  metrics-exporter-prometheus and jsonwebtoken, so no second TLS stack is
  introduced. Local and CI databases are unaffected, since `prefer` still falls
  back to plaintext. A hosted URL needs `sslmode=verify-full`, not `require`:
  under `require` sqlx accepts any certificate and ignores `sslrootcert`
  entirely, which is encryption with nothing authenticating the far end. The
  image now carries the Amazon RDS root bundle at
  `/etc/ssl/rds-global-bundle.pem` for `sslrootcert` to name.

- 2026-08-11: Every push to master publishes `ghcr.io/geolang/ptolemy`, tagged
  `master` and `sha-<short-sha>`, and a `v*` tag publishes that version plus
  `latest`. The workflow builds the image, starts it against a PostGIS container
  and waits on `/api/v1/readyz` before pushing anything, so a commit that breaks
  startup or migrations fails here rather than in a consumer's CI. Downstream
  repos that need a real ptolemy can now pull one instead of gating their tests
  on a hand-run server.

### Changed

- 2026-08-12: sha2 on 0.11 and hmac on 0.13, which digest 0.11 requires. The API
  key digest is hex encoded by the `hex` crate instead of `{:x}`, which digest
  0.11 no longer implements. Both strings that outlive the process are pinned by
  golden tests first: `api_keys.key_hash`, or no stored key matches again, and
  the `X-Ptolemy-Signature` webhook header, which receivers verify.

- 2026-08-12: Relationship classes carry `is_composite`. `POST
  /datasets/{id}/relationships` accepts it and both `GET
  /relationship-classes/{id}` and `GET /datasets/{id}/relationships` return it.
  The column has been there since migration 013 with no route setting it, so an
  Esri geodatabase migration lost the composite flag on the way in. A body that
  omits the field creates a simple class, which is what every existing row
  already is.

- 2026-08-09: Four more routes tell a bad request apart from a server fault.
  `POST /branches/{id}/geoprocessing/split` answers 400 when PostGIS refuses the
  geometry or the splitter, `POST /branches/{id}/3d/minkowski-sum` answers 400
  when the buffer geometry is not the polygon SFCGAL needs, and
  `POST /topologies/{name}/add-face` answers 400 when the face is not a polygon
  or the topology's edges do not bound it, all three carrying the refusal. A
  topology whose edges contradict each other stays a 500, since no change to the
  request fixes it. `POST /branches/{id}/geoprocessing/contour` answers 501
  naming `ST_ContourLines` on a PostGIS build without it, which includes the 3.4
  image CI runs. The route sweep now reports no 500s.

- 2026-08-09: `POST /rasters/{id}/tiles` answers 400 for raster bytes PostGIS
  cannot decode, where it used to answer 500. The decoder's complaint is
  reported to the client instead of being logged as a server fault.

- 2026-08-08: Linear referencing route creation now derives an M dimension
  from cumulative geodesic length, so ordinary LineString WKB can be stored in
  the `LineStringM` route column and used by route events. Raster tile upload
  now decodes PostGIS raster WKB with `ST_RastFromWKB` instead of attempting a
  `bytea` to `raster` cast. Integration tests exercise both write paths.

- 2026-08-04: The routing routes work where pgRouting is installed: junction
  and edge uuids are ranked to the bigints `pgr_*` functions want with
  `row_number()` inside each statement, and the results rank back to uuids, so
  shortest-path, astar, isochrone, tsp and connectivity return real paths
  (validated end to end against pgRouting 3.8, with an integration test that
  runs wherever the extension exists). An unknown junction answers an empty
  path, not an error. The isochrone and tsp responses carry junction uuids now
  (`node`/`edge` uuid fields, `ordered_junctions`) instead of fabricated
  bigints. Migration 029 drops the unused `pgr_network_edges` view, whose
  text-to-bigint cast failed on any real row. `POST
  /branches/{id}/reproject` is removed rather than rewritten: it updated the
  read-only `features` view, so it never worked against the current schema.

- 2026-08-04: The fixable class of the route sweep's standing 500s is fixed,
  taking the sweep's report from 24 to 7, and the 7 left are fixture artifacts
  or decision-shaped (tracked in the platform DESIGN_TODO). The h3, sfcgal,
  pgRouting and pointcloud route groups answer 501 naming the missing extension
  instead of 500 (checks run after auth and validation so they never mask a 403
  or 400, and the pointcloud catalog routes stay ungated since migration 015
  only types `pa` as `pcpatch` where the extension exists). `analytics/union`
  and `geoprocessing/convex-hull` aggregate on geometry and cast the result to
  geography, which is the form PostGIS has, so both answer areas in square
  meters instead of failing every request. `analytics/anomalies` computes its
  centroid in its own CTE so the aggregate nesting is legal. `POST /incidents`
  on a missing branch is 404, fixed at the storage-layer branch lookup
  (`fetch_optional` mapped to `NotFound`). Integration tests cover each, with
  the 501 assertions flipping when the extension is installed.

- 2026-08-02: The unused org tenancy layer is gone: the `/api/v1/orgs*` routes,
  the `org_members` fallback in the permission `/check` paths, and migration
  `028` dropping `organizations`, `org_members` and `datasets.org_id`. The
  fallback could report a permission the write ladder would refuse, since the
  ladder never read orgs. Tenancy is per-user dataset grants and visibility.

- 2026-08-01: Every mounted route is called against a migrated database on every
  CI run, and a query naming a column or a table the schema does not have fails
  the build. Four feature families had shipped with one, three of them found by
  accident: nothing could catch them, because every query in `ptolemy-api` is a
  runtime `sqlx::query` and not one is a compile-time `query!`, so sqlx's offline
  checking does not apply without rewriting every call site. The new sweep,
  `crates/ptolemy-api/tests/route_sweep.rs`, reads the route list off the router
  rather than from a list someone maintains, so a route added tomorrow is covered
  without anyone remembering to add it, calls each one with fixture data, and
  fails on SQLSTATE 42703 and 42P01. It reads them off the log, since a handler
  flattens a database error to `internal error` with a 500 and the ArcGIS facade
  answers 200 with the failure in the body: `errors::log_db_error` is now the one
  place a database error is logged, and it records the SQLSTATE. A route the
  sweep cannot build a fixture for is listed in the test with a reason, and a
  route whose extractor refuses the request fails the sweep, because then the
  handler never ran and the sweep proved nothing about it.
  Three things it found are fixed with it. `/geoprocessing/voronoi` took its
  envelope from a `geometry` column that the subquery it selects from does not
  have, so the route was a 500 for every request that left `envelope` out.
  `/topologies/{name}/simplify` read `edge_id` from `ST_GetFaceEdges`, which
  returns `(sequence, edge)`, and cast a record to `topology.TopoElement`, which
  is an array: it never parsed, for any input. And the RBAC error path echoed the
  raw database message to the client, which every other module logs instead.
  The five MobilityDB analytics routes (`/trajectories/{id}/at`, `/speed`,
  `/distance`, `/simplify` and the nearest-approach route) and the four pgvector
  `similarity` routes now answer `501` naming the extension they need. All nine
  called functions or columns that only exist with the extension installed, so on
  the stock PostGIS that CI and the compose stack run they were a 500 over a name
  nothing defines. Their behaviour where the extension is present is unchanged.

- 2026-08-01: A write needs a grant. A dataset with no permission rows anywhere
  used to accept writes from any editor the role gate let through, which was a
  compatibility rule for datasets that predate enforcement, and it meant one
  forgotten grant left a dataset open to every account on the instance. The
  ladder now fails closed: with neither the branch nor the dataset holding rows,
  an enforced caller is denied. An `admin` role token still bypasses the ladder,
  and auth-off mode is unchanged, so an instance admin is who makes the first
  grant on such a dataset. Migration `027` backfills one instead, giving every
  dataset with no rows an admin grant for its `created_by`. A `created_by` that
  is blank or a machine label (`unknown`, `system`, `cli`, a connector name) is
  skipped: with auth off that column is free text from the request rather than a
  verified subject, and `unknown` in particular is the subject the OIDC callback
  used to mint tokens under, so granting to it would hand one owner to several
  people.
  Those datasets stay writable by instance admins only until one of them grants.
  Revoking a dataset's last `admin` row is still refused. Revoking its last row
  of any other kind is now allowed, because it closes the dataset instead of
  reopening it. Two things the review of that ladder turned up are fixed with it.
  The OIDC callback no longer falls back to `sub: "unknown"` when the userinfo
  lookup fails, returns a non-2xx or names no subject: it answers `502` and mints
  nothing, because a fallback subject logs every failed lookup in as one shared
  user. And a grant with a blank `user_id` is now `400` on both scopes, since it
  only ever laid down a row waiting for a token whose `sub` is empty.

- 2026-08-01: A layer's ArcGIS generation is a point on its own event clock, the
  epoch milliseconds of the latest change on `main`, instead of the depth of the
  changeset chain. Depth could not see an attachment, because uploading one commits
  no changeset: a client that loaded features in one commit and then uploaded the
  attachments recorded a generation whose changeset predated all of them, so the
  next delta reported every attachment as an add and duplicated them, while one
  deleted later was created and deleted inside that same window and was reported in
  neither list, staying forever. The clock is the newest of the head changeset's
  time and the times attachments on the branch were created and deleted at, so an
  upload moves the cursor past itself. A window is now half open, `(G, the clock at
  the submit]`, with the feature diff running from the deepest changeset at or
  before `G` and both ends fixed at the submit, so nothing is reported twice and
  nothing falls between two windows. Every comparison is on that one clock, which
  makes the boundary exact rather than approximate: `PgStore::commit` now takes a
  changeset's `created_at` from the database, as every other timestamp a commit
  writes already did, and reads it back so the answer says what was stored. A
  generation below the layer's first commit is refused naming the floor, which is
  what a cursor recorded under the old depth numbering hits: as a clock reading it
  is 1970, and answering it would report the whole layer as an add. Generation 0
  still means a full extraction. The protocol shape is unchanged, and a job id from
  before this release is refused as one this service did not issue.

- 2026-08-01: `/api/v1/datasets/{id}/style` answers the `images` a picture marker
  or fill translates into, between `layers` and `losses`. Keyed by the name the
  emitted layers reference the bitmap under, each holding the `data_uri` the symbol
  inlined and the `width` and `height` in CSS pixels the consumer registers it at.
  Always present, empty for a style with no pictures, so a consumer needs no test
  for the key. Passed through as jung-esri built it: nothing here decodes the
  base64 or looks at the bytes.

- 2026-08-01: Change files from the ArcGIS facade report attachment changes, so a
  delta no longer keeps stale attachments. Three pieces. Attachments are soft
  deleted: a delete stamps `deleted_at` and the row stays, every read on every
  route filters tombstones out, and a second delete is refused as not found the
  way it was when the row went. A layer with a real `objectid` publishes a virtual
  `globalid` field of type `esriFieldTypeGlobalID`, whose value is the feature's
  uuid as a guid in braces and upper case: `/query` serves it, `outFields` may name
  it, and `where globalid = '{...}'` and `globalid IN (...)` filter by it with or
  without braces and in any case, which is the query a consumer resolves an
  attachment's parent feature through. `applyEdits` drops a client-supplied
  `globalid` attribute as it drops a client-supplied object id on an add, and a
  row-number layer publishes no `globalIdField` at all. The change file's
  attachment sections are a time window rather than a generation diff, because an
  attachment commits no changeset: adds are what arrived after the requested
  generation's changeset and is still live, `deleteIds` are the attachment global
  ids of what was there when the window opened and went inside it, one created and
  deleted inside the window is in neither, and `updates` is always empty because a
  replacement here is a delete and an upload. An add carries `attachmentId`,
  `globalId`, `parentGlobalId`, `contentType`, `name`, `size` and an absolute
  facade download URL. The boundary is approximate: the changeset timestamp comes
  from the API process's clock and an attachment's from the database's, and the
  window has no upper bound, so an attachment arriving between the submit and the
  fetch is in the answer rather than left out with every attachment uploaded since
  the last commit.

- 2026-07-31: The ArcGIS facade accepts `X-Esri-Authorization: Bearer <jwt>`,
  the header an Esri-ecosystem client puts its token in. verne sends exactly that
  and no `Authorization` header at all, so it could reach public datasets only.
  Read on `/arcgis/rest/services` paths and nowhere else, exactly as the `token`
  request parameter is: the precedence is `Authorization`, then this header, then
  the parameter, so a client that can send the standard header is never downgraded
  and the parameter is never preferred to a header. It is a credential and not a
  promotion: the token still has to carry the role and the grant for what it is
  asking, so a viewer's token opens a private layer's metadata and is refused for
  a write. A value in any other shape than `Bearer <token>`, or an empty one,
  carries no credential.

- 2026-07-31: `having` on the ArcGIS facade's query, over the grouped answer
  `outStatistics` and `groupByFieldsForStatistics` produce. Accepted under both of
  the parameter's names, `having` and `havingClause`, because Esri's REST reference
  uses the second and the ArcGIS JS API's own property is the first; one clause
  under both names is answered and two different ones are refused. The grammar is
  the where clause's, through the same parser, and the primary form is the one the
  REST reference documents: an aggregate function over a field of the layer,
  `COUNT(houses) > 1000` or `AVG(pop) >= 20 AND MIN(score) >= 5`. The docs are
  explicit that those aggregates need not appear in `outStatistics`, so one that
  does not is computed to filter the groups and is never projected into the
  response. `COUNT(*)` and `COUNT(1)` count the rows in a group and `COUNT(field)`
  counts the values that are there, which is both what SQL does and what Esri
  documents. Naming a column the answer projects, by its grouped field name or its
  `outStatisticFieldName`, also works: the REST reference says the parameter does
  not take an `outStatisticFieldName`, so that form is an extension rather than
  the contract. The seven aggregates are the same closed set `outStatistics` has,
  a field resolves through the layer, and a numeric aggregate over a field the
  layer declares as text is refused in the same words `outStatistics` refuses it
  in. The predicate filters whole groups after the aggregation, and ordering and
  paging compose after it. Nothing a client sends reaches the SQL: every column of
  the subquery is renamed to this crate's own `c1`..`cN` and the predicate is
  rendered over those, so neither an alias nor a function a client wrote becomes
  an identifier, and every literal and every property key is bound. The where
  clause's type rules apply, from the SQL type each aggregated column already
  holds: a count compares as a whole number, `sum`, `avg`, `stddev` and `var` as
  doubles, and a `min`, a `max` or a grouped field by the kind of the field it
  read. One clause may add at most 32 aggregates the answer does not already
  carry, the same cap `outStatistics` has. It needs both parameters to have
  something to filter, and the missing one is named: an ungrouped statistics query
  is one row, which a predicate could only keep or drop whole. A bare field of the
  layer names no column of a grouped answer and is refused by name, with the
  aggregate to write instead. A layer's `advancedQueryCapabilities` now declares
  `supportsHavingClause`, and `having` is no longer refused by name.

- 2026-07-31: The ArcGIS facade tracks changes, so a client that read a service
  once can ask what moved since. A layer's generation is the depth of `main`'s
  head, the number of changesets from the root to it: the service root states
  `ChangeTracking` among its capabilities and publishes
  `changeTrackingInfo.layerServerGens`, and `POST extractChanges` takes that
  number back as `layerServerGens`, answers a `statusUrl`, whose status answers
  `Completed` with a `resultUrl` on the first ask, whose change file holds the
  object ids of the rows added, updated and deleted between that generation's
  changeset and the head, out of the store's own diff. The job carries the whole
  request in an opaque id and nothing is stored server side, and the window is
  pinned to the head the submit saw, so a commit that lands before the fetch
  belongs to the next window rather than smearing this one. A job id is untrusted
  data: the changeset it names has to be on the resolved layer's own `main` chain,
  so one edited to name another dataset's history is refused rather than answered
  with it. Only a layer with a real integer `objectid` publishes any of this, for
  the reason only such a layer takes edits, and neither does a dataset reading a
  table ptolemy does not own, whose rows change outside ptolemy's history: both
  refuse `extractChanges` by name. A change file's features carry the object id
  and no geometry, because a client fetches the rows themselves through `/query`,
  and a changed row with no object id to name it by refuses the whole window
  rather than being left out of a list that would report it unchanged. The
  attachment arrays are always empty, because the attachments table keeps no
  tombstone and a deleted attachment leaves nothing to diff. All three routes are
  reads: `extractChanges` is public like the query POST, dataset visibility
  applies to each of them, and `dataFormat=sqlite`, the positional `serverGens`
  form and a `returnInserts`/`returnUpdates`/`returnDeletes` of `false` are
  refused by name.

- 2026-07-31: The ArcGIS facade's query answers statistics, distinct values and
  any order a client asks for. `orderByFields` takes a comma list of
  `field [ASC|DESC]` over the layer's fields instead of the object id alone, with
  nulls last in both directions and the object id appended as the tiebreaker so
  paging stays deterministic. `returnDistinctValues=true` answers the distinct
  values of the fields `outFields` names, paged and orderable like rows.
  `outStatistics` answers `count`, `sum`, `min`, `max`, `avg`, `stddev` and `var`,
  grouped by `groupByFieldsForStatistics` or over everything the filters selected,
  with `count` typed as an integer, the numeric aggregates as doubles and a min or
  max keeping the type of the field it read. `where`, `objectIds` and the envelope
  compose with all three, and a layer's `advancedQueryCapabilities` now declares
  `supportsStatistics`, `supportsDistinct` and
  `supportsPaginationOnAggregatedQueries`. Nothing a client sends reaches the SQL:
  a field name is resolved through the layer and bound, a statistic type is
  matched against a closed set and rendered as this crate's own function name, and
  an `outStatisticFieldName` is a JSON key only, because the columns are read by
  position. It is still held to `^[A-Za-z_][A-Za-z0-9_]{0,63}$` and refused by
  name when it fails that, rather than escaped into place. Numeric statistics read
  the same guarded cast the where clause compares with, so a field declaring a
  type its values disagree with sums the numbers it has instead of failing the
  query. Two divergences from Esri: `returnDistinctValues` needs `outFields` to
  name its fields, because Esri's answer for `*` is the whole table back under a
  different name, and a distinct or grouped row carries an object id only when the
  client asked for that field, because no one feature is behind it.

- 2026-07-31: A migrated dataset's Esri symbology is served two ways. The ArcGIS
  facade's layer metadata carries `drawingInfo` when the dataset has a symbology
  rule whose symbol is tagged `{"format": "esri-drawing-info"}`, which is what
  verne writes when it migrates a hosted feature layer. The stored document goes
  back verbatim, so QGIS's ArcGIS REST provider and the ArcGIS JS API draw the
  data the way the original service did. A dataset with no such rule has no
  `drawingInfo` key, exactly as before. Rules are found by that format tag and
  never by the rule name, because the name a writer picks is a convention and the
  tag is what the document promises to be. `GET /api/v1/datasets/{id}/style`
  translates the same document into Mapbox GL layers for everything that is not
  an Esri client, through `jung-esri` from the sibling jung repo, and answers
  `{source, sourceLayer, layers, losses}`. `source` defaults to `ptolemy` and
  `sourceLayer` to the dataset name, both overridable by query parameter, and
  the geometry the translator draws for comes from the dataset's
  `geometry_type`. `losses` is part of the answer rather than a log line: a
  client showing a migrated layer needs to know the renderer had a size ramp
  nobody drew. A dataset with no stored Esri style answers 404, and one whose
  stored document is missing its `drawingInfo` key, holds something other than an
  object, or whose geometry has no single symbol kind to draw, answers 422 naming
  what is wrong rather than 500. Both are reads and apply dataset visibility the
  way the other read routes do.
- 2026-07-31: Attachments on the ArcGIS FeatureServer facade, reads and writes:
  `GET 0/{oid}/attachments`, `GET 0/{oid}/attachments/{attachmentId}`,
  `GET POST 0/queryAttachments`, and `POST 0/{oid}/addAttachment`,
  `0/{oid}/updateAttachment` and `0/{oid}/deleteAttachments`. They serve the same
  rows the native `/api/v1` attachment routes do, through the same store methods,
  so a file an Esri client uploads is the one the native API downloads by uuid.
  Esri's `attachmentId` is a JSON number and ptolemy's key is a uuid, so the
  number is 48 bits of SHA-256 over the uuid: the same answer in every process
  and after any number of deletes, inside the integer range a double holds
  exactly, and never positional, so no renumbering can misdirect a delete. Every
  lookup by that id lists the feature's attachments and matches on it. An id that
  names none is refused by name and an id that names two is refused as a
  collision naming the native API, rather than resolved to whichever row was
  listed first. `globalId` carries the uuid in braces and upper case, which is
  the key the native API takes. An upload is `multipart/form-data` with an
  `attachment` file part, capped at 32 MiB with the cap named in the refusal, and
  a download is always served as an attachment with `nosniff`, because the stored
  content type is whatever the uploading client said it was. The store has no
  update for an attachment, so `updateAttachment` writes the new row and then
  deletes the old one, and the result carries the new derived id.
  `deleteAttachments` is all or nothing like `applyEdits`: one unknown id refuses
  the batch before anything is deleted. `queryAttachments` groups by
  `parentObjectId`, leaves out a feature that carries none, refuses
  `definitionExpression` and `keywords` by name, and refuses an object id no
  feature carries rather than answering it as a feature with no attachments.
  Reads are anonymous like the facade's other reads. The three writes take the
  gate `applyEdits` takes: a token with write access, the same per-dataset
  ladder, the `token` request parameter, Geoservices-shaped refusals, and no
  writes at all on a layer whose object ids are row numbers, because such an id
  names a different feature after any delete. A layer's metadata now declares
  `hasAttachments` and `supportsQueryAttachments` true, whether or not it holds an
  attachment yet.

- 2026-07-31: `where` on the ArcGIS FeatureServer `/query` route, so an Esri
  client's own attribute filter runs instead of being refused. The SQL-92 subset
  those clients send: comparisons (`=`, `<>`, `!=`, `<`, `>`, `<=`, `>=`) with
  the field on either side, `IN` and `NOT IN`, `LIKE` and `NOT LIKE` with `%` and
  `_`, `BETWEEN`, `IS NULL`, `IS NOT NULL`, `AND`, `OR`, `NOT` and parentheses
  with SQL precedence, integer and decimal numbers, single-quoted strings with
  `''` for a quote, `NULL`, and `DATE` or `TIMESTAMP 'yyyy-mm-dd hh:mm:ss'`
  literals. The clause applies to rows, `returnCountOnly` and `returnIdsOnly`,
  and combines with `objectIds` and the envelope filter by AND. It is a
  recursive-descent parser in the crate rather than a SQL-parsing dependency: a
  clause parses to a tree over the layer's declared fields and then renders SQL
  with every literal bound, so no request text reaches the SQL and
  `where=name='x''; DROP TABLE datasets;--'` is one string literal that matches
  nothing. Field names are matched without regard to case against the layer's
  fields and an unknown one is refused by name. The object id compares against
  the layer's own id, a field declared integer or float compares as a number,
  everything else compares as text, and a number against a text field compares as
  the spelling the client sent. A `DATE` or `TIMESTAMP` literal is normalised to
  RFC 3339 UTC text and compared as text, which assumes stored dates are written
  that way, as verne writes them. Everything else, functions, arithmetic,
  subqueries, `CASE`, `EXTRACT`, `LIKE ... ESCAPE`, quoted identifiers, a
  comment, a `;`, is refused by name as an Esri-shaped error rather than
  silently dropped, and so is a clause longer than 32768 characters or nested
  deeper than 32.

- 2026-07-31: `POST /arcgis/rest/services/{service}/FeatureServer/0/applyEdits`,
  so an Esri client edits ptolemy rather than only reading it. Form-encoded
  `adds`, `updates` and `deletes`, esriJSON geometry in EPSG:4326 or Web Mercator
  (3857/102100, converted in-process), and every edit in one request becomes one
  commit on the dataset's `main` branch through the same store path `/api/v1`
  commits take. Deliberate divergence from Esri: because the batch is one commit,
  any failure refuses the whole batch as an `error` object naming the cause
  instead of reporting per-row results. Object ids on an add are assigned by the
  service as max + 1 and a client-supplied one is ignored. An update merges the
  attributes it carries over the ones the feature holds and keeps its geometry
  when it sends none. A layer whose ids are row numbers rather than a real
  `objectid` column refuses every edit, because a delete renumbers the rest. An
  editable layer's metadata now says `Query,Create,Update,Delete` with
  `allowGeometryUpdates` and per-field `editable` true, and a row-number layer
  still says `Query`. Writes need a token with write access and the same
  per-dataset ladder as `/api/v1`, and reads stay anonymous. Esri clients have no
  header for a credential, so a `token` request parameter is accepted on
  `/arcgis` paths only, from the query string and not from a form body: putting a
  token in a URL means anything that records URLs records it, so prefer
  `Authorization: Bearer`, which still wins when both are sent. Auth refusals on
  the facade are Geoservices-shaped, HTTP 200 with error code 499 (token
  required), 498 (invalid token) or 403 (role or grant insufficient).

- 2026-07-31: A read-only ArcGIS FeatureServer (Geoservices REST) frontend at
  `/arcgis/rest/services`, so an Esri client connects to ptolemy unchanged. One
  dataset is one single-layer service, layer id 0, read from the dataset's `main`
  branch. Serves the catalog, the service root, the layer definition and
  `/query` (GET and form POST) with `objectIds`, `outFields`, `returnGeometry`,
  `returnCountOnly`, `returnIdsOnly`, `resultOffset`/`resultRecordCount` paging
  with `exceededTransferLimit`, an `esriGeometryEnvelope` intersects filter, and
  `f=json`, `f=pjson` or `f=geojson`. OBJECTID comes from an integer `objectid`
  field when the dataset's schema declares one and is otherwise synthesized from
  feature order. A parameter it cannot honor is refused rather than ignored, and
  refusals follow the Geoservices convention of HTTP 200 with an `error` object.
  Datasets whose `geometry_type` is `geometry` or `geometry_collection` are
  excluded: an Esri layer declares exactly one geometry type. Dataset visibility
  is enforced exactly as on the other read routes. Verified against verne's
  extractor: `verne inspect` and `verne extract` both run clean.

- `geometry_type: "geometry"` on a dataset, for a source container whose features
  differ from each other. Distinct from `geometry_collection`, which is one
  feature whose geometry is a collection.

- Attachments may belong to a dataset instead of a feature, via
  `GET`/`POST /datasets/{id}/attachments`. A style's icon or overlay image
  belongs to no single feature. Download, meta and delete serve both kinds.

- A feature version carries an optional valid time, `valid_from` and `valid_to`,
  set per operation on commit and returned on reads. Both null means no time was
  recorded. `GET /branches/{id}/features?valid_at=<RFC3339>` keeps only the
  features whose half-open range `[valid_from, valid_to)` covers the instant.

- A feature version may carry the geometry as its source recorded it, before
  reprojection to 4326: `native_geometry_wkb_hex` on a commit operation, with
  its reference as exactly one of `native_srid` (an EPSG code) or
  `native_crs_wkt` (the WKT definition, for a reference no single code names,
  such as a compound one). Read back exactly with
  `GET /branches/{id}/features/{feature_id}/native`. NULL means the version has
  no distinct original: a 4326 source, an edit, or a repaired geometry. An
  original with srid 4326 or a blank WKT is stored as NULL rather than as a
  duplicate or an unstatable claim, an update never inherits the previous
  version's original, and a merge carries originals across unchanged.

### Fixed

- 2026-08-13: `POST /branches/{id}/geoprocessing/merge` and
  `POST /branches/{id}/geoprocessing/simplify` no longer panic on a NULL
  geometry. `ST_Union` over ids that match nothing is NULL rather than an empty
  geometry, and `ST_Simplify` returns NULL for a feature it simplified away, so
  a request either way killed the handler mid-response instead of answering.
  Both now read the geometry as nullable and answer 200 with it null, which is
  what `convex-hull` already did for a branch with no features. `simplify`
  reports `points_after` 0 for a feature that collapsed. `contour` has the same
  latent read, and is left: its query pairs `ST_Dump` with `generate_series`,
  which pads the shorter side with NULLs, but no PostGIS build has the
  `ST_ContourLines` the query calls, so the route answers 501 before it can
  reach the read.

- 2026-08-13: `GET /branches/{id}/permissions/{user}/check?required=write` no
  longer answers allowed for a user the write would refuse. It read the user's
  branch row and fell back to their dataset row when there was none, while a
  write stops at the branch scope as soon as that branch has any rows at all, so
  a dataset write grant was reported as covering a branch that had been given
  its own grantees. The check now reads the same two scopes the write ladder
  reads and picks between them the same way. A `read` check still counts a
  dataset grant, which is what dataset visibility does. Both check routes also
  reject a `required` that is not `read`, `write` or `admin` with a 400, since
  any other string ranked below every grant and answered allowed for anyone
  holding a row.

- 2026-08-13: the conflict listing, merge preview and resolve routes decide a
  conflict the same way the merge does: a side whose version still matches the
  merge base did not change the feature, so an earlier merge's own copy of the
  other branch's work is no longer listed as conflicting or held for a
  resolution. The listing and preview now report what the base held instead of
  leaving those fields null.

- 2026-08-13: merging a branch that is already merged answers up to date instead
  of writing another merge changeset. A merge commit now records the source head
  it brought in (`changesets.merge_parent_id`, migration 030), every lineage walk
  follows both parents, and the merge base advances, so a re-merge carries only
  what the source changed since the last one. Conflicts are decided against the
  base, so a previous merge's own copy of the source's work no longer reads as a
  conflicting edit. The merge response gains `up_to_date`, with `changeset` null
  exactly when it is true.

- 2026-07-31: two `applyEdits` batches of adds racing on one layer could be given
  the same object ids. The highest id is read before the commit that raises it, so
  both batches read the same one; nothing downstream refuses a duplicate, because
  an objectid column is an ordinary property with no unique constraint behind it.
  The read and the commit now run under a transaction-scoped Postgres advisory
  lock keyed on the branch, so the assignment is serialized per layer while reads
  and edits to other layers run untouched. The lock cannot outlive the request:
  the database releases it when the transaction ends. A batch that waits more than
  five seconds for it is refused with code 503 and succeeds on a retry, which is
  also what keeps a pool whose connections are all waiting from deadlocking.

- 2026-07-31: `RUST_LOG` with `tower_http=debug` logged the `token` query
  parameter, which is a live credential on the ArcGIS facade because the
  Geoservices protocol has no header for one. The request span now records the uri
  with the token value replaced by `REDACTED` and everything else in it intact.

- 2026-07-31: the ArcGIS `where` clause and the object id read rendered the
  property key into the SQL as a quoted literal, which is safe only while
  `standard_conforming_strings` is on. Both now bind the key as a parameter, so no
  property key can be quoted wrongly whatever it holds. A derived-fields layer
  takes its field names from the keys its features carry, so those keys are
  arbitrary text.

- 2026-07-31: `applyEdits` refused an add with no geometry as "an added feature
  needs a geometry", which read as a bug rather than a limit. It now names the
  layer and says why: every feature here is a geometry and its attributes, so
  Esri's attribute-only add, for a table or a shape filled in later, has nothing
  to store. An update may still carry attributes alone and keeps the geometry the
  feature has.

- `POST /branches/{id}/import/geojson` and `/import/csv` imported nothing: every
  row failed on the `feature_versions` primary key and the missing `dataset_id`,
  and the endpoint still answered 200 with `imported: 0`. Both now build the
  parsed features into one changeset through the normal commit path, so imported
  data is visible to branch reads, and a request whose rows all fail answers 422.

- A write of 1000 rows or more refreshes planner statistics on `feature_versions`,
  `changesets` and `branches` after it commits, so reads served right after a bulk
  import are no longer planned against pre-import statistics. Tunable with
  `PTOLEMY_ANALYZE_ROW_THRESHOLD`.

## [0.1.0] - 2026-05-30

### Added

- Initial release.
