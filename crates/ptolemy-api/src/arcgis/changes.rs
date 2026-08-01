// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! `extractChanges`: what changed on the dataset's `main` branch since a
//! generation the client already holds.
//!
//! A layer's generation is a point on its own event clock: the epoch milliseconds
//! of the latest thing that happened on `main`, which is the newest of the head
//! changeset's time and the times attachments on the branch were created and
//! deleted at. A branch nothing has happened to is at 0. It only ever grows, as
//! long as time does, so it is a cursor a client can write down and send back.
//!
//! A clock rather than a count of commits, because uploading an attachment
//! commits no changeset. Counting commits made every attachment invisible to the
//! cursor: a client that loaded features in one commit and then uploaded the
//! attachments recorded a generation whose changeset predated every one of them,
//! so the next delta reported them all as adds and duplicated them, while one of
//! them deleted later was created and deleted inside that same window and was
//! reported in neither list, staying forever.
//!
//! A window from generation G is therefore half open, `(G, the clock at the
//! submit]`, over both kinds of event. The features in it are the store's own
//! diff from the deepest changeset the client already held, which is the newest
//! one at or before G, to the head the submit pinned. A generation naming no
//! changeset that early diffs from nothing.
//!
//! Only a layer with a real objectid column publishes any of this. A row-number
//! layer's ids shift when a feature is deleted, so a list of the ids that changed
//! would point the client at whatever moved into their place: the same reason
//! `applyEdits` refuses one. Nor does a dataset whose rows come from a table
//! ptolemy does not own, because ptolemy's changesets say nothing about what
//! happens in that table.
//!
//! The job is stateless. The protocol is submit, poll, fetch, and there is
//! nothing here worth a job table: the job id carries the head the window is
//! pinned to and the two generations it runs between, the poll always answers
//! `Completed`, and the fetch recomputes the diff.
//!
//! A change file's features carry the object id and nothing else. The consumer
//! this exists for reads the ids and fetches the rows themselves through
//! `/query`, so a geometry here would be bytes nobody reads.
//!
//! The attachments in the window are the ones the tombstone migration 026 put on
//! the table made visible: one created inside it and still there when it closed
//! is an add, one that was already there when it opened and went inside it is a
//! `deleteId`, and one created and deleted inside it cancels, because the client
//! never held it. Every comparison is on the same clock the generation is, so the
//! boundary is exact rather than approximate: both a changeset's time and an
//! attachment's now come from the database, and one instant is always the same
//! integer, being truncated to the millisecond rather than rounded.
//!
//! `updates` is always empty. Replacing an attachment here is a delete and an
//! upload, which mints a new uuid, so the pair is reported as those two things
//! and the consumer applies them in that order to the same effect.
//!
//! Both ends of the window are fixed at the submit, which is what makes a
//! generation a cursor that neither duplicates nor loses: the client records the
//! generation the change file reports, the next window opens exactly there, and an
//! event that lands between the submit and the fetch belongs to that next one.
//!
//! The route table stays in the parent module, as the attachment routes do.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Form, Path, Query, State},
    http::HeaderMap,
};
use ptolemy_core::diff::DiffOp;
use serde_json::{Value, json};
use sqlx::Row;
use sqlx::postgres::PgRow;
use uuid::Uuid;

use super::{
    EsriError, LAYER_ID, Layer, NEEDS_A_REAL_OID, Oid, Params, attachments, base_url,
    oids_by_feature, require_esri_json, resolve, service_url, shown,
};
use crate::{AppState, auth::Actor};

/// The message a dataset backed by a foreign table is refused with. Its rows
/// change where ptolemy cannot see it, so it has no generation to hand out and a
/// window between two of ptolemy's changesets would describe none of its edits.
const NO_CHANGESETS: &str = "reads a table ptolemy does not own, so its rows change outside \
     ptolemy's history and there is no generation window to answer for. Read it through /query, \
     which always answers the table's current rows.";

// ─── The event clock ────────────────────────────────────────────────

/// A timestamp as the generation it is: epoch milliseconds, truncated rather than
/// rounded, so one instant is always the same integer and a comparison against a
/// generation is exact in both directions. Fixed text over a column name this
/// module writes, never a client's.
fn millis_of(column: &str) -> String {
    format!("floor(EXTRACT(EPOCH FROM {column}) * 1000)::bigint")
}

/// The branch's event clock: the latest thing that happened on it, as a
/// generation, and 0 for a branch nothing has happened to.
///
/// Three kinds of event, because those are the three a change file reports: the
/// head changeset, which is the newest one on the chain, and an attachment on the
/// branch being created or deleted. A client reads this off the service root and
/// sends it back, so an attachment uploaded after the last commit still moves the
/// cursor past itself.
async fn event_clock(store: &AppState, branch_id: Uuid) -> Result<i64, EsriError> {
    let row = sqlx::query(&format!(
        "SELECT GREATEST(
             COALESCE((SELECT {head} FROM changesets c
                         JOIN branches b ON b.head = c.id
                        WHERE b.id = $1), 0),
             COALESCE((SELECT max({created}) FROM attachments WHERE branch_id = $1), 0),
             COALESCE((SELECT max({deleted}) FROM attachments WHERE branch_id = $1), 0)
         ) AS clock",
        head = millis_of("c.created_at"),
        created = millis_of("created_at"),
        deleted = millis_of("deleted_at"),
    ))
    .bind(branch_id)
    .fetch_one(store.read_pool())
    .await?;
    Ok(row.get("clock"))
}

