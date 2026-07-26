// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! External datasets: read-only views over PostGIS tables ptolemy does not own.
//!
//! A relation or column name cannot be a bind parameter, so the only barrier
//! between a registration request and the SQL we run is [`ExternalTable::parse`]:
//! every part must be an unquoted-safe ASCII identifier of at most 63 bytes.
//! Because no accepted name can contain a double quote, quoting each part is
//! then exact. The type has private fields and validates on deserialize, so an
//! `ExternalTable` value anywhere in the process has already passed that check.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// NAMEDATALEN - 1: what Postgres stores for an identifier.
const MAX_IDENT_LEN: usize = 63;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExternalError {
    #[error(
        "{field} '{value}' is not a valid postgres identifier (expected [A-Za-z_][A-Za-z0-9_]* up to 63 bytes)"
    )]
    InvalidIdentifier { field: &'static str, value: String },
    #[error("external_table must be 'table' or 'schema.table'")]
    BadRelationForm,
}

/// A validated relation plus the two columns ptolemy needs from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawExternalTable")]
pub struct ExternalTable {
    table: String,
    id_column: String,
    geometry_column: String,
}

#[derive(Deserialize)]
struct RawExternalTable {
    table: String,
    id_column: String,
    geometry_column: String,
}

impl TryFrom<RawExternalTable> for ExternalTable {
    type Error = ExternalError;
    fn try_from(raw: RawExternalTable) -> Result<Self, Self::Error> {
        ExternalTable::parse(&raw.table, &raw.id_column, &raw.geometry_column)
    }
}

fn check_ident(field: &'static str, value: &str) -> Result<(), ExternalError> {
    let ok = !value.is_empty()
        && value.len() <= MAX_IDENT_LEN
        && value.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(ExternalError::InvalidIdentifier {
            field,
            value: value.to_string(),
        })
    }
}

/// Safe only for names that passed [`check_ident`]: they cannot contain the
/// quote character, so there is nothing to escape.
fn quote_ident(value: &str) -> String {
    format!("\"{value}\"")
}

/// Escape a validated identifier used as a SQL string literal (a jsonb key).
/// Also quote-free by construction; the wrapper keeps that intent visible.
fn literal(value: &str) -> String {
    format!("'{value}'")
}

impl ExternalTable {
    pub fn parse(
        table: &str,
        id_column: &str,
        geometry_column: &str,
    ) -> Result<Self, ExternalError> {
        let parts: Vec<&str> = table.split('.').collect();
        if parts.is_empty() || parts.len() > 2 {
            return Err(ExternalError::BadRelationForm);
        }
        for part in &parts {
            check_ident("external_table", part)?;
        }
        check_ident("external_id_column", id_column)?;
        check_ident("external_geometry_column", geometry_column)?;
        Ok(Self {
            table: table.to_string(),
            id_column: id_column.to_string(),
            geometry_column: geometry_column.to_string(),
        })
    }

    pub fn table(&self) -> &str {
        &self.table
    }

    pub fn id_column(&self) -> &str {
        &self.id_column
    }

    pub fn geometry_column(&self) -> &str {
        &self.geometry_column
    }

    /// `"schema"."table"`, usable both in a FROM clause and as the text a
    /// `::regclass` cast resolves, so the probe checks the same relation the
    /// reads will hit.
    pub fn quoted_relation(&self) -> String {
        self.table
            .split('.')
            .map(quote_ident)
            .collect::<Vec<_>>()
            .join(".")
    }
}

/// Bounds a projected CRS can be expected to accept a window within, and the
/// largest window side we will reproject. A window outside these gets no
/// pre-filter rather than risking a PROJ domain error, which would abort the read.
const SAFE_LON_DEG: f64 = 180.0;
const SAFE_LAT_DEG: f64 = 85.0;
const MAX_WINDOW_DEG: f64 = 45.0;

