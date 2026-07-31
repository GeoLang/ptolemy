// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! The `where` parameter of an ArcGIS query, as the SQL-92 subset Esri clients
//! send.
//!
//! Two steps, and the split is what makes this safe: [`parse`] turns request
//! text into a tree whose only field references are fields the layer declared
//! and whose only values are Rust values, and [`Predicate::sql`] renders that
//! tree with every value as a bound parameter. No request text is ever rendered
//! into SQL, so a quote has nothing to break out of: the clause
//! `name = 'x''; delete everything'` is one string literal and compares as one.
//!
//! What this does not accept is refused by name rather than dropped, as
//! everywhere else on this facade: a filter that silently did not apply hands
//! the client rows it did not ask for. The parser is total, so every input is
//! either a predicate or a refusal naming what it could not read.
//!
//! Dates: the store holds a date as whatever text was written, and verne writes
//! RFC 3339 in UTC on migrated data, `2024-03-01T12:00:00Z`. A `DATE` or
//! `TIMESTAMP` literal is normalised to that shape and compared as text, which
//! orders correctly because RFC 3339 in one zone sorts lexicographically. A
//! dataset whose dates are written any other way will not compare the way a
//! client expects, and nothing here can tell that it is happening.

use super::{Bind, Kind, Layer};

/// The longest clause this parser will look at. Long enough for the object id
/// lists real clients send, short enough that a clause cannot be a way to make
/// the server build something huge.
const MAX_LENGTH: usize = 32768;

/// How deep parentheses and `NOT` may nest. The parser is recursive descent, so
/// this is what stands between a hostile clause and the stack.
const MAX_DEPTH: usize = 32;

/// A number as PostgreSQL will read one. Fixed text in the query, never a
/// client's: see [`Column::sql`].
const NUMBER: &str = "^[+-]?([0-9]+[.]?[0-9]*|[.][0-9]+)([eE][+-]?[0-9]+)?$";

/// Words that name something this parser does not implement. Each is refused by
/// name rather than read as a field: a field of that name would be a
/// coincidence, and reading it as one would answer the wrong rows.
const RESERVED: [(&str, &str); 12] = [
    ("select", "a subquery"),
    ("case", "a CASE expression"),
    ("when", "a CASE expression"),
    ("cast", "a CAST"),
    ("extract", "EXTRACT"),
    ("exists", "an EXISTS subquery"),
    ("any", "ANY"),
    ("all", "ALL"),
    ("some", "SOME"),
    ("escape", "a LIKE ESCAPE clause"),
    ("current_date", "CURRENT_DATE"),
    ("current_timestamp", "CURRENT_TIMESTAMP"),
];

/// Words this grammar reads as operators, so none of them can name a value.
const OPERATORS: [&str; 7] = ["and", "or", "not", "is", "in", "like", "between"];

// ─── The tree ───────────────────────────────────────────────────────

/// A parsed clause: a tree over the layer's own fields and Rust values.
///
/// `BETWEEN`, `NOT IN`, `NOT LIKE` and `IS NOT NULL` are not here. Each is
/// exactly what SQL defines it as, so [`Parser::condition`] builds that instead
/// and there is one less shape to render: `a BETWEEN x AND y` is
/// `a >= x AND a <= y`, and the three negated forms are `NOT` over the plain
/// one, three-valued logic included.
pub(super) enum Predicate {
    /// A comparison of two literals, worked out here rather than in the
    /// database. `1=1` is what an Esri client sends for "no filter", and that is
    /// nearly the only form of it that occurs. `None` is SQL's unknown.
    Const(Option<bool>),
    Not(Box<Predicate>),
    And(Vec<Predicate>),
    Or(Vec<Predicate>),
    Compare {
        column: Column,
        /// One of the six SQL spellings, from [`Parser`] and never from a
        /// client's text.
        op: &'static str,
        value: Value,
    },
    In {
        column: Column,
        values: List,
    },
    Like {
        column: Column,
        pattern: String,
    },
    IsNull {
        column: Column,
    },
}

/// A column to compare, as the layer declared it. The name is the layer's own
/// text and never the client's, which is what keeps it out of harm's way; the
/// kind is what decides whether the comparison runs on numbers or on text.
#[derive(Clone)]
pub(super) struct Column {
    name: String,
    kind: Kind,
}

/// A comparison's value, already in the shape its column is compared in. Text
/// is optional because `NULL` is a value a client can write and SQL compares
/// with, always to unknown.
pub(super) enum Value {
    Number(f64),
    Text(Option<String>),
}

/// The values an `IN` list holds, in one shape for the whole list.
pub(super) enum List {
    Numbers(Vec<f64>),
    Texts(Vec<Option<String>>),
}

impl Column {
    /// The column as SQL over the `numbered` CTE, in the shape it is compared
    /// in. That CTE selects the properties and the object id as bare columns, so
    /// there is no table alias to qualify with.
    ///
    /// A numeric comparison reads the value as a number only when it looks like
    /// one, so a dataset whose values disagree with its declared type answers no
    /// rows rather than failing the whole query: a schema can be declared after
    /// the rows were written.
    fn sql(&self, numeric: bool) -> String {
        match (self.kind, numeric) {
            // the object id is already a bigint in the CTE. float8 holds every
            // id below 2^53 exactly, which is every id anything here assigns
            (Kind::Oid, true) => "oid::float8".to_string(),
            (Kind::Oid, false) => "oid::text".to_string(),
            (_, true) => {
                let text = self.text();
                format!("(CASE WHEN {text} ~ '{NUMBER}' THEN ({text})::float8 END)")
            }
            (_, false) => self.text(),
        }
    }

    /// The property as text, escaped the way [`super::Oid::sql`] escapes it. The
    /// name came out of the layer's fields, so this is hygiene rather than the
    /// thing that keeps the clause safe.
    fn text(&self) -> String {
        format!("(properties->>'{}')", self.name.replace('\'', "''"))
    }
}

