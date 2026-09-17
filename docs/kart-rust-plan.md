# Geodata Version Control in Rust: Architecture Plan

A Kart-like distributed VCS for geospatial and tabular data, written in Rust, built on Git's object model.

None of this is built. It is a design for a separate CLI tool, not a description
of Ptolemy, and every checkbox below is unchecked. Ptolemy's own versioning is a
changeset DAG in PostGIS, described in its README.

---

## 1. Project Overview

**Name:** `geokart` (working title)

**Goal:** A CLI tool that provides Git-like distributed version control for geospatial datasets (vector, raster, point cloud). Users can `init`, `import`, `commit`, `diff`, `merge`, `push`, `pull` geodata — editing in-place via GIS working copies (GeoPackage, PostGIS, etc.).

**Why Rust over Kart's Python:**

Nothing in the right column is measured. It is what the design is aiming at.

| Aspect | Kart (Python) | geokart (Rust), intended |
|--------|--------------|----------------|
| Startup time | ~500ms (Python interpreter) | ~5ms (native binary) |
| Large dataset ops | GIL-limited, ~1 thread | Rayon work-stealing, all cores |
| Memory | GC pressure on millions of features | Zero-copy MessagePack, arena allocation |
| Distribution | 80MB+ bundled Python + GDAL + SQLite | Single static binary (~15MB) |
| Correctness | Runtime type errors | Compile-time guarantees |

---

## 2. Core Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    CLI (clap)                           │
├──────────┬──────────┬───────────┬───────────┬───────────┤
│  init    │  import  │  commit   │  diff     │  merge    │
│  clone   │  export  │  status   │  log      │  branch   │
│  push    │  pull    │  checkout │  reset    │  resolve  │
├──────────┴──────────┴───────────┴───────────┴───────────┤
│              Working Copy Adapters                       │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌────────────┐ │
│  │GeoPackage│ │ PostGIS  │ │ SQL Srv  │ │   MySQL    │ │
│  └──────────┘ └──────────┘ └──────────┘ └────────────┘ │
├─────────────────────────────────────────────────────────┤
│              Dataset Layer                              │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌────────────┐ │
│  │ TableV3  │ │ Raster   │ │PointClou │ │  Metadata  │ │
│  └──────────┘ └──────────┘ └──────────┘ └────────────┘ │
├─────────────────────────────────────────────────────────┤
│              Storage Engine (libgit2 / gitoxide)        │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌────────────┐ │
│  │  Trees   │ │  Blobs   │ │ Commits  │ │   Refs     │ │
│  └──────────┘ └──────────┘ └──────────┘ └────────────┘ │
└─────────────────────────────────────────────────────────┘
```

### Crate Layout

```
geokart/
├── Cargo.toml                  # workspace
├── crates/
│   ├── geokart-cli/            # Binary entry point
│   │   └── src/main.rs
│   ├── geokart-core/           # Dataset model, diff, merge
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── dataset.rs      # Dataset trait + registry
│   │       ├── table_v3.rs     # Vector/tabular dataset format
│   │       ├── raster_v1.rs    # Raster tile dataset
│   │       ├── pointcloud_v1.rs# LAS/LAZ point cloud dataset
│   │       ├── schema.rs       # Column schema + legends
│   │       ├── diff.rs         # Row-level diffing engine
│   │       ├── merge.rs        # Three-way merge + conflicts
│   │       ├── feature.rs      # Feature encoding/decoding
│   │       └── path.rs         # Feature path generation (int/hash schemes)
│   ├── geokart-git/            # Git object storage via gitoxide
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── repo.rs         # Repository init/open/clone
│   │       ├── tree.rs         # Tree walking + building
│   │       ├── blob.rs         # Blob read/write
│   │       ├── commit.rs       # Commit creation
│   │       ├── refs.rs         # Branch/tag management
│   │       ├── remote.rs       # Push/pull/fetch
│   │       └── lfs.rs          # Git LFS for raster/pointcloud tiles
│   ├── geokart-wc/             # Working copy adapters
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── traits.rs       # WorkingCopy trait
│   │       ├── gpkg.rs         # GeoPackage via rusqlite
│   │       ├── postgis.rs      # PostGIS via sqlx
│   │       ├── tracking.rs     # Change tracking tables
│   │       └── spatial_filter.rs # Geographic subset clones
│   └── geokart-encoding/       # Serialization
│       └── src/
│           ├── lib.rs
│           ├── msgpack.rs      # MessagePack feature encoding
│           ├── geometry.rs     # GeoPackage binary geometry encoding
│           └── base64.rs       # URL-safe base64 for paths
```

---

## 3. Git Object Model — How Geodata Maps to Git

Kart's key insight: **every feature row is a Git blob, organized in Git trees**.

### Repository Layout (inside `.git`)

```
HEAD → refs/heads/main → commit
                            │
                            ▼
                          tree (root)
                            ├── parcels/
                            │   └── .table-dataset/
                            │       ├── meta/
                            │       │   ├── title           (blob: "City Parcels")
                            │       │   ├── description     (blob: "...")
                            │       │   ├── schema.json     (blob: [{id,name,dataType}...])
                            │       │   ├── legend/
                            │       │   │   └── <sha256>    (blob: msgpack [col_id, ...])
                            │       │   └── crs/
                            │       │       └── EPSG:4326.wkt (blob: WKT)
                            │       └── feature/
                            │           ├── A/A/A/B/kU0=    (blob: msgpack [legend, val...])
                            │           ├── A/A/B/C/abc=    (blob: msgpack [...])
                            │           └── ...
                            └── imagery/
                                └── .raster-dataset/
                                    ├── meta/...
                                    └── tile/...