/// Margin added to the window before reprojecting it: this fraction of its longer
/// side, plus a floor for a degenerate (point) window.
///
/// Reprojecting a window and taking the window of a reprojection are not the same
/// operation for a projected CRS — only the vertices move, so straight edges stay
/// straight where the true image curves. The margin covers that gap. It can only
/// ever admit extra candidate rows, never drop one, because the caller keeps its
/// exact predicate.
const MARGIN_FRACTION: f64 = 0.05;
const MARGIN_FLOOR_DEG: f64 = 0.001;

/// Everything needed to read an external dataset: the relation and the ids the
/// derived table has to report so it looks like the `features` view.
#[derive(Debug, Clone)]
pub struct ExternalSource {
    pub dataset_id: Uuid,
    /// SRID recorded at registration. Geometry is reprojected to 4326 when it
    /// differs, because the read stack builds its envelopes in 4326.
    pub srid: i32,
    pub table: ExternalTable,
}

impl ExternalSource {
    /// A derived table with the exact columns of the `features` view
    /// (id, branch_id, dataset_id, geometry, properties), so existing read SQL
    /// works unchanged over a table ptolemy does not own.
    ///
    /// `branch_expr` is SQL for the branch id: pass `"$1"` where the caller
    /// already binds the branch, otherwise a literal cast.
    ///
    /// `overlaps_4326` is SQL for a geometry in EPSG:4326 that returned rows must
    /// overlap, so the read can be pushed down; see [`Self::prefilter`]. Pass
    /// `None` for a read with no spatial restriction.
    ///
    /// Nothing interpolated here is caller text. Identifiers passed
    /// [`ExternalTable::parse`], a [`Uuid`] renders only as hex and dashes, and
    /// both SQL fragments are built by the calling query out of bind
    /// placeholders — no request bytes reach this string.
    pub fn features_subquery(&self, branch_expr: &str, overlaps_4326: Option<&str>) -> String {
        let relation = self.table.quoted_relation();
        let id = quote_ident(&self.table.id_column);
        let geom = quote_ident(&self.table.geometry_column);
        let geom_key = literal(&self.table.geometry_column);
        let dataset_id = self.dataset_id;
        let geometry = if self.is_4326() {
            format!("t.{geom}")
        } else {
            format!("ST_Transform(t.{geom}, 4326)")
        };
        let prefilter = overlaps_4326
            .and_then(|w| self.prefilter(w))
            .map(|p| format!(" AND {p}"))
            .unwrap_or_default();
        // ptolemy identifies features by uuid; a foreign key of any type is
        // hashed into one. Stable across queries, so paging and single-feature
        // get agree. The original key stays visible in properties.
        format!(
            "(SELECT md5(t.{id}::text)::uuid AS id, \
             {branch_expr}::uuid AS branch_id, \
             '{dataset_id}'::uuid AS dataset_id, \
             {geometry} AS geometry, \
             to_jsonb(t) - {geom_key} AS properties \
             FROM {relation} t WHERE t.{id} IS NOT NULL{prefilter})"
        )
    }

    /// The same rows in the column shape the storage read queries use for their
    /// `latest` CTE, so those queries only swap their FROM clause.
    pub fn latest_subquery(&self, branch_expr: &str, overlaps_4326: Option<&str>) -> String {
        let inner = self.features_subquery(branch_expr, overlaps_4326);
        format!(
            "(SELECT id AS feature_id, branch_id, dataset_id, 'insert' AS operation, \
             geometry, properties FROM {inner} ext)"
        )
    }

    /// Whether the relation's geometry is already in the SRID reads are served in.
    pub fn is_4326(&self) -> bool {
        self.srid == 4326 || self.srid == 0
    }