impl Predicate {
    /// The predicate as SQL over the `numbered` CTE, appending one bind per
    /// literal in the order the placeholders are numbered.
    ///
    /// Every value is a placeholder. Nothing a client wrote is rendered here,
    /// which is the whole point of parsing it first.
    pub(super) fn sql(&self, next: &mut i32, binds: &mut Vec<Bind>) -> String {
        match self {
            Predicate::Const(None) => "NULL".to_string(),
            Predicate::Const(Some(true)) => "TRUE".to_string(),
            Predicate::Const(Some(false)) => "FALSE".to_string(),
            Predicate::Not(inner) => format!("(NOT {})", inner.sql(next, binds)),
            Predicate::And(terms) => joined(terms, " AND ", next, binds),
            Predicate::Or(terms) => joined(terms, " OR ", next, binds),
            Predicate::Compare { column, op, value } => {
                let (numeric, bind, cast) = match value {
                    Value::Number(v) => (true, Bind::Number(*v), "float8"),
                    Value::Text(v) => (false, Bind::Text(v.clone()), "text"),
                };
                let place = placeholder(next, binds, bind);
                format!("({} {op} {place}::{cast})", column.sql(numeric))
            }
            Predicate::In { column, values } => {
                let (numeric, bind, cast) = match values {
                    List::Numbers(v) => (true, Bind::Numbers(v.clone()), "float8[]"),
                    List::Texts(v) => (false, Bind::Texts(v.clone()), "text[]"),
                };
                let place = placeholder(next, binds, bind);
                format!("({} = ANY({place}::{cast}))", column.sql(numeric))
            }
            Predicate::Like { column, pattern } => {
                let place = placeholder(next, binds, Bind::Text(Some(pattern.clone())));
                // ESCAPE '' turns off PostgreSQL's backslash escape, which
                // SQL-92 LIKE does not have: a backslash a client sent is a
                // backslash to match, not an escape for what follows it
                format!("({} LIKE {place}::text ESCAPE '')", column.sql(false))
            }
            Predicate::IsNull { column } => format!("({} IS NULL)", column.sql(false)),
        }
    }
}

/// The next placeholder, with its value bound. Every literal in a clause goes
/// through here, which is what keeps request data out of the SQL text.
fn placeholder(next: &mut i32, binds: &mut Vec<Bind>, bind: Bind) -> String {
    let at = *next;
    *next += 1;
    binds.push(bind);
    format!("${at}")
}

fn joined(terms: &[Predicate], with: &str, next: &mut i32, binds: &mut Vec<Bind>) -> String {
    let parts: Vec<String> = terms.iter().map(|term| term.sql(next, binds)).collect();
    format!("({})", parts.join(with))
}

// ─── Tokens ─────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum Token {
    /// A bare word: a keyword or a field name, as written.
    Word(String),
    /// A number as written, so a comparison against a text field can compare
    /// the spelling the client sent rather than one this parser chose.
    Number(String),
    /// A string literal, with `''` already read as one quote.
    Text(String),
    Open,
    Close,
    Comma,
    /// A comparison operator in its SQL spelling.
    Op(&'static str),
    /// An arithmetic character: a sign when a number follows it, and arithmetic
    /// anywhere else, which is refused.
    Sign(char),
}

impl Token {
    /// The token as an error message names it. Long text is cut: a refusal
    /// quotes what it could not read, and a clause may carry kilobytes of it.
    fn shown(&self) -> String {
        match self {
            Token::Word(word) => shown(word),
            Token::Number(number) => shown(number),
            Token::Text(text) => format!("'{}'", shown(text)),
            Token::Open => "(".to_string(),
            Token::Close => ")".to_string(),
            Token::Comma => ",".to_string(),
            Token::Op(op) => (*op).to_string(),
            Token::Sign(sign) => sign.to_string(),
        }
    }
}

/// Text as a refusal quotes it, cut short: a clause may carry kilobytes, and a
/// refusal that repeats all of it is a refusal nobody can read.
pub(super) fn shown(text: &str) -> String {
    const LIMIT: usize = 40;
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    let head: String = text.chars().take(LIMIT).collect();
    format!("{head}...")
}

fn arithmetic(sign: char) -> String {
    format!("arithmetic ('{sign}') is not supported in a where clause")
}

/// The clause as tokens, or the first thing in it that has no meaning here.
fn tokens(clause: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = clause.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        let next = chars.get(at + 1).copied();
        if c.is_whitespace() {
            at += 1;
            continue;
        }
        match c {
            '(' => {
                out.push(Token::Open);
                at += 1;
            }
            ')' => {
                out.push(Token::Close);
                at += 1;
            }
            ',' => {
                out.push(Token::Comma);
                at += 1;
            }
            '=' => {
                out.push(Token::Op("="));
                at += 1;
            }
            '<' => {
                let (op, width) = match next {
                    Some('>') => ("<>", 2),
                    Some('=') => ("<=", 2),
                    _ => ("<", 1),
                };
                out.push(Token::Op(op));
                at += width;
            }
            '>' => {
                let (op, width) = match next {
                    Some('=') => (">=", 2),
                    _ => (">", 1),
                };
                out.push(Token::Op(op));
                at += width;
            }
            '!' if next == Some('=') => {
                out.push(Token::Op("<>"));
                at += 2;
            }
            '-' if next == Some('-') => {
                return Err("a comment ('--') is not supported in a where clause".to_string());
            }
            '/' if next == Some('*') => {
                return Err("a comment ('/*') is not supported in a where clause".to_string());
            }
            '|' if next == Some('|') => {
                return Err("concatenation ('||') is not supported in a where clause".to_string());
            }
            '+' | '-' | '*' | '/' | '%' => {
                out.push(Token::Sign(c));
                at += 1;
            }
            '\'' => {
                let (text, end) = string_at(&chars, at)?;
                out.push(Token::Text(text));
                at = end;
            }
            '"' | '[' | ']' | '`' => {
                return Err(
                    "a quoted identifier is not supported in a where clause; a field name is \
                     bare here"
                        .to_string(),
                );
            }
            ';' => {
                return Err(
                    "';' is not supported in a where clause, which is one condition".to_string(),
                );
            }
            '{' | '}' => {
                return Err(
                    "the ODBC '{...}' escape is not supported in a where clause; a date literal \
                     is DATE 'yyyy-mm-dd'"
                        .to_string(),
                );
            }
            _ if c.is_ascii_digit() || (c == '.' && next.is_some_and(|d| d.is_ascii_digit())) => {
                let (number, end) = number_at(&chars, at);
                out.push(Token::Number(number));
                at = end;
            }
            _ if c.is_alphabetic() || c == '_' => {
                let start = at;
                while chars
                    .get(at)
                    .is_some_and(|held| held.is_alphanumeric() || *held == '_')
                {
                    at += 1;
                }
                out.push(Token::Word(chars[start..at].iter().collect()));
            }
            _ => return Err(format!("'{c}' has no meaning in a where clause")),
        }
    }
    Ok(out)
}