```

### Why This Works

1. **Git deduplicates** — unchanged features share blobs across commits (zero storage cost)
2. **Git packs efficiently** — similar features delta-compress beautifully
3. **Three-way merge is free** — Git's tree merge handles file-level conflicts; Kart adds row-level semantics
4. **Push/pull/clone just work** — standard Git remotes (GitHub, GitLab, bare repos)
5. **Branching is O(1)** — just a ref pointer

---

## 4. Key Rust Crate Dependencies

| Purpose | Crate | Why |
|---------|-------|-----|
| Git operations | `gix` (gitoxide) | Pure Rust, no libgit2 C dependency, async-capable |
| CLI | `clap` | derive-based arg parsing |
| MessagePack | `rmp-serde` | Feature serialization (Kart-compatible) |
| GeoPackage I/O | `rusqlite` + `gpkg` | SQLite with SpatiaLite extension |
| PostGIS | `sqlx` | Async PostgreSQL |
| Geometry | `geo` + `wkb` | Geometry types + WKB encoding |
| Parallelism | `rayon` | Parallel feature processing |
| Spatial index | `rstar` | R-tree for spatial filtering |
| LFS | Custom or `git-lfs` protocol | Large raster/point cloud tiles |
| Progress | `indicatif` | Progress bars for large operations |
| Serialization | `serde` + `serde_json` | Schema, config, JSON output |
| Hashing | `sha2` | Feature path hashing |
| Base64 | `base64` | URL-safe base64 for feature paths |
| CRS | `proj` | Coordinate transformations |
| Error handling | `anyhow` / `thiserror` | Error propagation |

---

## 5. Feature Encoding (MessagePack)

Kart stores each feature as a MessagePack blob. We must be **byte-compatible** for interop:

```rust
use rmp_serde::{Deserializer, Serializer};
use serde::{Deserialize, Serialize};

/// A stored feature: legend hash + column values
#[derive(Serialize, Deserialize)]
struct StoredFeature {
    legend: String,           // SHA-256 of the legend
    values: Vec<ColumnValue>, // One per column (excl. PK, which is in path)
}

/// Column values use MessagePack extension types
enum ColumnValue {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
    Date(String),           // "YYYY-MM-DD"
    Time(String),           // "HH:MM:SS.ssss"
    Timestamp(String),      // ISO 8601
    Interval(String),       // ISO 8601 duration
    Numeric(String),        // Decimal as string
    Geometry(GpkgGeometry), // GeoPackage binary format, ext type 'G' (71)
}
```

### Geometry Encoding

Geometries use the **GeoPackage Binary** format (not WKB directly):

```rust
struct GpkgGeometry {
    /// Standard GeoPackage binary envelope
    /// Magic: 0x47, 0x50 ("GP")
    /// Version: 0x00
    /// Flags: envelope type + endianness
    /// SRS ID: always 0 (CRS stored in schema)
    /// Envelope: bbox doubles
    /// WKB payload
    data: Vec<u8>,
}