    /// A predicate on the relation's *own* geometry column, in its own SRID, that
    /// every row satisfying `overlaps_4326` must also satisfy. A plain GiST index
    /// on that column serves it; without it a spatial read on a projected relation
    /// can only sequentially scan, because the caller's predicate sits on
    /// `ST_Transform(geom, 4326)` and no index covers that expression.
    ///
    /// `None` when the relation is already in 4326: there the exposed geometry *is*
    /// the column, so the caller's own predicate already reaches the index and
    /// nothing needs adding.
    ///
    /// Deliberately conservative in two ways, because the caller keeps its exact
    /// 4326 predicate and so extra candidates cost time but cannot change results:
    /// the window is widened by `MARGIN_FRACTION` before reprojection, and a
    /// window outside the safe lon/lat range or wider than `MAX_WINDOW_DEG`
    /// yields an all-covering box — no restriction — rather than a reprojection
    /// that PROJ might reject. A window that is NULL or empty lands in the same
    /// fallback.
    pub fn prefilter(&self, overlaps_4326: &str) -> Option<String> {
        if self.is_4326() {
            return None;
        }
        let geom = quote_ident(&self.table.geometry_column);
        let srid = self.srid;
        Some(format!(
            "t.{geom} && (SELECT CASE \
               WHEN ST_XMin(w.g) >= -{SAFE_LON_DEG} AND ST_XMax(w.g) <= {SAFE_LON_DEG} \
                AND ST_YMin(w.g) >= -{SAFE_LAT_DEG} AND ST_YMax(w.g) <= {SAFE_LAT_DEG} \
                AND ST_XMax(w.g) - ST_XMin(w.g) <= {MAX_WINDOW_DEG} \
                AND ST_YMax(w.g) - ST_YMin(w.g) <= {MAX_WINDOW_DEG} \
               THEN ST_Transform(ST_Expand(w.g, greatest(ST_XMax(w.g) - ST_XMin(w.g), \
                    ST_YMax(w.g) - ST_YMin(w.g)) * {MARGIN_FRACTION} + {MARGIN_FLOOR_DEG}), {srid}) \
               ELSE ST_SetSRID(ST_MakeEnvelope(-1e15, -1e15, 1e15, 1e15), {srid}) \
             END FROM (SELECT ST_Envelope({overlaps_4326}) AS g) w)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_and_qualified_names() {
        let t = ExternalTable::parse("parcels", "gid", "geom").unwrap();
        assert_eq!(t.quoted_relation(), "\"parcels\"");
        let t = ExternalTable::parse("public.Parcels_2024", "gid", "the_geom").unwrap();
        assert_eq!(t.quoted_relation(), "\"public\".\"Parcels_2024\"");
    }

    /// The injection barrier: no name that could break out of a quoted
    /// identifier or a string literal may ever reach the SQL builder.
    #[test]
    fn rejects_injection_attempts() {
        let hostile = [
            "t\"; drop table x;--",
            "t'; drop table x;--",
            "parcels; DROP TABLE users",
            "parcels\"",
            "pg_catalog.pg_authid; --",
            "a.b.c",
            "",
            "1parcels",
            "par cels",
            "par-cels",
            "geom)--",
            "tab\\\"le",
            "réservé",
        ];
        for name in hostile {
            assert!(
                ExternalTable::parse(name, "gid", "geom").is_err(),
                "table name accepted: {name}"
            );
            assert!(
                ExternalTable::parse("parcels", name, "geom").is_err(),
                "id column accepted: {name}"
            );
            assert!(
                ExternalTable::parse("parcels", "gid", name).is_err(),
                "geometry column accepted: {name}"
            );
        }
    }

    #[test]
    fn rejects_over_long_identifier() {
        let long = "a".repeat(64);
        assert!(ExternalTable::parse(&long, "gid", "geom").is_err());
        assert!(ExternalTable::parse(&"a".repeat(63), "gid", "geom").is_ok());
    }

    /// Deserializing must not be a way around `parse`.
    #[test]
    fn deserialize_validates() {
        let hostile =
            r#"{"table":"t\"; drop table x;--","id_column":"gid","geometry_column":"geom"}"#;
        assert!(serde_json::from_str::<ExternalTable>(hostile).is_err());
        let ok = r#"{"table":"parcels","id_column":"gid","geometry_column":"geom"}"#;
        assert!(serde_json::from_str::<ExternalTable>(ok).is_ok());
    }

    #[test]
    fn subquery_has_the_features_view_shape() {
        let src = ExternalSource {
            dataset_id: Uuid::nil(),
            srid: 4326,
            table: ExternalTable::parse("public.parcels", "gid", "geom").unwrap(),
        };
        let sql = src.features_subquery("$1", None);
        assert!(sql.contains("md5(t.\"gid\"::text)::uuid AS id"));
        assert!(sql.contains("$1::uuid AS branch_id"));
        assert!(sql.contains("'00000000-0000-0000-0000-000000000000'::uuid AS dataset_id"));
        assert!(sql.contains("t.\"geom\" AS geometry"));
        assert!(sql.contains("to_jsonb(t) - 'geom' AS properties"));
        assert!(sql.contains("FROM \"public\".\"parcels\" t"));
    }

    fn projected() -> ExternalSource {
        ExternalSource {
            dataset_id: Uuid::nil(),
            srid: 3857,
            table: ExternalTable::parse("parcels", "gid", "geom").unwrap(),
        }
    }

    /// A 4326 relation needs no pre-filter: the exposed geometry *is* the column,
    /// so the caller's own predicate already reaches an ordinary GiST index.
    #[test]
    fn a_4326_source_gets_no_prefilter() {
        let src = ExternalSource {
            dataset_id: Uuid::nil(),
            srid: 4326,
            table: ExternalTable::parse("parcels", "gid", "geom").unwrap(),
        };
        assert_eq!(src.prefilter("$2"), None);
        assert!(src.is_4326());
        // srid 0 means "unset", which the read stack treats as 4326
        let unset = ExternalSource { srid: 0, ..src };
        assert_eq!(unset.prefilter("$2"), None);
        // and the subquery is byte-identical with or without a window
        assert_eq!(
            unset.features_subquery("$1", None),
            unset.features_subquery("$1", Some("$2"))
        );
    }

    /// The pre-filter must restrict the relation's own column, in its own SRID,
    /// so a plain GiST index applies.
    #[test]
    fn a_projected_source_prefilters_on_the_raw_column() {
        let sql = projected().prefilter("$2").unwrap();
        assert!(sql.starts_with("t.\"geom\" && "), "{sql}");
        assert!(sql.contains("ST_Transform(ST_Expand("), "{sql}");
        assert!(sql.contains(", 3857)"), "{sql}");
        // the window reaches the SQL only through the caller's placeholder
        assert!(sql.contains("ST_Envelope($2)"), "{sql}");
        // and it never wraps the column in a transform, which is what defeated
        // the index before
        assert!(!sql.contains("ST_Transform(t."), "{sql}");
    }

    /// A window outside the reprojectable range must become an all-covering box,
    /// not a reprojection PROJ could reject, and never a narrower restriction.
    #[test]
    fn an_unreprojectable_window_falls_back_to_no_restriction() {
        let sql = projected().prefilter("$2").unwrap();
        assert!(sql.contains("ELSE ST_SetSRID(ST_MakeEnvelope(-1e15, -1e15, 1e15, 1e15), 3857)"));
        // the guard covers both the lon/lat range and the window size
        for bound in ["-180", "85", "<= 45"] {
            assert!(sql.contains(bound), "guard missing {bound}: {sql}");
        }
    }

    /// The pre-filter goes inside the derived table's WHERE, next to the id check.
    #[test]
    fn the_window_lands_in_the_subquery() {
        let src = projected();
        let plain = src.features_subquery("$1", None);
        assert!(plain.ends_with("WHERE t.\"gid\" IS NOT NULL)"), "{plain}");
        let filtered = src.features_subquery("$1", Some("$2"));
        assert!(
            filtered.contains("WHERE t.\"gid\" IS NOT NULL AND t.\"geom\" && "),
            "{filtered}"
        );
        // latest_subquery carries it through unchanged
        assert!(
            src.latest_subquery("$1", Some("$2"))
                .contains("t.\"geom\" && ")
        );
    }

    #[test]
    fn non_4326_geometry_is_reprojected() {
        let src = ExternalSource {
            dataset_id: Uuid::nil(),
            srid: 3857,
            table: ExternalTable::parse("parcels", "gid", "geom").unwrap(),
        };
        assert!(
            src.features_subquery("$1", None)
                .contains("ST_Transform(t.\"geom\", 4326) AS geometry")
        );
    }
}
