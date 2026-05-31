// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

use clap::{Parser, Subcommand};
use ptolemy_core::branch::Branch;
use ptolemy_core::dataset::{Dataset, GeometryType};
use ptolemy_core::diff::DiffOp;
use ptolemy_storage::PgStore;
use serde_json::json;
use std::sync::Arc;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "ptolemy",
    about = "Versioned geodatabase & collaboration platform"
)]
struct Cli {
    /// PostgreSQL connection URL
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Maximum database connections in the pool
    #[arg(long, env = "PTOLEMY_DB_MAX_CONNECTIONS", default_value = "10")]
    db_max_connections: u32,

    /// Minimum database connections in the pool
    #[arg(long, env = "PTOLEMY_DB_MIN_CONNECTIONS", default_value = "2")]
    db_min_connections: u32,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the API server
    Serve {
        /// Listen address
        #[arg(long, default_value = "0.0.0.0:3000")]
        bind: String,
    },

    /// Run database migrations
    Migrate,

    /// Dataset management
    Dataset {
        #[command(subcommand)]
        cmd: DatasetCmd,
    },

    /// Branch management
    Branch {
        #[command(subcommand)]
        cmd: BranchCmd,
    },

    /// Commit changes to a branch
    Commit {
        /// Branch ID
        #[arg(long)]
        branch: Uuid,
        /// Commit message
        #[arg(long, short)]
        message: String,
        /// Author name
        #[arg(long)]
        author: String,
        /// GeoJSON file to import as inserts (optional)
        #[arg(long)]
        geojson: Option<String>,
    },

    /// Merge a source branch into a target branch
    Merge {
        /// Source branch ID
        #[arg(long)]
        source: Uuid,
        /// Target branch ID
        #[arg(long)]
        target: Uuid,
        /// Author name
        #[arg(long)]
        author: String,
    },

    /// Show commit history for a branch
    Log {
        /// Branch ID
        #[arg(long)]
        branch: Uuid,
        /// Max number of entries
        #[arg(long, default_value = "20")]
        limit: i64,
    },

    /// List features on a branch
    Features {
        /// Branch ID
        #[arg(long)]
        branch: Uuid,
    },

    /// Show diff between two changesets
    Diff {
        /// From changeset ID
        #[arg(long)]
        from: Uuid,
        /// To changeset ID
        #[arg(long)]
        to: Uuid,
    },

    /// Import geospatial file (auto-detects GeoJSON, Shapefile, GeoPackage)
    Import {
        /// Branch ID
        #[arg(long)]
        branch: Uuid,
        /// Path to file (.geojson, .json, .shp, .gpkg)
        file: String,
        /// Author name
        #[arg(long)]
        author: String,
        /// Commit message
        #[arg(long, short, default_value = "Import features")]
        message: String,
    },

    /// Export features as GeoJSON
    Export {
        /// Branch ID
        #[arg(long)]
        branch: Uuid,
        /// Output file (stdout if omitted)
        #[arg(long, short)]
        output: Option<String>,
    },

    /// Export branch as GeoPackage (.gpkg) for offline editing
    GpkgExport {
        /// Branch ID
        #[arg(long)]
        branch: Uuid,
        /// Output .gpkg file path
        #[arg(long, short)]
        output: String,
        /// Layer name in the GeoPackage
        #[arg(long, default_value = "features")]
        layer: String,
    },

    /// Backup the database to a file
    Backup {
        /// Output file path (.sql or .dump)
        output: String,
        /// Use custom format (restorable with `ptolemy restore`)
        #[arg(long)]
        custom: bool,
    },

    /// Restore a database from a backup file
    Restore {
        /// Backup file path
        input: String,
        /// Drop existing tables before restore
        #[arg(long)]
        clean: bool,
    },

    /// Generate an API key for programmatic access
    ApiKey {
        #[command(subcommand)]
        cmd: ApiKeyCmd,
    },
}

#[derive(Subcommand)]
enum ApiKeyCmd {
    /// Generate a new API key
    Create {
        /// Key name/description
        name: String,
        /// Role: admin, editor, or viewer
        #[arg(long, default_value = "viewer")]
        role: String,
        /// Expiry in days (0 = never)
        #[arg(long, default_value = "365")]
        expires_days: u64,
    },
    /// List active API keys
    List,
    /// Revoke an API key
    Revoke {
        /// Key prefix or full key to revoke
        key: String,
    },
}

#[derive(Subcommand)]
enum DatasetCmd {
    /// Create a new dataset
    Create {
        /// Dataset name
        name: String,
        /// SRID (default 4326)
        #[arg(long, default_value = "4326")]
        srid: i32,
        /// Geometry type
        #[arg(long, default_value = "point")]
        geometry_type: String,
        /// Creator name
        #[arg(long)]
        created_by: String,
    },
    /// List all datasets
    List,
    /// Show dataset info
    Show { id: Uuid },
}