impl GpkgGeometry {
    fn encode(geom: &geo::Geometry, has_z: bool, has_m: bool) -> Self { ... }
    fn decode(&self) -> geo::Geometry { ... }
}
```

---

## 6. Feature Paths

Each feature is stored at a deterministic path derived from its primary key:

```rust
/// Generate the file path for a feature given its primary key
fn feature_path(pk: &[ColumnValue], path_structure: &PathStructure) -> String {
    match path_structure.scheme {
        Scheme::Int => {
            // Single integer PK — use base64 encoding of the integer directly
            let pk_int = pk[0].as_i64().unwrap();
            let dir = int_to_path(pk_int, path_structure.branches, path_structure.levels);
            let filename = urlsafe_b64(rmp_serde::to_vec(pk).unwrap());
            format!("{}/{}", dir, filename)
        }
        Scheme::MsgpackHash => {
            // Any PK type(s) — SHA-256 hash of msgpack(pk) for directory
            let pk_bytes = rmp_serde::to_vec(pk).unwrap();
            let hash = sha256(&pk_bytes);
            let dir = hash_to_path(&hash, path_structure.branches, path_structure.levels);
            let filename = urlsafe_b64(pk_bytes);
            format!("{}/{}", dir, filename)
        }
    }
}
```

---

## 7. Schema Evolution via Legends

Kart's legend system allows reading old features after schema changes without rewriting them:

```rust
struct Schema {
    columns: Vec<Column>,
}

struct Column {
    id: Uuid,        // Stable across renames
    name: String,
    data_type: DataType,
    primary_key_index: Option<u32>,
    // Extra type info (size, length, precision, geometryCRS, etc.)
    extra: HashMap<String, serde_json::Value>,
}

struct Legend {
    /// The ordered list of column IDs at the time of writing
    column_ids: Vec<Uuid>,
}

impl Legend {
    fn hash(&self) -> String {
        let bytes = rmp_serde::to_vec(&self.column_ids).unwrap();
        hex::encode(sha256(&bytes))
    }
}

/// Decode a feature using legend + current schema
fn decode_feature(
    stored: &StoredFeature,
    legend: &Legend,
    current_schema: &Schema,
) -> HashMap<String, ColumnValue> {
    // 1. Zip legend column_ids with stored values
    let id_values: HashMap<Uuid, &ColumnValue> =
        legend.column_ids.iter().zip(&stored.values).collect();

    // 2. Map to current schema column names
    let mut row = HashMap::new();
    for col in &current_schema.columns {
        match id_values.get(&col.id) {
            Some(val) => row.insert(col.name.clone(), (*val).clone()),
            None => row.insert(col.name.clone(), ColumnValue::Null), // New column
        };
    }
    // Columns in legend but not in current schema are silently dropped
    row
}
```

---

## 8. Diff Engine

Row-level diff by walking two Git trees:

```rust
enum FeatureDelta {
    Insert { pk: Vec<ColumnValue>, new: HashMap<String, ColumnValue> },
    Delete { pk: Vec<ColumnValue>, old: HashMap<String, ColumnValue> },
    Update {
        pk: Vec<ColumnValue>,
        old: HashMap<String, ColumnValue>,
        new: HashMap<String, ColumnValue>,
        changed_columns: Vec<String>,
    },
}

fn diff_datasets(
    repo: &gix::Repository,
    old_commit: gix::ObjectId,
    new_commit: gix::ObjectId,
    dataset_path: &str,
) -> Vec<FeatureDelta> {
    let old_tree = repo.find_commit(old_commit).tree();
    let new_tree = repo.find_commit(new_commit).tree();

    let old_features = old_tree.lookup(format!("{dataset_path}/.table-dataset/feature"));
    let new_features = new_tree.lookup(format!("{dataset_path}/.table-dataset/feature"));

    // Walk both trees, comparing blob OIDs
    // Same OID = unchanged (skip), different OID = update, missing = insert/delete
    // This is O(n) where n = number of changed features, not total features
    diff_trees(old_features, new_features)
}
```

The critical optimization: **Git blob OID comparison**. If two features have the same SHA, they're identical — skip them. This makes diff O(changed) not O(total).

---

## 9. Three-Way Merge

```rust
struct MergeConflict {
    pk: Vec<ColumnValue>,
    ancestor: Option<HashMap<String, ColumnValue>>,
    ours: Option<HashMap<String, ColumnValue>>,
    theirs: Option<HashMap<String, ColumnValue>>,
    conflict_type: ConflictType,
}