/// One changeset on `main`, and the generation it sits at.
struct Commit {
    id: Uuid,
    at: i64,
}

/// `main`'s changeset chain, head first and the root last.
///
/// The whole chain rather than one lookup, because one walk answers all three
/// questions asked of it: which changeset a generation's diff runs from, when the
/// branch's first event was, and whether a changeset a client sent back is on this
/// branch at all.
struct Chain(Vec<Commit>);

impl Chain {
    async fn of(store: &AppState, branch_id: Uuid) -> Result<Chain, EsriError> {
        let rows = sqlx::query(&format!(
            "WITH RECURSIVE chain AS (
                 SELECT c.id, c.parent_id, c.created_at, 0 AS behind
                   FROM changesets c
                   JOIN branches b ON b.head = c.id
                  WHERE b.id = $1
               UNION ALL
                 SELECT c.id, c.parent_id, c.created_at, ch.behind + 1
                   FROM changesets c
                   JOIN chain ch ON ch.parent_id = c.id
             )
             SELECT id, {at} AS at FROM chain ORDER BY behind",
            at = millis_of("created_at"),
        ))
        .bind(branch_id)
        .fetch_all(store.read_pool())
        .await?;
        Ok(Chain(
            rows.iter()
                .map(|row| Commit {
                    id: row.get("id"),
                    at: row.get("at"),
                })
                .collect(),
        ))
    }

    fn head(&self) -> Option<Uuid> {
        self.0.first().map(|held| held.id)
    }

    /// The generation the branch's first commit sits at, which is the earliest one
    /// any window here can open at. `None` for a branch with no commit.
    fn root_at(&self) -> Option<i64> {
        self.0.last().map(|held| held.at)
    }

    /// The changeset a window opening at `generation` runs its diff from: the
    /// deepest one at or before it, which is the newest state the client already
    /// holds. `None` when the branch had no commit that early, and the diff then
    /// starts from nothing.
    ///
    /// The chain is newest first and a commit is always younger than its parent,
    /// so the first one at or before the generation is the deepest one.
    fn base(&self, generation: i64) -> Option<Uuid> {
        self.0
            .iter()
            .find(|held| held.at <= generation)
            .map(|held| held.id)
    }

    /// Whether a changeset is on this branch's own chain. What makes a job id
    /// untrusted data rather than a lookup key.
    fn holds(&self, changeset: Uuid) -> bool {
        self.0.iter().any(|held| held.id == changeset)
    }
}

/// Whether ptolemy's changesets describe this layer's rows and its object ids
/// name the same feature tomorrow. Both are needed before a generation means
/// anything.
async fn trackable(store: &AppState, layer: &Layer) -> Result<bool, EsriError> {
    if !matches!(layer.oid, Oid::Property(_)) {
        return Ok(false);
    }
    Ok(!external(store, layer.dataset_id).await?)
}

/// The field a change file names rows by, or the refusal a layer that cannot be
/// tracked gets. Refused by name for the same reason a query parameter is: a
/// client that got an empty change file would read it as "nothing changed".
async fn tracked_field<'a>(store: &AppState, layer: &'a Layer) -> Result<&'a str, EsriError> {
    let Oid::Property(field) = &layer.oid else {
        return Err(EsriError::bad_request(format!(
            "'{}' {NEEDS_A_REAL_OID}",
            layer.name
        )));
    };
    if external(store, layer.dataset_id).await? {
        return Err(EsriError::bad_request(format!(
            "'{}' {NO_CHANGESETS}",
            layer.name
        )));
    }
    Ok(field)
}

async fn external(store: &AppState, dataset_id: Uuid) -> Result<bool, EsriError> {
    let row = sqlx::query(
        "SELECT external_table IS NOT NULL AS foreign_rows FROM datasets WHERE id = $1",
    )
    .bind(dataset_id)
    .fetch_one(store.read_pool())
    .await?;
    Ok(row.get("foreign_rows"))
}

/// What the service root publishes about change tracking, or `None` for a layer
/// that cannot be tracked, whose root says nothing about it at all.
///
/// `minServerGen` is the clock like `serverGen`: every generation back to the
/// branch's first commit is still answerable, because the history it is read off
/// is never pruned here, but a client is told the current one so it starts from
/// where the service is now rather than from the beginning of time.
pub(super) async fn tracking_info(
    store: &AppState,
    layer: &Layer,
) -> Result<Option<Value>, EsriError> {
    if !trackable(store, layer).await? {
        return Ok(None);
    }
    let clock = event_clock(store, layer.branch_id).await?;
    Ok(Some(json!({
        "lastSyncDate": Value::Null,
        "layerServerGens": [{"id": 0, "minServerGen": clock, "serverGen": clock}],
    })))
}