#[derive(Subcommand)]
enum BranchCmd {
    /// Create a new branch
    Create {
        /// Dataset ID
        #[arg(long)]
        dataset: Uuid,
        /// Branch name
        name: String,
        /// Fork from this branch ID (copies head)
        #[arg(long)]
        fork_from: Option<Uuid>,
        /// Creator name
        #[arg(long)]
        created_by: String,
    },
    /// List branches for a dataset
    List {
        /// Dataset ID
        #[arg(long)]
        dataset: Uuid,
    },
    /// Show branch info
    Show { id: Uuid },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(cli.db_max_connections)
        .min_connections(cli.db_min_connections)
        .connect(&cli.database_url)
        .await?;
    let store = Arc::new(PgStore::new(pool));

    match cli.command {
        Commands::Serve { bind } => {
            let app = ptolemy_api::app(store.clone());
            let listener = tokio::net::TcpListener::bind(&bind).await?;
            tracing::info!("Ptolemy listening on {bind}");
            tracing::info!("Metrics available at http://{bind}/metrics");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
            tracing::info!("Server shut down gracefully");
        }

        Commands::Migrate => {
            store.migrate().await?;
            println!("Migrations applied successfully.");
        }

        Commands::Dataset { cmd } => match cmd {
            DatasetCmd::Create {
                name,
                srid,
                geometry_type,
                created_by,
            } => {
                let ds = Dataset {
                    id: Uuid::now_v7(),
                    name: name.clone(),
                    srid,
                    geometry_type: parse_geom_type(&geometry_type),
                    created_at: OffsetDateTime::now_utc(),
                    created_by,
                };
                store.create_dataset(&ds).await?;
                println!("Created dataset '{}' ({})", name, ds.id);
            }
            DatasetCmd::List => {
                let datasets = store.list_datasets().await?;
                for ds in datasets {
                    println!(
                        "{} | {} | srid={} | {}",
                        ds.id, ds.name, ds.srid, ds.created_by
                    );
                }
            }
            DatasetCmd::Show { id } => {
                let ds = store.get_dataset(id).await?;
                println!("{}", serde_json::to_string_pretty(&ds)?);
            }
        },

        Commands::Branch { cmd } => match cmd {
            BranchCmd::Create {
                dataset,
                name,
                fork_from,
                created_by,
            } => {
                let head = if let Some(src_id) = fork_from {
                    let src = store.get_branch(src_id).await?;
                    src.head
                } else {
                    None
                };
                let branch = Branch {
                    id: Uuid::now_v7(),
                    dataset_id: dataset,
                    name: name.clone(),
                    head,
                    created_at: OffsetDateTime::now_utc(),
                    created_by,
                };
                store.create_branch(&branch).await?;
                println!("Created branch '{}' ({})", name, branch.id);
            }
            BranchCmd::List { dataset } => {
                let branches = store.list_branches(dataset).await?;
                for b in branches {
                    let head_str = b
                        .head
                        .map(|h| h.to_string())
                        .unwrap_or_else(|| "(empty)".to_string());
                    println!("{} | {} | head={}", b.id, b.name, head_str);
                }
            }
            BranchCmd::Show { id } => {
                let b = store.get_branch(id).await?;
                println!("{}", serde_json::to_string_pretty(&b)?);
            }
        },

        Commands::Commit {
            branch,
            message,
            author,
            geojson,
        } => {
            let ops = if let Some(path) = geojson {
                parse_geojson_to_ops(&std::fs::read_to_string(&path)?)?
            } else {
                vec![]
            };
            let changeset = store.commit(branch, &message, &author, &ops).await?;
            println!("Committed {} ({} operations)", changeset.id, ops.len());
        }

        Commands::Merge {
            source,
            target,
            author,
        } => {
            let result = store.merge(source, target, &author).await?;
            match result {
                ptolemy_storage::MergeResult::Success(cs) => {
                    println!("Merge successful: {}", cs.id);
                }
                ptolemy_storage::MergeResult::Conflicts(conflicts) => {
                    println!("Merge has {} conflict(s):", conflicts.len());
                    for c in &conflicts {
                        println!("  - feature {} conflict", c.feature_id);
                    }
                    std::process::exit(1);
                }
            }
        }

        Commands::Log { branch, limit } => {
            let history = store.get_branch_history(branch, limit).await?;
            for cs in history {
                let parent = cs
                    .parent_id
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "(root)".to_string());
                println!(
                    "{} | {} | {} | parent={}",
                    cs.id, cs.author, cs.message, parent
                );
            }
        }

        Commands::Features { branch } => {
            let features = store.list_features_at_head(branch).await?;
            // Output as GeoJSON FeatureCollection
            let fc = features_to_geojson(&features);
            println!("{}", serde_json::to_string_pretty(&fc)?);
        }

