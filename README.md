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
| **pgvector** | Vector similarity search, feature deduplication, k-means clustering |
| **pg_trgm** | Fuzzy text search for data catalog |
| **pointcloud** | LiDAR/point cloud storage and spatial queries |
| **MobilityDB** | Moving object trajectories, speed/distance analysis |

### Standards Implemented

- **STAC 1.0** — SpatioTemporal Asset Catalog for raster discovery
- **OGC Tiles** — Standard tile matrix sets (WebMercatorQuad, WorldCRS84Quad)
- **CQL2** — Common Query Language for spatial/attribute filtering
- **OGC API - Features** — Part 1 & 2 compliant

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
| **v1.6** | Background jobs, conflict resolution API, gRPC bulk ops | ✓ Done |

## Architecture

```
┌───────────────────────────────────────────┐
│  Clients (QGIS Plugin, Web UI, CLI)       │
├───────────────────────────────────────────┤
│  ptolemy-api (Axum REST/gRPC service)     │
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

## Configuration

| Variable | Description | Default |
|----------|-------------|---------|
| `DATABASE_URL` | PostgreSQL connection URL | (required) |
| `PTOLEMY_JWT_SECRET` | JWT signing secret (enables auth when set) | (disabled) |
| `PTOLEMY_OIDC_ISSUER_URL` | OIDC provider URL (e.g. Keycloak realm) | (disabled) |
| `PTOLEMY_OIDC_CLIENT_ID` | OAuth2 client ID | — |
| `PTOLEMY_OIDC_CLIENT_SECRET` | OAuth2 client secret | — |
| `PTOLEMY_OIDC_REDIRECT_URL` | Callback URL for OIDC flow | — |
| `PTOLEMY_DB_MAX_CONNECTIONS` | Max DB pool connections | 10 |
| `PTOLEMY_DB_MIN_CONNECTIONS` | Min DB pool connections | 2 |

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
| GET | `/api/v1/datasets/{id}/branches` | List branches |
| POST | `/api/v1/datasets/{id}/branches` | Create branch |
| GET | `/api/v1/branches/{id}` | Get branch |
| GET | `/api/v1/branches/{id}/history` | Commit log |
| GET | `/api/v1/branches/{id}/features` | List features (paginated) |
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
| GET | `/metrics` | Prometheus metrics |
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
| GET | `/api/v1/orgs` | List organizations |
| POST | `/api/v1/orgs` | Create organization |
| GET | `/api/v1/orgs/{id}/members` | List members |
| POST | `/api/v1/orgs/{id}/members` | Add member |
| GET | `/api/v1/orgs/{id}/datasets` | Org datasets |
| GET | `/api/v1/conflicts/{id}` | List merge conflicts |
| POST | `/api/v1/conflicts/{id}/resolve` | Resolve conflicts |
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
| POST | `/api/v1/branches/{id}/similarity/search` | Similarity search |
| GET | `/api/v1/branches/{id}/similarity/duplicates` | Find duplicates |
| POST | `/api/v1/branches/{id}/similarity/embed` | Generate embeddings |
| POST | `/api/v1/branches/{id}/similarity/cluster` | K-means clustering |
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
| GET | `/api/v1/trajectories/{id}/at?timestamp=` | Position at time |
| GET | `/api/v1/trajectories/{id}/speed` | Speed analysis |
| GET | `/api/v1/trajectories/{id}/distance` | Distance/duration |
| POST | `/api/v1/trajectories/{id}/simplify` | Simplify trajectory |
| POST | `/api/v1/datasets/{id}/trajectories/nearest` | Nearest approach |
| **CQL2 + OGC Tiles** | | |
| POST | `/api/v1/branches/{id}/features/filter` | CQL2 filter query |
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
| POST | `/api/v1/branches/{id}/reproject` | Reproject all features |
| GET | `/api/v1/crs/search?q=` | Search coordinate systems |
| GET | `/api/v1/crs/{srid}` | Get CRS details |

## Building

```bash
cargo build --release
```

## Project Structure

```
crates/
├── ptolemy-core/      # Domain types, merge logic, diff algorithms
├── ptolemy-storage/   # PostGIS storage backend
├── ptolemy-api/       # Axum REST API server
└── ptolemy-cli/       # CLI binary (server + admin commands)
```

## License

GNU Affero General Public License v3.0 (AGPL-3.0). See [LICENSE](LICENSE) for details.

## Prior Art & Differentiation

| Project | Status | Limitation |
|---------|--------|-----------|
| [GeoGig](https://geogig.org/) | Abandoned | Java, heavy, poor DX |
| [Kart](https://kartproject.org/) | Active | GeoPackage-only, no multi-user server |
| [pg_version](https://github.com/CartoDB/cartodb-postgresql) | Limited | Single-table temporal, no branching |

Ptolemy aims to be: **fast (Rust), server-native (PostGIS), with git-quality branching/merging UX**.