enum ConflictType {
    BothModified,           // Same feature edited differently
    ModifyDelete,           // One side modified, other deleted
    BothAdded,              // Same PK added with different values
    SchemaConflict,         // Incompatible schema changes
}

fn merge(
    repo: &gix::Repository,
    ours: gix::ObjectId,
    theirs: gix::ObjectId,
) -> Result<MergeResult> {
    let base = find_merge_base(repo, ours, theirs)?;

    let diff_ours = diff_datasets(repo, base, ours, ...);
    let diff_theirs = diff_datasets(repo, base, theirs, ...);

    let mut resolved = Vec::new();
    let mut conflicts = Vec::new();

    // Group deltas by PK
    // If only one side changed a feature → auto-resolve
    // If both sides made identical changes → auto-resolve
    // If both sides changed different columns → auto-merge columns
    // If both sides changed same column differently → conflict

    // Schema merge: union of added columns, conflict on incompatible type changes

    MergeResult { resolved, conflicts }
}
```

---

## 10. Working Copy Adapters

### GeoPackage (Primary)

```rust
trait WorkingCopy {
    /// Apply repository state to the working copy
    fn checkout(&mut self, tree: &gix::Tree, datasets: &[DatasetInfo]) -> Result<()>;

    /// Detect changes made in the working copy since last checkout
    fn diff_from_repo(&self, tree: &gix::Tree) -> Result<Vec<FeatureDelta>>;

    /// Reset working copy to match repository state
    fn reset(&mut self, tree: &gix::Tree) -> Result<()>;
}

struct GeoPackageWorkingCopy {
    conn: rusqlite::Connection,
}

impl GeoPackageWorkingCopy {
    fn new(path: &Path) -> Result<Self> {
        let conn = rusqlite::Connection::open(path)?;
        conn.load_extension("mod_spatialite")?;
        // Create tracking tables: _geokart_track (table_name, pk, change_type)
        Self::init_tracking(&conn)?;
        Ok(Self { conn })
    }
}

impl WorkingCopy for GeoPackageWorkingCopy {
    fn checkout(&mut self, tree: &gix::Tree, datasets: &[DatasetInfo]) -> Result<()> {
        // For each dataset:
        //   1. CREATE TABLE with schema from meta/schema.json
        //   2. INSERT features decoded from tree blobs
        //   3. Register geometry column with gpkg_geometry_columns
        //   4. Create spatial index (rtree)
        //   5. Install triggers on INSERT/UPDATE/DELETE that write to _geokart_track
    }

    fn diff_from_repo(&self, tree: &gix::Tree) -> Result<Vec<FeatureDelta>> {
        // Read _geokart_track to find changed PKs
        // For each changed PK, read current row from working copy
        // Compare against repo version
    }
}
```

### PostGIS Working Copy

```rust
struct PostGISWorkingCopy {
    pool: sqlx::PgPool,
    schema: String,  // PostgreSQL schema name
}