        Commands::Diff { from, to } => {
            let diff = store.diff(Some(from), to).await?;
            println!("{}", serde_json::to_string_pretty(&diff)?);
        }

        Commands::Import {
            branch,
            file,
            author,
            message,
        } => {
            let path = std::path::Path::new(&file);
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();

            let ops = match ext.as_str() {
                "shp" => parse_shapefile_to_ops(path)?,
                "gpkg" => parse_geopackage_to_ops(path)?,
                _ => {
                    let content = std::fs::read_to_string(&file)?;
                    parse_geojson_to_ops(&content)?
                }
            };

            let count = ops.len();
            let changeset = store.commit(branch, &message, &author, &ops).await?;
            println!("Imported {} features as changeset {}", count, changeset.id);
        }

        Commands::Export { branch, output } => {
            let features = store.list_features_at_head(branch).await?;
            let fc = features_to_geojson(&features);
            let json = serde_json::to_string_pretty(&fc)?;
            if let Some(path) = output {
                std::fs::write(&path, &json)?;
                println!("Exported {} features to {}", features.len(), path);
            } else {
                println!("{json}");
            }
        }

        Commands::GpkgExport {
            branch,
            output,
            layer,
        } => {
            let features = store.list_features_at_head(branch).await?;
            export_geopackage(&features, &output, &layer)?;
            println!(
                "Exported {} features to GeoPackage: {}",
                features.len(),
                output
            );
        }

        Commands::Backup { output, custom } => {
            let format_flag = if custom { "-Fc" } else { "-Fp" };
            let status = std::process::Command::new("pg_dump")
                .args([format_flag, "-f", &output, &cli.database_url])
                .status()?;
            if status.success() {
                println!("Backup written to {output}");
            } else {
                anyhow::bail!("pg_dump failed with exit code: {:?}", status.code());
            }
        }

        Commands::Restore { input, clean } => {
            let path = std::path::Path::new(&input);
            // Detect format by trying pg_restore first (custom format), fall back to psql (plain SQL)
            let status = if clean {
                std::process::Command::new("pg_restore")
                    .args(["--clean", "--if-exists", "-d", &cli.database_url, &input])
                    .status()
            } else {
                std::process::Command::new("pg_restore")
                    .args(["-d", &cli.database_url, &input])
                    .status()
            };

            match status {
                Ok(s) if s.success() => println!("Restore complete from {input}"),
                _ => {
                    // Fall back to psql for plain SQL files
                    let content = std::fs::read_to_string(path)?;
                    sqlx::raw_sql(&content).execute(store.pool()).await?;
                    println!("Restore complete from {input} (plain SQL)");
                }
            }
        }

        Commands::ApiKey { cmd } => match cmd {
            ApiKeyCmd::Create {
                name,
                role,
                expires_days,
            } => {
                let key = generate_api_key();
                let key_hash = hash_api_key(&key);
                let role_enum = match role.as_str() {
                    "admin" => "admin",
                    "editor" => "editor",
                    _ => "viewer",
                };
                let expires_at = if expires_days > 0 {
                    Some(OffsetDateTime::now_utc() + time::Duration::days(expires_days as i64))
                } else {
                    None
                };
                sqlx::query(
                    "INSERT INTO api_keys (id, name, key_hash, key_prefix, role, expires_at, created_at)
                     VALUES ($1, $2, $3, $4, $5, $6, NOW())",
                )
                .bind(Uuid::now_v7())
                .bind(&name)
                .bind(&key_hash)
                .bind(&key[..8])
                .bind(role_enum)
                .bind(expires_at)
                .execute(store.pool())
                .await?;
                println!("API Key created (save this — it won't be shown again):");
                println!("  Key:  {key}");
                println!("  Name: {name}");
                println!("  Role: {role_enum}");
            }
            ApiKeyCmd::List => {
                let rows = sqlx::query_as::<_, (String, String, String, Option<time::OffsetDateTime>)>(
                    "SELECT key_prefix, name, role, expires_at FROM api_keys WHERE revoked_at IS NULL ORDER BY created_at DESC",
                )
                .fetch_all(store.pool())
                .await?;
                println!(
                    "{:<10} {:<20} {:<8} EXPIRES",
                    "PREFIX", "NAME", "ROLE"
                );
                for (prefix, name, role, expires) in rows {
                    let exp = expires
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "never".into());
                    println!("{:<10} {:<20} {:<8} {}", prefix, name, role, exp);
                }
            }
            ApiKeyCmd::Revoke { key } => {
                let result = sqlx::query(
                    "UPDATE api_keys SET revoked_at = NOW() WHERE key_prefix = $1 OR key_hash = $2",
                )
                .bind(&key)
                .bind(hash_api_key(&key))
                .execute(store.pool())
                .await?;
                if result.rows_affected() > 0 {
                    println!("API key revoked.");
                } else {
                    println!("No matching API key found.");
                }
            }
        },
    }

    Ok(())
}