/// The string literal opening at `at`, and where it ended. `''` is one quote,
/// which is the only escape SQL-92 has and the one an Esri client sends.
fn string_at(chars: &[char], at: usize) -> Result<(String, usize), String> {
    let mut at = at + 1;
    let mut text = String::new();
    loop {
        match chars.get(at) {
            None => {
                return Err(
                    "an unterminated string literal: a where clause's quotes have to balance"
                        .to_string(),
                );
            }
            Some('\'') if chars.get(at + 1) == Some(&'\'') => {
                text.push('\'');
                at += 2;
            }
            Some('\'') => return Ok((text, at + 1)),
            Some(held) => {
                text.push(*held);
                at += 1;
            }
        }
    }
}

fn number_at(chars: &[char], at: usize) -> (String, usize) {
    let start = at;
    let mut at = at;
    let digits = |at: &mut usize| {
        while chars.get(*at).is_some_and(char::is_ascii_digit) {
            *at += 1;
        }
    };
    digits(&mut at);
    if chars.get(at) == Some(&'.') {
        at += 1;
        digits(&mut at);
    }
    // an exponent only when it is one: `1east` is a number and then a word
    if matches!(chars.get(at), Some('e' | 'E')) {
        let mut ahead = at + 1;
        if matches!(chars.get(ahead), Some('+' | '-')) {
            ahead += 1;
        }
        if chars.get(ahead).is_some_and(char::is_ascii_digit) {
            at = ahead;
            digits(&mut at);
        }
    }
    (chars[start..at].iter().collect(), at)
}

// ─── The parser ─────────────────────────────────────────────────────

/// One side of a comparison.
#[derive(Clone)]
enum Operand {
    Column(Column),
    Lit(Lit),
}

/// A literal, before a column has said which shape it is compared in.
#[derive(Clone)]
enum Lit {
    /// The value and the spelling it arrived as, because a number compared
    /// against a text field compares as the client wrote it.
    Number(f64, String),
    Text(String),
    Null,
}

impl Lit {
    /// The text this literal compares as: a number as its spelling, and `NULL`
    /// as SQL's NULL.
    fn text(&self) -> Option<String> {
        match self {
            Lit::Number(_, spelling) => Some(spelling.clone()),
            Lit::Text(text) => Some(text.clone()),
            Lit::Null => None,
        }
    }
}

/// Whether a field's declared type makes a numeric comparison the right one.
///
/// A number against a text field compares as text: the field holds text,
/// nothing declared its values to be numbers, and casting them to compare would
/// answer on a reading of the data the layer never published.
fn numeric_field(column: &Column) -> bool {
    matches!(column.kind, Kind::Oid | Kind::Integer | Kind::Double)
}

/// A literal as the value a column is compared against, in the shape that
/// column compares in.
fn value_of(column: &Column, lit: &Lit) -> Value {
    match lit {
        Lit::Number(value, _) if numeric_field(column) => Value::Number(*value),
        other => Value::Text(other.text()),
    }
}

/// A comparison of two literals. `None` is SQL's unknown, which any comparison
/// with NULL is.
fn evaluate(a: &Lit, op: &str, b: &Lit) -> Option<bool> {
    let order = match (a, b) {
        (Lit::Null, _) | (_, Lit::Null) => return None,
        (Lit::Number(x, _), Lit::Number(y, _)) => x.partial_cmp(y)?,
        _ => a.text()?.cmp(&b.text()?),
    };
    Some(match op {
        "=" => order.is_eq(),
        "<>" => order.is_ne(),
        "<" => order.is_lt(),
        "<=" => order.is_le(),
        ">" => order.is_gt(),
        _ => order.is_ge(),
    })
}

/// The same comparison written the other way round, for a clause that puts its
/// literal first.
fn flipped(op: &str) -> &'static str {
    match op {
        "<" => ">",
        "<=" => ">=",
        ">" => "<",
        ">=" => "<=",
        "<>" => "<>",
        _ => "=",
    }
}

fn comparison(left: Operand, op: &'static str, right: Operand) -> Result<Predicate, String> {
    match (left, right) {
        (Operand::Column(column), Operand::Lit(lit)) => {
            let value = value_of(&column, &lit);
            Ok(Predicate::Compare { column, op, value })
        }
        (Operand::Lit(lit), Operand::Column(column)) => {
            let value = value_of(&column, &lit);
            Ok(Predicate::Compare {
                column,
                op: flipped(op),
                value,
            })
        }
        (Operand::Lit(a), Operand::Lit(b)) => Ok(Predicate::Const(evaluate(&a, op, &b))),
        (Operand::Column(_), Operand::Column(_)) => Err(
            "comparing two fields is not supported in a where clause; one side has to be a value"
                .to_string(),
        ),
    }
}

/// A `DATE` or `TIMESTAMP` literal as the RFC 3339 UTC text the store holds. A
/// date with no time is midnight, the time may be separated by a space or a `T`,
/// and a trailing `Z` may already be there.
///
/// A fractional second or an offset is refused: this compares as text, so a
/// literal that does not look like what is stored would silently answer the
/// wrong rows.
fn date_text(raw: &str) -> Result<String, String> {
    let text = raw.trim();
    let bad = || {
        Err(format!(
            "a date literal is 'yyyy-mm-dd' or 'yyyy-mm-dd hh:mm:ss' in UTC, not '{}'",
            shown(text)
        ))
    };
    let bytes = text.as_bytes();
    if bytes.len() < 10
        || !digits(bytes, [0, 1, 2, 3, 5, 6, 8, 9])
        || bytes[4] != b'-'
        || bytes[7] != b'-'
    {
        return bad();
    }
    let date = &text[..10];
    let rest = &text[10..];
    if rest.is_empty() {
        return Ok(format!("{date}T00:00:00Z"));
    }
    let Some(rest) = rest
        .strip_prefix(' ')
        .or_else(|| rest.strip_prefix('T'))
        .or_else(|| rest.strip_prefix('t'))
    else {
        return bad();
    };
    let time = rest.trim();
    let time = time
        .strip_suffix('Z')
        .or_else(|| time.strip_suffix('z'))
        .unwrap_or(time)
        .trim_end();
    let bytes = time.as_bytes();
    if bytes.len() != 8
        || !digits(bytes, [0, 1, 3, 4, 6, 7])
        || bytes[2] != b':'
        || bytes[5] != b':'
    {
        return bad();
    }
    Ok(format!("{date}T{time}Z"))
}

