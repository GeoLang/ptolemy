# Ptolemy

**Open-source enterprise geodatabase & collaboration platform.**

Ptolemy provides versioned spatial data management — branch, commit, diff, and merge geographic datasets with git-like workflows. Built on PostGIS, designed for teams.

## Why Ptolemy?

Enterprise GIS users are locked into proprietary platforms (Esri, Hexagon) primarily because of versioned geodatabase workflows — multi-user editing with conflict detection, branching, and audit trails. Ptolemy brings these capabilities to the open-source stack.

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
| WS | `/ws/branches/{id}` | Real-time branch events |

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