fn parse_geom_type(s: &str) -> GeometryType {
    match s {
        "point" => GeometryType::Point,
        "linestring" => GeometryType::LineString,
        "polygon" => GeometryType::Polygon,
        "multipoint" => GeometryType::MultiPoint,
        "multilinestring" => GeometryType::MultiLineString,
        "multipolygon" => GeometryType::MultiPolygon,
        _ => GeometryType::Point,
    }
}

/// Parse a GeoJSON FeatureCollection into DiffOps (inserts).
fn parse_geojson_to_ops(content: &str) -> anyhow::Result<Vec<DiffOp>> {
    let v: serde_json::Value = serde_json::from_str(content)?;
    let features = v["features"].as_array().ok_or_else(|| {
        anyhow::anyhow!("Expected GeoJSON FeatureCollection with 'features' array")
    })?;

    let mut ops = Vec::with_capacity(features.len());
    for f in features {
        let geometry = &f["geometry"];
        let properties = f.get("properties").cloned().unwrap_or(json!({}));
        // Encode geometry as simple WKB point if it's a point, otherwise store raw JSON in properties
        let wkb = geojson_geometry_to_wkb(geometry)?;
        ops.push(DiffOp::Insert {
            feature_id: Uuid::now_v7(),
            geometry_wkb: wkb,
            properties,
        });
    }
    Ok(ops)
}

/// Convert a GeoJSON geometry object to WKB (supports Point, LineString, Polygon).
fn geojson_geometry_to_wkb(geom: &serde_json::Value) -> anyhow::Result<Vec<u8>> {
    if geom.is_null() {
        return Ok(point_wkb(0.0, 0.0));
    }
    let geom_type = geom["type"].as_str().unwrap_or("Point");
    match geom_type {
        "Point" => {
            let coords = geom["coordinates"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("Point missing coordinates"))?;
            let x = coords.first().and_then(|v| v.as_f64()).unwrap_or(0.0);
            let y = coords.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
            Ok(point_wkb(x, y))
        }
        "LineString" => {
            let coords = geom["coordinates"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("LineString missing coordinates"))?;
            Ok(linestring_wkb(coords))
        }
        "Polygon" => {
            let rings = geom["coordinates"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("Polygon missing coordinates"))?;
            Ok(polygon_wkb(rings))
        }
        _ => {
            // Fallback: store as point at 0,0 with geometry in properties
            Ok(point_wkb(0.0, 0.0))
        }
    }
}

fn point_wkb(x: f64, y: f64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(21);
    buf.push(0x01); // little-endian
    buf.extend_from_slice(&1u32.to_le_bytes()); // WKB type: Point
    buf.extend_from_slice(&x.to_le_bytes());
    buf.extend_from_slice(&y.to_le_bytes());
    buf
}

fn linestring_wkb(coords: &[serde_json::Value]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(0x01); // little-endian
    buf.extend_from_slice(&2u32.to_le_bytes()); // WKB type: LineString
    buf.extend_from_slice(&(coords.len() as u32).to_le_bytes());
    for coord in coords {
        if let Some(arr) = coord.as_array() {
            let x = arr.first().and_then(|v| v.as_f64()).unwrap_or(0.0);
            let y = arr.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
            buf.extend_from_slice(&x.to_le_bytes());
            buf.extend_from_slice(&y.to_le_bytes());
        }
    }
    buf
}

fn polygon_wkb(rings: &[serde_json::Value]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(0x01); // little-endian
    buf.extend_from_slice(&3u32.to_le_bytes()); // WKB type: Polygon
    buf.extend_from_slice(&(rings.len() as u32).to_le_bytes());
    for ring in rings {
        if let Some(coords) = ring.as_array() {
            buf.extend_from_slice(&(coords.len() as u32).to_le_bytes());
            for coord in coords {
                if let Some(arr) = coord.as_array() {
                    let x = arr.first().and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let y = arr.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
                    buf.extend_from_slice(&x.to_le_bytes());
                    buf.extend_from_slice(&y.to_le_bytes());
                }
            }
        }
    }
    buf
}

fn features_to_geojson(features: &[ptolemy_core::Feature]) -> serde_json::Value {
    let geojson_features: Vec<serde_json::Value> = features
        .iter()
        .map(|f| {
            let geometry = wkb_to_geojson_geometry(&f.geometry_wkb);
            json!({
                "type": "Feature",
                "id": f.id.to_string(),
                "geometry": geometry,
                "properties": f.properties,
            })
        })
        .collect();

    json!({
        "type": "FeatureCollection",
        "features": geojson_features,
    })
}