// ─── Job ids ────────────────────────────────────────────────────────

/// The whole of a submitted request: the head the window is pinned to, `None`
/// for a branch that had no commit when it was submitted, the generation the
/// window opens at, and the clock it closes at.
///
/// Both ends are fixed here rather than at the fetch. The head pins which commits
/// the diff covers, and `at` pins which attachment events do and is the generation
/// the change file reports back, so the next window opens exactly where this one
/// closed: nothing is reported twice and nothing falls between two of them.
///
/// It travels in the job id, base64url so a client handles one opaque token, and
/// the server keeps nothing. It carries no secret and it is not signed, because
/// nothing about it is trusted: the changeset has to be on the resolved layer's
/// own chain, the generation has to be one this service would have issued, and the
/// clock cannot be ahead of the layer's own. A tampered id therefore cannot reach
/// another dataset's history, and a malformed one is a refusal rather than a panic.
#[derive(Debug, PartialEq)]
struct Job {
    head: Option<Uuid>,
    generation: i64,
    at: i64,
}

impl Job {
    fn encode(&self) -> String {
        use base64::Engine;
        let head = self.head.map(|id| id.to_string()).unwrap_or_default();
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!("{head}:{}:{}", self.generation, self.at))
    }

    fn decode(text: &str) -> Result<Job, EsriError> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(text)
            .map_err(|_| unknown_job(text))?;
        let held = String::from_utf8(bytes).map_err(|_| unknown_job(text))?;
        // three fields exactly, so an id issued by the version of this service
        // that counted commits is refused rather than read as a clock
        let [head, generation, at] = held.split(':').collect::<Vec<&str>>()[..] else {
            return Err(unknown_job(text));
        };
        let head = match head {
            "" => None,
            named => Some(Uuid::parse_str(named).map_err(|_| unknown_job(text))?),
        };
        let generation: i64 = generation.parse().map_err(|_| unknown_job(text))?;
        let at: i64 = at.parse().map_err(|_| unknown_job(text))?;
        if generation < 0 || at < generation {
            return Err(unknown_job(text));
        }
        Ok(Job {
            head,
            generation,
            at,
        })
    }
}

/// One answer for every id this service did not issue for this layer, whatever
/// is wrong with it: a client can do nothing with the difference, and telling it
/// apart would say which changesets exist.
fn unknown_job(text: &str) -> EsriError {
    EsriError::bad_request(format!(
        "'{}' is not a job this service issued for this layer. Submit extractChanges again \
         to get one.",
        shown(text)
    ))
}

// ─── extractChanges ─────────────────────────────────────────────────

/// Extract parameters that change which edits the answer covers. Each is
/// accepted as `true`, which is what the answer already does, and refused
/// otherwise rather than ignored: a client that asked for updates alone and got
/// the inserts too has been answered a question it did not ask.
const EDIT_KINDS: [&str; 3] = ["returnInserts", "returnUpdates", "returnDeletes"];

pub(super) async fn extract_changes(
    State(store): State<AppState>,
    actor: Actor,
    headers: HeaderMap,
    Path(service): Path<String>,
    Form(params): Form<Vec<(String, String)>>,
) -> Result<Json<Value>, EsriError> {
    let params = Params(params);
    require_esri_json(&params)?;
    check_layers(&params)?;
    check_data_format(&params)?;
    for name in EDIT_KINDS {
        if params.get(name).is_some() && !params.flag(name)? {
            return Err(EsriError::bad_request(format!(
                "{name}=false is not supported: a change file here holds every edit in the \
                 window, so the answer cannot leave one kind out"
            )));
        }
    }
    // returnIdsOnly is read and not refused: a change file here carries the
    // object ids and nothing else either way, so both answers are the same one

    let layer = resolve(&store, &actor, &service, LAYER_ID).await?;
    tracked_field(&store, &layer).await?;
    let chain = Chain::of(&store, layer.branch_id).await?;
    let asked = requested_generation(&params)?;
    let clock = event_clock(&store, layer.branch_id).await?;
    if asked > clock {
        return Err(EsriError::bad_request(format!(
            "serverGen {asked} is ahead of this layer, which is at generation {clock}. Ask for \
             {clock} or a generation behind it."
        )));
    }
    check_on_the_clock(asked, &chain)?;

    let job = Job {
        head: chain.head(),
        generation: asked,
        at: clock,
    };
    Ok(Json(json!({
        "statusUrl": under_service(&headers, &service, "jobs", &job.encode()),
    })))
}

/// Whether a generation is one this service's clock could have issued.
///
/// A generation below the branch's first commit is refused rather than answered.
/// An earlier version of this service counted commits, so the cursor a client
/// recorded then is a small number like 4, which as a clock reading is 1970 and
/// names no changeset to diff from: the window would open at nothing and answer
/// every row and every attachment the layer has as an add, which is the
/// duplication this clock exists to stop. Told apart from a legitimate 0, which is
/// the clock a branch nothing has happened to publishes and a full extraction.
fn check_on_the_clock(generation: i64, chain: &Chain) -> Result<(), EsriError> {
    let Some(root) = chain.root_at() else {
        return Ok(());
    };
    if generation == 0 || generation >= root {
        return Ok(());
    }
    Err(EsriError::bad_request(format!(
        "serverGen {generation} predates this layer's clock, which starts at {root}. A generation \
         is the epoch milliseconds of the last change on the layer, and a version of this service \
         before it counted commits instead, so a generation recorded then names no point in this \
         history. Extract the layer in full and record the generation that answer carries."
    )))
}

