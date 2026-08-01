// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! `extractChanges`: what changed on the dataset's `main` branch since a
//! generation the client already holds.
//!
//! A layer's generation is the depth of `main`'s head: how many changesets stand
//! between the root and the head along the parent chain, and 0 for a branch with
//! no commit yet. Depth only ever grows, because a commit appends to `main` and
//! this facade's own writes commit there, so it is a cursor a client can write
//! down and send back. Generation N names the ancestor at that depth, and the
//! window is the store's own diff from that ancestor to the head.
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
//! pinned to and the generation it starts from, the poll always answers
//! `Completed`, and the fetch recomputes the diff. Pinning the head at submit is
//! what keeps a commit that lands between the submit and the fetch out of the
//! answer, so the change file and the generation it reports agree.
//!
//! A change file's features carry the object id and nothing else. The consumer
//! this exists for reads the ids and fetches the rows themselves through
//! `/query`, so a geometry here would be bytes nobody reads.
//!
//! Attachment changes are reported off the attachments table's own timestamps,
//! not off a generation. An attachment commits no changeset, so it advances no
//! generation and there is no ancestor to diff against: the window is the time
//! range that starts at the `created_at` of the changeset the requested
//! generation names, and the beginning of time at generation 0. An attachment
//! created after that start and still live is an add, and one deleted after it
//! that was already there at the start is a delete, by the tombstone migration
//! 026 put on the table.
//!
//! Being a time range makes it approximate at the boundary instant, in two ways
//! a client should know about. An attachment uploaded in the same instant as the
//! changeset that bounds the window may fall on either side of it, because both
//! timestamps come from the one database clock and nothing orders them. And the
//! range has no upper bound, unlike the feature diff, which is pinned to the head
//! the submit saw: an attachment that arrives between the submit and the fetch is
//! in the answer. Bounding it at the pinned head instead would leave out every
//! attachment uploaded since the last commit, which is most of them, since
//! uploading one is not a commit.
//!
//! `updates` is always empty. Replacing an attachment here is a delete and an
//! upload, which mints a new uuid, so the pair is reported as those two things
//! and the consumer applies them in that order to the same effect.
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
use time::OffsetDateTime;
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

// ─── Generations ────────────────────────────────────────────────────

/// `main`'s changeset chain, head first and the root last.
///
/// The whole chain rather than a count, because one walk answers all three
/// questions asked of it: how deep the head is, which changeset a generation
/// names, and whether a changeset a client sent back is on this branch at all.
struct Chain(Vec<Uuid>);