/// Convert WKB back to GeoJSON geometry (supports Point, LineString, Polygon).
fn wkb_to_geojson_geometry(wkb: &[u8]) -> serde_json::Value {
    if wkb.len() < 5 {
        return json!({"type": "Point", "coordinates": [0, 0]});
    }
    // Skip byte order byte, read type
    let wkb_type = u32::from_le_bytes([wkb[1], wkb[2], wkb[3], wkb[4]]);
    match wkb_type {
        1 => {
            // Point
            if wkb.len() >= 21 {
                let x = f64::from_le_bytes(wkb[5..13].try_into().unwrap());
                let y = f64::from_le_bytes(wkb[13..21].try_into().unwrap());
                json!({"type": "Point", "coordinates": [x, y]})
            } else {
                json!({"type": "Point", "coordinates": [0, 0]})
            }
        }
        2 => {
            // LineString
            if wkb.len() >= 9 {
                let n = u32::from_le_bytes(wkb[5..9].try_into().unwrap()) as usize;
                let mut coords = Vec::with_capacity(n);
                for i in 0..n {
                    let offset = 9 + i * 16;
                    if offset + 16 <= wkb.len() {
                        let x = f64::from_le_bytes(wkb[offset..offset + 8].try_into().unwrap());
                        let y =
                            f64::from_le_bytes(wkb[offset + 8..offset + 16].try_into().unwrap());
                        coords.push(json!([x, y]));
                    }
                }
                json!({"type": "LineString", "coordinates": coords})
            } else {
                json!({"type": "LineString", "coordinates": []})
            }
        }
        3 => {
            // Polygon
            if wkb.len() >= 9 {
                let num_rings = u32::from_le_bytes(wkb[5..9].try_into().unwrap()) as usize;
                let mut rings = Vec::with_capacity(num_rings);
                let mut offset = 9;
                for _ in 0..num_rings {
                    if offset + 4 > wkb.len() {
                        break;
                    }
                    let n =
                        u32::from_le_bytes(wkb[offset..offset + 4].try_into().unwrap()) as usize;
                    offset += 4;
                    let mut coords = Vec::with_capacity(n);
                    for _ in 0..n {
                        if offset + 16 <= wkb.len() {
                            let x = f64::from_le_bytes(wkb[offset..offset + 8].try_into().unwrap());
                            let y = f64::from_le_bytes(
                                wkb[offset + 8..offset + 16].try_into().unwrap(),
                            );
                            coords.push(json!([x, y]));
                            offset += 16;
                        }
                    }
                    rings.push(coords);
                }
                json!({"type": "Polygon", "coordinates": rings})
            } else {
                json!({"type": "Polygon", "coordinates": []})
            }
        }
        _ => json!({"type": "Point", "coordinates": [0, 0]}),
    }
}

/// Import features from a Shapefile (.shp).
/// Reads the .shp and its companion .dbf for attributes.
fn parse_shapefile_to_ops(path: &std::path::Path) -> anyhow::Result<Vec<DiffOp>> {
    use shapefile::dbase::FieldValue;
    let reader =
        shapefile::read(path).map_err(|e| anyhow::anyhow!("Failed to read shapefile: {e}"))?;

    let mut ops = Vec::new();
    for (shape, record) in reader {
        let wkb = shape_to_wkb(&shape);
        let mut properties = serde_json::Map::new();
        for (name, value) in record.into_iter() {
            let json_val = match value {
                FieldValue::Character(Some(s)) => serde_json::Value::String(s),
                FieldValue::Numeric(Some(n)) => {
                    serde_json::Value::Number(serde_json::Number::from_f64(n).unwrap_or(0.into()))
                }
                FieldValue::Float(Some(f)) => serde_json::Value::Number(
                    serde_json::Number::from_f64(f as f64).unwrap_or(0.into()),
                ),
                FieldValue::Integer(i) => json!(i),
                FieldValue::Double(d) => {
                    serde_json::Value::Number(serde_json::Number::from_f64(d).unwrap_or(0.into()))
                }
                FieldValue::Logical(Some(b)) => serde_json::Value::Bool(b),
                _ => serde_json::Value::Null,
            };
            properties.insert(name, json_val);
        }

        ops.push(DiffOp::Insert {
            feature_id: Uuid::now_v7(),
            geometry_wkb: wkb,
            properties: serde_json::Value::Object(properties),
        });
    }
    Ok(ops)
}

