# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added

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