fn digits<const N: usize>(bytes: &[u8], at: [usize; N]) -> bool {
    at.iter()
        .all(|index| bytes.get(*index).is_some_and(u8::is_ascii_digit))
}

/// The clause as a predicate over `layer`'s fields, or a refusal naming what
/// could not be read.
pub(super) fn parse(clause: &str, layer: &Layer) -> Result<Predicate, String> {
    if clause.len() > MAX_LENGTH {
        return Err(format!(
            "a where clause longer than {MAX_LENGTH} characters"
        ));
    }
    let tokens = tokens(clause)?;
    if tokens.is_empty() {
        return Err("an empty condition".to_string());
    }
    let mut parser = Parser {
        tokens,
        at: 0,
        layer,
        depth: 0,
    };
    let predicate = parser.or_expr()?;
    match parser.peek() {
        None => Ok(predicate),
        Some(Token::Close) => Err("a ')' with no '(' to open it".to_string()),
        Some(Token::Sign(sign)) => Err(arithmetic(*sign)),
        Some(token) => Err(format!(
            "'{}' where the condition had already ended",
            token.shown()
        )),
    }
}

struct Parser<'a> {
    tokens: Vec<Token>,
    at: usize,
    layer: &'a Layer,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    /// The token after the one being looked at. Two decisions need it: whether a
    /// word is a function call, and whether it is a date literal's keyword.
    fn after(&self) -> Option<&Token> {
        self.tokens.get(self.at + 1)
    }

    fn keyword(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(held)) if held.eq_ignore_ascii_case(word))
    }

    fn take(&mut self, word: &str) -> bool {
        let held = self.keyword(word);
        if held {
            self.at += 1;
        }
        held
    }

    /// One step further into the recursion, refused past [`MAX_DEPTH`]. Every
    /// caller pairs this with a decrement, so a clause of many shallow groups is
    /// not refused for the depth of the ones before it.
    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(format!("a condition nested deeper than {MAX_DEPTH}"));
        }
        Ok(())
    }

    fn or_expr(&mut self) -> Result<Predicate, String> {
        let mut terms = vec![self.and_expr()?];
        while self.take("or") {
            terms.push(self.and_expr()?);
        }
        Ok(if terms.len() == 1 {
            terms.swap_remove(0)
        } else {
            Predicate::Or(terms)
        })
    }

    fn and_expr(&mut self) -> Result<Predicate, String> {
        let mut terms = vec![self.not_expr()?];
        while self.take("and") {
            terms.push(self.not_expr()?);
        }
        Ok(if terms.len() == 1 {
            terms.swap_remove(0)
        } else {
            Predicate::And(terms)
        })
    }

    /// `NOT` binds tighter than `AND`, which binds tighter than `OR`.
    fn not_expr(&mut self) -> Result<Predicate, String> {
        if self.take("not") {
            self.enter()?;
            let inner = self.not_expr()?;
            self.depth -= 1;
            return Ok(Predicate::Not(Box::new(inner)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Predicate, String> {
        if matches!(self.peek(), Some(Token::Open)) {
            // a '(' here opens a group; an IN list's own '(' is read by `in_list`
            self.at += 1;
            self.enter()?;
            let inner = self.or_expr()?;
            self.depth -= 1;
            if !matches!(self.peek(), Some(Token::Close)) {
                return Err("a '(' with no ')' to close it".to_string());
            }
            self.at += 1;
            return Ok(inner);
        }
        self.condition()
    }

    /// One condition: an operand and whatever tests it.
    fn condition(&mut self) -> Result<Predicate, String> {
        let left = self.operand()?;

        if self.take("is") {
            let negated = self.take("not");
            if !self.take("null") {
                return Err(
                    "IS takes NULL or NOT NULL here; nothing else is supported in a where clause"
                        .to_string(),
                );
            }
            let predicate = match left {
                Operand::Column(column) => Predicate::IsNull { column },
                // `NULL IS NULL`, which is at least an answer
                Operand::Lit(lit) => Predicate::Const(Some(matches!(lit, Lit::Null))),
            };
            return Ok(negate(predicate, negated));
        }

        let negated = self.take("not");
        if self.take("in") {
            return self.in_list(left, negated);
        }
        if self.take("like") {
            return self.like(left, negated);
        }
        if self.take("between") {
            return self.between(left, negated);
        }
        if negated {
            return Err("NOT takes IN, LIKE or BETWEEN after a value".to_string());
        }

        let op = match self.peek() {
            Some(Token::Op(op)) => *op,
            Some(Token::Sign(sign)) => return Err(arithmetic(*sign)),
            Some(token) => {
                return Err(format!(
                    "'{}' where a comparison operator was expected",
                    token.shown()
                ));
            }
            None => return Err("a value with nothing to compare it to".to_string()),
        };
        self.at += 1;
        let right = self.operand()?;
        comparison(left, op, right)
    }

    fn in_list(&mut self, left: Operand, negated: bool) -> Result<Predicate, String> {
        let Operand::Column(column) = left else {
            return Err("IN needs a field on its left".to_string());
        };
        if !matches!(self.peek(), Some(Token::Open)) {
            return Err("IN takes a parenthesised list of values".to_string());
        }
        self.at += 1;
        if matches!(self.peek(), Some(Token::Close)) {
            return Err("an empty IN list".to_string());
        }
        let mut lits = Vec::new();
        loop {
            let Operand::Lit(lit) = self.operand()? else {
                return Err("IN takes a list of values, not a field".to_string());
            };
            lits.push(lit);
            match self.peek() {
                Some(Token::Comma) => self.at += 1,
                Some(Token::Close) => {
                    self.at += 1;
                    break;
                }
                Some(token) => {
                    return Err(format!(
                        "'{}' in an IN list, where ',' or ')' was expected",
                        token.shown()
                    ));
                }
                None => return Err("an IN list with no ')' to close it".to_string()),
            }
        }

        // one shape for the whole list: a list that is not all numbers compares
        // as text, and so does a list against a field that holds text
        let numbers: Option<Vec<f64>> = lits
            .iter()
            .map(|lit| match lit {
                Lit::Number(value, _) => Some(*value),
                _ => None,
            })
            .collect();
        let values = match numbers {
            Some(numbers) if numeric_field(&column) => List::Numbers(numbers),
            _ => List::Texts(lits.iter().map(Lit::text).collect()),
        };
        Ok(negate(Predicate::In { column, values }, negated))
    }

    fn like(&mut self, left: Operand, negated: bool) -> Result<Predicate, String> {
        let Operand::Column(column) = left else {
            return Err("LIKE needs a field on its left".to_string());
        };
        let pattern = match self.operand()? {
            Operand::Lit(Lit::Text(pattern)) => pattern,
            _ => return Err("LIKE takes a string pattern".to_string()),
        };
        if self.keyword("escape") {
            return Err("a LIKE ESCAPE clause is not supported in a where clause".to_string());
        }
        Ok(negate(Predicate::Like { column, pattern }, negated))
    }

    /// SQL defines `a BETWEEN x AND y` as `a >= x AND a <= y`, so that is what
    /// this builds, and `NOT BETWEEN` is `NOT` over it.
    fn between(&mut self, left: Operand, negated: bool) -> Result<Predicate, String> {
        let low = self.operand()?;
        if !self.take("and") {
            return Err("BETWEEN takes two values with AND between them".to_string());
        }
        let high = self.operand()?;
        let lower = comparison(left.clone(), ">=", low)?;
        let upper = comparison(left, "<=", high)?;
        Ok(negate(Predicate::And(vec![lower, upper]), negated))
    }

    fn operand(&mut self) -> Result<Operand, String> {
        if let Some(Token::Sign(sign)) = self.peek() {
            let sign = *sign;
            // a sign belongs to the number it precedes, which is how a client
            // writes `x >= -180`; anywhere else it is arithmetic
            if matches!(sign, '+' | '-')
                && let Some(Token::Number(spelling)) = self.after()
            {
                let spelling = format!("{sign}{spelling}");
                self.at += 2;
                return Ok(Operand::Lit(number(spelling)?));
            }
            return Err(arithmetic(sign));
        }

        let Some(token) = self.peek() else {
            return Err("a condition that ends where a value was expected".to_string());
        };
        match token {
            Token::Number(spelling) => {
                let spelling = spelling.clone();
                self.at += 1;
                Ok(Operand::Lit(number(spelling)?))
            }
            Token::Text(text) => {
                let text = text.clone();
                self.at += 1;
                Ok(Operand::Lit(Lit::Text(text)))
            }
            Token::Word(word) => {
                let word = word.clone();
                self.word_operand(&word)
            }
            Token::Open => Err(
                if matches!(self.after(), Some(Token::Word(word)) if word.eq_ignore_ascii_case("select"))
                {
                    "a subquery is not supported in a where clause".to_string()
                } else {
                    "a parenthesised value is not supported in a where clause".to_string()
                },
            ),
            Token::Close => Err("a ')' where a value was expected".to_string()),
            Token::Comma => Err("a ',' where a value was expected".to_string()),
            Token::Op(op) => Err(format!("'{op}' where a value was expected")),
            Token::Sign(sign) => Err(arithmetic(*sign)),
        }
    }

    /// A word where a value was expected: a function call, `NULL`, a date
    /// literal, something this parser does not implement, or a field.
    fn word_operand(&mut self, word: &str) -> Result<Operand, String> {
        if matches!(self.after(), Some(Token::Open)) {
            return Err(format!(
                "the function '{}(...)' is not supported in a where clause",
                shown(word)
            ));
        }
        if word.eq_ignore_ascii_case("null") {
            self.at += 1;
            return Ok(Operand::Lit(Lit::Null));
        }
        // DATE and TIMESTAMP name the literal that follows them, and a field of
        // either name is still a field, which is why the literal has to be there
        if (word.eq_ignore_ascii_case("date") || word.eq_ignore_ascii_case("timestamp"))
            && let Some(Token::Text(raw)) = self.after()
        {
            let text = date_text(raw)?;
            self.at += 2;
            return Ok(Operand::Lit(Lit::Text(text)));
        }
        if let Some((_, what)) = RESERVED
            .iter()
            .find(|(name, _)| word.eq_ignore_ascii_case(name))
        {
            return Err(format!("{what} is not supported in a where clause"));
        }
        if OPERATORS.iter().any(|held| word.eq_ignore_ascii_case(held)) {
            return Err(format!("'{word}' where a value was expected"));
        }
        let field = self
            .layer
            .field(word)
            .ok_or_else(|| format!("the layer has no field '{}'", shown(word)))?;
        self.at += 1;
        Ok(Operand::Column(Column {
            name: field.name.clone(),
            kind: field.kind,
        }))
    }
}

fn negate(predicate: Predicate, negated: bool) -> Predicate {
    if negated {
        Predicate::Not(Box::new(predicate))
    } else {
        predicate
    }
}

/// A number token as the literal it is. A spelling too big for a double becomes
/// an infinity, which compares as one rather than failing: it is still a number
/// the client wrote.
fn number(spelling: String) -> Result<Lit, String> {
    let value = spelling
        .parse::<f64>()
        .map_err(|_| format!("'{}' is not a number", shown(&spelling)))?;
    Ok(Lit::Number(value, spelling))
}

#[cfg(test)]
mod tests {
    use super::super::{Field, Oid};
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
    /// comparison should take.
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
                field("open", Kind::BooleanText),
                field("tags", Kind::JsonText),
                field("seen", Kind::Text),
            ],
            oid: Oid::RowNumber,
        }
    }

    /// The SQL and the binds a clause renders to, numbered from `$2` the way a
    /// query numbers them.
    fn rendered(clause: &str) -> (String, Vec<Bind>) {
        let layer = layer();
        let predicate = parse(clause, &layer).unwrap_or_else(|e| panic!("{clause}: {e}"));
        let mut next = 2;
        let mut binds = Vec::new();
        let sql = predicate.sql(&mut next, &mut binds);
        assert_eq!(next as usize, 2 + binds.len(), "{clause}: {sql}");
        (sql, binds)
    }

    fn sql_of(clause: &str) -> String {
        rendered(clause).0
    }

    fn refusal(clause: &str) -> String {
        match parse(clause, &layer()) {
            Ok(_) => panic!("'{clause}' was accepted"),
            Err(why) => why,
        }
    }

    #[test]
    fn a_text_comparison_binds_the_string_and_renders_a_placeholder() {
        let (sql, binds) = rendered("name = 'Boston'");
        assert_eq!(sql, "((properties->>'name') = $2::text)");
        assert_eq!(binds, vec![Bind::Text(Some("Boston".to_string()))]);
    }

    /// The clause that would be an injection if any of it were rendered. It is
    /// one string literal, so it compares as one and matches nothing.
    #[test]
    fn a_quote_in_a_literal_stays_in_the_literal() {
        // the DROP form of this is in the integration tests, which run it
        // against a real database and then prove the table is still there
        let (sql, binds) = rendered("name = 'x''; delete everything;--'");
        assert_eq!(sql, "((properties->>'name') = $2::text)");
        assert_eq!(
            binds,
            vec![Bind::Text(Some("x'; delete everything;--".to_string()))]
        );
        assert!(!sql.contains("delete"), "{sql}");
    }

    #[test]
    fn a_declared_number_compares_as_a_number() {
        let (sql, binds) = rendered("pop >= 1000");
        assert!(sql.contains("::float8"), "{sql}");
        assert!(sql.contains("(properties->>'pop')"), "{sql}");
        assert!(sql.ends_with(">= $2::float8)"), "{sql}");
        assert_eq!(binds, vec![Bind::Number(1000.0)]);
        // a decimal against a double, and a negative one
        assert!(sql_of("score < -1.5").contains("::float8"));
        assert_eq!(rendered("score < -1.5").1, vec![Bind::Number(-1.5)]);
    }

    /// A number against a field that holds text compares as text, as the
    /// spelling the client sent: nothing declared those values to be numbers.
    #[test]
    fn a_number_against_a_text_field_compares_as_text() {
        let (sql, binds) = rendered("name = 007");
        assert_eq!(sql, "((properties->>'name') = $2::text)");
        assert_eq!(binds, vec![Bind::Text(Some("007".to_string()))]);
        // and so do the two kinds that are text with a declared shape
        assert_eq!(sql_of("open = 1"), "((properties->>'open') = $2::text)");
        assert_eq!(sql_of("tags = 1"), "((properties->>'tags') = $2::text)");
    }

    #[test]
    fn the_object_id_compares_against_the_id_column() {
        assert_eq!(sql_of("objectid = 5"), "(oid::float8 = $2::float8)");
        assert_eq!(sql_of("OBJECTID <> 5"), "(oid::float8 <> $2::float8)");
        assert_eq!(sql_of("objectid = 'x'"), "(oid::text = $2::text)");
        assert_eq!(sql_of("objectid IS NULL"), "(oid::text IS NULL)");
    }

    /// A numeric comparison reads a value that is not a number as no value, so
    /// data that disagrees with its declared type answers no rows rather than
    /// failing the query.
    #[test]
    fn a_numeric_comparison_guards_the_cast() {
        let sql = sql_of("pop = 1");
        assert!(sql.contains("CASE WHEN (properties->>'pop') ~ "), "{sql}");
        assert!(
            sql.contains("THEN ((properties->>'pop'))::float8 END"),
            "{sql}"
        );
    }

    #[test]
    fn every_operator_spelling_is_read() {
        for (clause, op) in [
            ("pop = 1", "="),
            ("pop <> 1", "<>"),
            ("pop != 1", "<>"),
            ("pop < 1", "<"),
            ("pop <= 1", "<="),
            ("pop > 1", ">"),
            ("pop >= 1", ">="),
        ] {
            let sql = sql_of(clause);
            assert!(
                sql.contains(&format!(" {op} $2::float8)")),
                "{clause}: {sql}"
            );
        }
    }

    /// A client that writes the literal first means the same comparison, so the
    /// operator is turned round rather than the clause refused.
    #[test]
    fn a_literal_on_the_left_flips_the_operator() {
        assert!(
            sql_of("1000 < pop").ends_with("> $2::float8)"),
            "{}",
            sql_of("1000 < pop")
        );
        assert!(sql_of("1000 >= pop").ends_with("<= $2::float8)"));
        assert!(sql_of("'a' = name").ends_with("= $2::text)"));
    }

    /// `1=1` is what an Esri client sends for "no filter", and it is a
    /// comparison of two literals like any other.
    #[test]
    fn two_literals_are_compared_here() {
        assert_eq!(sql_of("1=1"), "TRUE");
        assert_eq!(sql_of("1 = 1"), "TRUE");
        assert_eq!(sql_of("1=0"), "FALSE");
        assert_eq!(sql_of("'a' < 'b'"), "TRUE");
        assert_eq!(sql_of("2 <= 1"), "FALSE");
        // a comparison with NULL is unknown, and unknown selects nothing
        assert_eq!(sql_of("1 = NULL"), "NULL");
        assert_eq!(sql_of("NULL IS NULL"), "TRUE");
    }

    /// `= NULL` never matches. That is what IS NULL is for, and it is what SQL
    /// does with a NULL on either side of a comparison.
    #[test]
    fn comparing_a_field_with_null_is_a_null_bind() {
        let (sql, binds) = rendered("name = NULL");
        assert_eq!(sql, "((properties->>'name') = $2::text)");
        assert_eq!(binds, vec![Bind::Text(None)]);
    }

    #[test]
    fn is_null_and_is_not_null() {
        assert_eq!(sql_of("name IS NULL"), "((properties->>'name') IS NULL)");
        assert_eq!(
            sql_of("name IS NOT NULL"),
            "(NOT ((properties->>'name') IS NULL))"
        );
        assert_eq!(sql_of("name is null"), "((properties->>'name') IS NULL)");
    }

    #[test]
    fn in_takes_one_shape_for_the_whole_list() {
        let (sql, binds) = rendered("pop IN (1, 2, 3)");
        assert!(sql.ends_with("= ANY($2::float8[]))"), "{sql}");
        assert_eq!(binds, vec![Bind::Numbers(vec![1.0, 2.0, 3.0])]);

        let (sql, binds) = rendered("name IN ('a', 'b')");
        assert_eq!(sql, "((properties->>'name') = ANY($2::text[]))");
        assert_eq!(
            binds,
            vec![Bind::Texts(vec![
                Some("a".to_string()),
                Some("b".to_string())
            ])]
        );

        // one value that is not a number makes the whole list text, and a NULL
        // in a list stays a NULL so that NOT IN behaves as SQL says
        let (sql, binds) = rendered("pop IN (1, 'x', NULL)");
        assert_eq!(sql, "((properties->>'pop') = ANY($2::text[]))");
        assert_eq!(
            binds,
            vec![Bind::Texts(vec![
                Some("1".to_string()),
                Some("x".to_string()),
                None
            ])]
        );

        assert_eq!(
            sql_of("name NOT IN ('a')"),
            "(NOT ((properties->>'name') = ANY($2::text[])))"
        );
    }

    #[test]
    fn like_keeps_the_wildcards_and_turns_off_the_escape_character() {
        let (sql, binds) = rendered("name LIKE 'a%b_c'");
        assert_eq!(sql, "((properties->>'name') LIKE $2::text ESCAPE '')");
        assert_eq!(binds, vec![Bind::Text(Some("a%b_c".to_string()))]);
        assert_eq!(
            sql_of("name NOT LIKE 'a%'"),
            "(NOT ((properties->>'name') LIKE $2::text ESCAPE ''))"
        );
        // a backslash is a character to match, not an escape
        assert_eq!(
            rendered(r"name LIKE 'a\%'").1,
            vec![Bind::Text(Some(r"a\%".to_string()))]
        );
    }

    #[test]
    fn between_is_two_comparisons() {
        let (sql, binds) = rendered("pop BETWEEN 10 AND 20");
        assert!(sql.contains(" >= $2::float8)"), "{sql}");
        assert!(sql.contains(" <= $3::float8)"), "{sql}");
        assert!(sql.contains(" AND "), "{sql}");
        assert_eq!(binds, vec![Bind::Number(10.0), Bind::Number(20.0)]);

        let negated = sql_of("pop NOT BETWEEN 10 AND 20");
        assert!(negated.starts_with("(NOT ("), "{negated}");
    }

    #[test]
    fn not_binds_tighter_than_and_which_binds_tighter_than_or() {
        // NOT takes the one condition after it, not the AND around it
        let sql = sql_of("NOT pop = 1 AND name = 'a'");
        assert!(sql.starts_with("((NOT ("), "{sql}");
        assert!(
            sql.ends_with("AND ((properties->>'name') = $3::text))"),
            "{sql}"
        );
        // AND groups inside OR
        let sql = sql_of("name = 'a' OR name = 'b' AND name = 'c'");
        assert_eq!(
            sql,
            "(((properties->>'name') = $2::text) OR (((properties->>'name') = $3::text) AND ((properties->>'name') = $4::text)))"
        );
        // and parentheses override it
        let sql = sql_of("(name = 'a' OR name = 'b') AND name = 'c'");
        assert_eq!(
            sql,
            "((((properties->>'name') = $2::text) OR ((properties->>'name') = $3::text)) AND ((properties->>'name') = $4::text))"
        );
        // a chain of one operator is one flat list, in the order it was written
        let (sql, binds) = rendered("name='a' AND name='b' AND name='c'");
        assert_eq!(sql.matches(" AND ").count(), 2, "{sql}");
        assert_eq!(binds.len(), 3);
        assert_eq!(binds[0], Bind::Text(Some("a".to_string())));
        assert_eq!(binds[2], Bind::Text(Some("c".to_string())));
    }

    #[test]
    fn keywords_are_read_in_any_case() {
        for clause in [
            "name is null and pop > 1",
            "NAME IS NULL AND POP > 1",
            "Name Is Null And Pop > 1",
            "name in ('a') or name like 'b%'",
            "pop between 1 and 2",
            "name not like 'a'",
        ] {
            assert!(parse(clause, &layer()).is_ok(), "{clause}");
        }
    }

    #[test]
    fn a_field_name_matches_without_regard_to_case() {
        // the name in the SQL is the layer's, not the spelling the client sent
        assert_eq!(sql_of("NaMe IS NULL"), "((properties->>'name') IS NULL)");
    }

    #[test]
    fn a_date_literal_becomes_the_utc_text_the_store_holds() {
        assert_eq!(
            rendered("seen >= DATE '2024-03-01'").1,
            vec![Bind::Text(Some("2024-03-01T00:00:00Z".to_string()))]
        );
        assert_eq!(
            rendered("seen < TIMESTAMP '2024-03-01 12:30:00'").1,
            vec![Bind::Text(Some("2024-03-01T12:30:00Z".to_string()))]
        );
        assert_eq!(
            rendered("seen < timestamp '2024-03-01T12:30:00Z'").1,
            vec![Bind::Text(Some("2024-03-01T12:30:00Z".to_string()))]
        );
        // a date compares as text even against a field declared numeric
        assert_eq!(
            sql_of("pop > DATE '2024-03-01'"),
            "((properties->>'pop') > $2::text)"
        );
        // what it will not guess at
        for clause in [
            "seen > DATE '2024-03-01T12:30:00+02:00'",
            "seen > DATE '2024-03-01 12:30:00.123'",
            "seen > DATE '01/03/2024'",
            "seen > DATE 'yesterday'",
            "seen > DATE ''",
        ] {
            assert!(refusal(clause).contains("date literal"), "{clause}");
        }
    }

    /// A field called `date` is a field, because the keyword form needs the
    /// literal right after it.
    #[test]
    fn date_is_still_a_field_name() {
        let mut layer = layer();
        layer.fields.push(field("date", Kind::Text));
        let predicate = parse("date = '2024-03-01'", &layer).unwrap();
        let mut next = 2;
        let mut binds = Vec::new();
        assert_eq!(
            predicate.sql(&mut next, &mut binds),
            "((properties->>'date') = $2::text)"
        );
    }

    /// A property whose name carries a quote cannot break the SQL it is
    /// rendered into: the name is escaped the way the object id column is.
    #[test]
    fn a_field_name_with_a_quote_is_escaped() {
        let mut layer = layer();
        layer.fields.push(field("it's", Kind::Text));
        let predicate = parse("it's IS NULL", &layer);
        // the tokenizer reads `it` and then an unterminated string, which is
        // what a client sending that name gets: there is no way to write it
        assert!(predicate.is_err());
        // reached the other way, through a comparison the parser does build
        let column = Column {
            name: "it's".to_string(),
            kind: Kind::Text,
        };
        assert_eq!(column.text(), "(properties->>'it''s')");
    }

    #[test]
    fn an_unknown_field_is_refused_by_name() {
        assert!(refusal("nosuchfield = 1").contains("nosuchfield"),);
        assert!(refusal("nosuchfield = 1").contains("no field"));
    }

    #[test]
    fn what_it_will_not_read_is_refused_by_name() {
        for (clause, names) in [
            ("upper(name) = 'A'", "upper"),
            ("EXTRACT(year FROM seen) = 2024", "EXTRACT"),
            ("CAST(pop AS text) = '1'", "CAST"),
            ("pop = (SELECT 1)", "subquery"),
            ("pop IN (SELECT pop FROM x)", "subquery"),
            ("CASE WHEN pop = 1 THEN 1 END = 1", "CASE"),
            ("pop + 1 = 2", "arithmetic"),
            ("pop = 1 * 2", "arithmetic"),
            ("pop / 2 = 1", "arithmetic"),
            ("name LIKE 'a' ESCAPE '!'", "ESCAPE"),
            ("\"name\" = 'a'", "quoted identifier"),
            ("[name] = 'a'", "quoted identifier"),
            ("name = 'a'; drop", "';'"),
            ("name = 'a' -- rest", "comment"),
            ("name = 'a' /* rest */", "comment"),
            ("name || 'a' = 'b'", "concatenation"),
            ("name = 'unterminated", "unterminated"),
            ("name = {ts '2024-03-01'}", "ODBC"),
            ("name = name", "two fields"),
            ("name IS TRUE", "IS takes NULL"),
            ("pop BETWEEN 1", "BETWEEN takes two"),
            ("pop IN ()", "empty IN list"),
            ("pop IN (1", "no ')'"),
            ("(pop = 1", "no ')'"),
            ("pop = 1)", "no '('"),
            ("pop = 1 name = 2", "already ended"),
            ("pop", "nothing to compare"),
            ("pop =", "ends where a value"),
            ("AND pop = 1", "where a value was expected"),
            ("pop NOT = 1", "NOT takes IN, LIKE or BETWEEN"),
            ("name LIKE 5", "string pattern"),
            ("1 IN (1)", "IN needs a field"),
            ("'a' LIKE 'a'", "LIKE needs a field"),
            ("pop @ 1", "'@' has no meaning"),
            ("pop = 1 AND", "ends where a value"),
        ] {
            let why = refusal(clause);
            assert!(
                why.contains(names),
                "'{clause}' was refused as '{why}', which does not name {names}"
            );
        }
    }

    /// Recursive descent on request text needs a floor, and the floor is a
    /// refusal rather than the stack.
    #[test]
    fn nesting_is_capped() {
        let deep = format!("{}pop=1{}", "(".repeat(2000), ")".repeat(2000));
        assert!(refusal(&deep).contains("nested deeper"));
        let nots = format!("{}pop=1", "NOT ".repeat(2000));
        assert!(refusal(&nots).contains("nested deeper"));
        // and what fits still parses
        let shallow = format!("{}pop=1{}", "(".repeat(30), ")".repeat(30));
        assert!(parse(&shallow, &layer()).is_ok());
    }

    /// A chain is a flat list rather than a tree, so a long one costs no depth
    /// in the parser, the renderer or the drop.
    #[test]
    fn a_long_chain_is_flat() {
        let clause = std::iter::repeat_n("pop=1", 2000)
            .collect::<Vec<_>>()
            .join(" AND ");
        let (sql, binds) = rendered(&clause);
        assert_eq!(binds.len(), 2000);
        assert_eq!(sql.matches(" AND ").count(), 1999);
    }

    #[test]
    fn a_clause_longer_than_the_cap_is_refused() {
        let long = format!("name = '{}'", "a".repeat(MAX_LENGTH));
        assert!(refusal(&long).contains("longer than"));
    }

    #[test]
    fn a_huge_number_is_a_number() {
        let (_, binds) = rendered(&format!("pop > {}", "9".repeat(400)));
        assert_eq!(binds, vec![Bind::Number(f64::INFINITY)]);
        // and against a text field it is the text the client wrote
        let (_, binds) = rendered("name > 99999999999999999999999999");
        assert_eq!(
            binds,
            vec![Bind::Text(Some("99999999999999999999999999".to_string()))]
        );
    }

    #[test]
    fn unicode_is_a_literal_like_any_other() {
        let (sql, binds) = rendered("name = 'Köln 東京 🗺'");
        assert_eq!(sql, "((properties->>'name') = $2::text)");
        assert_eq!(binds, vec![Bind::Text(Some("Köln 東京 🗺".to_string()))]);
        // a unicode field name is a field name, since a property key can be one
        let mut layer = layer();
        layer.fields.push(field("café", Kind::Text));
        assert!(parse("café IS NULL", &layer).is_ok());
    }

    /// The clause is request text, so the parser has to be total: every input
    /// is a predicate or a refusal, and never a panic.
    #[test]
    fn nothing_panics_whatever_arrives() {
        let layer = layer();
        let check = |clause: &str| {
            if let Ok(predicate) = parse(clause, &layer) {
                let mut next = 2;
                let mut binds = Vec::new();
                let sql = predicate.sql(&mut next, &mut binds);
                assert_eq!(next as usize, 2 + binds.len(), "{clause}: {sql}");
            }
        };

        // every prefix of a clause that uses the whole grammar
        let whole = "NOT (pop BETWEEN -1.5e2 AND 20) AND name IN ('a','b') OR seen >= DATE \
                     '2024-03-01' AND name LIKE '%x_' AND tags IS NOT NULL";
        for end in whole.char_indices().map(|(at, _)| at).chain([whole.len()]) {
            check(&whole[..end]);
        }

        // the shapes that break a parser, by hand
        for clause in [
            "",
            " ",
            "''",
            "'",
            "''''",
            "((((",
            "))))",
            ",,,,",
            "= = =",
            "NOT",
            "NOT NOT",
            "IS NULL",
            "IN ()",
            "BETWEEN AND",
            "pop BETWEEN AND 2",
            "pop IN (,)",
            "pop IN (1,,2)",
            "DATE",
            "DATE DATE",
            "TIMESTAMP ''",
            "pop = .",
            "pop = .5",
            "pop = 5.",
            "pop = 1e",
            "pop = 1e+",
            "pop = 1e999999999",
            "pop = --1",
            "pop = -'a'",
            "pop = +",
            "\0",
            "name = '\0'",
            "\u{202e}",
            "name = 'a' AND 'a",
            "seen > DATE '2024-03-01",
        ] {
            check(clause);
        }

        // and a deterministic sweep over the characters that mean something
        let alphabet: Vec<char> = "ab019 '()=<>!,-+*/%._NOTANDORISNULLIKEBETWEENDATE\"[];\\|éö"
            .chars()
            .collect();
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
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
}
