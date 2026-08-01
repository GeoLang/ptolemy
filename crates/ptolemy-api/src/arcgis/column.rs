// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! The one place a layer's field becomes SQL, and the one place a selected value
//! becomes JSON.
//!
//! A where clause, an `ORDER BY`, a `SELECT DISTINCT` and a statistic all have to
//! read the same property the same way. They read it through here, so there is
//! one rendering to keep right rather than four: the key is bound rather than
//! rendered, and a numeric read is guarded by the pattern below so data that
//! disagrees with its declared type reads as no value instead of failing the
//! whole query.

use serde_json::Value;
use sqlx::Row;
use sqlx::postgres::PgRow;

use super::{Bind, Field, Kind};

/// A number as PostgreSQL will read one. Fixed text in the query, never a
/// client's: see [`Column::sql`].
const NUMBER: &str = "^[+-]?([0-9]+[.]?[0-9]*|[.][0-9]+)([eE][+-]?[0-9]+)?$";

/// A column to read, as the layer declared it. The name is the layer's own text
/// and never the client's, and it is bound rather than rendered like every other
/// value here; the kind is what decides whether the read runs on numbers or on
/// text.
#[derive(Clone)]
pub(super) struct Column {
    pub(super) name: String,
    pub(super) kind: Kind,
}

impl Column {
    pub(super) fn of(field: &Field) -> Column {
        Column {
            name: field.name.clone(),
            kind: field.kind,
        }
    }

    /// Whether the field's declared type makes a numeric read the right one.
    ///
    /// A number against a text field compares as text: the field holds text,
    /// nothing declared its values to be numbers, and casting them to compare
    /// would answer on a reading of the data the layer never published.
    pub(super) fn numeric(&self) -> bool {
        matches!(self.kind, Kind::Oid | Kind::Integer | Kind::Double)
    }

    /// The column as the value itself, for a query that selects or orders it
    /// rather than comparing it: a number where the layer declared one and text
    /// everywhere else, and the object id as the bigint the CTE already holds
    /// rather than the double a comparison casts it to. What an order, a distinct
    /// read and a min or max run on.
    pub(super) fn natural(&self, next: &mut i32, binds: &mut Vec<Bind>) -> String {
        match self.kind {
            Kind::Oid => "oid".to_string(),
            _ => self.sql(self.numeric(), next, binds),
        }
    }

    /// The column as SQL over the `numbered` CTE, in the shape it is read in.
    /// That CTE selects the properties and the object id as bare columns, so
    /// there is no table alias to qualify with.
    ///
    /// A numeric read reads the value as a number only when it looks like one, so
    /// a dataset whose values disagree with its declared type answers no rows
    /// rather than failing the whole query: a schema can be declared after the
    /// rows were written.
    pub(super) fn sql(&self, numeric: bool, next: &mut i32, binds: &mut Vec<Bind>) -> String {
        match (self.kind, numeric) {
            // the object id is already a bigint in the CTE. float8 holds every
            // id below 2^53 exactly, which is every id anything here assigns
            (Kind::Oid, true) => "oid::float8".to_string(),
            (Kind::Oid, false) => "oid::text".to_string(),
            (_, true) => {
                // one bind for the key, named twice: a placeholder can be
                // referenced as often as the statement needs it
                let text = self.text(next, binds);
                format!("(CASE WHEN {text} ~ '{NUMBER}' THEN ({text})::float8 END)")
            }
            (_, false) => self.text(next, binds),
        }
    }

    /// The property as text, with the key bound the way [`super::Oid::sql`] binds
    /// it. A property key is a layer's own text rather than a client's, but
    /// binding it means no key can be quoted wrongly whatever it holds, and the
    /// query no longer needs `standard_conforming_strings` to be on.
    pub(super) fn text(&self, next: &mut i32, binds: &mut Vec<Bind>) -> String {
        let place = placeholder(next, binds, Bind::Text(Some(self.name.clone())));
        format!("(properties->>{place}::text)")
    }
}

/// The next placeholder, with its value bound. Every literal in a query goes
/// through here, which is what keeps request data out of the SQL text.
pub(super) fn placeholder(next: &mut i32, binds: &mut Vec<Bind>, bind: Bind) -> String {
    let at = *next;
    *next += 1;
    binds.push(bind);
    format!("${at}")
}

/// How a selected column arrives from PostgreSQL.
///
/// Decided by what the SQL says rather than by the Esri type: `count` is a bigint
/// over a text column, and a guarded numeric read is a float8 whatever the field
/// declares.
#[derive(Clone, Copy)]
pub(super) enum Read {
    Int8,
    Float8,
    Text,
}

impl Read {
    /// How a column of this kind arrives when the query selects the value itself
    /// rather than a statistic over it, which is [`Column::natural`].
    pub(super) fn of(kind: Kind) -> Read {
        match kind {
            Kind::Oid => Read::Int8,
            Kind::Integer | Kind::Double => Read::Float8,
            Kind::Text | Kind::BooleanText | Kind::JsonText => Read::Text,
        }
    }
}

/// One column of an attributes-only answer: what the client calls it, the Esri
/// type it is declared as, and how the row is read.
///
/// A distinct query and a statistics query both answer rows that are not
/// features, so their columns are described here rather than by the layer's own
/// field list: a statistic's name is the alias the client asked for, and its type
/// is the statistic's own.
pub(super) struct Out {
    pub(super) field: Field,
    pub(super) read: Read,
}

impl Out {
    /// A selected value of the layer's own field, under its own name.
    pub(super) fn of(field: &Field) -> Out {
        Out {
            field: Field {
                name: field.name.clone(),
                alias: field.alias.clone(),
                kind: field.kind,
            },
            read: Read::of(field.kind),
        }
    }

    /// The column's value in one row, as the declared type.
    pub(super) fn value(&self, row: &PgRow, at: usize) -> Value {
        match self.read {
            Read::Int8 => row
                .get::<Option<i64>, _>(at)
                .map(Value::from)
                .unwrap_or(Value::Null),
            Read::Text => row
                .get::<Option<String>, _>(at)
                .map(Value::from)
                .unwrap_or(Value::Null),
            Read::Float8 => match row.get::<Option<f64>, _>(at) {
                None => Value::Null,
                // an integer field answers an integer, as it does on a plain
                // query: a value that is not one has no integer to answer with
                Some(held) if self.field.kind == Kind::Integer => integer(held),
                Some(held) => Value::from(held),
            },
        }
    }
}

/// A double as the integer it holds, or nothing when it holds none. `Value::from`
/// answers null for an infinity or a NaN, so every unrepresentable value is one
/// null rather than two shapes of it.
fn integer(held: f64) -> Value {
    let representable = held.fract() == 0.0 && held >= i64::MIN as f64 && held <= i64::MAX as f64;
    if representable {
        Value::from(held as i64)
    } else {
        Value::Null
    }
}
