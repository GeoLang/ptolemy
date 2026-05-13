# Ptolemy

**Open-source enterprise geodatabase & collaboration platform.**

Ptolemy provides versioned spatial data management — branch, commit, diff, and merge geographic datasets with git-like workflows. Built on PostGIS, designed for teams.

## Why Ptolemy?

Enterprise GIS users are locked into proprietary platforms (Esri, Hexagon) primarily because of versioned geodatabase workflows — multi-user editing with conflict detection, branching, and audit trails. Ptolemy brings these capabilities to the open-source stack.

### Key Features (Roadmap)

| Version | Feature |
|---------|---------|
| **v0.1** | Versioned feature store: branch, commit, diff, merge spatial datasets. CLI + REST API. |
| **v0.2** | Conflict resolution, role-based access control, audit log. |
| **v0.3** | Offline sync protocol, QGIS plugin for field-to-server workflows. |
| **v0.4** | Web review UI — pull-request-style review for geodata changes. |

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
```

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

Mozilla Public License 2.0. See [LICENSE](LICENSE) for details.

## Prior Art & Differentiation

| Project | Status | Limitation |
|---------|--------|-----------|
| [GeoGig](https://geogig.org/) | Abandoned | Java, heavy, poor DX |
| [Kart](https://kartproject.org/) | Active | GeoPackage-only, no multi-user server |
| [pg_version](https://github.com/CartoDB/cartodb-postgresql) | Limited | Single-table temporal, no branching |

Ptolemy aims to be: **fast (Rust), server-native (PostGIS), with git-quality branching/merging UX**.
