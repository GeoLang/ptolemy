// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! The `orderByFields` parameter: `field`, `field ASC` or `field DESC`, comma
//! separated.
//!
//! Two steps, like the where clause: [`parse`] reads the request text into terms
//! that name nothing but a field and a direction, and the two renderers turn
//! those into SQL over the layer's own fields. A direction is one of two fixed
//! words this module chose, a field name is bound rather than rendered, and a
//! term the query cannot order by is refused by name: an answer in an order the
//! client did not ask for is an answer it cannot page through.
//!
//! An order is always total. A plain row query breaks ties on the object id and
//! then the feature id, and an aggregated one breaks them on the columns it
//! grouped or distincted by, which are unique in that answer by construction.
//! Without that, two pages of the same query could show the same row twice and
//! miss another.

use super::column::Column;
use super::{Bind, Layer, shown};

/// The most sort terms one request may name. A client orders by one or two
/// fields; this is only here so a list cannot be a way to make the server build
/// something huge.
const MAX_TERMS: usize = 32;

/// One `field [ASC|DESC]`, as written. The field is still the client's text here:
/// what it may name depends on which kind of query it orders, so resolving it is
/// the renderer's job.
pub(super) struct Term {
    pub(super) field: String,
    pub(super) descending: bool,
}

impl Term {
    /// `DESC` or nothing, as the two fixed words this module chose rather than
    /// anything a client wrote.
    fn direction(&self) -> &'static str {
        if self.descending { " DESC" } else { "" }
    }
}

/// The terms `orderByFields` names, or a refusal naming what could not be read.
/// An empty list is no ordering rather than an error: `orderByFields=` is what a
/// client sends when it has nothing to sort by.
pub(super) fn parse(raw: &str) -> Result<Vec<Term>, String> {
    let mut terms = Vec::new();
    for piece in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if terms.len() == MAX_TERMS {
            return Err(format!("more than {MAX_TERMS} fields to order by"));
        }
        let mut words = piece.split_whitespace();
        // a non-empty piece has a first word
        let field = words.next().unwrap_or_default().to_string();
        let descending = match words.next() {
            None => false,
            Some(word) if word.eq_ignore_ascii_case("asc") => false,
            Some(word) if word.eq_ignore_ascii_case("desc") => true,
            Some(word) => {
                return Err(format!(
                    "'{}' where ASC or DESC was expected, in '{}'",
                    shown(word),
                    shown(piece)
                ));
            }
        };
        if let Some(extra) = words.next() {
            return Err(format!(
                "'{}' after the direction, in '{}': a term is a field name and at most one of \
                 ASC or DESC",
                shown(extra),
                shown(piece)
            ));
        }
        terms.push(Term { field, descending });
    }
    Ok(terms)
}

/// The `ORDER BY` body for a query that answers rows of the layer, over the
/// layer's own fields and with the paging tiebreaker last.
///
/// Nulls sort last in both directions rather than taking PostgreSQL's default,
/// which flips with the direction: a client that pages a partly-empty field
/// should not find the empty rows at one end of one page and the other end of the
/// next.
pub(super) fn rows_sql(
    terms: &[Term],
    layer: &Layer,
    next: &mut i32,
    binds: &mut Vec<Bind>,
) -> Result<String, String> {
    let mut parts = Vec::new();
    for term in terms {
        let field = layer
            .field(&term.field)
            .ok_or_else(|| format!("the layer has no field '{}'", shown(&term.field)))?;
        let held = Column::of(field).natural(next, binds);
        parts.push(format!("{held}{} NULLS LAST", term.direction()));
    }
    // the object id is the order paging runs in, and the feature id settles the
    // rows that have no object id to be ordered by
    parts.push("oid NULLS LAST, id".to_string());
    Ok(parts.join(", "))
}