/// The one layer this service has, as the request has to name it. A change file
/// covers the layers it was asked about, so a request naming another layer would
/// otherwise be answered with a window over a layer it never mentioned.
fn check_layers(params: &Params) -> Result<(), EsriError> {
    let Some(raw) = params
        .get("layers")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Err(EsriError::bad_request(format!(
            "layers must name the layer to extract changes for, which is {LAYER_ID}"
        )));
    };
    let named: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if named != [LAYER_ID] {
        return Err(EsriError::bad_request(format!(
            "the service has one layer, id {LAYER_ID}; layers '{}' names something else",
            shown(raw)
        )));
    }
    Ok(())
}

/// The only encoding a change file comes in here. `sqlite` is a geodatabase
/// Esri's own service writes and nothing in this crate can build one, so it is
/// refused by name rather than answered with JSON a client cannot open.
fn check_data_format(params: &Params) -> Result<(), EsriError> {
    match params.get("dataFormat").map(str::trim).unwrap_or_default() {
        "" => Ok(()),
        held if held.eq_ignore_ascii_case("json") => Ok(()),
        held => Err(EsriError::bad_request(format!(
            "dataFormat '{}' is not supported in this version of the service, which writes a \
             change file in json only",
            shown(held)
        ))),
    }
}

/// The generation the request asks to start from, off `layerServerGens`.
///
/// One entry, because the service has one layer, and its id has to be that
/// layer's. `serverGens`, the positional form the same operation also takes on
/// Esri's services, is refused by name: it says which generation without saying
/// which layer it belongs to.
fn requested_generation(params: &Params) -> Result<i64, EsriError> {
    if params.get("serverGens").is_some() {
        return Err(EsriError::bad_request(
            "serverGens is not supported in this version of the service. Send layerServerGens, \
             which names the layer each generation belongs to.",
        ));
    }
    let Some(raw) = params
        .get("layerServerGens")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Err(EsriError::bad_request(
            "layerServerGens must say which generation to extract changes since, as \
             [{\"id\": 0, \"serverGen\": <n>}]",
        ));
    };
    let bad = |why: &str| {
        EsriError::bad_request(format!(
            "layerServerGens '{}' {why}. It is [{{\"id\": 0, \"serverGen\": <n>}}].",
            shown(raw)
        ))
    };
    let value: Value =
        serde_json::from_str(raw).map_err(|e| bad(&format!("is not valid JSON: {e}")))?;
    let held = match &value {
        Value::Array(entries) => entries.as_slice(),
        // a bare entry rather than a list of one, as clients send elsewhere
        Value::Object(_) => std::slice::from_ref(&value),
        _ => return Err(bad("must be a JSON array of generations")),
    };
    let [entry] = held else {
        return Err(bad(&format!(
            "names {} layers, and this service has one",
            held.len()
        )));
    };
    if entry.get("id").and_then(number) != Some(0) {
        return Err(bad(&format!(
            "must name layer {LAYER_ID}, which is the only layer this service has"
        )));
    }
    match entry.get("serverGen").and_then(number) {
        Some(generation) if generation >= 0 => Ok(generation),
        _ => Err(bad(
            "must carry a serverGen that is a whole number, 0 or more",
        )),
    }
}