impl Chain {
    async fn of(store: &AppState, branch_id: Uuid) -> Result<Chain, EsriError> {
        let rows = sqlx::query(
            "WITH RECURSIVE chain AS (
                 SELECT c.id, c.parent_id, 0 AS behind
                   FROM changesets c
                   JOIN branches b ON b.head = c.id
                  WHERE b.id = $1
               UNION ALL
                 SELECT c.id, c.parent_id, ch.behind + 1
                   FROM changesets c
                   JOIN chain ch ON ch.parent_id = c.id
             )
             SELECT id FROM chain ORDER BY behind",
        )
        .bind(branch_id)
        .fetch_all(store.read_pool())
        .await?;
        Ok(Chain(rows.iter().map(|row| row.get("id")).collect()))
    }

    /// The generation the head is at, which is the chain's length.
    fn depth(&self) -> i64 {
        self.0.len() as i64
    }

    fn head(&self) -> Option<Uuid> {
        self.0.first().copied()
    }

    /// The changeset a generation names. `None` at generation 0, which is the
    /// branch before its first commit and the point a diff from nothing starts
    /// at. A generation past the head names nothing.
    fn at(&self, generation: i64) -> Option<Uuid> {
        if generation <= 0 || generation > self.depth() {
            return None;
        }
        self.0.get((self.depth() - generation) as usize).copied()
    }

    /// The generation a changeset sits at, or `None` when it is not on this
    /// chain. What makes a job id untrusted data rather than a lookup key.
    fn generation_of(&self, changeset: Uuid) -> Option<i64> {
        let behind = self.0.iter().position(|held| *held == changeset)?;
        Some(self.depth() - behind as i64)
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
/// `minServerGen` is the head's depth like `serverGen`: every generation behind
/// the head is still answerable, because the chain it is read off is the history
/// itself and nothing here prunes it, but a client is told the current one so it
/// starts from where the service is now rather than from the beginning of time.
pub(super) async fn tracking_info(
    store: &AppState,
    layer: &Layer,
) -> Result<Option<Value>, EsriError> {
    if !trackable(store, layer).await? {
        return Ok(None);
    }
    let depth = Chain::of(store, layer.branch_id).await?.depth();
    Ok(Some(json!({
        "lastSyncDate": Value::Null,
        "layerServerGens": [{"id": 0, "minServerGen": depth, "serverGen": depth}],
    })))
}

// ─── Job ids ────────────────────────────────────────────────────────

/// The whole of a submitted request: the head the window is pinned to, `None`
/// for a branch that had no commit when it was submitted, and the generation the
/// window starts from.
///
/// It travels in the job id, base64url so a client handles one opaque token, and
/// the server keeps nothing. It carries no secret and it is not signed, because
/// nothing about it is trusted: the changeset has to be on the resolved layer's
/// own chain and the generation has to be one that chain reaches, or the request
/// is refused. A tampered id therefore cannot reach another dataset's history,
/// and a malformed one is a refusal rather than a panic.
#[derive(Debug, PartialEq)]
struct Job {
    head: Option<Uuid>,
    generation: i64,
}

impl Job {
    fn encode(&self) -> String {
        use base64::Engine;
        let head = self.head.map(|id| id.to_string()).unwrap_or_default();
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!("{head}:{}", self.generation))
    }

    fn decode(text: &str) -> Result<Job, EsriError> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(text)
            .map_err(|_| unknown_job(text))?;
        let held = String::from_utf8(bytes).map_err(|_| unknown_job(text))?;
        let (head, generation) = held.split_once(':').ok_or_else(|| unknown_job(text))?;
        let head = match head {
            "" => None,
            named => Some(Uuid::parse_str(named).map_err(|_| unknown_job(text))?),
        };
        let generation: i64 = generation.parse().map_err(|_| unknown_job(text))?;
        if generation < 0 {
            return Err(unknown_job(text));
        }
        Ok(Job { head, generation })
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
    let depth = chain.depth();
    if asked > depth {
        return Err(EsriError::bad_request(format!(
            "serverGen {asked} is ahead of this layer, which is at generation {depth}. Ask for \
             {depth} or a generation behind it."
        )));
    }

    let job = Job {
        head: chain.head(),
        generation: asked,
    };
    Ok(Json(json!({
        "statusUrl": under_service(&headers, &service, "jobs", &job.encode()),
    })))
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
/// change file names rows by, the head the window is pinned to and the changeset
/// it starts from.
struct Window {
    layer: Layer,
    oid_field: String,
    job: Job,
    /// The pinned head's own generation, which is where the window ends and what
    /// the change file reports back. Not the layer's generation now: a commit
    /// that landed since the submit is outside this window.
    at: i64,
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
    let at = match job.head {
        None => 0,
        Some(head) => chain.generation_of(head).ok_or_else(|| unknown_job(id))?,
    };
    if job.generation > at {
        return Err(unknown_job(id));
    }
    let from = chain.at(job.generation);
    Ok(Window {
        layer,
        oid_field,
        job,
        at,
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
        "layerServerGens": [{"id": 0, "serverGen": held.at}],
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

/// The section an attachment falls in, against the window's start. `None` is
/// generation 0, which starts at the beginning of time: everything live is then
/// an add, and nothing was there before the window to be reported gone.
fn section(
    created_at: OffsetDateTime,
    deleted_at: Option<OffsetDateTime>,
    start: Option<OffsetDateTime>,
) -> Section {
    let inside = |held: OffsetDateTime| start.is_none_or(|start| held > start);
    match deleted_at {
        None if inside(created_at) => Section::Add,
        // there before the window opened and gone inside it
        Some(gone) if inside(gone) && !inside(created_at) => Section::Delete,
        _ => Section::Neither,
    }
}

/// The instant the window starts at, which is when the changeset the requested
/// generation names was committed. `None` at generation 0.
async fn window_start(
    store: &AppState,
    from: Option<Uuid>,
) -> Result<Option<OffsetDateTime>, EsriError> {
    let Some(changeset) = from else {
        return Ok(None);
    };
    let row = sqlx::query("SELECT created_at FROM changesets WHERE id = $1")
        .bind(changeset)
        .fetch_one(store.read_pool())
        .await?;
    Ok(Some(row.get("created_at")))
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
    let start = window_start(store, held.from).await?;
    // both timestamps are read against the one start, and the row is classified
    // in Rust so the two sections cannot drift apart: see `section`
    let rows = sqlx::query(
        "SELECT id, feature_id, name, content_type, size_bytes, created_at, deleted_at
           FROM attachments
          WHERE branch_id = $1 AND feature_id IS NOT NULL
            AND ($2::timestamptz IS NULL OR created_at > $2 OR deleted_at > $2)
          ORDER BY created_at, id",
    )
    .bind(held.layer.branch_id)
    .bind(start)
    .fetch_all(store.read_pool())
    .await?;

    let mut added: Vec<&PgRow> = Vec::new();
    let mut delete_ids: Vec<String> = Vec::new();
    for row in &rows {
        match section(row.get("created_at"), row.get("deleted_at"), start) {
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

    /// A chain of four commits, head first, as the walk builds it.
    fn chain() -> Chain {
        Chain(vec![
            Uuid::from_u128(4),
            Uuid::from_u128(3),
            Uuid::from_u128(2),
            Uuid::from_u128(1),
        ])
    }

    #[test]
    fn depth_is_how_many_commits_the_head_stands_on() {
        assert_eq!(chain().depth(), 4);
        assert_eq!(Chain(Vec::new()).depth(), 0);
        assert_eq!(Chain(Vec::new()).head(), None);
        assert_eq!(chain().head(), Some(Uuid::from_u128(4)));
    }

    #[test]
    fn a_generation_names_the_ancestor_at_that_depth() {
        let chain = chain();
        assert_eq!(chain.at(1), Some(Uuid::from_u128(1)));
        assert_eq!(chain.at(3), Some(Uuid::from_u128(3)));
        assert_eq!(chain.at(4), chain.head());
        // generation 0 is the branch before its first commit, which is where a
        // diff from nothing starts
        assert_eq!(chain.at(0), None);
        // and nothing past the head is on the chain
        assert_eq!(chain.at(5), None);
        assert_eq!(Chain(Vec::new()).at(1), None);
    }

    #[test]
    fn a_changeset_off_the_chain_has_no_generation() {
        let chain = chain();
        assert_eq!(chain.generation_of(Uuid::from_u128(1)), Some(1));
        assert_eq!(chain.generation_of(Uuid::from_u128(4)), Some(4));
        // another dataset's changeset, which is what a tampered job id carries
        assert_eq!(chain.generation_of(Uuid::from_u128(99)), None);
    }

    #[test]
    fn a_job_id_round_trips() {
        let job = Job {
            head: Some(Uuid::from_u128(7)),
            generation: 3,
        };
        let held = Job::decode(&job.encode()).unwrap();
        assert_eq!(held, job);

        // the empty branch, which has no head to pin to
        let empty = Job {
            head: None,
            generation: 0,
        };
        assert_eq!(Job::decode(&empty.encode()).unwrap(), empty);

        // and the id is opaque rather than a URL a client could read a uuid off
        assert!(!job.encode().contains('-'), "{}", job.encode());
    }

    #[test]
    fn a_tampered_job_id_is_refused_rather_than_guessed_at() {
        for held in [
            "",
            "not base64!!",
            // valid base64url, no separator
            &base64_of("nothing here"),
            // a separator and no uuid
            &base64_of("head:1"),
            // a uuid and no generation
            &base64_of("00000000-0000-0000-0000-000000000007:"),
            &base64_of("00000000-0000-0000-0000-000000000007:tomorrow"),
            // a generation before the root
            &base64_of("00000000-0000-0000-0000-000000000007:-1"),
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

    /// The window's start, and three instants around it.
    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    #[test]
    fn an_attachment_falls_in_the_section_its_timestamps_put_it_in() {
        let start = Some(at(100));

        // arrived inside the window and still there
        assert_eq!(section(at(150), None, start), Section::Add);
        // there before the window and still there, so the client already has it
        assert_eq!(section(at(50), None, start), Section::Neither);
        // there before the window and gone inside it
        assert_eq!(section(at(50), Some(at(150)), start), Section::Delete);
        // arrived and went inside the window, which the client never saw
        assert_eq!(section(at(120), Some(at(150)), start), Section::Neither);
        // gone before the window opened, which the client already knows
        assert_eq!(section(at(10), Some(at(50)), start), Section::Neither);
        // the boundary instant itself is outside the window, so an attachment
        // that shares it with the changeset belongs to the window before this one
        assert_eq!(section(at(100), None, start), Section::Neither);
    }

    /// Generation 0 starts at the beginning of time: every live attachment is an
    /// add, and nothing was there before it to report gone.
    #[test]
    fn generation_zero_holds_every_live_attachment_and_no_deletes() {
        assert_eq!(section(at(1), None, None), Section::Add);
        assert_eq!(section(at(1), Some(at(2)), None), Section::Neither);
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
