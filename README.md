# Ptolemy

[![CI](https://github.com/GeoLang/ptolemy/actions/workflows/ci.yml/badge.svg)](https://github.com/GeoLang/ptolemy/actions)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)

**Open-source enterprise geodatabase & collaboration platform.**

Ptolemy provides versioned spatial data management — branch, commit, diff, and merge geographic datasets with git-like workflows. Built on PostGIS, designed for teams.

## Why Ptolemy?

Enterprise GIS users are locked into proprietary platforms (Esri, Hexagon) primarily because of versioned geodatabase workflows — multi-user editing with conflict detection, branching, and audit trails. Ptolemy brings these capabilities to the open-source stack.

## Third-Party Integrations

Ptolemy leverages the best battle-tested PostgreSQL extensions and standards:

| Extension | Purpose |
|-----------|---------|
| **pgRouting** | Graph routing: Dijkstra, A*, TSP, isochrones, connected components |
| **PostGIS Topology** | Native topology primitives (faces, edges, nodes), validation |
| **SFCGAL** | 3D geometry operations: extrude, volume, Minkowski sum, straight skeleton |
| **h3-pg** | Uber H3 hexagonal spatial indexing, aggregation, compaction |
| **pg_partman** | Automatic time-based partitioning (audit logs) |
| **pgvector** | Vector similarity search, feature deduplication, distance-ranked bucketing. Without it the `similarity` routes answer `501` |
| **pg_trgm** | Fuzzy text search for data catalog |
| **pointcloud** | LiDAR/point cloud storage and spatial queries |
| **MobilityDB** | Moving object trajectories, speed/distance analysis. Without it a trajectory is stored as JSONB and the analytics routes answer `501` |

### Standards Implemented

- **STAC 1.0** — SpatioTemporal Asset Catalog for raster discovery
- **OGC Tiles** — Standard tile matrix sets (WebMercatorQuad, WorldCRS84Quad)
- **CQL2** — Common Query Language for spatial/attribute filtering
- **OGC API - Features** — Part 1 & 2 compliant
- **ArcGIS Geoservices REST** — FeatureServer reads, `applyEdits` writes, attachments and `extractChanges` deltas, so Esri clients connect unchanged

### Key Features (Roadmap)

| Version | Milestone | Status |
|---------|-----------|--------|
| **v0.1** | Core types, Store trait, API skeleton, CLI | ✓ Done |
| **v0.2** | SQL migrations, full CRUD, branching, changesets, commit engine | ✓ Done |
| **v0.3** | Diff engine, three-way merge, conflict detection, REST API | ✓ Done |
| **v0.4** | Auth (JWT/RBAC), WebSocket collaboration, CLI workflows, GeoJSON I/O | ✓ Done |
| **v0.5** | Prometheus metrics, OIDC SSO, graceful shutdown, connection pool tuning | ✓ Done |
| **v0.6** | Spatial query API, MVT tile serving, pagination, batch operations | ✓ Done |
| **v0.7** | QGIS plugin, offline sync protocol, field-to-server workflows | ✓ Done |
| **v0.8** | Web review UI, pull-request-style geodata review, map diffs | ✓ Done |
| **v0.9** | Schema validation, topology rules, data quality reports | ✓ Done |
| **v1.0** | Webhooks, CDC event stream, change notifications | ✓ Done |
| **v1.1** | Spatial analytics (buffer, union, clustering, anomaly detection) | ✓ Done |
| **v1.2** | OGC API - Features compliance, audit logging | ✓ Done |
| **v1.3** | Webhook delivery engine, schema enforcement, topology gate | ✓ Done |
| **v1.4** | SSE streaming, feature locking, temporal queries | ✓ Done |
| **v1.5** | Data catalog, multi-tenancy, rate limiting | ✓ Done |
| **v1.6** | Background jobs, conflict resolution API | ✓ Done |

## Architecture

```
┌───────────────────────────────────────────┐
│  Clients (QGIS Plugin, Web UI, CLI)       │
├───────────────────────────────────────────┤
│  ptolemy-api (Axum REST service)          │
│  - Dataset CRUD                           │
│  - Branch/commit/merge operations         │
│  - Feature read/write scoped to branches  │
│  - Change subscriptions (webhooks/SSE)    │
├───────────────────────────────────────────┤
│  ptolemy-core (domain types & logic)      │
│  - Changeset DAG                          │
│  - Three-way merge algorithm              │
│  - Diff computation (geometry + attrs)    │
├───────────────────────────────────────────┤
│  ptolemy-storage (backend abstraction)    │
│  - PostgreSQL/PostGIS implementation      │
│  - Temporal tables for version history    │
│  - Spatial indexes on all versions        │
├───────────────────────────────────────────┤
│  PostgreSQL + PostGIS                     │
└───────────────────────────────────────────┘
```

## Data Model

Ptolemy uses a **changeset DAG** (directed acyclic graph) inspired by git:

- **Dataset**: A collection of spatial features with shared schema (≈ feature class).
- **Branch**: A named pointer to the latest changeset. Default branch is `main`.
- **Changeset**: An atomic set of feature edits (insert/update/delete). Each changeset points to its parent(s), forming the DAG.
- **Feature**: A spatial object with UUID, WKB geometry, and JSON properties.

### Merge Strategy

Three-way merge using the common ancestor changeset:
1. Compute diff(ancestor → ours) and diff(ancestor → theirs).
2. Non-conflicting changes (different features, or same feature different attributes) merge automatically.
3. Conflicting changes (same feature, same attribute modified differently) are surfaced for manual resolution.
4. Geometry conflicts use spatial comparison (tolerance-based equality).

### Your data is just PostGIS

Ptolemy stores features in plain PostGIS tables, not a proprietary format. Every
feature version is a row in `feature_versions` with a PostGIS `geometry` column
(GIST-indexed) and a JSONB `properties` column; the `features` view resolves each
branch to its current feature set. Anything that speaks PostgreSQL can read it
directly, with or without any Ptolemy service running:

```sql
-- current features on a branch, plain SQL
SELECT id, geometry, properties
FROM features
WHERE branch_id = '...'
  AND ST_DWithin(geometry, ST_Point(7.42, 43.73)::geography, 500);
```

psql, GDAL/OGR (`ogr2ogr -f GPKG out.gpkg PG:"dbname=ptolemy" -sql "..."`), and
QGIS's native PostGIS connector all work against the database as-is. Backup and
restore is standard `pg_dump`/`pg_restore`. If you stop using Ptolemy, your data
is already in the most widely supported spatial database there is.

One caveat for ad-hoc SQL: the `features` view walks the changeset chain of every
branch in the database before your `WHERE branch_id = …` filters it, so its cost
is set by total instance history, not by the branch you asked for. That is fine
for interactive queries and wrong for a hot path — the API does not use the view,
it builds the same rows from the one branch's ancestor chain per query. On an
instance with 89k changesets, reading a 100-feature branch was 115 ms through the
view and 8.9 ms branch-scoped.

#### Browse your existing PostGIS read-only

The reverse also works: point Ptolemy at tables you already have and browse them
through the normal API and the viewer, without importing or copying anything.
Register the relation as an *external dataset*:

```bash
curl -X POST http://localhost:3000/api/v1/datasets \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{
    "name": "parcels",
    "created_by": "you",
    "external_table": "public.parcels",
    "external_id_column": "gid",
    "external_geometry_column": "geom"
  }'
```

Registration checks the relation exists, that the geometry column really is
PostGIS geometry, and that Ptolemy can select from it, then creates the dataset
and its `main` branch. From there the ordinary read endpoints work: feature
listing and paging, bbox, CQL2 filters, OGC API - Features collections and
items, GeoJSON/CSV export, vector tiles.

The dataset is read-only. Commits, merges, imports, QGIS push and branch
creation all return 409. Non-geometry columns become the feature `properties`;
each row's id is hashed into a stable UUID, and the original key stays in
`properties`. Geometry not in EPSG:4326 is reprojected on read.

Every non-geometry column is published, and a `public` dataset serves reads to
anonymous callers. Register a view that selects only the columns you want
public, not a table with columns you don't. If the relation has columns that
must stay internal, register it with `"visibility": "private"` as well — an
external dataset is gated on read exactly like a versioned one (see
[Access control](#access-control)).

**An ordinary GiST index on the geometry column is all you need**, whatever the
relation's SRID. Ptolemy exposes external geometry in EPSG:4326, so on a projected
relation the read's own predicate sits on `ST_Transform(geom, 4326)`, which no
index covers. Spatial reads therefore also push a second, index-served predicate
onto the relation's own column in its own SRID: the query window is reprojected
into that SRID and widened slightly, so it can only admit extra candidate rows,
never drop one — the exact 4326 predicate still decides the result. On a 60k-row
polygon table in EPSG:3857 that took bbox from 213 ms (sequential scan) to 63 ms
(index scan), and a z6 tile from 268 ms to 18 ms. Covers bbox, intersects, within,
vector tiles, OGC items and CQL2 spatial filters.

Two things it deliberately does not do. A window wider than 45° or reaching past
±85° latitude is not reprojected at all (PROJ may reject it), so those near-global
reads scan as before — they return most of the relation anyway. And a CQL2 spatial
op under `or` or `not` is not pushed down, because a matching row need not satisfy
it.

Set `PTOLEMY_EXTERNAL_DATABASE_URL` to read external datasets from a different
database than Ptolemy's own. Use a role with `SELECT` and nothing else:

```sql
CREATE ROLE ptolemy_ro LOGIN PASSWORD '...';
GRANT CONNECT ON DATABASE yourdb TO ptolemy_ro;
GRANT USAGE ON SCHEMA public TO ptolemy_ro;
GRANT SELECT ON public.parcels TO ptolemy_ro;
```

Then the read-only guarantee is enforced by PostgreSQL, not only by Ptolemy.

## Quick Start

```bash
# Prerequisites: PostgreSQL with PostGIS extension
createdb ptolemy
psql ptolemy -c "CREATE EXTENSION postgis;"

# Run migrations
ptolemy migrate --database-url postgres://localhost/ptolemy

# Start the server
ptolemy serve --database-url postgres://localhost/ptolemy

# API is now available at http://localhost:3000/api/v1
# Metrics at http://localhost:3000/metrics
```

## Container image

Every push to `master` publishes `ghcr.io/geolang/ptolemy`, tagged `master` and
`sha-<short-sha>`; a `v*` tag publishes that version plus `latest`. Pin to a
`sha-` tag if you need a fixed API surface.

```bash
docker run -p 3000:3000 \
  -e DATABASE_URL=postgres://ptolemy:ptolemy@db/ptolemy \
  -e PTOLEMY_JWT_SECRET=$(openssl rand -hex 32) \
  ghcr.io/geolang/ptolemy:master
```

`DATABASE_URL` and `PTOLEMY_JWT_SECRET` are the two it refuses to start without,
the second unless `PTOLEMY_AUTH_DISABLED=true`. Everything else in
[Configuration](#configuration) has a default.

`serve` applies migrations before it binds, so no separate `ptolemy migrate` step
is needed. The image binds `0.0.0.0:3000` and answers `/api/v1/healthz` as soon
as the process is up and `/api/v1/readyz` once the database is reachable; wait on
`readyz` rather than `healthz` if you are about to issue requests.

Pair it with `postgis/postgis:16-3.4`. The first migration declares a PostGIS
`geometry` column, so a database without the extension fails to migrate; H3,
pgRouting, SFCGAL and the rest are created if installed and skipped if not.

## Helm chart

`deploy/helm/ptolemy` runs the image against an in-cluster postgres, whose URL
it builds from `postgresql.auth`.

```bash
helm install ptolemy deploy/helm/ptolemy
```

To use a database outside the cluster instead, put the whole `DATABASE_URL` in a
secret and name it. The URL is then yours to write, so it can carry the
`sslmode` and `sslrootcert` a managed database wants, and the password stays out
of `values.yaml`.

```bash
kubectl create secret generic ptolemy-database \
  --from-literal=url='postgres://user:pass@host/ptolemy?sslmode=verify-full'

helm install ptolemy deploy/helm/ptolemy \
  --set externalDatabase.existingSecret=ptolemy-database
```

The key defaults to `url`, and `externalDatabase.existingSecretKey` changes it.
`postgresql.auth` goes unread once the secret is named.

## Database TLS

Ptolemy is built with rustls, so it can connect to a PostgreSQL server that
requires TLS. Which protection you get is decided entirely by the `sslmode`
parameter on `DATABASE_URL`, and the default is `prefer`: try TLS, and drop back
to a plaintext socket if the server will not do it. That is what makes the local
PostGIS container and the test database work with no query string at all.

For anything not on localhost, put `sslmode=verify-full` on the URL. Beware
`sslmode=require`, which is what most hosting providers tell you to paste: it
encrypts the connection but accepts any certificate the server offers, including
one from an attacker in the middle, and it stays that way even if you also set
`sslrootcert`. Only `verify-ca` and `verify-full` check the certificate, and only
`verify-full` checks that the hostname matches it.

The Mozilla root bundle is compiled into the binary, so a provider whose
certificate chains to a public CA needs nothing more than `verify-full`. Neon is
one of those. Amazon RDS is not: it signs with Amazon's own RDS roots, which no
public root store carries. Give it the bundle explicitly.

```
DATABASE_URL=postgres://user:pass@host/ptolemy?sslmode=verify-full&sslrootcert=/etc/ssl/rds-global-bundle.pem
```

The container image ships that bundle at `/etc/ssl/rds-global-bundle.pem`, pulled
from `https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem` at build
time. Running the binary outside the image means downloading it yourself and
pointing `sslrootcert` wherever you put it. `sslrootcert` adds to the compiled-in
roots rather than replacing them, so the same binary still verifies public CAs.
`PTOLEMY_EXTERNAL_DATABASE_URL` takes the same parameters and needs its own copy
of them. On the Helm chart the URL carrying them is the one in
`externalDatabase.existingSecret`.

## Configuration

| Variable | Description | Default |
|----------|-------------|---------|
| `DATABASE_URL` | PostgreSQL connection URL | (required) |
| `PTOLEMY_JWT_SECRET` | JWT signing secret, 32+ bytes (required to serve) | (required) |
| `PTOLEMY_AUTH_DISABLED` | Set to `true` to serve with auth off | `false` |
| `PTOLEMY_OIDC_ISSUER_URL` | OIDC provider URL (e.g. Keycloak realm) | (disabled) |
| `PTOLEMY_OIDC_CLIENT_ID` | OAuth2 client ID | — |
| `PTOLEMY_OIDC_CLIENT_SECRET` | OAuth2 client secret | — |
| `PTOLEMY_OIDC_REDIRECT_URL` | Callback URL for OIDC flow | — |
| `PTOLEMY_EXTERNAL_DATABASE_URL` | Database holding external datasets; use a read-only role | (primary pool) |
| `PTOLEMY_DB_MAX_CONNECTIONS` | Max DB pool connections | 10 |
| `PTOLEMY_DB_MIN_CONNECTIONS` | Min DB pool connections | 2 |
| `PTOLEMY_ANALYZE_ROW_THRESHOLD` | Rows in one write that trigger a planner-statistics refresh; `0` leaves it to autoanalyze | 1000 |

### Planner statistics after a bulk import

Every branch read walks the changeset ancestor chain, and postgres picks that
plan from the statistics it holds for `feature_versions`, `changesets` and
`branches`. Straight after an import those statistics still describe an empty
database, so reads get a plan sized for an empty table and cost tens of
milliseconds each until autoanalyze catches up, minutes later. A write that
touches at least `PTOLEMY_ANALYZE_ROW_THRESHOLD` rows therefore runs `ANALYZE`
on those three tables once it has committed, off the request path, and
concurrent bulk writes share one run. It cannot fail or delay a write: if the
database refuses the `ANALYZE`, for instance because the connecting role does
not own the tables, the failure is logged and autoanalyze takes over.

With auth on, audit fields (`author`, `created_by`, `granted_by`, `locked_by`)
are taken from the token subject and the value in the request body is ignored.
With `PTOLEMY_AUTH_DISABLED=true` there is no token, so the body value is
recorded as-is.

## ArcGIS FeatureServer

Point an Esri client at `/arcgis/rest/services` and it sees a FeatureServer.
Each dataset is one single-layer service, `{service}` is the dataset name or its
uuid, the layer is always id 0, and everything runs on the dataset's `main`
branch. Dataset visibility applies exactly as it does to the other read routes.

Query supports `where` (the SQL-92 subset Esri clients send: comparisons, `IN`,
`LIKE`, `BETWEEN`, `IS NULL`, boolean logic and `DATE` literals), `objectIds`,
`outFields`, `returnGeometry`, `returnCountOnly`, `returnIdsOnly`,
`orderByFields` over any field with `ASC` or `DESC`,
`resultOffset`/`resultRecordCount` paging with `exceededTransferLimit`, an
`esriGeometryEnvelope` + `esriSpatialRelIntersects` filter, `outSR` and `inSR`
as 4326 or Web Mercator (3857/102100), and `f=json`, `f=pjson` or `f=geojson`.
`returnDistinctValues` answers the distinct values of the fields `outFields`
names, and `outStatistics` with `groupByFieldsForStatistics` answers `count`,
`sum`, `min`, `max`, `avg`, `stddev` and `var`. Both of those answer attributes
and no geometry, page the way rows do, and take an `orderByFields` over the
columns they return. `having` (or `havingClause`, the two names Esri's own
docs and JS API use) filters the grouped answer, in the same grammar as `where`.
It names aggregates rather than rows: `COUNT(houses) > 1000`,
`AVG(pop) >= 20 AND MIN(score) >= 5`, over the same seven functions
`outStatistics` offers. An aggregate it names need not be in `outStatistics`, and
one that is not is computed to filter the groups and not served back.
`COUNT(*)` and `COUNT(1)` count rows, `COUNT(field)` counts the values that are
there. Naming a projected column instead, by its grouped field name or its
`outStatisticFieldName`, also works as an extension. It needs both
`outStatistics` and `groupByFieldsForStatistics`, and ordering and paging apply
after it. Anything else it cannot honor is refused rather than ignored. Refusals
follow the Geoservices convention: HTTP 200 with an `{"error": {...}}` body.

Two divergences worth knowing on the aggregated shapes: `returnDistinctValues`
needs `outFields` to name its fields, because Esri's answer for `*` is the whole
table back under a different name, and a distinct or grouped row carries an
object id only when the client asked for that field, because no one feature is
behind it.

`applyEdits` writes: the whole batch becomes one commit on `main` and any
failure refuses all of it. Layers need a real integer `objectid` field to be
editable.

Credentials on these routes: `Authorization: Bearer <jwt>` as everywhere else,
then `X-Esri-Authorization: Bearer <jwt>`, which is where an Esri-ecosystem
client such as verne puts its token, then a `token` request parameter, for a
browser-hosted client that can send no header at all. The first one present wins.
Both of the extra forms are read under `/arcgis/rest/services` and nowhere else,
and neither grants anything the standard header would not: the token still needs
the role and the grant for what it is asking.

Attachments are served and edited through the Esri routes (per-feature list and
download, `queryAttachments`, multipart `addAttachment`/`updateAttachment`/
`deleteAttachments`), with the writes gated like `applyEdits`.

`extractChanges` answers what changed on `main` since a generation the client
already holds. A layer's generation is a point on its own event clock: the epoch
milliseconds of the latest thing that happened on `main`, which is the newest of
the head changeset's time and the times attachments on the branch were created and
deleted at, and 0 for a branch nothing has happened to. The service root states
`ChangeTracking` among its capabilities and publishes
`changeTrackingInfo.layerServerGens`, and a client sends that number back.

A clock rather than a count of commits, because uploading an attachment commits no
changeset. A count left every attachment invisible to the cursor: a client that
loaded features in one commit and then uploaded the attachments recorded a
generation whose changeset predated all of them, so the next delta reported them
all as adds and duplicated them, while one deleted later was created and deleted
inside that same window and was reported in neither list, staying forever.

The job is stateless: `POST extractChanges` with `layers=0` and
`layerServerGens=[{"id": 0, "serverGen": <n>}]` answers a `statusUrl`, the status
answers `Completed` with a `resultUrl` on the first ask, and the change file holds
the object ids of the rows added, updated and deleted in the window. A window is
half open, `(<n>, the clock at the submit]`: the features in it are the diff from
the deepest changeset at or before `<n>`, which is the newest state the client
already held, to the head the submit pinned. Both ends are fixed at the submit, so
anything landing between the submit and the fetch belongs to the next window, and
the generation the change file reports is where that next one opens. A job id is
opaque and carries the whole request, so nothing is stored server side, and one
this service did not issue is refused rather than answered.

A generation below the layer's first commit is refused naming the floor, and so is
one ahead of its clock. Both are cursors this service's clock could not have
issued: in particular a cursor recorded when generations counted commits is a small
number, which as a clock reading is 1970 and would open a window before the layer
existed and report everything in it as an add. The refusal says to extract the
layer in full and record the generation that answer carries. Generation 0 is not
one of those, being the clock an untouched branch publishes and a full extraction.

Only a layer with a real integer `objectid` publishes any of that, for the same
reason only such a layer takes edits, and neither does a dataset whose rows come
from a table ptolemy does not own, whose rows change outside ptolemy's history.
Features in a change file carry the object id and no geometry, because a client
fetches the rows themselves through `/query`. `dataFormat=sqlite`, the positional
`serverGens` form and a `returnInserts`/`returnUpdates`/`returnDeletes` of
`false` are refused by name.

A change file reports attachment changes over that same window. An attachment
created inside it and still there when it closed is an add carrying `attachmentId`,
`globalId`, `parentGlobalId`, `contentType`, `name`, `size` and an absolute `url`
to fetch the bytes from; one already there when it opened that went inside it is an
attachment global id in `deleteIds`; one created and deleted inside it is in
neither, because the client never held it. `updates` is always empty, since
replacing an attachment here is a delete and an upload.

Every comparison is on the one clock, so the boundary is exact: a changeset's time
and an attachment's both come from the database, and an instant is always the same
generation, being truncated to the millisecond rather than rounded.

Deleting an attachment is a soft delete: the row keeps its bytes and gains a
`deleted_at`, which is what a change file diffs. Every read filters tombstones
out, on the Esri routes and on `/api/v1` alike, so a deleted attachment is gone
from every listing, download and metadata read and a second delete is refused as
not found.

A layer with a real `objectid` also publishes a virtual `globalid` field, declared
as `esriFieldTypeGlobalID` and named by `globalIdField`. Its value is the feature's
own uuid as a guid in braces and upper case, which is the shape Esri clients and
verne expect. `/query` serves it, `outFields` may name it, and `where` filters by
it: `globalid = '{...}'` and `globalid IN ('{...}', ...)` both work, with or
without the braces and in any case, which is how a consumer resolves the parent
feature of an attachment that did not itself change. It is not a property and
`applyEdits` never writes one: a client-supplied `globalid` attribute is dropped,
as a client-supplied object id on an add is. A row-number layer publishes no
`globalIdField`, for the same reason it takes no edits.

Layer metadata carries `drawingInfo` when the dataset has a symbology rule whose
symbol is tagged `{"format": "esri-drawing-info"}`, which is what verne writes
when it migrates a hosted feature layer. The stored document is served back
verbatim, so an Esri client draws migrated data the way the original service did.
A dataset with no such rule has no `drawingInfo` key at all.
`GET /api/v1/datasets/{id}/style` translates that same document into Mapbox GL
layers for non-Esri clients, with `source` and `sourceLayer` overridable by query
parameter and everything the translation could not carry over listed under
`losses`. `images` carries the bitmaps a picture marker or fill inlines, keyed by
the name the layers reference them under and each holding a `data_uri`, a `width`
and a `height` in CSS pixels: the consumer registers them before the layers draw.
The key is always present, empty for a style with no pictures in it.

Not served: datasets whose `geometry_type` is `geometry` or
`geometry_collection`, which have no single Esri layer type.

## Access control

Two layers. The token's `role` claim decides what kind of request you may make
at all: `viewer` reads, `editor` writes, `admin` also reaches config, ACL,
membership, audit and `/metrics`. Per-dataset grants then decide *which* data
you may touch. Both are off entirely with `PTOLEMY_AUTH_DISABLED=true`, which is
why that mode is for development only.

Grants are rows in `dataset_permissions` and `branch_permissions`, one per user
per scope, with permission `read`, `write` or `admin` (admin > write > read).

### Who manages grants

The `/permissions` endpoints need a valid token but not the `admin` role, because
delegation is per dataset. A caller gets in if it holds the instance `admin` role,
or an `admin` grant on the dataset in question — which also covers grants on that
dataset's branches. Anything else is `403` (or `404` if the dataset is private and
the caller has no grant on it, so ids are not confirmed).

A branch-level `admin` grant does **not** carry delegation: it would let a branch
grantee widen their own scope.

A dataset with no rows has no dataset admin, so only an instance admin can make
the first grant. Normally the creator auto-grant supplies one.

Revoking the dataset's last `admin` row is refused, for everyone including
instance admins, because it would leave nobody able to manage its grants. Grant
a replacement first, then revoke. Stepping down as owner is grant-then-revoke,
in that order.

Revoking the last row of any other kind is allowed: it leaves the dataset with
no rows, which denies every write rather than opening one. Branch rows have no
rule of their own either, removing them all falls back to the dataset scope.

### Writes

On commit, batch commit, merge (plain, topology-aware, review and
conflict-resolving), GeoJSON/CSV import, QGIS push, WFS transaction, sync push,
branch creation, repair and compaction:

1. An `admin` role token bypasses per-dataset grants.
2. Otherwise, if the target **branch** has any permission rows, the caller needs
   `write` or `admin` **on that branch**. A dataset-level grant does not reach
   into a branch that has its own rows.
3. Otherwise the caller needs `write` or `admin` on the **dataset**.

Denial is `403`. A write needs a grant: a dataset with no rows anywhere denies
everyone except an `admin` role token, which is who makes its first grant.
Creating a dataset with auth on inserts an `admin` row for the creator, so a new
dataset is owned from the moment it exists.

Datasets created before that auto-grant, or with auth off, have no rows. The
`027` migration gives each of them an admin grant for its `created_by`. It skips
a `created_by` that is blank or a machine label (`unknown`, `system`, `cli`, a
connector name), because those are not identities anyone holds a token for:
those datasets are writable by instance admins only until one of them grants.

Only an explicit grant lets you write, and the `/permissions/{user}/check`
endpoints answer with the same ladder: a `write` or `admin` check on a branch
that has rows of its own ignores dataset grants, exactly as the write does. A
`read` check is the visibility question instead, which a grant on the dataset
answers whatever the branch holds. `required` takes `read`, `write` or `admin`
and nothing else. The unused org layer (`organizations`, `org_members`,
`datasets.org_id`) was dropped in migration `028`.

### Reads: dataset visibility

Each dataset has `visibility`, `public` (the default) or `private`. Set it on
create, or later with `PATCH /api/v1/datasets/{id}` (instance admin, or an
`admin` grant on that dataset).

`public` keeps today's behavior: reads are anonymous, no token needed.

For `private`, every read that serves the dataset's content needs an instance
admin token or a caller holding *any* grant (`read`, `write` or `admin`) on the
dataset or on one of its branches. That covers feature listing and get, spatial
and CQL2 queries, OGC items, GeoJSON/CSV/FlatGeobuf export, MVT tiles, history,
diff, temporal queries, H3, similarity search, QGIS pull and layer definition,
geoprocessing and analytics reads, sync pull, and the vertical listings — the
check runs before the handler, keyed on every id the request names. External
datasets are covered the same way.

Unauthorized private reads answer `404`, not `403`, so a dataset id cannot be
confirmed by probing.

Enumeration is gated by the same rule, so a private dataset is simply absent
from `GET /api/v1/datasets`, `/api/v1/catalog/search`, `/api/v1/ogc/collections`,
`/api/v1/stac/collections` and `/api/v1/qgis/datasets`
for a caller with no grant. The filter is a SQL predicate applied inside each
query, so a paged search's `limit` counts only rows the caller may see.

Raster tiles are not covered: `GET /api/v1/stac/search` returns tile ids and
bounds from `raster_tiles` without naming a dataset, and raster catalogs have no
visibility of their own yet.

## API Endpoints

### Real-Time Collaboration Relay

Ptolemy includes an ephemeral room-based WebSocket relay at `/ws/rooms/{room_id}` for
real-time viewer collaboration.  Every JSON message sent by one participant is broadcast
to all other participants in the same room.  No messages are persisted — rooms are created
on first connection and dropped when the last client disconnects.

**Intended use cases:**

| Feature | Description |
|---------|-------------|
| **View sync** | Broadcast camera state (lat, lng, zoom, bearing, pitch) so a follower's viewer mirrors the leader's view |
| **Cursor sharing** | Share mouse position on the map between collaborators |
| **Presence** | Track which users are online in a room |
| **Chat** | Real-time text messaging within a room |

**Protocol (JSON over WebSocket):**

```jsonc
// Client → Server (broadcast to all other clients in the room)
{ "type": "Join", "user_id": "u1", "user_name": "Alice", "asset_id": "my-room" }
{ "type": "Camera", "user_id": "u1", "latitude": 40.7, "longitude": -73.9, "zoom": 14, "bearing": 0, "pitch": 45 }
{ "type": "Cursor", "user_id": "u1", "latitude": 40.71, "longitude": -73.91 }
{ "type": "Chat", "user_id": "u1", "user_name": "Alice", "message": "Look at this area" }
{ "type": "Leave", "user_id": "u1", "asset_id": "my-room" }
```

Messages are opaque to the server — it simply relays any valid text frame to all other
subscribers.  The message schema above is a convention used by ViewTopia's collaboration
client but any JSON structure will work.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/health` | Health check |
| GET | `/api/v1/datasets` | List datasets |
| POST | `/api/v1/datasets` | Create dataset |
| GET | `/api/v1/datasets/{id}` | Get dataset |
| PATCH | `/api/v1/datasets/{id}` | Set dataset visibility (dataset admin) |
| GET | `/api/v1/datasets/{id}/branches` | List branches |
| POST | `/api/v1/datasets/{id}/branches` | Create branch |
| GET | `/api/v1/branches/{id}` | Get branch |
| GET | `/api/v1/branches/{id}/history` | Commit log |
| GET | `/api/v1/branches/{id}/features` | List features (paginated) |
| GET | `/api/v1/branches/{id}/features/{feature_id}/native` | Pre-reprojection original geometry, exact |
| GET | `/api/v1/branches/{id}/features/bbox` | Spatial bbox filter |
| POST | `/api/v1/branches/{id}/features/intersects` | Spatial intersects filter |
| POST | `/api/v1/branches/{id}/features/within` | Spatial within filter |
| GET | `/api/v1/branches/{id}/features/count` | Feature count |
| GET | `/api/v1/branches/{id}/tiles/{z}/{x}/{y}.mvt` | MVT vector tiles |
| POST | `/api/v1/branches/{id}/commit` | Commit changes |
| POST | `/api/v1/branches/{id}/batch` | Batch commit (bulk ops) |
| POST | `/api/v1/branches/{target}/merge/{source}` | Merge branches |
| GET | `/api/v1/diff/{from}/{to}` | Diff changesets |
| GET | `/api/v1/sync/pull` | Pull branch snapshot (full or incremental) |
| POST | `/api/v1/sync/push` | Push local edits to branch |
| GET | `/api/v1/sync/status` | Check if local is behind remote |
| GET | `/api/v1/reviews` | List merge requests |
| POST | `/api/v1/reviews` | Create merge request |
| GET | `/api/v1/reviews/{id}` | Get merge request |
| PUT | `/api/v1/reviews/{id}/approve` | Approve review |
| PUT | `/api/v1/reviews/{id}/close` | Close review |
| POST | `/api/v1/reviews/{id}/merge` | Merge via review |
| GET | `/api/v1/reviews/{id}/diff` | Review diff |
| GET | `/api/v1/reviews/{id}/comments` | List comments |
| POST | `/api/v1/reviews/{id}/comments` | Add comment |
| GET | `/metrics` | Prometheus metrics (admin token) |
| GET | `/auth/oidc/login` | OIDC SSO login |
| GET | `/auth/oidc/callback` | OIDC callback |
| GET | `/review` | Web review UI |
| GET | `/api/v1/datasets/{id}/schema` | Get dataset schema |
| PUT | `/api/v1/datasets/{id}/schema` | Set dataset schema |
| GET | `/api/v1/datasets/{id}/topology` | List topology rules |
| POST | `/api/v1/datasets/{id}/topology` | Add topology rule |
| DELETE | `/api/v1/topology/{id}` | Delete topology rule |
| GET | `/api/v1/branches/{id}/quality` | Data quality report |
| POST | `/api/v1/branches/{id}/repair` | Auto-repair invalid geometries |
| GET | `/api/v1/datasets/{id}/webhooks` | List webhooks |
| POST | `/api/v1/datasets/{id}/webhooks` | Create webhook |
| DELETE | `/api/v1/webhooks/{id}` | Delete webhook |
| GET | `/api/v1/datasets/{id}/events` | List CDC events |
| POST | `/api/v1/datasets/{id}/events` | Emit custom event |
| GET | `/api/v1/branches/{id}/analytics/buffer` | Buffer analysis |
| GET | `/api/v1/branches/{id}/analytics/union` | Union analysis |
| GET | `/api/v1/branches/{id}/analytics/clusters` | DBSCAN clustering |
| GET | `/api/v1/branches/{id}/analytics/anomalies` | Spatial anomaly detection |
| GET | `/api/v1/branches/{id}/analytics/stats` | Spatial statistics |
| GET | `/api/v1/ogc` | OGC landing page |
| GET | `/api/v1/ogc/conformance` | OGC conformance |
| GET | `/api/v1/ogc/collections` | OGC collections |
| GET | `/api/v1/ogc/collections/{id}/items` | OGC feature items |
| GET | `/api/v1/ogc/collections/{id}/items/{fid}` | OGC single feature |
| GET | `/arcgis/rest/services` | ArcGIS service catalog |
| GET | `/arcgis/rest/services/{service}/FeatureServer` | ArcGIS service root |
| GET | `/arcgis/rest/services/{service}/FeatureServer/0` | ArcGIS layer metadata |
| GET POST | `/arcgis/rest/services/{service}/FeatureServer/0/query` | ArcGIS feature query |
| POST | `/arcgis/rest/services/{service}/FeatureServer/0/applyEdits` | ArcGIS batch edits |
| GET POST | `/arcgis/rest/services/{service}/FeatureServer/0/queryAttachments` | ArcGIS attachment listing |
| GET | `/arcgis/rest/services/{service}/FeatureServer/0/{oid}/attachments` | ArcGIS feature attachments |
| POST | `.../0/{oid}/addAttachment`, `updateAttachment`, `deleteAttachments` | ArcGIS attachment edits |
| POST | `/arcgis/rest/services/{service}/FeatureServer/extractChanges` | ArcGIS change extraction |
| GET | `/arcgis/rest/services/{service}/FeatureServer/jobs/{jobId}` | ArcGIS extract job status |
| GET | `/arcgis/rest/services/{service}/FeatureServer/changefiles/{jobId}` | ArcGIS change file |
| GET | `/api/v1/audit` | Audit log |
| GET | `/api/v1/branches/{id}/locks` | List feature locks |
| POST | `/api/v1/branches/{id}/locks` | Lock a feature |
| DELETE | `/api/v1/branches/{bid}/locks/{fid}` | Unlock a feature |
| GET | `/api/v1/branches/{id}/features?at=` | Temporal query (features at time) |
| GET | `/api/v1/catalog/search` | Search datasets (text + tags) |
| GET | `/api/v1/datasets/{id}/tags` | List dataset tags |
| POST | `/api/v1/datasets/{id}/tags` | Add tag |
| DELETE | `/api/v1/datasets/{id}/tags/{tag}` | Remove tag |
| GET | `/api/v1/datasets/{id}/metadata` | Get dataset metadata |
| PUT | `/api/v1/datasets/{id}/metadata` | Set dataset metadata |
| GET | `/api/v1/datasets/{id}/permissions` | List dataset grants (dataset admin) |
| POST | `/api/v1/datasets/{id}/permissions` | Grant on a dataset (dataset admin) |
| DELETE | `/api/v1/datasets/{id}/permissions/{user}` | Revoke on a dataset (dataset admin) |
| GET | `/api/v1/branches/{id}/permissions` | List branch grants (dataset admin) |
| POST | `/api/v1/branches/{id}/permissions` | Grant on a branch (dataset admin) |
| DELETE | `/api/v1/branches/{id}/permissions/{user}` | Revoke on a branch (dataset admin) |
| GET | `/api/v1/conflicts/{id}` | List merge conflicts |
| GET | `/api/v1/branches/{target}/merge/{source}/preview` | Merge preview with conflict GeoJSON |
| POST | `/api/v1/branches/{target}/merge/{source}/resolve` | Resolve conflicts and create the merge commit |
| GET | `/api/v1/events/stream` | SSE real-time event stream |
| WS | `/ws/branches/{id}` | Real-time branch events |
| WS | `/ws/rooms/{room_id}` | Ephemeral collaboration relay (presence, view sync, chat) |
| **Networks** | | |
| GET | `/api/v1/datasets/{id}/networks` | List geometric networks |
| POST | `/api/v1/datasets/{id}/networks` | Create network |
| GET | `/api/v1/networks/{id}` | Get network |
| GET | `/api/v1/networks/{id}/junctions` | List junctions |
| POST | `/api/v1/networks/{id}/junctions` | Add junction |
| GET | `/api/v1/networks/{id}/edges` | List edges |
| POST | `/api/v1/networks/{id}/edges` | Add edge |
| POST | `/api/v1/networks/{id}/trace` | Network trace (upstream/downstream) |
| POST | `/api/v1/networks/{id}/shortest-path` | Dijkstra shortest path |
| GET | `/api/v1/networks/{id}/connectivity` | Connectivity report |
| **Linear Referencing** | | |
| GET | `/api/v1/datasets/{id}/routes` | List LRS routes |
| POST | `/api/v1/datasets/{id}/routes` | Create route |
| GET | `/api/v1/routes/{id}` | Get route |
| GET | `/api/v1/routes/{id}/events` | List route events |
| POST | `/api/v1/routes/{id}/events` | Create event (point/linear) |
| GET | `/api/v1/routes/{id}/locate?lng=&lat=` | Locate point on route (measure) |
| GET | `/api/v1/routes/{id}/subline?from_measure=&to_measure=` | Extract sub-line |
| **Raster/Imagery** | | |
| GET | `/api/v1/datasets/{id}/rasters` | List raster catalogs |
| POST | `/api/v1/datasets/{id}/rasters` | Create raster catalog |
| GET | `/api/v1/rasters/{id}` | Get raster catalog |
| GET | `/api/v1/rasters/{id}/tiles` | List tiles |
| POST | `/api/v1/rasters/{id}/tiles` | Upload tile |
| GET | `/api/v1/rasters/{id}/value?lng=&lat=` | Pixel value at point |
| GET | `/api/v1/rasters/{id}/stats` | Band statistics |
| **Domains & Rules** | | |
| GET | `/api/v1/datasets/{id}/domains` | List domains |
| POST | `/api/v1/datasets/{id}/domains` | Create domain (coded value / range) |
| GET | `/api/v1/domains/{id}` | Get domain |
| DELETE | `/api/v1/domains/{id}` | Delete domain |
| GET | `/api/v1/datasets/{id}/subtypes` | List subtypes |
| POST | `/api/v1/datasets/{id}/subtypes` | Create subtype |
| GET | `/api/v1/subtypes/{id}` | Get subtype |
| DELETE | `/api/v1/subtypes/{id}` | Delete subtype |
| GET | `/api/v1/datasets/{id}/attribute-rules` | List attribute rules |
| POST | `/api/v1/datasets/{id}/attribute-rules` | Create attribute rule |
| GET | `/api/v1/attribute-rules/{id}` | Get rule |
| PUT | `/api/v1/attribute-rules/{id}` | Update rule |
| DELETE | `/api/v1/attribute-rules/{id}` | Delete rule |
| POST | `/api/v1/attribute-rules/{id}/validate` | Validate rule expression |
| **Relationships** | | |
| GET | `/api/v1/datasets/{id}/relationships` | List relationship classes |
| POST | `/api/v1/datasets/{id}/relationships` | Create relationship class |
| GET | `/api/v1/relationship-classes/{id}` | Get relationship class |
| DELETE | `/api/v1/relationship-classes/{id}` | Delete relationship class |
| GET | `/api/v1/relationship-classes/{id}/records` | List records |
| POST | `/api/v1/relationship-classes/{id}/records` | Create record |
| DELETE | `/api/v1/relationship-records/{id}` | Delete record |
| GET | `/api/v1/features/{id}/related` | Navigate relationships |
| **Cartography** | | |
| GET | `/api/v1/datasets/{id}/symbology` | List symbology rules |
| POST | `/api/v1/datasets/{id}/symbology` | Create symbology rule |
| GET | `/api/v1/datasets/{id}/style` | Stored Esri style as Mapbox GL layers |
| GET | `/api/v1/symbology/{id}` | Get symbology rule |
| PUT | `/api/v1/symbology/{id}` | Update symbology |
| DELETE | `/api/v1/symbology/{id}` | Delete symbology |
| GET | `/api/v1/datasets/{id}/labels` | List label rules |
| POST | `/api/v1/datasets/{id}/labels` | Create label rule |
| GET | `/api/v1/labels/{id}` | Get label rule |
| PUT | `/api/v1/labels/{id}` | Update label |
| DELETE | `/api/v1/labels/{id}` | Delete label |
| **PostGIS Topology** | | |
| GET | `/api/v1/datasets/{id}/topologies` | List topologies |
| POST | `/api/v1/datasets/{id}/topologies` | Create topology |
| POST | `/api/v1/topologies/{name}/validate` | Validate topology |
| GET | `/api/v1/topologies/{name}/faces` | List faces |
| GET | `/api/v1/topologies/{name}/edges` | List edges |
| GET | `/api/v1/topologies/{name}/nodes` | List nodes |
| POST | `/api/v1/topologies/{name}/add-face` | Add face |
| POST | `/api/v1/topologies/{name}/simplify` | Simplify topology |
| **SFCGAL 3D** | | |
| POST | `/api/v1/branches/{id}/3d/extrude` | Extrude 2D → 3D |
| POST | `/api/v1/branches/{id}/3d/volume` | Compute volume |
| POST | `/api/v1/branches/{id}/3d/intersection` | 3D intersection |
| POST | `/api/v1/branches/{id}/3d/straight-skeleton` | Straight skeleton |
| POST | `/api/v1/branches/{id}/3d/minkowski-sum` | Minkowski sum |
| POST | `/api/v1/branches/{id}/3d/tesselate` | Tesselation |
| POST | `/api/v1/branches/{id}/3d/visibility` | Visibility/line-of-sight |
| **H3 Indexing** | | |
| POST | `/api/v1/branches/{id}/h3/index` | Index features with H3 |
| GET | `/api/v1/branches/{id}/h3/hexagons` | Get covering hexagons |
| GET | `/api/v1/branches/{id}/h3/aggregate` | Aggregate by hex cell |
| GET | `/api/v1/branches/{id}/h3/neighbors` | K-ring neighbors |
| POST | `/api/v1/branches/{id}/h3/compact` | Compact hex set |
| GET | `/api/v1/h3/cell?lng=&lat=` | Point → H3 cell |
| GET | `/api/v1/h3/boundary?cell=` | Cell → boundary polygon |
| **Vector Similarity** | | |
| POST | `/api/v1/branches/{id}/similarity/search` | Similarity search, needs pgvector |
| GET | `/api/v1/branches/{id}/similarity/duplicates` | Find duplicates, needs pgvector |
| POST | `/api/v1/branches/{id}/similarity/embed` | Generate embeddings, needs pgvector |
| POST | `/api/v1/branches/{id}/similarity/cluster` | K-means clustering, needs pgvector |
| **Point Cloud** | | |
| GET | `/api/v1/datasets/{id}/pointclouds` | List point cloud catalogs |
| POST | `/api/v1/datasets/{id}/pointclouds` | Create catalog |
| GET | `/api/v1/pointclouds/{id}` | Get catalog |
| GET | `/api/v1/pointclouds/{id}/patches` | List patches |
| POST | `/api/v1/pointclouds/{id}/patches` | Add patch |
| POST | `/api/v1/pointclouds/{id}/query` | Spatial query |
| GET | `/api/v1/pointclouds/{id}/stats` | Catalog stats |
| POST | `/api/v1/pointclouds/{id}/profile` | Elevation profile |
| **Trajectories** | | |
| GET | `/api/v1/datasets/{id}/trajectories` | List trajectories |
| POST | `/api/v1/datasets/{id}/trajectories` | Create trajectory |
| GET | `/api/v1/trajectories/{id}` | Get trajectory |
| GET | `/api/v1/trajectories/{id}/at?timestamp=` | Position at time, needs MobilityDB |
| GET | `/api/v1/trajectories/{id}/speed` | Speed analysis, needs MobilityDB |
| GET | `/api/v1/trajectories/{id}/distance` | Distance/duration, needs MobilityDB |
| POST | `/api/v1/trajectories/{id}/simplify` | Simplify trajectory, needs MobilityDB |
| POST | `/api/v1/datasets/{id}/trajectories/nearest` | Nearest approach, needs MobilityDB |
| **CQL2 + OGC Tiles** | | |
| POST | `/api/v1/branches/{id}/features/filter` | CQL2-JSON filter query, `limit` max 10000 |
| GET | `/api/v1/tiles/tileMatrixSets` | List tile matrix sets |
| GET | `/api/v1/tiles/tileMatrixSets/{tms}` | Get tile matrix set |
| GET | `/api/v1/datasets/{id}/tiles/{tms}/{z}/{x}/{y}` | OGC vector tile |
| **STAC** | | |
| GET | `/api/v1/stac` | STAC root catalog |
| GET | `/api/v1/stac/collections` | STAC collections |
| GET | `/api/v1/stac/collections/{id}` | STAC collection |
| GET | `/api/v1/stac/collections/{id}/items` | STAC items |
| GET | `/api/v1/stac/collections/{id}/items/{item_id}` | STAC item |
| GET | `/api/v1/stac/search` | STAC search |
| **Format & CRS** | | |
| GET | `/api/v1/branches/{id}/export/geojson` | Export GeoJSON |
| GET | `/api/v1/branches/{id}/export/csv` | Export CSV |
| GET | `/api/v1/branches/{id}/export/flatgeobuf` | Export FlatGeobuf |
| POST | `/api/v1/branches/{id}/transform` | Transform single geometry CRS |
| POST | `/api/v1/branches/{id}/import/geojson` | Import a FeatureCollection |
| POST | `/api/v1/branches/{id}/import/csv` | Import point rows from CSV |
| GET | `/api/v1/crs/search?q=` | Search coordinate systems |
| GET | `/api/v1/crs/{srid}` | Get CRS details |

Both imports answer `{imported, skipped, changeset_id, errors}`. Rows that
cannot be parsed are skipped and named in `errors`; the rest land as one
changeset on the branch, visible to reads like any other commit. A request whose
rows all fail answers 422 and writes no changeset.

## CLI Commands

### Data Import

Import geospatial data from multiple formats (auto-detected by file extension):

```bash
# Import GeoJSON
ptolemy import --dataset <id> --branch main --file data.geojson

# Import Shapefile (reads .shp + .dbf)
ptolemy import --dataset <id> --branch main --file parcels.shp

# Import GeoPackage
ptolemy import --dataset <id> --branch main --file terrain.gpkg
```

### API Keys

Manage programmatic access keys (SHA-256 hashed, never stored in plaintext):

```bash
# Create a new key (shown only once!)
ptolemy apikey create --name "CI Pipeline" --role editor

# List active keys (shows prefix only)
ptolemy apikey list

# Revoke by prefix or full key
ptolemy apikey revoke ptk_abc123
```

### Backup & Restore

Database backup/restore using PostgreSQL native tools:

```bash
# Backup to a compressed dump
ptolemy backup --output ptolemy_backup.dump

# Restore from dump
ptolemy restore --input ptolemy_backup.dump
```

## Building

```bash
cargo build --release
```

## Tests

The suite needs a migrated PostGIS database:

```bash
DATABASE_URL=postgres://postgres:postgres@localhost/ptolemy_test cargo test --all -- --test-threads=1
```

`crates/ptolemy-api/tests/route_sweep.rs` is one test that calls every route
mounted on the router, reading the route list off the router itself so a new
route is covered without being added anywhere. It fails on SQLSTATE 42703
(undefined column) and 42P01 (undefined table), which is what a handler naming a
column the migrations do not create looks like, and every query here is a runtime
`sqlx::query` that nothing else checks against the schema. It prints what it
covered and every 500 it saw. Routes it deliberately skips are listed in the
test with a reason each.

## Project Structure

```
crates/
├── ptolemy-core/          # Domain types, merge logic, diff algorithms
├── ptolemy-storage/       # PostGIS storage backend
├── ptolemy-geopackage/    # GeoPackage import/export for offline editing
├── ptolemy-mongodb/       # MongoDB storage backend
├── ptolemy-elasticsearch/ # Elasticsearch indexing backend
├── ptolemy-api/           # Axum REST API server
└── ptolemy-cli/           # CLI binary (server + admin commands)
```

## License

AGPL-3.0-or-later, see [LICENSE](LICENSE).

Copyright (C) 2026 Grok Image Compression Inc.

## Prior Art & Differentiation

| Project | Status | Limitation |
|---------|--------|-----------|
| [GeoGig](https://geogig.org/) | Abandoned | Java, heavy, poor DX |
| [Kart](https://kartproject.org/) | Active | GeoPackage-only, no multi-user server |
| [pg_version](https://github.com/CartoDB/cartodb-postgresql) | Limited | Single-table temporal, no branching |

Ptolemy aims to be: **fast (Rust), server-native (PostGIS), with git-quality branching/merging UX**.