/// Convert a shapefile Shape to WKB.
fn shape_to_wkb(shape: &shapefile::Shape) -> Vec<u8> {
    match shape {
        shapefile::Shape::Point(p) => point_wkb(p.x, p.y),
        shapefile::Shape::PointZ(p) => point_wkb(p.x, p.y),
        shapefile::Shape::PointM(p) => point_wkb(p.x, p.y),
        shapefile::Shape::Polyline(pl) => {
            if let Some(part) = pl.parts().first() {
                let coords: Vec<serde_json::Value> =
                    part.iter().map(|p| json!([p.x, p.y])).collect();
                linestring_wkb(&coords)
            } else {
                point_wkb(0.0, 0.0)
            }
        }
        shapefile::Shape::PolylineZ(pl) => {
            if let Some(part) = pl.parts().first() {
                let coords: Vec<serde_json::Value> =
                    part.iter().map(|p| json!([p.x, p.y])).collect();
                linestring_wkb(&coords)
            } else {
                point_wkb(0.0, 0.0)
            }
        }
        shapefile::Shape::Polygon(pg) => {
            let rings: Vec<serde_json::Value> = pg
                .rings()
                .iter()
                .map(|ring| {
                    let points: Vec<serde_json::Value> =
                        ring.points().iter().map(|p| json!([p.x, p.y])).collect();
                    serde_json::Value::Array(points)
                })
                .collect();
            polygon_wkb(&rings)
        }
        shapefile::Shape::PolygonZ(pg) => {
            let rings: Vec<serde_json::Value> = pg
                .rings()
                .iter()
                .map(|ring| {
                    let points: Vec<serde_json::Value> =
                        ring.points().iter().map(|p| json!([p.x, p.y])).collect();
                    serde_json::Value::Array(points)
                })
                .collect();
            polygon_wkb(&rings)
        }
        _ => point_wkb(0.0, 0.0),
    }
}

/// Import features from a GeoPackage (.gpkg) file.
/// Reads the first feature table found in the GeoPackage.
fn parse_geopackage_to_ops(path: &std::path::Path) -> anyhow::Result<Vec<DiffOp>> {
    use rusqlite::Connection;

    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;

    // Find the first feature table
    let table_name: String = conn
        .query_row(
            "SELECT table_name FROM gpkg_contents WHERE data_type = 'features' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .map_err(|_| anyhow::anyhow!("No feature tables found in GeoPackage"))?;

    // Get the geometry column name
    let geom_col: String = conn
        .query_row(
            "SELECT column_name FROM gpkg_geometry_columns WHERE table_name = ?1",
            [&table_name],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "geom".to_string());

    // Read all features
    let mut stmt = conn.prepare(&format!("SELECT * FROM \"{table_name}\""))?;
    let col_count = stmt.column_count();
    let col_names: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();

    let mut ops = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let mut wkb = Vec::new();
        let mut properties = serde_json::Map::new();

        for (i, name) in col_names.iter().enumerate() {
            if name == &geom_col {
                // GeoPackage geometry: strip GP header (8 bytes) to get WKB
                if let Ok(blob) = row.get::<_, Vec<u8>>(i) {
                    if blob.len() > 8 && blob[0] == 0x47 && blob[1] == 0x50 {
                        // Standard GP header: magic(2) + version(1) + flags(1) + srs_id(4) = 8
                        let flags = blob[3];
                        let envelope_type = (flags >> 1) & 0x07;
                        let envelope_size = match envelope_type {
                            0 => 0,
                            1 => 32,
                            2 | 3 => 48,
                            4 => 64,
                            _ => 0,
                        };
                        let header_size = 8 + envelope_size;
                        if blob.len() > header_size {
                            wkb = blob[header_size..].to_vec();
                        }
                    } else {
                        wkb = blob;
                    }
                }
            } else if name == "fid" || name == "ogc_fid" {
                // Skip auto-increment primary key
            } else {
                // Try to extract as different types
                if let Ok(v) = row.get::<_, String>(i) {
                    properties.insert(name.clone(), serde_json::Value::String(v));
                } else if let Ok(v) = row.get::<_, f64>(i) {
                    if let Some(n) = serde_json::Number::from_f64(v) {
                        properties.insert(name.clone(), serde_json::Value::Number(n));
                    }
                } else if let Ok(v) = row.get::<_, i64>(i) {
                    properties.insert(name.clone(), json!(v));
                }
            }
        }

        if wkb.is_empty() {
            wkb = point_wkb(0.0, 0.0);
        }

        ops.push(DiffOp::Insert {
            feature_id: Uuid::now_v7(),
            geometry_wkb: wkb,
            properties: serde_json::Value::Object(properties),
        });
    }
    Ok(ops)
}

