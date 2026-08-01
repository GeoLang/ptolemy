// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! The `outStatistics` and `groupByFieldsForStatistics` parameters: aggregates
//! over the rows a query selected, one row per group or one row for all of them.
//!
//! Nothing a client wrote reaches the SQL. A statistic type is matched against a
//! closed set and rendered as this module's own function name, a field name is
//! resolved through the layer and bound, and the `outStatisticFieldName` a client
//! asked for is a JSON key only: the columns are read by position, so the alias
//! never becomes an identifier in a statement. It is still validated as one, so a
//! client that sent something that could not be a field name is told rather than
//! answered under a name it will not recognise.
//!
//! `having` filters the rows this answer already holds, so it resolves against
//! them rather than against the layer: see [`Statistics::aliases`] and the
//! [`Columns`] implementation below. It needs a grouping to filter, and the query
//! refuses it by name without one.

use serde_json::Value;

use super::column::{Cell, Column, Out, Read, alias_at};
use super::where_clause::Columns;
use super::{Bind, Field, Kind, Layer, shown};

/// The most statistics one request may ask for. A dashboard asks for a handful;
/// this is only here so a list cannot be a way to make the server build something
/// huge.
const MAX_STATISTICS: usize = 32;

/// The most fields one request may group by. Every extra field multiplies the
/// groups, and a client that wants the rows themselves should ask for the rows.
const MAX_GROUPS: usize = 16;

/// The aggregates this facade implements, each with the one PostgreSQL function
/// it renders as. Fixed text, matched from a closed set: a `statisticType` that
/// is not one of these is refused by name rather than passed on.
#[derive(Clone, Copy, PartialEq)]
enum Statistic {
    Count,
    Sum,
    Min,
    Max,
    Avg,
    StdDev,
    Var,
}

impl Statistic {
    fn of(name: &str) -> Option<Statistic> {
        let held = [
            ("count", Statistic::Count),
            ("sum", Statistic::Sum),
            ("min", Statistic::Min),
            ("max", Statistic::Max),
            ("avg", Statistic::Avg),
            ("stddev", Statistic::StdDev),
            ("var", Statistic::Var),
        ];
        held.iter()
            .find(|(spelling, _)| name.eq_ignore_ascii_case(spelling))
            .map(|(_, statistic)| *statistic)
    }

    /// Esri's own name for it, which is what a default alias is built from.
    fn label(self) -> &'static str {
        match self {
            Statistic::Count => "count",
            Statistic::Sum => "sum",
            Statistic::Min => "min",
            Statistic::Max => "max",
            Statistic::Avg => "avg",
            Statistic::StdDev => "stddev",
            Statistic::Var => "var",
        }
    }

    /// The PostgreSQL aggregate. `stddev` and `var` are the sample forms, which is
    /// what Esri's own `stddev` and `var` are: a layer is a sample of the world,
    /// and one row therefore has no deviation rather than a zero one.
    fn function(self) -> &'static str {
        match self {
            Statistic::Count => "count",
            Statistic::Sum => "sum",
            Statistic::Min => "min",
            Statistic::Max => "max",
            Statistic::Avg => "avg",
            Statistic::StdDev => "stddev_samp",
            Statistic::Var => "var_samp",
        }
    }

    /// Whether the statistic only means anything over numbers. A count counts
    /// values of any type and a min or max orders them, so those two run on a text
    /// field as they do on a number.
    fn needs_numbers(self) -> bool {
        matches!(
            self,
            Statistic::Sum | Statistic::Avg | Statistic::StdDev | Statistic::Var
        )
    }
}

/// One requested statistic, resolved against the layer.
struct Stat {
    column: Column,
    statistic: Statistic,
}

impl Stat {
    fn sql(&self, next: &mut i32, binds: &mut Vec<Bind>) -> String {
        let held = match self.statistic {
            // a count counts the values that are there, so it reads the property
            // as text: the guarded numeric read would leave out every value that
            // does not look like a number, and those are values too
            Statistic::Count => self.column.sql(false, next, binds),
            Statistic::Min | Statistic::Max => self.column.natural(next, binds),
            _ => self.column.sql(true, next, binds),
        };
        format!("{}({held})", self.statistic.function())
    }