/// A JSON number, or the number a JSON string holds: a form-encoded client may
/// quote either field of a generation.
fn number(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// An absolute URL under this service, which is what a client follows a job by.
/// The same base the catalog builds service URLs from, so both point at the host
/// the client actually reached.
fn under_service(headers: &HeaderMap, service: &str, route: &str, job: &str) -> String {
    format!("{}/{route}/{job}", service_url(&base_url(headers), service))
}

// ─── The job and its change file ────────────────────────────────────

/// A job id as the two poll routes read it: the layer it names, the field a
/// change file names rows by, the two ends the submit pinned and the changeset the
/// feature diff runs from.
struct Window {
    layer: Layer,
    oid_field: String,
    job: Job,
    from: Option<Uuid>,
}

/// The window a job id names, refusing anything about it that this layer's
/// history does not bear out.
async fn window(
    store: &AppState,
    actor: &Actor,
    service: &str,
    id: &str,
) -> Result<Window, EsriError> {
    let layer = resolve(store, actor, service, LAYER_ID).await?;
    let oid_field = tracked_field(store, &layer).await?.to_string();
    let job = Job::decode(id)?;
    let chain = Chain::of(store, layer.branch_id).await?;
    // the pinned head has to be this branch's own history, or a job id edited to
    // name another dataset's changeset would be answered with that dataset's
    // changes
    if let Some(head) = job.head
        && !chain.holds(head)
    {
        return Err(unknown_job(id));
    }
    // and neither end can be past where this layer has got to, so an edited clock
    // cannot hand out a cursor that skips events this service never reported
    if job.at > event_clock(store, layer.branch_id).await? {
        return Err(unknown_job(id));
    }
    // the same refusal the submit gives, because a job id is data a client sends
    // rather than a key this service looked up
    check_on_the_clock(job.generation, &chain)?;
    let from = chain.base(job.generation);
    Ok(Window {
        layer,
        oid_field,
        job,
        from,
    })
}

/// The poll. There is no work to wait for, so the first ask is the answer.
pub(super) async fn job_status(
    State(store): State<AppState>,
    actor: Actor,
    headers: HeaderMap,
    Path((service, id)): Path<(String, String)>,
    Query(params): Query<Vec<(String, String)>>,
) -> Result<Json<Value>, EsriError> {
    let params = Params(params);
    require_esri_json(&params)?;
    let held = window(&store, &actor, &service, &id).await?;
    // re-encoded rather than echoed, so the URL a client follows is the one this
    // service reads and not whatever spelling reached it
    Ok(Json(json!({
        "status": "Completed",
        "responseType": "esriDataChangesResponseTypeEdits",
        "resultUrl": under_service(&headers, &service, "changefiles", &held.job.encode()),
    })))
}

/// The change file itself, recomputed against the head the job pinned.
///
/// The headers are read for the same reason the job routes read them: an
/// attachment's bytes are named by an absolute URL, and the host a client can
/// reach this service by is the one it asked on.
pub(super) async fn change_file(
    State(store): State<AppState>,
    actor: Actor,
    headers: HeaderMap,
    Path((service, id)): Path<(String, String)>,
    Query(params): Query<Vec<(String, String)>>,
) -> Result<Json<Value>, EsriError> {
    let params = Params(params);
    require_esri_json(&params)?;
    let held = window(&store, &actor, &service, &id).await?;
    Ok(Json(
        edits(&store, &held, &base_url(&headers), &service).await?,
    ))
}

/// One feature as a change file names it: the object id and nothing else.
/// Deliberately minimal. A client reads the ids out of a change file and fetches
/// the rows themselves through `/query`, where the geometry, the reference it
/// comes in and the attribute types are one code path, so a shape here would be
/// bytes nobody reads.
fn row(oid: i64, field: &str) -> Value {
    json!({"attributes": {field: oid}})
}

async fn edits(
    store: &AppState,
    held: &Window,
    base: &str,
    service: &str,
) -> Result<Value, EsriError> {
    let field = held.oid_field.as_str();
    let mut adds: Vec<Value> = Vec::new();
    let mut updates: Vec<Value> = Vec::new();
    let mut removed: Vec<Uuid> = Vec::new();
    let mut delete_ids: Vec<i64> = Vec::new();

    // no head means the window was pinned to a branch with no commit, so nothing
    // had happened yet whatever has happened since
    if let Some(head) = held.job.head {
        for op in store.diff(held.from, head).await?.operations {
            match op {
                DiffOp::Insert {
                    feature_id,
                    properties,
                    ..
                } => adds.push(row(oid_in(&properties, field, feature_id)?, field)),
                DiffOp::Update {
                    feature_id,
                    properties,
                    ..
                } => {
                    let properties = properties.unwrap_or(Value::Null);
                    updates.push(row(oid_in(&properties, field, feature_id)?, field));
                }
                DiffOp::Delete { feature_id } => removed.push(feature_id),
            }
        }
        if !removed.is_empty() {
            let before = properties_before_delete(store, head, &removed).await?;
            for feature_id in &removed {
                let properties = before.get(feature_id).ok_or_else(|| {
                    EsriError::bad_request(format!(
                        "feature {feature_id} was deleted in this window and no version of it \
                         before that carries an object id, so '{}' has no id to report it gone \
                         by. A change file that left it out would report it as still there.",
                        held.layer.name
                    ))
                })?;
                delete_ids.push(oid_in(properties, field, *feature_id)?);
            }
        }
    }

    Ok(json!({
        "edits": [{
            "id": 0,
            "features": {"adds": adds, "updates": updates, "deleteIds": delete_ids},
            "attachments": attachment_edits(store, held, base, service).await?,
        }],
        "layerServerGens": [{"id": 0, "serverGen": held.job.at}],
    }))
}

// ─── Attachment sections ────────────────────────────────────────────

/// Which section of a change file one attachment belongs in.
#[derive(Debug, PartialEq)]
enum Section {
    Add,
    Delete,
    /// Outside the window either way, or created and deleted inside it. A file
    /// the client never saw is not a delete: naming it would tell the client to
    /// drop an attachment it never had.
    Neither,
}

/// The section an attachment falls in, over the half-open window `(from, to]`.
///
/// Both of its own times are generations, so this is integer comparison and the
/// boundary is exact. Whether it is there is asked as of `to` rather than as of
/// now: one deleted after the window closed is an add here and a delete in the
/// next window, which is the order it happened in.
fn section(created: i64, deleted: Option<i64>, from: i64, to: i64) -> Section {
    let inside = |held: i64| held > from && held <= to;
    match deleted {
        _ if inside(created) && deleted.is_none_or(|gone| gone > to) => Section::Add,
        // there before the window opened and gone inside it
        Some(gone) if inside(gone) && created <= from => Section::Delete,
        _ => Section::Neither,
    }
}

/// What arrived on the layer's branch inside the window and what went.
///
/// `updates` is always empty: see the module documentation. `deleteIds` carries
/// attachment global ids rather than the numbers `adds` carry as `attachmentId`,
/// which is what Esri puts there and what the consumer pairs by.
async fn attachment_edits(
    store: &AppState,
    held: &Window,
    base: &str,
    service: &str,
) -> Result<Value, EsriError> {
    let from = held.job.generation;
    let to = held.job.at;
    // the window's lower end narrows the read and every row is then classified in
    // Rust, so the two sections cannot drift apart: see `section`
    let rows = sqlx::query(&format!(
        "SELECT id, feature_id, name, content_type, size_bytes,
                {created} AS created_gen, {deleted} AS deleted_gen
           FROM attachments
          WHERE branch_id = $1 AND feature_id IS NOT NULL
            AND ({created} > $2 OR {deleted} > $2)
          ORDER BY created_at, id",
        created = millis_of("created_at"),
        deleted = millis_of("deleted_at"),
    ))
    .bind(held.layer.branch_id)
    .bind(from)
    .fetch_all(store.read_pool())
    .await?;

    let mut added: Vec<&PgRow> = Vec::new();
    let mut delete_ids: Vec<String> = Vec::new();
    for row in &rows {
        match section(row.get("created_gen"), row.get("deleted_gen"), from, to) {
            Section::Add => added.push(row),
            Section::Delete => delete_ids.push(attachments::global_id(row.get("id"))),
            Section::Neither => {}
        }
    }

    // the parent's object id, which the download URL names. An attachment whose
    // feature is no longer on the branch is left out: the same change file reports
    // that feature gone, so the client drops it and everything hanging off it.
    let parents: Vec<Uuid> = added.iter().map(|row| row.get("feature_id")).collect();
    let oids = oids_by_feature(store, &held.layer, &parents).await?;
    let adds: Vec<Value> = added
        .iter()
        .filter_map(|row| {
            let parent: Uuid = row.get("feature_id");
            let oid = oids.get(&parent)?;
            let id: Uuid = row.get("id");
            Some(json!({
                "attachmentId": attachments::derived_id(id),
                "globalId": attachments::global_id(id),
                "parentGlobalId": attachments::global_id(parent),
                "contentType": row.get::<String, _>("content_type"),
                "name": row.get::<String, _>("name"),
                "size": row.get::<i64, _>("size_bytes"),
                "url": attachments::download_url(base, service, *oid, id),
            }))
        })
        .collect();

    Ok(json!({"adds": adds, "updates": [], "deleteIds": delete_ids}))
}

/// The object id a version's properties carry.
///
/// The exact key, which is the read `/query` answers the id with, so a change
/// file never names a row by an id the layer itself does not show. Text is taken
/// as the number it holds, as the query's own cast does.
///
/// A version with no integer there refuses the whole request. There is no channel
/// in this protocol to report a row that was left out, and a change list missing
/// a row tells the client that row did not change, which is worse than an error
/// naming the feature.
fn oid_in(properties: &Value, field: &str, feature_id: Uuid) -> Result<i64, EsriError> {
    match properties.get(field).and_then(number) {
        Some(oid) => Ok(oid),
        None => Err(EsriError::bad_request(format!(
            "feature {feature_id} changed in this window and carries no integer '{field}', so \
             there is no object id to name it by, and a change file that left it out would \
             report it as unchanged"
        ))),
    }
}

/// What each deleted feature held before it went. A delete version carries no
/// properties of its own, so the object id comes off the last version that had
/// them, along the pinned head's own chain.
async fn properties_before_delete(
    store: &AppState,
    head: Uuid,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Value>, EsriError> {
    let rows = sqlx::query(
        "WITH RECURSIVE chain AS (
             SELECT id, parent_id FROM changesets WHERE id = $1
           UNION ALL
             SELECT c.id, c.parent_id FROM changesets c JOIN chain ch ON ch.parent_id = c.id
         )
         SELECT DISTINCT ON (fv.feature_id) fv.feature_id, fv.properties
           FROM feature_versions fv
           JOIN chain ch ON fv.changeset_id = ch.id
          WHERE fv.feature_id = ANY($2::uuid[]) AND fv.operation <> 'delete'
          ORDER BY fv.feature_id, fv.created_at DESC, fv.id DESC",
    )
    .bind(head)
    .bind(ids)
    .fetch_all(store.read_pool())
    .await?;
    Ok(rows
        .iter()
        .map(|row| (row.get("feature_id"), row.get("properties")))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chain of four commits a second apart, head first, as the walk builds it.
    /// The generations are the milliseconds the commits sit at.
    fn chain() -> Chain {
        Chain(
            [(4, 4000), (3, 3000), (2, 2000), (1, 1000)]
                .into_iter()
                .map(|(id, at)| Commit {
                    id: Uuid::from_u128(id),
                    at,
                })
                .collect(),
        )
    }

    #[test]
    fn the_head_and_the_first_commit_are_the_ends_of_the_chain() {
        assert_eq!(chain().head(), Some(Uuid::from_u128(4)));
        assert_eq!(chain().root_at(), Some(1000));
        assert_eq!(Chain(Vec::new()).head(), None);
        assert_eq!(Chain(Vec::new()).root_at(), None);
    }

    /// A window's diff runs from the newest commit the client already held, which
    /// is the deepest one at or before the generation it asks from.
    #[test]
    fn the_diff_runs_from_the_deepest_commit_at_or_before_the_generation() {
        let chain = chain();
        assert_eq!(chain.base(4000), Some(Uuid::from_u128(4)));
        assert_eq!(chain.base(3500), Some(Uuid::from_u128(3)));
        assert_eq!(chain.base(3000), Some(Uuid::from_u128(3)));
        assert_eq!(chain.base(1000), Some(Uuid::from_u128(1)));
        // a generation past the head still holds every commit
        assert_eq!(chain.base(9000), Some(Uuid::from_u128(4)));
        // before the first commit there is nothing to diff from, so a window
        // opening there covers the whole layer
        assert_eq!(chain.base(999), None);
        assert_eq!(chain.base(0), None);
        assert_eq!(Chain(Vec::new()).base(4000), None);
    }

    #[test]
    fn a_changeset_off_the_chain_is_not_held_by_it() {
        let chain = chain();
        assert!(chain.holds(Uuid::from_u128(1)));
        assert!(chain.holds(Uuid::from_u128(4)));
        // another dataset's changeset, which is what a tampered job id carries
        assert!(!chain.holds(Uuid::from_u128(99)));
    }

    /// A cursor from the version of this service that counted commits reads as
    /// 1970 on the clock, which would open a window before the layer existed and
    /// answer every row it has as an add. Refused by name instead.
    #[test]
    fn a_generation_below_the_first_commit_is_refused_but_zero_is_not() {
        let chain = chain();
        let refused = check_on_the_clock(1, &chain).unwrap_err();
        assert_eq!(refused.code, 400, "{}", refused.message);
        assert!(refused.message.contains("predates"), "{}", refused.message);
        assert!(refused.message.contains("1000"), "{}", refused.message);
        assert!(check_on_the_clock(4, &chain).is_err());
        assert!(check_on_the_clock(999, &chain).is_err());

        // the clock a branch nothing has happened to publishes, and a full
        // extraction, which is not a stale count of commits
        assert!(check_on_the_clock(0, &chain).is_ok());
        assert!(check_on_the_clock(1000, &chain).is_ok());
        assert!(check_on_the_clock(5000, &chain).is_ok());
        // a branch with no commit has no floor to be under
        assert!(check_on_the_clock(1, &Chain(Vec::new())).is_ok());
    }

    #[test]
    fn a_job_id_round_trips() {
        let job = Job {
            head: Some(Uuid::from_u128(7)),
            generation: 3000,
            at: 4000,
        };
        let held = Job::decode(&job.encode()).unwrap();
        assert_eq!(held, job);

        // the empty branch, which has no head to pin to
        let empty = Job {
            head: None,
            generation: 0,
            at: 0,
        };
        assert_eq!(Job::decode(&empty.encode()).unwrap(), empty);

        // and the id is opaque rather than a URL a client could read a uuid off
        let encoded = job.encode();
        assert!(
            !encoded.contains(&Uuid::from_u128(7).to_string()),
            "{encoded}"
        );
    }

    #[test]
    fn a_tampered_job_id_is_refused_rather_than_guessed_at() {
        let uuid = "00000000-0000-0000-0000-000000000007";
        for held in [
            "",
            "not base64!!",
            // valid base64url, no separator
            &base64_of("nothing here"),
            // a separator and no uuid
            &base64_of("head:1:2"),
            // an id from the version that counted commits, which carried two
            // fields: read as a clock it would open a window in 1970
            &base64_of(&format!("{uuid}:3")),
            // a uuid and no generation
            &base64_of(&format!("{uuid}::")),
            &base64_of(&format!("{uuid}:tomorrow:4000")),
            &base64_of(&format!("{uuid}:3000:tomorrow")),
            // a generation before the epoch
            &base64_of(&format!("{uuid}:-1:4000")),
            // a window that closes before it opens
            &base64_of(&format!("{uuid}:4000:3000")),
            // more fields than the id has
            &base64_of(&format!("{uuid}:1:2:3")),
        ] {
            assert!(Job::decode(held).is_err(), "{held}");
        }
    }

    fn base64_of(text: &str) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text)
    }

    #[test]
    fn a_generation_is_read_as_a_number_or_the_text_of_one() {
        let numeric = Params(vec![(
            "layerServerGens".into(),
            r#"[{"id":0,"serverGen":12}]"#.into(),
        )]);
        assert_eq!(requested_generation(&numeric).unwrap(), 12);
        let quoted = Params(vec![(
            "layerServerGens".into(),
            r#"[{"id":"0","serverGen":"12"}]"#.into(),
        )]);
        assert_eq!(requested_generation(&quoted).unwrap(), 12);
        // a bare entry rather than a list of one
        let bare = Params(vec![(
            "layerServerGens".into(),
            r#"{"id":0,"serverGen":0}"#.into(),
        )]);
        assert_eq!(requested_generation(&bare).unwrap(), 0);

        for raw in [
            "",
            "[{",
            "7",
            // another layer's generation, which this service cannot answer for
            r#"[{"id":1,"serverGen":3}]"#,
            r#"[{"id":0,"serverGen":3},{"id":1,"serverGen":3}]"#,
            r#"[{"id":0}]"#,
            r#"[{"id":0,"serverGen":-2}]"#,
            r#"[{"id":0,"serverGen":"soon"}]"#,
        ] {
            let params = Params(vec![("layerServerGens".into(), raw.into())]);
            assert!(requested_generation(&params).is_err(), "{raw}");
        }

        // the positional form is refused by name rather than read as layer 0's
        let positional = Params(vec![
            (
                "layerServerGens".into(),
                r#"[{"id":0,"serverGen":1}]"#.into(),
            ),
            ("serverGens".into(), "1,2".into()),
        ]);
        assert!(requested_generation(&positional).is_err());
    }

    #[test]
    fn the_request_has_to_name_the_one_layer_and_a_format_this_service_writes() {
        let named = Params(vec![("layers".into(), "0".into())]);
        assert!(check_layers(&named).is_ok());
        for raw in ["", "1", "0,1", "all"] {
            let params = Params(vec![("layers".into(), raw.into())]);
            assert!(check_layers(&params).is_err(), "{raw}");
        }

        assert!(check_data_format(&Params(vec![])).is_ok());
        let json = Params(vec![("dataFormat".into(), "json".into())]);
        assert!(check_data_format(&json).is_ok());
        let geodatabase = Params(vec![("dataFormat".into(), "sqlite".into())]);
        assert!(check_data_format(&geodatabase).is_err());
    }

    /// The window `(100, 200]`, and events on either side of both ends.
    #[test]
    fn an_attachment_falls_in_the_section_its_generations_put_it_in() {
        let held = |created, deleted| section(created, deleted, 100, 200);

        // arrived inside the window and still there
        assert_eq!(held(150, None), Section::Add);
        // there before the window and still there, so the client already has it
        assert_eq!(held(50, None), Section::Neither);
        // there before the window and gone inside it
        assert_eq!(held(50, Some(150)), Section::Delete);
        // arrived and went inside the window, which the client never saw
        assert_eq!(held(120, Some(150)), Section::Neither);
        // gone before the window opened, which the client already knows
        assert_eq!(held(10, Some(50)), Section::Neither);

        // the window is half open, so the generation it opens at belongs to the
        // window before it and the one it closes at belongs to this one
        assert_eq!(held(100, None), Section::Neither);
        assert_eq!(held(200, None), Section::Add);
        assert_eq!(held(50, Some(100)), Section::Neither);
        assert_eq!(held(50, Some(200)), Section::Delete);

        // and nothing after it closes is in it at all
        assert_eq!(held(250, None), Section::Neither);
        assert_eq!(held(50, Some(250)), Section::Neither);
    }

    /// One arriving inside the window and deleted after it closed is an add here
    /// and a delete in the next window, which is the order it happened in: asking
    /// whether it is there as of now instead would report it in neither, and the
    /// client would be told to drop something it was never sent.
    #[test]
    fn an_attachment_deleted_after_the_window_closed_is_still_an_add_in_it() {
        assert_eq!(section(150, Some(250), 100, 200), Section::Add);
        // the next window, which opens where this one closed
        assert_eq!(section(150, Some(250), 200, 300), Section::Delete);
    }

    /// Generation 0 opens at the epoch: every attachment the layer has is an add,
    /// and nothing was there before it to report gone.
    #[test]
    fn generation_zero_holds_every_live_attachment_and_no_deletes() {
        assert_eq!(section(1, None, 0, 200), Section::Add);
        assert_eq!(section(1, Some(2), 0, 200), Section::Neither);
    }

    #[test]
    fn a_changed_row_with_no_object_id_refuses_the_window() {
        let id = Uuid::from_u128(9);
        assert_eq!(
            oid_in(&json!({"OBJECTID": 42, "name": "a"}), "OBJECTID", id).unwrap(),
            42
        );
        // as the query's own cast reads it
        assert_eq!(
            oid_in(&json!({"OBJECTID": "42"}), "OBJECTID", id).unwrap(),
            42
        );
        // the key the layer publishes, not one that differs in case: /query
        // answers null for that row, and a change file must not name it
        assert!(oid_in(&json!({"objectid": 42}), "OBJECTID", id).is_err());
        assert!(oid_in(&json!({}), "OBJECTID", id).is_err());
        assert!(oid_in(&json!({"OBJECTID": Value::Null}), "OBJECTID", id).is_err());
        assert!(oid_in(&json!({"OBJECTID": "many"}), "OBJECTID", id).is_err());
    }
}