/// The `ORDER BY` body for a query that answers columns rather than features, as
/// output column ordinals.
///
/// An ordinal is fixed text and needs no expression to match the select list the
/// way `DISTINCT` and `GROUP BY` require, so a term that named a grouped field or
/// a statistic's alias orders by exactly the column the client is reading.
///
/// `unique` is how many leading columns are unique across the answer's rows, and
/// they are appended as the tiebreaker.
pub(super) fn columns_sql(
    terms: &[Term],
    columns: &[String],
    unique: usize,
) -> Result<String, String> {
    let at_of = |name: &str| {
        columns
            .iter()
            .position(|held| held.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                format!(
                    "'{}' is not one of the fields this query answers with ({})",
                    shown(name),
                    columns.join(", ")
                )
            })
    };
    let mut used = Vec::new();
    let mut parts = Vec::new();
    for term in terms {
        let at = at_of(&term.field)?;
        used.push(at);
        parts.push(format!("{}{}", at + 1, term.direction()));
    }
    for at in 0..unique {
        if !used.contains(&at) {
            parts.push(format!("{}", at + 1));
        }
    }
    Ok(parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::super::{Field, Kind, Oid};
    use super::*;
    use uuid::Uuid;

    fn field(name: &str, kind: Kind) -> Field {
        Field {
            name: name.to_string(),
            alias: name.to_string(),
            kind,
        }
    }

    fn layer() -> Layer {
        Layer {
            name: "places".to_string(),
            geometry: "esriGeometryPoint",
            dataset_id: Uuid::nil(),
            branch_id: Uuid::nil(),
            fields: vec![
                field("objectid", Kind::Oid),
                field("name", Kind::Text),
                field("pop", Kind::Integer),
            ],
            oid: Oid::RowNumber,
        }
    }

    /// The SQL and the binds a list of order terms renders to, numbered from `$2`
    /// the way a query numbers them.
    fn rendered(raw: &str) -> (String, Vec<Bind>) {
        let terms = parse(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
        let mut next = 2;
        let mut binds = Vec::new();
        let sql = rows_sql(&terms, &layer(), &mut next, &mut binds)
            .unwrap_or_else(|e| panic!("{raw}: {e}"));
        assert_eq!(next as usize, 2 + binds.len(), "{raw}: {sql}");
        (sql, binds)
    }

    fn refusal(raw: &str) -> String {
        let terms = match parse(raw) {
            Err(why) => return why,
            Ok(terms) => terms,
        };
        let mut next = 2;
        let mut binds = Vec::new();
        match rows_sql(&terms, &layer(), &mut next, &mut binds) {
            Ok(sql) => panic!("'{raw}' was accepted as '{sql}'"),
            Err(why) => why,
        }
    }

    #[test]
    fn no_terms_is_the_paging_order() {
        assert_eq!(rendered("").0, "oid NULLS LAST, id");
        assert_eq!(rendered("  ").0, "oid NULLS LAST, id");
        assert_eq!(rendered(",").0, "oid NULLS LAST, id");
    }

    #[test]
    fn a_text_field_orders_on_the_bound_key() {
        let (sql, binds) = rendered("name DESC");
        assert_eq!(
            sql,
            "(properties->>$2::text) DESC NULLS LAST, oid NULLS LAST, id"
        );
        assert_eq!(binds, vec![Bind::Text(Some("name".to_string()))]);
    }

    /// A numeric field orders on the same guarded cast the where clause compares
    /// with, so a value that is not a number sorts as no value rather than
    /// failing the query.
    #[test]
    fn a_numeric_field_orders_on_the_guarded_cast() {
        let (sql, binds) = rendered("pop");
        assert!(
            sql.starts_with("(CASE WHEN (properties->>$2::text) ~ "),
            "{sql}"
        );
        assert!(
            sql.contains("THEN ((properties->>$2::text))::float8 END) NULLS LAST"),
            "{sql}"
        );
        assert_eq!(binds, vec![Bind::Text(Some("pop".to_string()))]);
    }

    /// The id orders on the CTE's own id column, which needs no key bound and no
    /// cast: it is already the bigint the client sees.
    #[test]
    fn the_object_id_orders_on_the_id_column() {
        assert_eq!(
            rendered("objectid DESC").0,
            "oid DESC NULLS LAST, oid NULLS LAST, id"
        );
        assert_eq!(rendered("OBJECTID asc").1, Vec::new());
    }

    #[test]
    fn several_terms_keep_the_order_they_were_written_in() {
        let (sql, binds) = rendered("pop DESC, name, objectid");
        assert_eq!(sql.matches("NULLS LAST").count(), 4, "{sql}");
        assert!(sql.ends_with("oid NULLS LAST, oid NULLS LAST, id"), "{sql}");
        assert_eq!(binds.len(), 2, "{sql}");
    }

    #[test]
    fn what_it_will_not_read_is_refused_by_name() {
        for (raw, names) in [
            ("nosuchfield", "nosuchfield"),
            ("name UP", "UP"),
            ("name DESC NULLS LAST", "NULLS"),
            ("name, nosuchfield DESC", "nosuchfield"),
        ] {
            let why = refusal(raw);
            assert!(
                why.contains(names),
                "'{raw}' was refused as '{why}', which does not name {names}"
            );
        }
        let many = std::iter::repeat_n("name", MAX_TERMS + 1)
            .collect::<Vec<_>>()
            .join(",");
        assert!(refusal(&many).contains("more than"));
    }

    /// An aggregated answer orders by ordinal, and every term has to name a
    /// column that answer carries.
    #[test]
    fn columns_order_by_ordinal_with_the_unique_columns_last() {
        let columns = vec![
            "ward".to_string(),
            "kind".to_string(),
            "sum_pop".to_string(),
        ];
        let terms = parse("sum_pop DESC").unwrap();
        assert_eq!(columns_sql(&terms, &columns, 2).unwrap(), "3 DESC, 1, 2");
        // a term that already names a unique column is not repeated
        let terms = parse("KIND desc, ward").unwrap();
        assert_eq!(columns_sql(&terms, &columns, 2).unwrap(), "2 DESC, 1");
        // no terms is the tiebreaker alone
        assert_eq!(columns_sql(&[], &columns, 2).unwrap(), "1, 2");
        assert_eq!(columns_sql(&[], &columns, 0).unwrap(), "");
        let terms = parse("nosuchcolumn").unwrap();
        let why = columns_sql(&terms, &columns, 2).unwrap_err();
        assert!(why.contains("nosuchcolumn"), "{why}");
        assert!(why.contains("sum_pop"), "{why}");
    }

    /// The parameter is request text, so the parser has to be total: every input
    /// is a list of terms or a refusal, and never a panic.
    #[test]
    fn nothing_panics_whatever_arrives() {
        let layer = layer();
        let check = |raw: &str| {
            if let Ok(terms) = parse(raw) {
                let mut next = 2;
                let mut binds = Vec::new();
                if let Ok(sql) = rows_sql(&terms, &layer, &mut next, &mut binds) {
                    assert_eq!(next as usize, 2 + binds.len(), "{raw}: {sql}");
                }
                let columns = vec!["name".to_string(), "pop".to_string()];
                let _ = columns_sql(&terms, &columns, columns.len());
            }
        };
        for raw in [
            "",
            " ",
            ",",
            ",,,",
            "name,",
            ",name",
            "name name",
            "name asc desc",
            "ASC",
            "DESC",
            "\0",
            "name\0",
            "名前",
            "name\tDESC",
            "name\nDESC",
            "'name'",
            "name;",
            "name DESC--",
            "objectid,objectid,objectid",
        ] {
            check(raw);
        }

        let alphabet: Vec<char> = "ab,name pop objectid ASCDESC'\";()\0é".chars().collect();
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut roll = |bound: usize| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) as usize % bound
        };
        for _ in 0..4000 {
            let length = roll(24);
            let raw: String = (0..length)
                .map(|_| alphabet[roll(alphabet.len())])
                .collect();
            check(&raw);
        }
    }
}