    /// The column the statistic answers as: a count is a whole number of rows, the
    /// numeric aggregates are doubles whatever they read, and a min or max is a
    /// value of the field it read so it keeps that field's type.
    fn out(&self, name: String) -> Out {
        let kind = match self.statistic {
            Statistic::Count => Kind::Integer,
            Statistic::Min | Statistic::Max => self.column.kind,
            _ => Kind::Double,
        };
        let read = match self.statistic {
            Statistic::Count => Read::Int8,
            Statistic::Min | Statistic::Max => Read::of(self.column.kind),
            _ => Read::Float8,
        };
        Out {
            field: Field {
                alias: name.clone(),
                name,
                kind,
            },
            read,
        }
    }
}

/// A statistics query: the fields it groups by and the aggregates it answers,
/// resolved against the layer and validated.
pub(super) struct Statistics {
    groups: Vec<Column>,
    stats: Vec<Stat>,
    /// Every column of the answer, the grouped fields first and then the
    /// statistics, in the order the select list renders them.
    pub(super) outputs: Vec<Out>,
}

impl Statistics {
    /// The select list, with every property key bound. No column is aliased: the
    /// answer is read by position, so a client's `outStatisticFieldName` stays out
    /// of the statement entirely.
    pub(super) fn select_list(&self, next: &mut i32, binds: &mut Vec<Bind>) -> String {
        let mut parts: Vec<String> = self
            .groups
            .iter()
            .map(|group| group.natural(next, binds))
            .collect();
        parts.extend(self.stats.iter().map(|stat| stat.sql(next, binds)));
        parts.join(", ")
    }

    /// `GROUP BY` over the leading columns, by ordinal so the grouping is exactly
    /// the expression the select list holds and no client text is rendered twice.
    /// Nothing at all when the request groups by nothing, which is one row over
    /// everything the filters selected.
    pub(super) fn group_by(&self) -> String {
        if self.groups.is_empty() {
            return String::new();
        }
        let ordinals: Vec<String> = (1..=self.groups.len()).map(|at| at.to_string()).collect();
        format!(" GROUP BY {}", ordinals.join(", "))
    }

    /// The answer's column names, in select order.
    pub(super) fn columns(&self) -> Vec<String> {
        self.outputs
            .iter()
            .map(|out| out.field.name.clone())
            .collect()
    }

    /// How many leading columns are unique across the answer's rows, which is the
    /// grouped fields: one row per distinct combination of them.
    pub(super) fn unique(&self) -> usize {
        self.groups.len()
    }

    /// The names the wrapping subquery gives this answer's columns when a `having`
    /// clause filters it: `c1`..`cN`, in select order.
    ///
    /// Crate-generated on purpose. A `having` clause names a grouped field or a
    /// client's own `outStatisticFieldName`, and it is these that the predicate is
    /// rendered over, so neither of those names ever becomes an identifier in the
    /// statement.
    pub(super) fn aliases(&self) -> Vec<String> {
        (0..self.outputs.len()).map(alias_at).collect()
    }
}

/// A `having` clause resolves against the answer, not the layer: the rows it
/// filters are already aggregated and hold no properties. A name it carries is a
/// grouped field or a statistic's alias, and it resolves to that column's own
/// position and SQL type, which is what decides the shape a literal compares in.
impl Columns for Statistics {
    fn cell(&self, name: &str) -> Option<Cell> {
        let at = self
            .outputs
            .iter()
            .position(|out| out.field.name.eq_ignore_ascii_case(name))?;
        Some(Cell::aggregate(at, self.outputs[at].read))
    }

    fn missing(&self, name: &str) -> String {
        format!(
            "'{}' is not one of the columns this query answers with ({})",
            shown(name),
            self.columns().join(", ")
        )
    }
}