impl WorkingCopy for PostGISWorkingCopy {
    // Same interface, but uses PostgreSQL triggers for change tracking
    // and PostGIS geometry types natively
}
```

---

## 11. CLI Commands

```
geokart init [--import FORMAT:PATH] [DIR]
geokart clone URL [DIR] [--spatial-filter BBOX/WKT]
geokart import FORMAT:PATH [DATASET_NAME]
geokart export DATASET FORMAT:PATH
geokart status
geokart diff [COMMIT..COMMIT] [--output-format json|text|geojson|html]
geokart commit -m MESSAGE
geokart log [--oneline] [--graph]
geokart branch [NAME] [-d NAME]
geokart checkout BRANCH [-b]
geokart switch BRANCH [-c]
geokart merge BRANCH [--no-ff] [--ff-only]
geokart resolve DATASET --with={ours|theirs|interactive}
geokart reset [--hard|--soft]
geokart restore [PATH...]
geokart remote add|remove|list NAME URL
geokart push [REMOTE] [BRANCH]
geokart pull [REMOTE] [BRANCH]
geokart fetch [REMOTE]
geokart tag NAME [COMMIT]
geokart config [--global] KEY VALUE
geokart show COMMIT [--output-format json|text]
geokart data ls                 # list datasets
geokart data info DATASET       # show schema, row count, extent
geokart data schema DATASET     # show column definitions
geokart data version DATASET    # show dataset format version
```

Implemented via `clap` derive:

```rust
#[derive(Parser)]
#[command(name = "geokart", version, about = "Distributed version control for geodata")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Init(InitArgs),
    Clone(CloneArgs),
    Import(ImportArgs),
    Commit(CommitArgs),
    Diff(DiffArgs),
    Merge(MergeArgs),
    // ...
}
```

---

## 12. Import / Export Formats

| Format | Import | Export | Crate |
|--------|--------|--------|-------|
| GeoPackage (.gpkg) | ✅ | ✅ | `rusqlite` + SpatiaLite |
| Shapefile (.shp) | ✅ | ✅ | `shapefile` crate |
| GeoJSON (.geojson) | ✅ | ✅ | `geojson` crate |
| FlatGeobuf (.fgb) | ✅ | ✅ | `flatgeobuf` crate |
| CSV/TSV | ✅ | ✅ | `csv` crate |
| PostGIS (connection string) | ✅ | ✅ | `sqlx` |
| GeoParquet | ✅ | ✅ | `parquet` + `arrow` |
| KML | ✅ | ✅ | `quick-xml` |
| GeoTIFF (raster) | ✅ | ✅ | `gdal` bindings or `tiff` |
| LAS/LAZ (point cloud) | ✅ | ✅ | `las` crate |

---

## 13. Spatial Filtering (Partial Clone)

Clone only features within a geographic extent:

```rust
struct SpatialFilter {
    geometry: geo::Geometry,  // Bounding polygon
    crs: String,              // CRS of the filter geometry
}

