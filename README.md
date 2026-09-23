# Ptolemy

[![CI](https://github.com/GeoLang/ptolemy/actions/workflows/ci.yml/badge.svg)](https://github.com/GeoLang/ptolemy/actions)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)

Ptolemy is a versioned geodatabase on PostgreSQL and PostGIS. You branch,
commit, diff and merge geographic datasets the way you would a git repository,
and read them over a REST API, OGC API - Features and an ArcGIS FeatureServer
facade.

## Quick Start

Needs a PostgreSQL server with PostGIS.

```bash
createdb ptolemy
psql ptolemy -c "CREATE EXTENSION postgis"
cargo build --release
export DATABASE_URL=postgres://localhost/ptolemy
export PLATFORM_JWT_SECRET=$(openssl rand -hex 32)
./target/release/ptolemy serve
```

`serve` applies migrations before it binds `0.0.0.0:3000` (`--bind` changes
it). The API is at `http://localhost:3000/api/v1`. Reads are anonymous and
writes need a bearer token. `ptolemy api-key create dev --role admin` prints one.

`docker compose up --build` runs the same thing with PostGIS on 5432 and
Prometheus on 9090, using a fixed development secret. The Prometheus scrape gets
401 until you give it an admin token, see `deploy/prometheus.yml`.
`docker compose -f docker-compose.neon.yml --env-file .env.neon up --build`
runs it against Neon instead, with `.env.neon` copied from `.env.neon.example`.

Tagged releases attach prebuilt `ptolemy` binaries for x86_64 and aarch64 Linux
and macOS.

## Container image

Every push to `master` publishes `ghcr.io/geolang/ptolemy`, tagged `master` and
`sha-<short-sha>`. A `v*` tag publishes that version plus `latest`. Pin to a
`sha-` tag if you need a fixed API surface.

```bash
docker run -p 3000:3000 \
  -e DATABASE_URL=postgres://ptolemy:ptolemy@db/ptolemy \
  -e PLATFORM_JWT_SECRET=$(openssl rand -hex 32) \
  ghcr.io/geolang/ptolemy:master
```

It refuses to start without `DATABASE_URL`, or without `PLATFORM_JWT_SECRET`
unless `PTOLEMY_AUTH_DISABLED=true`. Wait on `/api/v1/readyz`, which answers
once the database does, rather than `/api/v1/healthz`, which answers as soon as
the process is up.

Pair it with `postgis/postgis:16-3.4`. The first migration declares a PostGIS
`geometry` column, so a database without PostGIS fails to migrate. A database
created by v0.1.0 upgrades in place.

## Helm chart

The chart deploys Ptolemy against a PostGIS database you already run. It brings
no database. It reads the database URL and the JWT secret from two Secrets you
create first, and `helm install` fails if either is not named.

```bash
kubectl create secret generic ptolemy-database \
  --from-literal=url='postgres://user:pass@host/ptolemy?sslmode=verify-full'

kubectl create secret generic ptolemy-auth \
  --from-literal=jwt-secret="$(openssl rand -hex 32)"

helm install ptolemy deploy/helm/ptolemy \
  --set externalDatabase.existingSecret=ptolemy-database \
  --set auth.existingSecret=ptolemy-auth
```

The database URL carries the `sslmode` and `sslrootcert` a managed database
needs, see [Database TLS](#database-tls). The JWT secret must be the same
`PLATFORM_JWT_SECRET` the other platform services use. The keys default to
`url` and `jwt-secret`, and `externalDatabase.existingSecretKey` and
`auth.existingSecretKey` change them.

The image is `ghcr.io/geolang/ptolemy` at `v<appVersion>` from `Chart.yaml`.
Set `image.tag` to run another published tag.

## Database TLS

The `sslmode` parameter on `DATABASE_URL` decides the protection, and the
default is `prefer`: TLS if the server offers it, plaintext if not.

For anything not on localhost use `sslmode=verify-full`. `sslmode=require`,
which most hosting providers tell you to paste, encrypts but accepts any
certificate, even with `sslrootcert` set. Only `verify-ca` and `verify-full`
check the certificate, and only `verify-full` checks the hostname.

The Mozilla root bundle is compiled in, so a provider whose certificate chains
to a public CA, such as Neon, needs only `verify-full`. Amazon RDS signs with
its own roots, so name the bundle:

```
DATABASE_URL=postgres://user:pass@host/ptolemy?sslmode=verify-full&sslrootcert=/etc/ssl/rds-global-bundle.pem
```

The container image ships that bundle at `/etc/ssl/rds-global-bundle.pem`,
downloaded from `https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem`
at build time. Outside the image, download it yourself. `sslrootcert` adds to
the compiled-in roots rather than replacing them. `PTOLEMY_EXTERNAL_DATABASE_URL`
needs its own copy of these parameters.

## Configuration

| Variable | Description | Default |
|----------|-------------|---------|
| `DATABASE_URL` | PostgreSQL connection URL | (required) |
| `PLATFORM_JWT_SECRET` | HS256 signing secret, 32 bytes or more, shared with the other GeoLang services | (required to serve) |
| `PTOLEMY_AUTH_DISABLED` | `true` serves with auth off, for development only | `false` |
| `PTOLEMY_OIDC_ISSUER_URL` | Keycloak realm URL. The authorize, token and userinfo URLs are built as `{issuer}/protocol/openid-connect/...` with no discovery document, so other providers do not work | (OIDC off) |
| `PTOLEMY_OIDC_CLIENT_ID` | OAuth2 client ID | |
| `PTOLEMY_OIDC_CLIENT_SECRET` | OAuth2 client secret | |
| `PTOLEMY_OIDC_REDIRECT_URL` | Callback URL for the OIDC flow | |
| `SMTP_URL` | SMTP relay for invitation email, e.g. `smtp://user:pass@mail.example.com:587?tls=required` | (no email) |
| `SMTP_FROM` | Sender address on invitation email | (no email) |
| `PUBLIC_BASE_URL` | Where the viewer is served, used to build the invitation link | (no email) |
| `PTOLEMY_EXTERNAL_DATABASE_URL` | Database holding external datasets. Use a read-only role | (primary pool) |
| `PTOLEMY_DB_MAX_CONNECTIONS` | Max DB pool connections | 10 |
| `PTOLEMY_DB_MIN_CONNECTIONS` | Min DB pool connections | 2 |
| `PTOLEMY_ANALYZE_ROW_THRESHOLD` | Rows in one write that trigger an `ANALYZE`. `0` leaves it to autoanalyze | 1000 |
| `PTOLEMY_EVENTS_RETENTION_DAYS` | Days a settled webhook delivery and its event are kept. `0` keeps them forever | 30 |
| `RUST_LOG` | Log filter | (the image sets `info,ptolemy=debug`) |

The OIDC callback answers `{access_token, user}`, where `access_token` is a
Ptolemy JWT with role `editor` for every user the provider signs in.

A write that touches at least `PTOLEMY_ANALYZE_ROW_THRESHOLD` rows runs
`ANALYZE` on `feature_versions`, `changesets` and `branches` after it commits,
off the request path. Without it, reads straight after a bulk import use a plan
made for empty tables until autoanalyze runs. If the role cannot `ANALYZE`
because it does not own the tables, the failure is logged and the write is
unaffected.

## Data model

- **Dataset**: spatial features sharing a schema, like an Esri feature class.
- **Branch**: a named pointer to a changeset. The default branch is `main`.
- **Changeset**: one commit of inserts, updates and deletes. It points to its
  parent, and a merge changeset also to the source head it brought in.
- **Feature**: a UUID, a WKB geometry in EPSG:4326 and JSON properties. A
  version may also carry its pre-reprojection geometry (`/native`) and
  `valid_from` and `valid_to` times, which `GET .../features?valid_at=` filters on.

### Merge

Three-way merge from the common ancestor:

1. Changes to different features merge automatically.
2. One feature edited on both sides merges only when the two sides wrote
   different property keys and neither touched the geometry or the validity
   times.
3. Anything else on one feature is a conflict: the same property key on both
   sides, or a geometry change on either side.
4. Geometries are compared as raw WKB bytes, so a change in vertex order or
   precision counts. There is no tolerance.

Conflicts can be resolved through `/api/v1/branches/{target}/merge/{source}/resolve`,
per feature `ours`, `theirs`, `custom`, `delete` or `auto_merge`.

### Your data is PostGIS

Every feature version is a row in `feature_versions` with a GiST-indexed
`geometry` column and a JSONB `properties` column. The `features` view resolves
each branch to its current features, so psql, `ogr2ogr` and QGIS's PostGIS
connector read it with no Ptolemy service running. Backup is `pg_dump`.

```sql
SELECT id, geometry, properties
FROM features
WHERE branch_id = '...'
  AND ST_DWithin(geometry, ST_Point(7.42, 43.73)::geography, 500)
```

The view walks every branch's changeset chain before your `WHERE` filters it,
so its cost grows with the whole instance's history. The API does not use it.
It reads one branch's ancestor chain per query.

### External datasets: your existing PostGIS, read-only

Register a table or view you already have and read it through the API and the
viewer without copying it:

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

Registration checks the relation exists, that the column is PostGIS geometry
and that Ptolemy can select from it, then creates the dataset and its `main`
branch. Feature listing and paging, bbox, CQL2, OGC items, GeoJSON and CSV
export and vector tiles all work. Commits, merges, imports, QGIS push, branch
creation and project attach answer 409.

Non-geometry columns become `properties`. Each row's id is hashed into a stable
UUID and the original key stays in `properties`. Geometry is served in
EPSG:4326.

Every non-geometry column is published, and a `public` dataset is readable
anonymously. Register a view that selects only the columns you want public, or
register with `"visibility": "private"`.

An ordinary GiST index on the geometry column is enough in any SRID. Spatial
reads (bbox, intersects, within, vector tiles, OGC items and CQL2 spatial
filters) add a predicate on the relation's own column in its own SRID, using a
slightly widened reprojected window, so the index serves it. That predicate is
skipped for a window wider than 45 degrees or past 85 degrees latitude, and for
a CQL2 spatial op under `or` or `not`.

`PTOLEMY_EXTERNAL_DATABASE_URL` reads external datasets from another database.
Give it a role with `SELECT` and nothing else, so PostgreSQL enforces the
read-only part:

```bash
psql yourdb -c "CREATE ROLE ptolemy_ro LOGIN PASSWORD '...'"
psql yourdb -c "GRANT CONNECT ON DATABASE yourdb TO ptolemy_ro"
psql yourdb -c "GRANT USAGE ON SCHEMA public TO ptolemy_ro"
psql yourdb -c "GRANT SELECT ON public.parcels TO ptolemy_ro"
```

## Access control

With auth on, a request needs:

- no token for `GET`, `HEAD` and `OPTIONS`, and for the query POSTs:
  `features/intersects`, `features/within`, `features/filter`, the point cloud
  `query` and `profile`, and the FeatureServer `query`, `queryAttachments` and
  `extractChanges`
- any valid token for `/ws/`, `/permissions`, `/api/v1/workspaces`,
  `/api/v1/projects` and `/api/v1/invitations/accept`
- role `admin` for webhooks, audit, `/metrics`, the replication feed and peers,
  a dataset's event history, and every PostGIS Topology route except `validate`
- role `editor` or `admin` for everything else

Per-dataset grants and dataset visibility then decide which data a request may
touch. `PTOLEMY_AUTH_DISABLED=true` turns off both layers.

With auth on, attribution fields (`author`, `created_by`, `granted_by`) come
from the token subject and the request body value is ignored.

### Tool tokens and API keys

A token with `token_use: "tool"` carries a `scope` array and no `role`.
`ptolemy:read` passes a read and `ptolemy:write` a write. It is refused on every
admin-role route, on `/api/v1/workspaces`, `/api/v1/projects`,
`/api/v1/invitations/accept` and `/api/v1/datasets/{id}/project`, so a delegated
agent can use its subject's grants but cannot hand a project a grant. A token
with both `role` and `token_use`, or an unknown `token_use`, is refused. Missing
scope is `403`.

An API key from `ptolemy api-key create` is sent as
`Authorization: Bearer ptk_...`. Its subject is `apikey:<row id>` and it runs
with the role stored on the row.

### Grants

Grants are rows in `dataset_permissions` and `branch_permissions`, one per user
per scope, with permission `read`, `write` or `admin`. A dataset attached to a
project also grants by project role: `viewer` reads, `editor` writes, `owner`
administers. The stronger of the explicit grant and the project role applies.

The `/permissions` routes admit an instance `admin`, an `admin` grant on the
dataset, or the `owner` of the dataset's project, and that covers the dataset's
branches too. Anyone else gets `403`, or `404` for a private dataset they cannot
read. A branch-level `admin` grant does not let its holder manage grants.

Revoking a dataset's last `admin` row is refused for everyone, including
instance admins and a project owner, so grant a replacement first. Revoking the
last row of any other kind is allowed and leaves the dataset denying writes.

`/permissions/{user}/check?required=read|write|admin` answers with the same
rules the write and read checks use.

### Writes

Commit, batch commit, every merge, imports, QGIS push, WFS transaction, sync
push, branch creation, repair and compaction check:

1. An `admin` role token passes.
2. If the target branch has permission rows, the caller needs `write` or
   `admin` on that branch. A dataset grant does not reach into it.
3. Otherwise the caller needs `write` or `admin` on the dataset, as a row or
   through its project.

Denial is `403`. Creating a dataset with auth on grants its creator `admin`. A
dataset with no rows and no project is writable by instance admins only.
Compute-only POSTs, such as geoprocessing, 3D, network analysis and similarity
search, need the `editor` role and no grant.

### Dataset visibility

Each dataset is `public` (the default) or `private`. Set it on create or with
`PATCH /api/v1/datasets/{id}` as an instance admin, a dataset admin or the
project owner.

A `private` dataset's content and everything derived from it (features,
queries, OGC items, exports, tiles, history, diff, H3, similarity, QGIS,
geoprocessing, analytics, sync pull and the vertical listings) needs an
instance admin token, any grant on the dataset or one of its branches, or any
role on its project. Otherwise it answers `404`, so ids cannot be probed.
External datasets are covered the same way. Listings leave it out: `/api/v1/datasets`,
`/api/v1/catalog/search`, `/api/v1/ogc/collections`, `/api/v1/stac/collections`
and `/api/v1/qgis/datasets`.

Raster tiles are not covered. `GET /api/v1/stac/search` returns tile ids and
bounds from `raster_tiles` without naming a dataset.

## Workspaces and projects

`/api/v1/workspaces` holds workspaces, `/api/v1/workspaces/{id}/projects` their
projects, and `/api/v1/projects` lists projects across the caller's
workspaces. Roles are `owner`, `editor` and `viewer`. Workspace membership is
inherited by its projects, a direct project membership grants that project
only, and the higher role wins. Owners manage members and invitations and
delete. Editors update metadata and create projects. Viewers read.

Invitations are created at `/api/v1/workspaces/{id}/invitations` or
`/api/v1/projects/{id}/invitations`, grant `editor` or `viewer`, expire, and are
accepted by an authenticated caller at `POST /api/v1/invitations/accept`. The
token is returned once at creation and stored as a SHA-256 hash.

With `SMTP_URL`, `SMTP_FROM` and `PUBLIC_BASE_URL` set, the create body takes an
optional `email` and the reply carries `email: {"status": "sent"}` or
`{"status": "failed", "error": ...}` beside the token. Sending happens on the
request. With any of the three unset, `email` is a 400 and
`GET /api/v1/capabilities` answers `{"email_configured": false}`.

Project roles do not reach Agora documents.

### Project state and attachments

`GET` and `PUT /api/v1/projects/{id}/state/{key}` store one JSON value per key,
up to 5 MB, last write wins. A read answers `{value, updated_at, updated_by}`.
ViewTopia keeps its map under `map` and its dashboards under `dashboards`.

`/api/v1/projects/{id}/attachments` holds the files that state refers to, up to
32 MiB per upload. Viewers read, editors write. `/api/v1/attachments/{id}`
refuses a project attachment.

### Datasets in projects

`PUT /api/v1/datasets/{id}/project` with `{"project_id": "..."}` attaches a
dataset to one project, and `DELETE` detaches it. Attaching needs an `admin`
grant on the dataset and `editor` or `owner` on the destination project, and
asks nothing of the project it leaves. Detaching needs either one. A project the
caller is not in answers `404`. `expected_project_id` in the body answers `409`
if the dataset moved since the caller read it.

Attaching makes the dataset `private` unless it was already in that project.
Detaching leaves it `private`. Project roles join the dataset scope only, so a
branch with its own rows still decides its own writes. An external dataset
cannot be attached. `project_id` is on every dataset read and ignored on create.

## Dataset schema

`PUT /api/v1/datasets/{id}/schema` sets typed fields with `required`,
`allowed_values`, `min` and `max`. `POST /api/v1/branches/{id}/commit` and
ArcGIS `applyEdits` refuse properties that break it. Batch commit, imports, sync
push and the QGIS writes do not check it, and `geometry_rules` are stored and
never checked.

## Audit log

Every mutation that answered 2xx writes one row: token subject, method, matched
route, the dataset or branch the write check used, request path and time. Reads
are not recorded, and neither is the query string, which on the ArcGIS routes
carries a token. `GET /api/v1/audit?limit=&actor=` reads it, admin only. The row
is written after the response, so a failed insert is logged and the write
stands. `ip_address` is always empty.

## Webhooks

`POST /api/v1/datasets/{id}/webhooks` subscribes a URL to a dataset's events,
admin only. `events` selects types and an empty array takes all:

| Event | Raised by |
|-------|-----------|
| `commit` | a commit on any branch of the dataset |
| `merge` | the merge commit a three-way merge lands |
| `branch_created` | a new branch |
| `schema_changed` | `PUT /api/v1/datasets/{id}/schema` |

`POST /api/v1/datasets/{id}/events` emits a custom type and refuses those four
with 400.

The event and one delivery per matching subscription are written in the
transaction of the change that raised them. A worker started by `serve` posts
each with `X-Ptolemy-Event`, `X-Ptolemy-Delivery` and, when the subscription has
a `secret`, `X-Ptolemy-Signature: sha256=<hmac>` over the exact body. It retries
with a doubling backoff, five attempts in all, and then leaves the row with its
last error. No endpoint returns the secret.

Once an hour the worker deletes deliveries settled more than
`PTOLEMY_EVENTS_RETENTION_DAYS` ago, then events no pending delivery needs, in
bounded batches.

## ArcGIS FeatureServer

Point an Esri client at `/arcgis/rest/services`. Each dataset is a single-layer
service named by its name or uuid, the layer id is always 0, and every request
runs on `main`. Visibility applies as on the other read routes. Datasets whose
`geometry_type` is `geometry` or `geometry_collection` are not served. The
routes answer permissive CORS.

### Query

- `where` in the SQL-92 subset Esri clients send: comparisons, `IN`, `LIKE`,
  `BETWEEN`, `IS NULL`, boolean logic and `DATE` literals
- `objectIds`, `outFields`, `returnGeometry`, `returnCountOnly`, `returnIdsOnly`
- `orderByFields` over any field, `ASC` or `DESC`
- `resultOffset` and `resultRecordCount`, with `exceededTransferLimit`
- `esriGeometryEnvelope` with `esriSpatialRelIntersects`
- `inSR` and `outSR` as 4326 or Web Mercator (3857 or 102100)
- `f=json`, `f=pjson` or `f=geojson`
- `returnDistinctValues`, which needs `outFields` to name its fields
- `outStatistics` with `groupByFieldsForStatistics`: `count`, `sum`, `min`,
  `max`, `avg`, `stddev` and `var`
- `having` or `havingClause`, which needs both of the above and names
  aggregates, e.g. `COUNT(houses) > 1000 AND AVG(pop) >= 20`. An aggregate
  need not be in `outStatistics`. `COUNT(*)` counts rows, `COUNT(field)` non-null
  values. A grouped field name or an `outStatisticFieldName` also works.
  Ordering and paging apply after it.

Distinct and grouped answers carry attributes and no geometry, page like rows,
take `orderByFields` over their own columns, and carry an object id only when
`outFields` names it. A parameter the facade cannot honor is refused, as HTTP
200 with an `{"error": {...}}` body.

### Edits and attachments

`applyEdits` makes the batch one commit on `main`, and any failure refuses all
of it. Only a layer with a real integer `objectid` field takes edits.

Attachments: per-feature list and download, `queryAttachments`, and multipart
`addAttachment`, `updateAttachment` and `deleteAttachments` up to 32 MiB, the
writes gated like `applyEdits`. A delete sets `deleted_at` and keeps the bytes,
and every read, here and under `/api/v1`, skips deleted rows.

A layer with an `objectid` also publishes a virtual `globalid` field
(`esriFieldTypeGlobalID`, named by `globalIdField`): the feature uuid in upper
case in braces. `outFields` and `where` (`=` and `IN`, braces and case optional)
accept it. `applyEdits` drops a client-supplied `globalid`.

### Credentials

`Authorization: Bearer <jwt>` first, then `X-Esri-Authorization: Bearer <jwt>`,
which verne sends, then a `token` query parameter for a browser client that can
send no header. The last two are read under `/arcgis/rest/services` only and
carry the same grants. The request log redacts `token`.

### extractChanges

A layer's generation is the epoch milliseconds of the newest of the head
changeset's time and the times attachments on `main` were created or deleted, or
0 for an empty branch. An attachment upload commits no changeset, which is why
the generation is a time and not a commit count. The service root lists
`ChangeTracking` and publishes `changeTrackingInfo.layerServerGens`.

`POST extractChanges` with `layers=0` and
`layerServerGens=[{"id": 0, "serverGen": <n>}]` answers a `statusUrl`, whose
first answer is `Completed` with a `resultUrl`. The change file lists the object
ids added, updated and deleted in the window `(<n>, generation at submit]`,
which is the diff from the newest changeset at or before `<n>` to the head at
submit. Its generation opens the next window. The job id encodes the request,
nothing is stored, and a job id this service did not issue is refused.

A generation below the layer's first commit or ahead of its current one is refused
with a message to extract the layer in full. Change files carry object ids and
no geometry, so fetch rows through `/query`. Refused by name: `dataFormat=sqlite`,
the positional `serverGens` form, and `returnInserts`, `returnUpdates` or
`returnDeletes` set to `false`. A layer without a real `objectid`, and an
external dataset, publish no change tracking.

Attachment changes use the same window. One created inside it and still present
is an add with `attachmentId`, `globalId`, `parentGlobalId`, `contentType`,
`name`, `size` and an absolute `url`. One present at the start and deleted
inside it is a global id in `deleteIds`. One created and deleted inside it is in
neither. `updates` is always empty, since replacing an attachment is a delete
and an upload.

### Symbology

Layer metadata carries `drawingInfo` when the dataset has a symbology rule whose
symbol is tagged `{"format": "esri-drawing-info"}`, which verne writes when it
migrates a hosted feature layer. It is served back verbatim.

`GET /api/v1/datasets/{id}/style` translates that document into Mapbox GL
layers. `source` and `sourceLayer` are query parameters, `losses` lists what
did not translate, and `images` holds the bitmaps picture symbols use, keyed by
name, each with a `data_uri`, `width` and `height` in CSS pixels, to register
before the layers draw.

## API Endpoints

A key a route does not declare is refused: `422` in a JSON body, `400` in a
query string, naming the key. Exempt are the OIDC callback,
`/api/v1/ogc/collections/{id}/items`, `/api/v1/stac/search` and the replication
routes, whose callers are outside this codebase.

### Collaboration relay

`/ws/rooms/{room_id}` relays every text frame a client sends to every other
client in the room, never back to the sender. Nothing is stored. A room exists
while someone is connected. The handshake needs a token, which a browser sends
as `new WebSocket(url, ["bearer", jwt])`, and the server echoes only `bearer`.
The subprotocol is read as a credential on `/ws/` paths only.

The server does not read the messages. ViewTopia uses these shapes for view
sync, cursors, presence and chat:

```jsonc
{ "type": "Join", "user_id": "u1", "user_name": "Alice", "asset_id": "my-room" }
{ "type": "Camera", "user_id": "u1", "latitude": 40.7, "longitude": -73.9, "zoom": 14, "bearing": 0, "pitch": 45 }
{ "type": "Cursor", "user_id": "u1", "latitude": 40.71, "longitude": -73.91 }
{ "type": "Chat", "user_id": "u1", "user_name": "Alice", "message": "Look at this area" }
{ "type": "Leave", "user_id": "u1", "asset_id": "my-room" }
```

### Routes

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/health` | Health check |
| GET | `/api/v1/healthz` | Liveness, 200 as soon as the process is up |
| GET | `/api/v1/readyz` | Readiness, 200 once the database answers |
| GET | `/api/v1/capabilities` | What this deployment can do, currently `email_configured` |
| GET POST | `/api/v1/workspaces` | List or create workspaces |
| GET PUT DELETE | `/api/v1/workspaces/{id}` | Read, update, or delete a workspace |
| GET | `/api/v1/workspaces/{id}/members` | List workspace members, owner only |
| PUT DELETE | `/api/v1/workspaces/{workspace_id}/members/{user_id}` | Set or remove a workspace member |
| GET POST | `/api/v1/workspaces/{id}/projects` | List or create projects in a workspace |
| GET POST | `/api/v1/workspaces/{id}/invitations` | List or create workspace invitations |
| DELETE | `/api/v1/workspaces/{workspace_id}/invitations/{invitation_id}` | Revoke a workspace invitation |
| GET | `/api/v1/projects` | List accessible projects |
| GET PUT DELETE | `/api/v1/projects/{id}` | Read, update, or delete a project |
| GET | `/api/v1/projects/{id}/members` | List project members, owner only |
| PUT DELETE | `/api/v1/projects/{project_id}/members/{user_id}` | Set or remove a project member |
| GET POST | `/api/v1/projects/{id}/invitations` | List or create project invitations |
| DELETE | `/api/v1/projects/{project_id}/invitations/{invitation_id}` | Revoke a project invitation |
| GET PUT | `/api/v1/projects/{id}/state/{key}` | Read or write one key of the project's shared state |
| GET POST | `/api/v1/projects/{id}/attachments` | List or upload the project's attachments |
| GET DELETE | `/api/v1/projects/{project_id}/attachments/{id}` | Download or delete one project attachment |
| POST | `/api/v1/invitations/accept` | Accept an invitation as the authenticated caller |
| GET | `/api/v1/datasets` | List datasets |
| POST | `/api/v1/datasets` | Create dataset |
| GET | `/api/v1/datasets/{id}` | Get dataset |
| PATCH | `/api/v1/datasets/{id}` | Set dataset visibility (dataset admin) |
| GET | `/api/v1/datasets/{id}/branches` | List branches |
| POST | `/api/v1/datasets/{id}/branches` | Create branch, optionally from `fork_from_branch` |
| GET | `/api/v1/branches/{id}` | Get branch |
| GET | `/api/v1/branches/{id}/history` | Last 100 changesets |
| GET | `/api/v1/branches/{id}/features` | List features, `cursor`, `limit` up to 10000, `valid_at` |
| GET | `/api/v1/branches/{id}/features/{feature_id}` | One live feature, geometry as hex WKB with its properties |
| GET | `/api/v1/branches/{id}/features/{feature_id}/native` | Pre-reprojection original geometry, exact |
| GET | `/api/v1/branches/{id}/features/bbox` | Spatial bbox filter |
| POST | `/api/v1/branches/{id}/features/intersects` | Spatial intersects filter |
| POST | `/api/v1/branches/{id}/features/within` | Spatial within filter |
| GET | `/api/v1/branches/{id}/features/count` | Feature count |
| GET | `/api/v1/branches/{id}/features/at?at=` | Features as of an RFC 3339 instant |
| GET | `/api/v1/branches/{id}/tiles/{z}/{x}/{y}` | MVT vector tiles |
| POST | `/api/v1/branches/{id}/commit` | Commit changes, checked against the dataset schema |
| POST | `/api/v1/branches/{id}/batch` | Batch commit, not checked against the schema |
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
| GET | `/metrics` | Prometheus metrics (admin) |
| GET | `/auth/oidc/login` | OIDC SSO login |
| GET | `/auth/oidc/callback` | OIDC callback |
| GET | `/auth/oidc/config` | Whether OIDC is enabled, and the issuer URL when it is |
| GET | `/review` | Merge request UI. Sends no token, and its map panel draws no diff |
| GET | `/conflicts` | Conflict resolution UI. Sends no token |
| GET | `/api/v1/datasets/{id}/schema` | Get dataset schema |
| PUT | `/api/v1/datasets/{id}/schema` | Set dataset schema |
| GET | `/api/v1/branches/{id}/quality` | Data quality report. The error and null-field lists are always empty |
| POST | `/api/v1/branches/{id}/repair` | Repair invalid geometries with `ST_MakeValid`, as one commit |
| GET | `/api/v1/datasets/{id}/webhooks` | List webhook subscriptions (admin) |
| POST | `/api/v1/datasets/{id}/webhooks` | Subscribe, `url` must be http or https (admin) |
| DELETE | `/api/v1/webhooks/{id}` | Delete subscription (admin) |
| GET | `/api/v1/datasets/{id}/events` | List events, `limit` only (admin) |
| POST | `/api/v1/datasets/{id}/events` | Emit a custom event, delivered like the built-in ones |
| GET | `/api/v1/branches/{id}/analytics/buffer` | Buffer one `feature_id` by `distance` meters |
| GET | `/api/v1/branches/{id}/analytics/union` | Union of all features and its area |
| GET | `/api/v1/branches/{id}/analytics/coverage` | Area covered by the live features buffered by `distance` meters, required, over 0 and at most 100000 |
| GET | `/api/v1/branches/{id}/analytics/clusters` | `ST_ClusterDBSCAN` clusters |
| GET | `/api/v1/branches/{id}/analytics/anomalies` | Features over 3 standard deviations from the centroid, and non-simple geometries |
| GET | `/api/v1/branches/{id}/analytics/stats` | Count, total area and length, extent, centroid |
| GET | `/api/v1/ogc` | OGC landing page |
| GET | `/api/v1/ogc/conformance` | OGC conformance |
| GET | `/api/v1/ogc/collections` | OGC collections |
| GET | `/api/v1/ogc/collections/{id}` | One OGC collection |
| GET | `/api/v1/ogc/collections/{id}/items` | OGC feature items |
| GET | `/api/v1/ogc/collections/{id}/items/{fid}` | OGC single feature |
| GET | `/arcgis/rest/services` | ArcGIS service catalog |
| GET | `/arcgis/rest/services/{service}/FeatureServer` | ArcGIS service root |
| GET | `/arcgis/rest/services/{service}/FeatureServer/0` | ArcGIS layer metadata |
| GET POST | `/arcgis/rest/services/{service}/FeatureServer/0/query` | ArcGIS feature query |
| POST | `/arcgis/rest/services/{service}/FeatureServer/0/applyEdits` | ArcGIS batch edits |
| GET POST | `/arcgis/rest/services/{service}/FeatureServer/0/queryAttachments` | ArcGIS attachment listing |
| GET | `/arcgis/rest/services/{service}/FeatureServer/0/{oid}/attachments` | ArcGIS feature attachments |
| GET | `.../0/{oid}/attachments/{attachmentId}` | Download one ArcGIS attachment |
| POST | `.../0/{oid}/addAttachment`, `updateAttachment`, `deleteAttachments` | ArcGIS attachment edits |
| POST | `/arcgis/rest/services/{service}/FeatureServer/extractChanges` | ArcGIS change extraction |
| GET | `/arcgis/rest/services/{service}/FeatureServer/jobs/{jobId}` | ArcGIS extract job status |
| GET | `/arcgis/rest/services/{service}/FeatureServer/changefiles/{jobId}` | ArcGIS change file |
| GET | `/api/v1/audit` | Audit log, `limit` and `actor` (admin) |
| GET | `/api/v1/catalog/search` | Search datasets (text + tags) |
| GET | `/api/v1/datasets/{id}/tags` | List dataset tags |
| POST | `/api/v1/datasets/{id}/tags` | Add tag |
| DELETE | `/api/v1/datasets/{id}/tags/{tag}` | Remove tag |
| GET | `/api/v1/datasets/{id}/metadata` | Get dataset metadata |
| PUT | `/api/v1/datasets/{id}/metadata` | Set dataset metadata |
| GET | `/api/v1/datasets/{id}/permissions` | List dataset grants (dataset admin) |
| POST | `/api/v1/datasets/{id}/permissions` | Grant on a dataset (dataset admin) |
| DELETE | `/api/v1/datasets/{id}/permissions/{user}` | Revoke on a dataset (dataset admin) |
| GET | `/api/v1/datasets/{id}/permissions/{user}/check` | Whether that user holds `required` on the dataset |
| PUT | `/api/v1/datasets/{id}/project` | Attach to a project and make it private (dataset admin, project editor) |
| DELETE | `/api/v1/datasets/{id}/project` | Detach, leaving it private (dataset admin or project editor) |
| GET | `/api/v1/branches/{id}/permissions` | List branch grants (dataset admin) |
| POST | `/api/v1/branches/{id}/permissions` | Grant on a branch (dataset admin) |
| DELETE | `/api/v1/branches/{id}/permissions/{user}` | Revoke on a branch (dataset admin) |
| GET | `/api/v1/branches/{id}/permissions/{user}/check` | Whether that user holds `required` on the branch |
| GET | `/api/v1/conflicts/{branch_id}` | Conflicts between that branch and its dataset's `main` |
| GET | `/api/v1/branches/{target}/merge/{source}/preview` | Merge preview with conflict GeoJSON |
| POST | `/api/v1/branches/{target}/merge/{source}/resolve` | Resolve conflicts and create the merge commit |
| WS | `/ws/rooms/{room_id}` | Collaboration relay |
| **Networks** | | |
| GET | `/api/v1/datasets/{id}/networks` | List geometric networks |
| POST | `/api/v1/datasets/{id}/networks` | Create network |
| GET | `/api/v1/networks/{id}` | Get network |
| GET | `/api/v1/networks/{id}/junctions` | List junctions |
| POST | `/api/v1/networks/{id}/junctions` | Add junction |
| GET | `/api/v1/networks/{id}/edges` | List edges |
| POST | `/api/v1/networks/{id}/edges` | Add edge |
| POST | `/api/v1/networks/{id}/trace` | Upstream or downstream trace, a recursive CTE |
| POST | `/api/v1/networks/{id}/shortest-path` | Dijkstra shortest path, needs pgRouting |
| POST | `/api/v1/networks/{id}/astar` | A* shortest path, needs pgRouting |
| POST | `/api/v1/networks/{id}/isochrone` | Driving distance from a junction, needs pgRouting |
| POST | `/api/v1/networks/{id}/tsp` | Traveling-salesman tour, needs pgRouting |
| GET | `/api/v1/networks/{id}/connectivity` | Connected components report, needs pgRouting |
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
| POST | `/api/v1/attribute-rules/{id}/validate` | Answers `valid: true` for any non-empty expression. Nothing parses it |
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
| **PostGIS Topology**, admin except `validate` | | |
| GET | `/api/v1/datasets/{id}/topologies` | List topologies |
| POST | `/api/v1/datasets/{id}/topologies` | Create topology |
| POST | `/api/v1/topologies/{name}/validate` | Validate topology |
| GET | `/api/v1/topologies/{name}/faces` | List faces |
| GET | `/api/v1/topologies/{name}/edges` | List edges |
| GET | `/api/v1/topologies/{name}/nodes` | List nodes |
| POST | `/api/v1/topologies/{name}/add-face` | Add face |
| POST | `/api/v1/topologies/{name}/simplify` | Answers `simplified` and changes nothing |
| **SFCGAL 3D**, needs SFCGAL | | |
| POST | `/api/v1/branches/{id}/3d/extrude` | Extrude 2D to 3D |
| POST | `/api/v1/branches/{id}/3d/volume` | Compute volume |
| POST | `/api/v1/branches/{id}/3d/intersection` | 3D intersection |
| POST | `/api/v1/branches/{id}/3d/straight-skeleton` | Straight skeleton |
| POST | `/api/v1/branches/{id}/3d/minkowski-sum` | Minkowski sum |
| POST | `/api/v1/branches/{id}/3d/tesselate` | Tesselation |
| POST | `/api/v1/branches/{id}/3d/visibility` | 3D distance from an observer point to a feature, and whether the line to its centroid meets it |
| **H3 Indexing**, needs h3-pg | | |
| POST | `/api/v1/branches/{id}/h3/index` | Index features with H3 |
| GET | `/api/v1/branches/{id}/h3/hexagons` | Get covering hexagons |
| GET | `/api/v1/branches/{id}/h3/aggregate` | Aggregate by hex cell |
| GET | `/api/v1/branches/{id}/h3/neighbors` | K-ring neighbors |
| POST | `/api/v1/branches/{id}/h3/compact` | Compact hex set |
| GET | `/api/v1/h3/cell?lng=&lat=` | Point to H3 cell |
| GET | `/api/v1/h3/boundary?cell=` | Cell to boundary polygon |
| **Vector Similarity**, needs pgvector | | |
| POST | `/api/v1/branches/{id}/similarity/search` | Similarity search |
| GET | `/api/v1/branches/{id}/similarity/duplicates` | Find duplicates |
| POST | `/api/v1/branches/{id}/similarity/embed` | Generate embeddings |
| POST | `/api/v1/branches/{id}/similarity/cluster` | Equal-size buckets by distance to the mean embedding |
| **Point Cloud** | | |
| GET | `/api/v1/datasets/{id}/pointclouds` | List point cloud catalogs |
| POST | `/api/v1/datasets/{id}/pointclouds` | Create catalog |
| GET | `/api/v1/pointclouds/{id}` | Get catalog |
| GET | `/api/v1/pointclouds/{id}/patches` | List patches |
| POST | `/api/v1/pointclouds/{id}/patches` | Add patch, needs pointcloud |
| POST | `/api/v1/pointclouds/{id}/query` | Spatial query |
| GET | `/api/v1/pointclouds/{id}/stats` | Catalog stats |
| POST | `/api/v1/pointclouds/{id}/profile` | Elevation profile, needs pointcloud |
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
| **Geoprocessing** | | |
| POST | `/api/v1/branches/{id}/geoprocessing/clip` | Clip features by a GeoJSON polygon |
| POST | `/api/v1/branches/{id}/geoprocessing/intersect` | Pairwise intersection |
| POST | `/api/v1/branches/{id}/geoprocessing/difference` | Difference of two feature sets |
| POST | `/api/v1/branches/{id}/geoprocessing/dissolve` | Dissolve by an attribute |
| POST | `/api/v1/branches/{id}/geoprocessing/spatial-join` | Join attributes by spatial relation |
| POST | `/api/v1/branches/{id}/geoprocessing/voronoi` | Voronoi polygons |
| POST | `/api/v1/branches/{id}/geoprocessing/convex-hull` | Convex hull |
| POST | `/api/v1/branches/{id}/geoprocessing/centroid` | Centroids |
| POST | `/api/v1/branches/{id}/geoprocessing/nearest-neighbor` | Nearest neighbours |
| POST | `/api/v1/branches/{id}/geoprocessing/distance-matrix` | Pairwise distances |
| POST | `/api/v1/branches/{id}/geoprocessing/contour` | Contour lines from point values, needs `ST_ContourLines` |
| POST | `/api/v1/branches/{id}/geoprocessing/merge` | Union named features into one geometry |
| POST | `/api/v1/branches/{id}/geoprocessing/split` | Split one feature by a GeoJSON line |
| POST | `/api/v1/branches/{id}/geoprocessing/simplify` | Simplify geometries |
| POST | `/api/v1/branches/{id}/geoprocessing/densify` | Add vertices along segments |
| **QGIS** | | |
| GET | `/api/v1/qgis/capabilities` | What the QGIS integration offers |
| GET | `/api/v1/qgis/datasets` | Datasets a QGIS client may load |
| GET | `/api/v1/qgis/branches/{id}/layer` | Layer definition for one branch |
| POST | `/api/v1/qgis/branches/{id}/transaction` | WFS-T style transaction |
| GET POST | `/api/v1/qgis/branches/{id}/sync` | Pull a snapshot, or push local edits |
| GET | `/api/v1/qgis/branches/{id}/conflicts` | Conflicts waiting on this branch |
| POST | `/api/v1/qgis/branches/{id}/conflicts/resolve` | Resolve one of them |
| **Attachments** | | |
| GET POST | `/api/v1/branches/{id}/features/{feature_id}/attachments` | List or upload a feature's attachments |
| GET POST | `/api/v1/datasets/{id}/attachments` | List or upload a dataset's attachments |
| GET DELETE | `/api/v1/attachments/{id}` | Download or delete one attachment |
| GET | `/api/v1/attachments/{id}/meta` | Attachment metadata without the bytes |
| **Schema evolution** | | |
| GET POST | `/api/v1/datasets/{id}/schema/migrations` | List schema migrations, or apply one |
| GET | `/api/v1/datasets/{id}/schema/version` | Current schema version |
| **Compaction** | | |
| POST | `/api/v1/branches/{id}/compact` | Prune old feature versions, keeping the most recent per feature |
| GET | `/api/v1/datasets/{id}/compaction-history` | What past compactions removed |
| **Replication**, admin | | |
| GET | `/api/v1/replication/feed/{branch_id}` | Ordered change feed a replica consumes |
| GET POST | `/api/v1/replication/peers` | List or register peers |
| POST | `/api/v1/replication/peers/{id}/sync` | Record how far a peer has consumed |
| **Industry verticals** | | |
| GET | `/api/v1/sensors` | A branch's features read as sensors |
| GET | `/api/v1/sensors/readings` | Features read as readings of one sensor |
| POST | `/api/v1/surveys/compare` | Cut, fill and net volume from two features' `mean_elevation` |
| GET | `/api/v1/construction/surveys` | Features read as surveys |
| GET | `/api/v1/construction/milestones` | Features read as milestones |
| GET | `/api/v1/fields` | Features read as agricultural fields |
| GET | `/api/v1/fields/ndvi` | NDVI held on one field's properties, classified |
| GET | `/api/v1/towers` | Features read as towers |
| POST | `/api/v1/coverage/simulate` | Hata path loss and a coverage circle for one tower |
| GET POST | `/api/v1/incidents` | Features read as incidents, or commit one |
| POST | `/api/v1/incidents/evacuate` | Danger-zone circle and the caller's assembly points by distance |
| GET | `/api/v1/parcels/search` | Search a branch by bbox, apn, address or owner |
| GET | `/api/v1/comps/search` | Comparable parcels within a radius of a point |
| POST | `/api/v1/parcels/split` | Answers the parcel geometry, does not split it |
| POST | `/api/v1/parcels/merge` | Answers the parcels' geometries, does not merge them |

A route that needs an extension answers `501` without it.

The geoprocessing routes answer GeoJSON and write no changeset, so commit a
result yourself to keep it. The vertical routes read conventions over feature
`properties` on a branch, such as a `sensor_type`, `mean_elevation` or
`ndvi_mean` property. `POST /api/v1/incidents` is the only one that commits.

Replication is the change feed plus a peer table, and the caller drives it.
Nothing pulls from a registered peer on its own.

Both imports answer `{imported, skipped, changeset_id, errors}`. Rows that fail
to parse are skipped and named in `errors`, and the rest land as one changeset.
A request whose rows all fail answers 422 and writes nothing. One request takes
at most 50,000 features and a 64 MiB body.

Feature and dataset attachment uploads take axum's default 2 MB body.

## Optional PostgreSQL extensions

Migrations create each of these when the server has it and skip it when not.
Ptolemy ships no database image, so install the ones you need yourself. Tests
exercise only the `501` branch of the pgRouting, SFCGAL, pgvector, pointcloud
and MobilityDB routes.

| Extension | Used for |
|-----------|----------|
| PostGIS Topology | `/api/v1/topologies` |
| pg_trgm | Index for the catalog's case-insensitive substring search. No ranking, no typo tolerance |
| pgRouting | Dijkstra, A*, TSP, connected components, isochrones |
| SFCGAL | 3D routes |
| h3-pg | H3 routes |
| pgvector | Similarity routes. The embedding is a SHA-256 hash spread over 256 floats, so identical text matches and near-identical text does not |
| pointcloud | Point cloud patches and profiles |
| MobilityDB | Trajectory analytics. Without it a trajectory is stored as JSONB |

## Standards

- **OGC API - Features** Part 1 (core, GeoJSON, OpenAPI 3.0) and Part 2 (CRS by
  reference). Every collection offers CRS84, EPSG:4326 and EPSG:3857, plus the
  dataset's own srid. CRS84 is longitude first and a geographic EPSG code
  latitude first, and `bbox-crs` is read the same way.
- **CQL2-JSON**: comparisons, `and`, `or`, `not`, `like`, `between`, `in`,
  `isNull`, `s_intersects`, `s_within`, `s_contains`
- **OGC Tiles**: WebMercatorQuad and WorldCRS84Quad
- **STAC 1.0** over raster catalogs
- **ArcGIS Geoservices REST** FeatureServer, see above

## Not built

| Subsystem | State |
|-----------|-------|
| Rate limiting | None. Put a proxy in front |
| Feature locking | None. Use branches and the merge conflict flow |
| Topology rule engine | None. PostGIS Topology under `/api/v1/topologies` is a separate subsystem |
| Domains, subtypes, attribute rules | Stored and served, enforced nowhere. There is no expression engine |
| Geometry type constraints | Stored and never checked |
| Multi-tenancy | None. One instance is one tenant |
| Parcel split and merge | `POST /api/v1/parcels/split` and `/parcels/merge` answer the input geometry as hex WKB and a message telling the caller to do it elsewhere. The geoprocessing `split` and `merge` routes do compute a geometry |
| Topology simplify | `POST /api/v1/topologies/{name}/simplify` computes a simplified edge, discards it and answers `simplified` |
| Review map diff | The `/review` map panel draws a basemap and no changes |
| Pluggable storage backends | `DataStore` in `ptolemy-core` is implemented by `ptolemy-geopackage`, `ptolemy-mongodb` and `ptolemy-elasticsearch`, and no binary uses any of them. The server and CLI use `PgStore`, which does not implement it |

## CLI

`serve`, `migrate`, `dataset` (`create`, `list`, `show`), `branch` (`create`,
`list`, `show`), `commit`, `merge`, `log`, `features`, `diff`, `import`,
`export`, `gpkg-export`, `backup`, `restore` and `api-key` (`create`, `list`,
`revoke`). `--database-url`, `--db-max-connections` and `--db-min-connections`
go before the subcommand and default to their environment variables.

`export` writes a GeoJSON FeatureCollection to `--output` or stdout.
`gpkg-export --branch <uuid> --output out.gpkg` writes a GeoPackage, with the
layer named by `--layer` (`features` by default).

### Import

The format is picked by extension: `.shp` (with its `.dbf`), `.gpkg`, and
anything else as GeoJSON. The file is positional, the branch is a uuid, and it
lands as one commit.

```bash
ptolemy import --branch <branch-uuid> --author you data.geojson
ptolemy import --branch <branch-uuid> --author you parcels.shp
ptolemy import --branch <branch-uuid> --author you terrain.gpkg
```

`--message` defaults to `Import features`.

### API keys

```bash
ptolemy api-key create "CI Pipeline" --role editor --expires-days 365
ptolemy api-key list
ptolemy api-key revoke ptk_abc123
```

The key is printed once and stored as a SHA-256 hash. `list` shows prefixes.
`revoke` takes a prefix or the full key. `--role` takes `admin`, `editor` or
`viewer`, and anything else becomes `viewer`. `--expires-days` defaults to 365,
and `0` means never.

### Backup and restore

```bash
ptolemy backup --custom ptolemy_backup.dump
ptolemy restore ptolemy_backup.dump
```

`backup` runs `pg_dump`, plain SQL unless `--custom`. `restore` tries
`pg_restore` and falls back to plain SQL, so it takes either. `--clean` drops
existing objects first.

## Tests

The suite needs a PostGIS database:

```bash
DATABASE_URL=postgres://postgres:postgres@localhost/ptolemy_test cargo test --all -- --test-threads=1
```

`crates/ptolemy-api/tests/route_sweep.rs` calls every route on the router,
reading the list off the router itself, and fails on SQLSTATE 42703 (undefined
column) and 42P01 (undefined table). Every query is a runtime `sqlx::query`, so
this is the only check of handler SQL against the migrated schema. It prints
what it covered and every 500. Skipped routes are listed in the test with a
reason each.

The MongoDB and Elasticsearch tests are ignored by default. Run them with
`cargo test -p ptolemy-mongodb -- --ignored` against `PTOLEMY_MONGO_URI`
(default `mongodb://localhost:27019`) and `cargo test -p ptolemy-elasticsearch
-- --ignored` against `PTOLEMY_ES_URL` (default `http://localhost:9209`).

## Project Structure

```
crates/
├── ptolemy-core/          # Domain types, diff, the DataStore trait
├── ptolemy-storage/       # PgStore: migrations, commits, merges, queries
├── ptolemy-api/           # Axum router
├── ptolemy-cli/           # the ptolemy binary
├── ptolemy-geopackage/    # GeoPackage DataStore, unused by the binary
├── ptolemy-mongodb/       # MongoDB DataStore, unused by the binary
└── ptolemy-elasticsearch/ # Elasticsearch DataStore, unused by the binary
```

The CLI reads shapefiles through the `shapefile` crate and GeoPackage through
`rusqlite`, not through `ptolemy-geopackage`.

## Prior art

| Project | Status | Difference |
|---------|--------|-----------|
| [GeoGig](https://geogig.org/) | Abandoned | Java |
| [Kart](https://kartproject.org/) | Active | Git-backed CLI with local working copies, no multi-user server |
| [pg_version](https://github.com/CartoDB/cartodb-postgresql) | Limited | Single-table temporal, no branching |

## License

AGPL-3.0-or-later, see [LICENSE](LICENSE).

Copyright (C) 2026 Grok Image Compression Inc.