/// The statistics a request asks for, or a refusal naming what could not be read.
pub(super) fn parse(raw: &str, groups: Option<&str>, layer: &Layer) -> Result<Statistics, String> {
    let value: Value =
        serde_json::from_str(raw).map_err(|e| format!("outStatistics is not valid JSON: {e}"))?;
    let Value::Array(items) = value else {
        return Err(format!(
            "outStatistics must be a JSON array of \
             {{statisticType, onStatisticField, outStatisticFieldName}}, not '{}'",
            shown(raw)
        ));
    };
    if items.is_empty() {
        return Err("outStatistics names no statistic".to_string());
    }
    if items.len() > MAX_STATISTICS {
        return Err(format!(
            "more than {MAX_STATISTICS} statistics in one request"
        ));
    }

    let mut group_columns = Vec::new();
    let mut outputs: Vec<Out> = Vec::new();
    for name in groups
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if group_columns.len() == MAX_GROUPS {
            return Err(format!(
                "more than {MAX_GROUPS} fields in groupByFieldsForStatistics"
            ));
        }
        let field = layer.field(name).ok_or_else(|| {
            format!(
                "groupByFieldsForStatistics names '{}', which the layer has no field for",
                shown(name)
            )
        })?;
        taken(&outputs, &field.name)
            .map_err(|held| format!("groupByFieldsForStatistics names '{held}' more than once"))?;
        group_columns.push(Column::of(field));
        outputs.push(Out::of(field));
    }

    let mut stats = Vec::new();
    for item in &items {
        let Value::Object(held) = item else {
            return Err(format!(
                "a statistic is an object with statisticType and onStatisticField, not '{}'",
                shown(&item.to_string())
            ));
        };
        let read = |key: &str| -> Option<&str> { held.get(key).and_then(Value::as_str) };

        let raw_type = read("statisticType").unwrap_or_default().trim();
        let statistic = Statistic::of(raw_type).ok_or_else(|| {
            format!(
                "statisticType '{}' is not supported in this version of the service, which \
                 answers count, sum, min, max, avg, stddev and var",
                shown(raw_type)
            )
        })?;

        let on = read("onStatisticField").unwrap_or_default().trim();
        let field = layer.field(on).ok_or_else(|| {
            format!(
                "onStatisticField '{}' is not a field of this layer",
                shown(on)
            )
        })?;
        let column = Column::of(field);
        if statistic.needs_numbers() && !column.numeric() {
            return Err(format!(
                "{} is not supported on '{}', which this layer declares as text: count, min and \
                 max read a field of any type",
                statistic.label(),
                field.name
            ));
        }
        let stat = Stat { column, statistic };

        // absent, null or empty all mean "name it for me". The default is built
        // from the layer's own field name rather than the spelling the client
        // sent, and it is not held to the identifier rule below: a layer with no
        // schema takes its field names from property keys, which can be anything,
        // and a client asking for a plain count of such a field is not the client
        // making that name up.
        let name = match read("outStatisticFieldName").map(str::trim) {
            None | Some("") => format!("{}_{}", statistic.label(), field.name),
            Some(asked) => {
                if !identifier(asked) {
                    return Err(format!(
                        "outStatisticFieldName '{}' is not a field name: it has to start with a \
                         letter or '_' and hold nothing but letters, digits and '_', up to 64 \
                         characters",
                        shown(asked)
                    ));
                }
                asked.to_string()
            }
        };
        taken(&outputs, &name).map_err(|held| format!("two statistics are both named '{held}'"))?;
        outputs.push(stat.out(name));
        stats.push(stat);
    }

    Ok(Statistics {
        groups: group_columns,
        stats,
        outputs,
    })
}

/// Whether an answer already carries a column of this name, matched the way a
/// client will read it back: two columns of one name would hand the client one
/// JSON key holding whichever of them serialized last.
fn taken(outputs: &[Out], name: &str) -> Result<(), String> {
    match outputs
        .iter()
        .find(|out| out.field.name.eq_ignore_ascii_case(name))
    {
        Some(held) => Err(shown(&held.field.name)),
        None => Ok(()),
    }
}

/// Whether a client's `outStatisticFieldName` is a field name, which is
/// `^[A-Za-z_][A-Za-z0-9_]{0,63}$`.
///
/// The name never reaches the SQL, so this is not what makes the statement safe.
/// It is what keeps the answer readable: a client that asked for a name no field
/// could have is told so, rather than served a key it will not look for.
fn identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    name.len() <= 64 && chars.all(|held| held.is_ascii_alphanumeric() || held == '_')
}

#[cfg(test)]
mod tests {
    use super::super::Oid;
    use super::*;
    use uuid::Uuid;

    fn field(name: &str, kind: Kind) -> Field {
        Field {
            name: name.to_string(),
            alias: name.to_string(),
            kind,
        }
    }