impl SpatialFilter {
    /// During clone/fetch, only download features whose geometry
    /// intersects this filter. Uses:
    /// 1. Git sparse-checkout to limit tree walking
    /// 2. Server-side filtering via custom git-upload-pack extension
    /// 3. Client-side post-filter for partial matches
    fn apply(&self, feature: &DecodedFeature) -> bool {
        if let Some(geom) = feature.geometry() {
            self.geometry.intersects(&geom)
        } else {
            true // Non-spatial features always included
        }
    }
}
```

---

## 14. Performance Targets

| Operation | Kart (Python) | geokart target | Strategy |
|-----------|--------------|----------------|----------|
| `init --import` 1M features | ~45s | <10s | Rayon parallel encoding + bulk tree building |
| `diff` (1k changes in 1M dataset) | ~3s | <0.5s | Tree OID comparison, skip unchanged subtrees |
| `commit` (1k changes) | ~2s | <0.3s | Incremental tree update |
| `status` (no changes) | ~1.5s | <0.2s | Tracking table query, no tree walk |
| `checkout` (switch branch, 1M features) | ~30s | <8s | Parallel decode + bulk SQL insert |
| Startup time | ~500ms | <10ms | No interpreter, static binary |

### Performance Strategies

1. **Parallel feature encoding** — `rayon::par_iter` over features during import
2. **Batch tree construction** — build Git trees in memory, write once
3. **Memory-mapped packfiles** — `gix` uses mmap for packfile access
4. **Streaming tree walks** — don't load entire tree into memory
5. **Connection pooling** — reuse DB connections for working copy operations
6. **Skip unchanged subtrees** — compare tree OIDs, not individual blobs
7. **Lazy feature decoding** — only decode features that are needed (e.g., diff only decodes changed ones)

---

## 15. Development Phases

### Phase 1: Foundation (Weeks 1-4)

- [ ] Workspace setup, CI, basic CLI skeleton
- [ ] `geokart-encoding`: MessagePack feature encoding/decoding (Kart-compatible)
- [ ] `geokart-encoding`: GeoPackage binary geometry encoding
- [ ] `geokart-encoding`: Feature path generation (int + msgpack/hash schemes)
- [ ] `geokart-git`: Repository init/open via `gix`
- [ ] `geokart-git`: Tree building + blob writing
- [ ] `geokart-core`: Schema + Legend types
- [ ] `geokart-core`: TableDatasetV3 read/write
- [ ] Tests: round-trip encode/decode features, verify Kart byte-compatibility

### Phase 2: Core Operations (Weeks 5-8)

- [ ] `geokart init` — create empty repo
- [ ] `geokart import GPKG:path` — import GeoPackage into repo
- [ ] `geokart-wc`: GeoPackage working copy with change tracking triggers
- [ ] `geokart checkout` — populate working copy from repo
- [ ] `geokart status` — detect working copy changes
- [ ] `geokart commit` — commit working copy changes
- [ ] `geokart log` — show commit history
- [ ] `geokart diff` — row-level diff between commits

### Phase 3: Branching & Merging (Weeks 9-12)

- [ ] `geokart branch` — create/list/delete branches
- [ ] `geokart switch` — switch branches, update working copy
- [ ] Three-way merge engine (row-level + column-level)
- [ ] `geokart merge` — merge branches
- [ ] Conflict detection and resolution (`geokart resolve`)
- [ ] Schema merge (add columns, type changes)

### Phase 4: Remotes & Collaboration (Weeks 13-16)

- [ ] `geokart-git`: Remote operations (push/pull/fetch/clone)
- [ ] `geokart clone URL`
- [ ] `geokart push` / `geokart pull`
- [ ] Git LFS integration for raster/point cloud tiles
- [ ] SSH + HTTPS authentication

### Phase 5: Advanced Features (Weeks 17-24)

- [ ] Spatial filtering (partial clone by bbox/polygon)
- [ ] PostGIS working copy adapter
- [ ] Additional import formats (Shapefile, GeoJSON, FlatGeobuf, GeoParquet)
- [ ] Raster dataset support (tile-level versioning)
- [ ] Point cloud dataset support (LAS/LAZ files)
- [ ] `geokart diff --output-format geojson` (visual diffs)
- [ ] Performance optimization: parallel import, streaming trees

### Phase 6: Polish & Ecosystem (Weeks 25-30)

- [ ] QGIS plugin (expose as WFS-T or direct GeoPackage)
- [ ] Web UI for browsing history and diffs
- [ ] GitHub/GitLab rendering integration
- [ ] Tab completion (clap_complete)
- [ ] Man pages
- [ ] Homebrew / apt / rpm packaging
- [ ] Kart repo compatibility (read existing Kart repositories)

---

## 16. Kart Compatibility Strategy

To read existing Kart repositories:

1. **Same Git structure** — `.table-dataset/meta/` and `.table-dataset/feature/` layout
2. **Same MessagePack encoding** — use `rmp-serde` with Kart's exact serialization format
3. **Same geometry encoding** — GeoPackage Binary with Kart's restrictions (LE only, srs_id=0)
4. **Same path structure** — `path-structure.json` with int/msgpack-hash schemes
5. **Same legend format** — MessagePack-encoded column ID arrays
6. **Same CRS storage** — WKT files in `meta/crs/`

Test: clone a Kart repo, run `geokart diff`, verify identical output.

---

## 17. Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Git library | `gix` (gitoxide) | Pure Rust, no C deps, actively maintained, used by `cargo` |
| Feature encoding | MessagePack (Kart-compatible) | Interop with existing Kart repos |
| Working copy change tracking | SQL triggers | Same approach as Kart, works with any GIS editor |
| Geometry library | `geo` + custom GpkgBinary | `geo` for operations, custom for storage encoding |
| Error handling | `anyhow` (CLI) + `thiserror` (libraries) | Standard Rust pattern |
| Async | No (sync + Rayon) | Git ops are CPU/IO bound, async adds complexity |
| Config | Git-compatible | `~/.gitconfig` + `.git/config`, same keys |

---

## 18. Risk Mitigation

| Risk | Mitigation |
|------|------------|
| `gix` API instability | Pin versions, wrap in abstraction layer |
| MessagePack edge cases | Extensive round-trip tests against Kart-generated data |
| GeoPackage trigger complexity | Port Kart's trigger SQL directly, test with QGIS/ArcGIS |
| Large dataset memory pressure | Streaming APIs, never load full dataset in memory |
| CRS transformation accuracy | Use `proj` crate (same PROJ library as GDAL) |
| Git LFS complexity | Start without LFS, add for raster/point cloud later |

---

## 19. Success Metrics

1. **Import 1M features from GeoPackage in <10 seconds**
2. **Diff 1k changes in 1M feature dataset in <0.5 seconds**
3. **Single binary <20MB, no runtime dependencies**
4. **100% compatible with Kart repositories (read + write)**
5. **Edit working copy in QGIS → commit → push → pull on another machine**
6. **All standard Git hosting works (GitHub, GitLab, Bitbucket)**