/// Export features to a GeoPackage (.gpkg) SQLite file.
/// Creates a minimal spec-compliant GeoPackage with the features as a layer.
fn export_geopackage(
    features: &[ptolemy_core::Feature],
    path: &str,
    layer_name: &str,
) -> anyhow::Result<()> {
    use rusqlite::Connection;

    // Remove file if exists
    if std::path::Path::new(path).exists() {
        std::fs::remove_file(path)?;
    }

    let conn = Connection::open(path)?;

    // Set GeoPackage application ID
    conn.execute_batch("PRAGMA application_id = 0x47504B47;")?; // 'GPKG'

    // Create GeoPackage metadata tables
    conn.execute_batch(
        "CREATE TABLE gpkg_spatial_ref_sys (
            srs_name TEXT NOT NULL,
            srs_id INTEGER NOT NULL PRIMARY KEY,
            organization TEXT NOT NULL,
            organization_coordsys_id INTEGER NOT NULL,
            definition TEXT NOT NULL,
            description TEXT
        );

        INSERT INTO gpkg_spatial_ref_sys VALUES
            ('WGS 84', 4326, 'EPSG', 4326,
             'GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]',
             'WGS 84 geographic coordinate system');

        CREATE TABLE gpkg_contents (
            table_name TEXT NOT NULL PRIMARY KEY,
            data_type TEXT NOT NULL,
            identifier TEXT UNIQUE,
            description TEXT DEFAULT '',
            last_change DATETIME NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
            min_x DOUBLE,
            min_y DOUBLE,
            max_x DOUBLE,
            max_y DOUBLE,
            srs_id INTEGER,
            CONSTRAINT fk_gc_r_srs_id FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys(srs_id)
        );

        CREATE TABLE gpkg_geometry_columns (
            table_name TEXT NOT NULL,
            column_name TEXT NOT NULL,
            geometry_type_name TEXT NOT NULL,
            srs_id INTEGER NOT NULL,
            z TINYINT NOT NULL,
            m TINYINT NOT NULL,
            CONSTRAINT pk_geom_cols PRIMARY KEY (table_name, column_name),
            CONSTRAINT fk_gc_tn FOREIGN KEY (table_name) REFERENCES gpkg_contents(table_name),
            CONSTRAINT fk_gc_srs FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys(srs_id)
        );"
    )?;

    // Create feature table
    conn.execute_batch(&format!(
        "CREATE TABLE \"{layer_name}\" (
            fid INTEGER PRIMARY KEY AUTOINCREMENT,
            feature_id TEXT NOT NULL,
            geom BLOB,
            properties TEXT
        );"
    ))?;

    // Register in gpkg_contents
    conn.execute(
        "INSERT INTO gpkg_contents (table_name, data_type, identifier, srs_id)
         VALUES (?1, 'features', ?1, 4326)",
        [layer_name],
    )?;

    // Register geometry column
    conn.execute(
        "INSERT INTO gpkg_geometry_columns (table_name, column_name, geometry_type_name, srs_id, z, m)
         VALUES (?1, 'geom', 'GEOMETRY', 4326, 0, 0)",
        [layer_name],
    )?;

    // Insert features
    let mut stmt = conn.prepare(&format!(
        "INSERT INTO \"{layer_name}\" (feature_id, geom, properties) VALUES (?1, ?2, ?3)"
    ))?;

    for feature in features {
        // GeoPackage uses its own binary format (GP header + WKB)
        let gpkg_geom = wkb_to_gpkg_binary(&feature.geometry_wkb);
        let props_str = serde_json::to_string(&feature.properties)?;
        stmt.execute(rusqlite::params![
            feature.id.to_string(),
            gpkg_geom,
            props_str,
        ])?;
    }

    Ok(())
}

/// Wrap WKB in a GeoPackage binary header.
/// GeoPackage Binary format: magic (2) + version (1) + flags (1) + srs_id (4) + WKB
fn wkb_to_gpkg_binary(wkb: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8 + wkb.len());
    buf.push(0x47); // 'G'
    buf.push(0x50); // 'P'
    buf.push(0x00); // version 0
    buf.push(0x01); // flags: little-endian, no envelope
    // SRS ID 4326 as little-endian i32
    buf.extend_from_slice(&4326i32.to_le_bytes());
    buf.extend_from_slice(wkb);
    buf
}

/// Wait for SIGINT (Ctrl+C) or SIGTERM for graceful shutdown.
async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to listen for Ctrl+C");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to listen for SIGTERM")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => { tracing::info!("Received Ctrl+C, shutting down..."); }
        _ = terminate => { tracing::info!("Received SIGTERM, shutting down..."); }
    }
}

/// Generate a random 32-byte API key encoded as base62.
fn generate_api_key() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let random: u128 = timestamp ^ 0xdeadbeef_cafebabe_12345678_9abcdef0;
    // Use UUID v7 for uniqueness + hex encoding for the key
    let id = Uuid::now_v7();
    format!("ptk_{}{:016x}", id.simple(), random as u64)
}

