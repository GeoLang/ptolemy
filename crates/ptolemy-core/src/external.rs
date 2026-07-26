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
    /// Nothing interpolated here is caller text. Identifiers passed
    /// [`ExternalTable::parse`], and a [`Uuid`] renders only as hex and dashes.
    pub fn features_subquery(&self, branch_expr: &str) -> String {
        let relation = self.table.quoted_relation();
        let id = quote_ident(&self.table.id_column);
        let geom = quote_ident(&self.table.geometry_column);
        let geom_key = literal(&self.table.geometry_column);
        let dataset_id = self.dataset_id;
        let geometry = if self.srid == 4326 || self.srid == 0 {
            format!("t.{geom}")
        } else {
            format!("ST_Transform(t.{geom}, 4326)")
        };
        // ptolemy identifies features by uuid; a foreign key of any type is
        // hashed into one. Stable across queries, so paging and single-feature
        // get agree. The original key stays visible in properties.
        format!(
            "(SELECT md5(t.{id}::text)::uuid AS id, \
             {branch_expr}::uuid AS branch_id, \
             '{dataset_id}'::uuid AS dataset_id, \
             {geometry} AS geometry, \
             to_jsonb(t) - {geom_key} AS properties \
             FROM {relation} t WHERE t.{id} IS NOT NULL)"
        )
    }

    /// The same rows in the column shape the storage read queries use for their
    /// `latest` CTE, so those queries only swap their FROM clause.
    pub fn latest_subquery(&self, branch_expr: &str) -> String {
        let inner = self.features_subquery(branch_expr);
        format!(
            "(SELECT id AS feature_id, branch_id, dataset_id, 'insert' AS operation, \
             geometry, properties FROM {inner} ext)"
        )
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
        let sql = src.features_subquery("$1");
        assert!(sql.contains("md5(t.\"gid\"::text)::uuid AS id"));
        assert!(sql.contains("$1::uuid AS branch_id"));
        assert!(sql.contains("'00000000-0000-0000-0000-000000000000'::uuid AS dataset_id"));
        assert!(sql.contains("t.\"geom\" AS geometry"));
        assert!(sql.contains("to_jsonb(t) - 'geom' AS properties"));
        assert!(sql.contains("FROM \"public\".\"parcels\" t"));
    }

    #[test]
    fn non_4326_geometry_is_reprojected() {
        let src = ExternalSource {
            dataset_id: Uuid::nil(),
            srid: 3857,
            table: ExternalTable::parse("parcels", "gid", "geom").unwrap(),
        };
        assert!(
            src.features_subquery("$1")
                .contains("ST_Transform(t.\"geom\", 4326) AS geometry")
        );
    }
}