    /// A layer with one field of every kind, so a test can say which shape a
    /// statistic should take.
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
                field("score", Kind::Double),
                field("ward", Kind::Text),
            ],
            oid: Oid::RowNumber,
        }
    }

    /// The select list, the group by and the binds a request renders to, numbered
    /// from `$2` the way a query numbers them.
    fn rendered(raw: &str, groups: Option<&str>) -> (String, String, Vec<Bind>) {
        let held = parse(raw, groups, &layer()).unwrap_or_else(|e| panic!("{raw}: {e}"));
        let mut next = 2;
        let mut binds = Vec::new();
        let select = held.select_list(&mut next, &mut binds);
        assert_eq!(next as usize, 2 + binds.len(), "{raw}: {select}");
        (select, held.group_by(), binds)
    }

    fn refusal(raw: &str, groups: Option<&str>) -> String {
        match parse(raw, groups, &layer()) {
            Ok(held) => panic!("'{raw}' was accepted with {} columns", held.outputs.len()),
            Err(why) => why,
        }
    }

    fn stat(kind: &str, field: &str) -> String {
        format!(r#"[{{"statisticType":"{kind}","onStatisticField":"{field}"}}]"#)
    }

    #[test]
    fn a_numeric_statistic_renders_the_guarded_cast_and_binds_the_key() {
        let (select, group, binds) = rendered(&stat("sum", "pop"), None);
        assert!(
            select.starts_with("sum((CASE WHEN (properties->>$2::text) ~ "),
            "{select}"
        );
        assert!(
            select.contains("THEN ((properties->>$2::text))::float8 END))"),
            "{select}"
        );
        assert_eq!(group, "", "one row over everything: {group}");
        assert_eq!(binds, vec![Bind::Text(Some("pop".to_string()))]);
    }

    /// A count reads the property as text, so a value that does not look like a
    /// number is still a value that is there.
    #[test]
    fn a_count_counts_text() {
        let (select, _, binds) = rendered(&stat("count", "name"), None);
        assert_eq!(select, "count((properties->>$2::text))");
        assert_eq!(binds, vec![Bind::Text(Some("name".to_string()))]);
        // and over the object id it counts the CTE's own column
        assert_eq!(
            rendered(&stat("count", "objectid"), None).0,
            "count(oid::text)"
        );
    }

    #[test]
    fn min_and_max_read_the_field_in_its_own_shape() {
        assert_eq!(
            rendered(&stat("min", "name"), None).0,
            "min((properties->>$2::text))"
        );
        assert!(
            rendered(&stat("max", "score"), None)
                .0
                .starts_with("max((CASE WHEN ")
        );
        assert_eq!(rendered(&stat("min", "objectid"), None).0, "min(oid)");
    }

    #[test]
    fn every_statistic_maps_to_one_function() {
        for (asked, function) in [
            ("count", "count("),
            ("sum", "sum("),
            ("min", "min("),
            ("max", "max("),
            ("avg", "avg("),
            ("stddev", "stddev_samp("),
            ("var", "var_samp("),
            ("STDDEV", "stddev_samp("),
            ("Avg", "avg("),
        ] {
            let (select, _, _) = rendered(&stat(asked, "pop"), None);
            assert!(select.starts_with(function), "{asked}: {select}");
        }
    }

    #[test]
    fn a_group_by_is_ordinals_over_the_leading_columns() {
        let (select, group, binds) = rendered(&stat("count", "pop"), Some("ward, name"));
        assert!(
            select.starts_with("(properties->>$2::text), (properties->>$3::text), count("),
            "{select}"
        );
        assert_eq!(group, " GROUP BY 1, 2");
        assert_eq!(binds.len(), 3, "{select}");
        assert_eq!(binds[0], Bind::Text(Some("ward".to_string())));
    }

    /// The columns a client reads: the grouped fields under their own names, then
    /// the statistics under the names they were asked for or given.
    #[test]
    fn the_columns_are_named_and_typed_by_what_they_answer() {
        let held = parse(
            r#"[{"statisticType":"count","onStatisticField":"pop"},
                {"statisticType":"avg","onStatisticField":"pop","outStatisticFieldName":"mean_pop"},
                {"statisticType":"min","onStatisticField":"name"}]"#,
            Some("ward"),
            &layer(),
        )
        .unwrap();
        assert_eq!(
            held.columns(),
            vec!["ward", "count_pop", "mean_pop", "min_name"]
        );
        assert_eq!(held.unique(), 1);
        let kinds: Vec<&'static str> = held
            .outputs
            .iter()
            .map(|out| out.field.kind.esri())
            .collect();
        assert_eq!(
            kinds,
            vec![
                "esriFieldTypeString",
                // a count is a whole number of rows
                "esriFieldTypeInteger",
                "esriFieldTypeDouble",
                // a min over text answers text
                "esriFieldTypeString",
            ]
        );
    }

    #[test]
    fn what_it_will_not_read_is_refused_by_name() {
        for (raw, groups, names) in [
            (stat("median", "pop").as_str(), None, "median"),
            (stat("sum", "name").as_str(), None, "name"),
            (stat("sum", "nosuchfield").as_str(), None, "nosuchfield"),
            (
                stat("count", "pop").as_str(),
                Some("nosuchfield"),
                "nosuchfield",
            ),
            (
                stat("count", "pop").as_str(),
                Some("ward,WARD"),
                "more than once",
            ),
            ("[]", None, "no statistic"),
            ("{}", None, "must be a JSON array"),
            ("[3]", None, "a statistic is an object"),
            ("not json", None, "not valid JSON"),
            (r#"[{"onStatisticField":"pop"}]"#, None, "statisticType"),
            (r#"[{"statisticType":"count"}]"#, None, "onStatisticField"),
            (
                r#"[{"statisticType":"count","onStatisticField":"pop","outStatisticFieldName":"total pop"}]"#,
                None,
                "total pop",
            ),
            (
                r#"[{"statisticType":"count","onStatisticField":"pop","outStatisticFieldName":"a\";--"}]"#,
                None,
                "is not a field name",
            ),
            (
                r#"[{"statisticType":"count","onStatisticField":"pop","outStatisticFieldName":"9lives"}]"#,
                None,
                "9lives",
            ),
            (
                r#"[{"statisticType":"count","onStatisticField":"pop","outStatisticFieldName":"total"},
                    {"statisticType":"sum","onStatisticField":"pop","outStatisticFieldName":"TOTAL"}]"#,
                None,
                "both named",
            ),
            (
                r#"[{"statisticType":"count","onStatisticField":"pop"},
                    {"statisticType":"count","onStatisticField":"POP"}]"#,
                None,
                "both named",
            ),
            (
                r#"[{"statisticType":"count","onStatisticField":"ward","outStatisticFieldName":"ward"}]"#,
                Some("ward"),
                "both named",
            ),
        ] {
            let why = refusal(raw, groups);
            assert!(
                why.contains(names),
                "'{raw}' was refused as '{why}', which does not name {names}"
            );
        }

        // the caps, each named in its refusal
        let many = format!(
            "[{}]",
            std::iter::repeat_n(
                r#"{"statisticType":"count","onStatisticField":"pop"}"#,
                MAX_STATISTICS + 1
            )
            .collect::<Vec<_>>()
            .join(",")
        );
        assert!(
            refusal(&many, None).contains(&format!("more than {MAX_STATISTICS}")),
            "{}",
            refusal(&many, None)
        );
        // the group cap, over a layer wide enough to reach it with distinct fields
        let mut wide = layer();
        let names: Vec<String> = (0..=MAX_GROUPS).map(|at| format!("g{at}")).collect();
        for name in &names {
            wide.fields.push(field(name, Kind::Text));
        }
        let why = match parse(&stat("count", "pop"), Some(&names.join(",")), &wide) {
            Ok(held) => panic!("{} group fields were accepted", held.unique()),
            Err(why) => why,
        };
        assert!(why.contains(&format!("more than {MAX_GROUPS}")), "{why}");
    }

    /// The aliases the wrapping subquery gives an answer, and the SQL a `having`
    /// clause over it renders to, numbered from `$2` the way a query numbers them.
    fn having(raw: &str, groups: Option<&str>, clause: &str) -> (String, Vec<Bind>) {
        let held = parse(raw, groups, &layer()).unwrap_or_else(|e| panic!("{raw}: {e}"));
        assert_eq!(held.aliases().len(), held.outputs.len());
        let predicate = super::super::where_clause::parse(clause, &held)
            .unwrap_or_else(|e| panic!("{clause}: {e}"));
        let mut next = 2;
        let mut binds = Vec::new();
        let sql = predicate.sql(&mut next, &mut binds);
        assert_eq!(next as usize, 2 + binds.len(), "{clause}: {sql}");
        (sql, binds)
    }

    fn having_refusal(raw: &str, groups: Option<&str>, clause: &str) -> String {
        let held = parse(raw, groups, &layer()).unwrap_or_else(|e| panic!("{raw}: {e}"));
        match super::super::where_clause::parse(clause, &held) {
            Ok(_) => panic!("'{clause}' was accepted"),
            Err(why) => why,
        }
    }

    /// Four statistics over one group, which is the answer every `having` test
    /// below filters: `ward`, `count_pop`, `sum_pop`, `min_name`, `max_score`.
    fn grouped() -> String {
        r#"[{"statisticType":"count","onStatisticField":"pop"},
            {"statisticType":"sum","onStatisticField":"pop"},
            {"statisticType":"min","onStatisticField":"name"},
            {"statisticType":"max","onStatisticField":"score"}]"#
            .to_string()
    }

    #[test]
    fn the_aliases_are_this_crate_s_own_names_in_select_order() {
        let held = parse(&grouped(), Some("ward"), &layer()).unwrap();
        assert_eq!(held.aliases(), vec!["c1", "c2", "c3", "c4", "c5"]);
        assert_eq!(
            held.columns(),
            vec!["ward", "count_pop", "sum_pop", "min_name", "max_score"]
        );
    }

    /// A `having` clause names a column of the answer and renders as the alias the
    /// subquery gave it, cast the way the where clause casts: a count and the
    /// numeric aggregates compare as numbers, and a min over text or a text group
    /// field compares as text.
    #[test]
    fn a_having_column_renders_as_its_alias_cast_by_what_it_holds() {
        let stats = grouped();
        for (clause, sql) in [
            // the group field, which is text
            ("ward = 'north'", "(c1 = $2::text)"),
            // a count is a whole number of rows
            ("count_pop > 1", "(c2::float8 > $2::float8)"),
            // sum is a double
            ("sum_pop >= 30", "(c3::float8 >= $2::float8)"),
            // a min over text keeps that field's type
            ("min_name < 'm'", "(c4 < $2::text)"),
            // a max over a double keeps that field's type
            ("max_score > 1.5", "(c5::float8 > $2::float8)"),
        ] {
            let (rendered, _) = having(&stats, Some("ward"), clause);
            assert!(rendered.ends_with(sql), "{clause}: {rendered}");
        }

        // an aggregated column costs no bind: the alias is fixed text, so only
        // the literal is bound and it takes the first placeholder
        let (_, binds) = having(&stats, Some("ward"), "count_pop > 1");
        assert_eq!(binds, vec![Bind::Number(1.0)]);

        // and a group field's own kind decides its cast, exactly as in a where
        // clause: a declared number groups and compares as one
        let (sql, binds) = having(&stats, Some("pop"), "pop > 15");
        assert_eq!(sql, "(c1::float8 > $2::float8)");
        assert_eq!(binds, vec![Bind::Number(15.0)]);
        // the object id is a number too, and it is the CTE's bigint underneath
        let (sql, _) = having(&stats, Some("objectid"), "objectid = 3");
        assert_eq!(sql, "(c1::float8 = $2::float8)");
    }

    /// The where clause's own rules for a literal whose shape disagrees with its
    /// column, applied to an aggregated column: a number against text compares as
    /// the spelling the client wrote, and text against a number compares as text.
    #[test]
    fn a_literal_takes_the_shape_of_the_column_it_is_compared_with() {
        let stats = grouped();
        let (sql, binds) = having(&stats, Some("ward"), "ward = 7");
        assert_eq!(sql, "(c1 = $2::text)");
        assert_eq!(binds, vec![Bind::Text(Some("7".to_string()))]);

        let (sql, binds) = having(&stats, Some("ward"), "count_pop = 'x'");
        assert_eq!(sql, "(c2::text = $2::text)");
        assert_eq!(binds, vec![Bind::Text(Some("x".to_string()))]);
    }

    /// The whole grammar is the where clause's, over the answer's columns.
    #[test]
    fn the_grammar_is_the_where_clause_s() {
        let stats = grouped();
        let held = |clause: &str| having(&stats, Some("ward"), clause).0;
        assert_eq!(held("sum_pop IS NULL"), "(c3::text IS NULL)");
        assert_eq!(
            held("count_pop IN (1, 2)"),
            "(c2::float8 = ANY($2::float8[]))"
        );
        assert_eq!(held("ward LIKE 'n%'"), "(c1 LIKE $2::text ESCAPE '')");
        assert_eq!(
            held("count_pop > 1 AND sum_pop < 100"),
            "((c2::float8 > $2::float8) AND (c3::float8 < $3::float8))"
        );
        assert!(held("NOT ward = 'north'").starts_with("(NOT "));
        assert!(held("sum_pop BETWEEN 10 AND 20").contains(" AND "));
        // a literal on the left turns the comparison round, as it does anywhere
        assert_eq!(held("30 <= sum_pop"), "(c3::float8 >= $2::float8)");
    }

    /// The clause is request text, and none of it is rendered: a hostile string
    /// literal is one bound value, and the alias it compares against is this
    /// crate's own.
    #[test]
    fn a_hostile_literal_is_bound_rather_than_rendered() {
        // the DROP form of this is in the integration tests, which run it against
        // a real database and then prove the layer is still there
        let (sql, binds) = having(
            &grouped(),
            Some("ward"),
            "ward = 'x''; delete everything;--'",
        );
        assert_eq!(sql, "(c1 = $2::text)");
        assert_eq!(
            binds,
            vec![Bind::Text(Some("x'; delete everything;--".to_string()))]
        );
        assert!(!sql.contains("delete"), "{sql}");
    }

    /// A client alias that looks like one of this crate's own names is still
    /// resolved by position, so it names the column the client is reading and
    /// cannot be confused with the alias of another one.
    #[test]
    fn a_client_alias_that_looks_like_an_internal_one_still_resolves_by_position() {
        let stats = r#"[{"statisticType":"count","onStatisticField":"pop",
                         "outStatisticFieldName":"c1"}]"#;
        let held = parse(stats, Some("ward"), &layer()).unwrap();
        assert_eq!(held.columns(), vec!["ward", "c1"]);
        // the client's "c1" is the second column, so it renders as c2 and the
        // group field keeps c1
        let (sql, _) = having(stats, Some("ward"), "c1 > 1");
        assert_eq!(sql, "(c2::float8 > $2::float8)");
        let (sql, _) = having(stats, Some("ward"), "ward = 'north'");
        assert_eq!(sql, "(c1 = $2::text)");
    }

    /// A `having` clause resolves against the answer and nothing else. A field the
    /// layer has but this answer does not carry is refused by name, with the
    /// columns it could have named listed.
    #[test]
    fn what_the_answer_does_not_carry_is_refused_by_name() {
        let stats = grouped();
        for (clause, names) in [
            ("nosuchcolumn = 1", "nosuchcolumn"),
            // a real field of the layer, which this answer does not carry
            ("score > 1", "score"),
            ("name = 'a'", "name"),
            // the field a statistic read, rather than the statistic
            ("pop > 1", "pop"),
        ] {
            let why = having_refusal(&stats, Some("ward"), clause);
            assert!(
                why.contains(names),
                "'{clause}' was refused as '{why}', which does not name {names}"
            );
            assert!(why.contains("sum_pop"), "{clause}: {why}");
        }
        // and the grammar's own refusals still apply
        assert!(
            having_refusal(&stats, Some("ward"), "upper(ward) = 'A'").contains("upper"),
            "a function was accepted"
        );
    }

    /// A `having` clause is request text like any other, so parsing it against an
    /// answer has to be total: every input is a predicate or a refusal, and never a
    /// panic. The same sweep the where clause runs, over the columns an answer
    /// carries rather than a layer's fields.
    #[test]
    fn nothing_panics_whatever_a_having_clause_holds() {
        let held = parse(&grouped(), Some("ward"), &layer()).unwrap();
        let check = |clause: &str| {
            if let Ok(predicate) = super::super::where_clause::parse(clause, &held) {
                let mut next = 2;
                let mut binds = Vec::new();
                let sql = predicate.sql(&mut next, &mut binds);
                assert_eq!(next as usize, 2 + binds.len(), "{clause}: {sql}");
                // no column of the answer is named in the statement, whatever the
                // clause asked for: only this crate's own aliases are
                for name in held.columns() {
                    assert!(!sql.contains(&name), "{clause}: {sql} names {name}");
                }
            }
        };

        // every prefix of a clause that uses the whole grammar over this answer
        let whole = "NOT (count_pop BETWEEN 1 AND 3) AND ward IN ('north','south') OR sum_pop >= \
                     30 AND min_name LIKE '%x_' AND max_score IS NOT NULL";
        for end in whole.char_indices().map(|(at, _)| at).chain([whole.len()]) {
            check(&whole[..end]);
        }

        for clause in [
            "",
            " ",
            "c1",
            "c1 = 1",
            "c99 = 1",
            "count_pop",
            "count_pop =",
            "'count_pop' = 1",
            "count_pop = count_pop",
            "((((",
            "sum_pop IN ()",
            "sum_pop IN (1,,2)",
            "ward = '\0'",
            "\u{202e}",
            "ward = 'unterminated",
            "count_pop + 1 = 2",
            "COUNT(pop) > 1",
            "sum_pop > 1; DELETE",
            "ward = 'a' -- rest",
        ] {
            check(clause);
        }

        let alphabet: Vec<char> =
            "ab019 '()=<>!,-+*/%._ANDORNOTISNULLIKEBETWEENwardcount_popsum_popmin_namec1\";\\|é"
                .chars()
                .collect();
        let mut seed: u64 = 0x51ed_2701_a3f4_9c6b;
        let mut roll = |bound: usize| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) as usize % bound
        };
        for _ in 0..4000 {
            let length = roll(28);
            let clause: String = (0..length)
                .map(|_| alphabet[roll(alphabet.len())])
                .collect();
            check(&clause);
        }
    }

    /// A name of exactly 64 characters is a name, and 65 is not: the rule is one
    /// leading character and up to 63 after it.
    #[test]
    fn the_identifier_rule_is_the_documented_one() {
        assert!(identifier("_a9"));
        assert!(identifier(&"a".repeat(64)));
        assert!(!identifier(&"a".repeat(65)));
        assert!(!identifier(""));
        assert!(!identifier("9a"));
        assert!(!identifier("a b"));
        assert!(!identifier("café"));
        assert!(!identifier("a-b"));
        assert!(!identifier("\"a\""));
        assert!(!identifier("a\0"));
    }

    /// The parameters are request text, so parsing has to be total: every input is
    /// a statistics query or a refusal, and never a panic.
    #[test]
    fn nothing_panics_whatever_arrives() {
        let layer = layer();
        let check = |raw: &str, groups: Option<&str>| {
            if let Ok(held) = parse(raw, groups, &layer) {
                let mut next = 2;
                let mut binds = Vec::new();
                let select = held.select_list(&mut next, &mut binds);
                assert_eq!(next as usize, 2 + binds.len(), "{raw}: {select}");
                assert_eq!(held.columns().len(), held.outputs.len());
                assert!(held.unique() <= held.outputs.len());
            }
        };

        for raw in [
            "",
            " ",
            "[",
            "]",
            "[]",
            "{}",
            "null",
            "[null]",
            "[[]]",
            "[{}]",
            r#"[{"statisticType":null,"onStatisticField":null}]"#,
            r#"[{"statisticType":1,"onStatisticField":2}]"#,
            r#"[{"statisticType":"count","onStatisticField":""}]"#,
            r#"[{"statisticType":"count","onStatisticField":"pop","outStatisticFieldName":null}]"#,
            r#"[{"statisticType":"count","onStatisticField":"pop","outStatisticFieldName":""}]"#,
            r#"[{"statisticType":"count","onStatisticField":"pop","outStatisticFieldName":7}]"#,
            r#"[{"statisticType":" COUNT ","onStatisticField":" POP "}]"#,
            "\0",
            "[\u{202e}]",
        ] {
            for groups in [
                None,
                Some(""),
                Some(","),
                Some("ward"),
                Some("\0"),
                Some("*"),
            ] {
                check(raw, groups);
            }
        }

        let alphabet: Vec<char> =
            r#"[]{}",:0179 statisticTypeonSFieldNamecountsumavgminmaxstddevvarpopnameward\0"#
                .chars()
                .collect();
        let mut seed: u64 = 0x1234_5678_9abc_def1;
        let mut roll = |bound: usize| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) as usize % bound
        };
        for _ in 0..4000 {
            let length = roll(48);
            let raw: String = (0..length)
                .map(|_| alphabet[roll(alphabet.len())])
                .collect();
            check(&raw, None);
            check(&raw, Some(&raw));
        }
    }
}