/// Hash an API key for storage (SHA-256).
fn hash_api_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_geojson_point() {
        let geojson = r#"{
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {"type": "Point", "coordinates": [-1.5, 52.0]},
                "properties": {"name": "Test"}
            }]
        }"#;
        let ops = parse_geojson_to_ops(geojson).unwrap();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            DiffOp::Insert {
                geometry_wkb,
                properties,
                ..
            } => {
                assert_eq!(geometry_wkb.len(), 21); // WKB point: 1 + 4 + 8 + 8
                assert_eq!(properties["name"], "Test");
            }
            _ => panic!("Expected Insert op"),
        }
    }

    #[test]
    fn parse_geojson_linestring() {
        let geojson = r#"{
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {"type": "LineString", "coordinates": [[0,0],[1,1],[2,2]]},
                "properties": {}
            }]
        }"#;
        let ops = parse_geojson_to_ops(geojson).unwrap();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            DiffOp::Insert { geometry_wkb, .. } => {
                // WKB LineString: 1 (byte order) + 4 (type) + 4 (num_points) + 3*16 (coords)
                assert_eq!(geometry_wkb.len(), 1 + 4 + 4 + 3 * 16);
            }
            _ => panic!("Expected Insert op"),
        }
    }

    #[test]
    fn parse_geojson_polygon() {
        let geojson = r#"{
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {"type": "Polygon", "coordinates": [[[0,0],[1,0],[1,1],[0,1],[0,0]]]},
                "properties": {"area": 1.0}
            }]
        }"#;
        let ops = parse_geojson_to_ops(geojson).unwrap();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            DiffOp::Insert { properties, .. } => {
                assert_eq!(properties["area"], 1.0);
            }
            _ => panic!("Expected Insert op"),
        }
    }

    #[test]
    fn parse_geojson_multiple_features() {
        let geojson = r#"{
            "type": "FeatureCollection",
            "features": [
                {"type": "Feature", "geometry": {"type": "Point", "coordinates": [0,0]}, "properties": {"id": 1}},
                {"type": "Feature", "geometry": {"type": "Point", "coordinates": [1,1]}, "properties": {"id": 2}},
                {"type": "Feature", "geometry": {"type": "Point", "coordinates": [2,2]}, "properties": {"id": 3}}
            ]
        }"#;
        let ops = parse_geojson_to_ops(geojson).unwrap();
        assert_eq!(ops.len(), 3);
    }

    #[test]
    fn wkb_roundtrip_point() {
        let wkb = point_wkb(-1.5, 52.0);
        let geojson = wkb_to_geojson_geometry(&wkb);
        assert_eq!(geojson["type"], "Point");
        let coords = geojson["coordinates"].as_array().unwrap();
        assert_eq!(coords[0].as_f64().unwrap(), -1.5);
        assert_eq!(coords[1].as_f64().unwrap(), 52.0);
    }

    #[test]
    fn wkb_roundtrip_linestring() {
        let coords = vec![json!([0.0, 0.0]), json!([1.0, 1.0])];
        let wkb = linestring_wkb(&coords);
        let geojson = wkb_to_geojson_geometry(&wkb);
        assert_eq!(geojson["type"], "LineString");
        let out_coords = geojson["coordinates"].as_array().unwrap();
        assert_eq!(out_coords.len(), 2);
    }

    #[test]
    fn geopackage_binary_header() {
        let wkb = point_wkb(1.0, 2.0);
        let gpkg = wkb_to_gpkg_binary(&wkb);
        assert_eq!(gpkg[0], 0x47); // 'G'
        assert_eq!(gpkg[1], 0x50); // 'P'
        assert_eq!(gpkg[2], 0x00); // version
        assert_eq!(gpkg[3], 0x01); // flags
        // SRS ID 4326 as LE i32
        let srs = i32::from_le_bytes([gpkg[4], gpkg[5], gpkg[6], gpkg[7]]);
        assert_eq!(srs, 4326);
        // Rest is WKB
        assert_eq!(&gpkg[8..], &wkb[..]);
    }

    #[test]
    fn api_key_generation() {
        let key = generate_api_key();
        assert!(key.starts_with("ptk_"));
        assert!(key.len() > 20);
    }

    #[test]
    fn api_key_hashing() {
        let key = "ptk_test_key_12345";
        let hash1 = hash_api_key(key);
        let hash2 = hash_api_key(key);
        assert_eq!(hash1, hash2); // deterministic
        assert_ne!(hash1, hash_api_key("different_key")); // different keys → different hashes
        assert_eq!(hash1.len(), 64); // SHA-256 hex = 64 chars
    }

    #[test]
    fn geojson_to_wkb_handles_missing_geometry() {
        let geom = json!(null);
        // Should not panic
        let result = geojson_geometry_to_wkb(&geom);
        assert!(result.is_ok());
    }

    #[test]
    fn geopackage_import_nonexistent_file() {
        let result = parse_geopackage_to_ops(std::path::Path::new("/nonexistent.gpkg"));
        assert!(result.is_err());
    }

    #[test]
    fn shapefile_import_nonexistent_file() {
        let result = parse_shapefile_to_ops(std::path::Path::new("/nonexistent.shp"));
        assert!(result.is_err());
    }
}
