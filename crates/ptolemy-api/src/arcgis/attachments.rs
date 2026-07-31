// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! The FeatureServer's attachment operations: the files an Esri client puts on a
//! feature and reads back off it.
//!
//! Every one of them goes through the same store methods the native attachment
//! routes use, so an attachment uploaded here is the same row `/api/v1` serves
//! and the write ladder is the one the rest of the crate runs.
//!
//! The route table stays in the parent module, which is where `lib.rs` says this
//! facade's routes live and where the route census in the integration tests
//! reads them from. Only the handlers are here.
//!
//! Reads are public like the facade's other reads. The three writes are gated
//! exactly as `applyEdits` is: the write ladder on the dataset, the `token`
//! parameter accepted because the protocol has no header for a credential, and a
//! layer whose object ids are row numbers refused outright.

use axum::{
    Json,
    body::Bytes,
    extract::{Form, Multipart, Path, Query, State, multipart::MultipartRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use ptolemy_storage::{Attachment, AttachmentMeta};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    EsriError, Layer, NEEDS_A_REAL_OID, Oid, Params, UNSUPPORTED_EDITS, features_by_oid,
    require_esri_json, resolve, unknown_oid,
};
use crate::{AppState, auth::Actor};

/// The largest attachment this facade takes. Nothing in the store or the native
/// routes caps one, so the cap is this facade's own and every refusal names it.
const MAX_ATTACHMENT_BYTES: usize = 32 * 1024 * 1024;

/// The largest multipart body an upload route accepts: the cap plus room for the
/// part headers and the other form fields, so a file at the cap fits and one
/// over it is refused by name rather than by the body limit.
pub(super) const MAX_UPLOAD_BODY: usize = MAX_ATTACHMENT_BYTES + 64 * 1024;

/// The name of the file part in an `addAttachment` or `updateAttachment` body.
/// Esri's own clients send exactly this.
const FILE_PART: &str = "attachment";

/// What an attachment is served and stored as when nothing said. Same default
/// the native upload route uses.
const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

/// The longest attachment name this facade stores. A name is a label, and the
/// column has no bound of its own, so a client cannot put a file's worth of
/// bytes in one.
const MAX_NAME_LEN: usize = 255;

// ─── Ids ────────────────────────────────────────────────────────────

/// The number an Esri client holds an attachment by.
///
/// `attachmentId` is a JSON number in the protocol and the store's key is a
/// uuid, so the number is derived from the uuid rather than stored beside it: 48
/// bits of SHA-256 over the uuid. That is inside the integer range a double
/// holds exactly, which is what a JSON number reaches a browser as, and it is
/// the same answer in every process and after any number of deletes.
///
/// Derived from a hash and not from the uuid's own leading bytes, which are a
/// millisecond timestamp in v7: two attachments uploaded to one feature in the
/// same millisecond would otherwise derive the same id and neither could be
/// named. A hash collision is still possible and is refused loudly rather than
/// resolved to whichever row came first: see [`find`].
fn derived_id(id: Uuid) -> i64 {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(id.as_bytes());
    digest[..6]
        .iter()
        .fold(0i64, |built, byte| (built << 8) | i64::from(*byte))
}

/// The uuid as Esri writes a global id: in braces and upper case.
fn global_id(id: Uuid) -> String {
    format!("{{{}}}", id.to_string().to_uppercase())
}

/// One attachment as an `attachmentInfo`. `globalId` carries the store's own key,
/// so a client that wants the native API has the id it needs.
fn info(meta: &AttachmentMeta) -> Value {
    json!({
        "id": derived_id(meta.id),
        "globalId": global_id(meta.id),
        "name": meta.name,
        "contentType": meta.content_type,
        "size": meta.size_bytes,
    })
}

/// The result shape `addAttachment` and `updateAttachment` answer with.
fn written(id: Uuid) -> Value {
    json!({
        "objectId": derived_id(id),
        "globalId": global_id(id),
        "success": true,
    })
}

/// The attachment on this feature whose derived id is `wanted`.
///
/// No match is refused by name. More than one is a collision in the derived id:
/// both attachments exist and neither can be named by that number, so the
/// request is refused rather than aimed at whichever row was listed first, which
/// on a delete would take the wrong file.
fn find(held: &[AttachmentMeta], wanted: i64, layer: &Layer, oid: i64) -> Result<Uuid, EsriError> {
    let mut matched = held.iter().filter(|meta| derived_id(meta.id) == wanted);
    let Some(first) = matched.next() else {
        return Err(EsriError::bad_request(format!(
            "'{}' has no attachment {wanted} on {} {oid}",
            layer.name,
            layer.oid.name()
        )));
    };
    if matched.next().is_some() {
        return Err(EsriError {
            code: 409,
            message: format!(
                "attachment id {wanted} names more than one attachment on {} {oid} of '{}', so it \
                 cannot name one to act on. Reach them by uuid through /api/v1/attachments.",
                layer.oid.name(),
                layer.name
            ),
        });
    }
    Ok(first.id)
}

/// An id out of a path segment or a parameter, as the number the protocol says
/// it is.
fn number(raw: &str, what: &str) -> Result<i64, EsriError> {
    raw.trim()
        .parse::<i64>()
        .map_err(|_| EsriError::bad_request(format!("{what} is an integer: '{raw}'")))
}

/// A comma-separated id list, in the order it was sent and without repeats, so a
/// grouped answer follows the request and a delete acts on each id once.
fn numbers(raw: &str, what: &str) -> Result<Vec<i64>, EsriError> {
    let mut out = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let id = part.parse::<i64>().map_err(|_| {
            EsriError::bad_request(format!(
                "{what} must be a comma-separated list of integers: '{raw}'"
            ))
        })?;
        if !out.contains(&id) {
            out.push(id);
        }
    }
    if out.is_empty() {
        return Err(EsriError::bad_request(format!(
            "{what} names no ids: '{raw}'"
        )));
    }
    Ok(out)
}

// ─── Resolution ─────────────────────────────────────────────────────

/// The feature a `{objectId}` segment names.
///
/// The id goes through the same numbered read the query answers with, so a
/// client asks about the feature it saw. An unknown id is refused by name rather
/// than answered as a feature with no attachments.
async fn feature_of(store: &AppState, layer: &Layer, oid: i64) -> Result<Uuid, EsriError> {
    let held = features_by_oid(store, layer, &[oid]).await?;
    held.get(&oid)
        .map(|(feature_id, _)| *feature_id)
        .ok_or_else(|| unknown_oid(layer, oid))
}

/// The layer and the feature a write names, once the caller is allowed to write
/// it.
///
/// The same two gates `apply_edits` runs, in the same order: a layer whose
/// object ids are row numbers takes no edits at all, because such an id names a
/// different feature after any delete and an attachment aimed by one would land
/// on that feature instead. Then the store's write ladder on the dataset, which
/// is what turns a token into permission on this dataset in particular.
///
/// Runs before the body is read, so neither a refused caller nor an unknown
/// object id costs an upload.
async fn writable_feature(
    store: &AppState,
    actor: &Actor,
    service: &str,
    layer_id: &str,
    oid: i64,
) -> Result<(Layer, Uuid), EsriError> {
    let layer = resolve(store, actor, service, layer_id).await?;
    if !matches!(layer.oid, Oid::Property(_)) {
        return Err(EsriError::bad_request(format!(
            "'{}' {NEEDS_A_REAL_OID}",
            layer.name
        )));
    }
    crate::visibility::ensure_writable(store, actor, layer.dataset_id)
        .await
        .map_err(EsriError::refused)?;
    let feature = feature_of(store, &layer, oid).await?;
    Ok((layer, feature))
}

// ─── Reads ──────────────────────────────────────────────────────────

pub(super) async fn list_attachments(
    State(store): State<AppState>,
    actor: Actor,
    Path((service, layer_id, oid)): Path<(String, String, String)>,
    Query(params): Query<Vec<(String, String)>>,
) -> Result<Json<Value>, EsriError> {
    let params = Params(params);
    require_esri_json(&params)?;
    let oid = number(&oid, "an object id")?;
    let layer = resolve(&store, &actor, &service, &layer_id).await?;
    let feature = feature_of(&store, &layer, oid).await?;
    let held = store.list_attachments(feature, layer.branch_id).await?;
    Ok(Json(json!({
        "attachmentInfos": held.iter().map(info).collect::<Vec<_>>(),
    })))
}

/// The bytes themselves, as the type they were stored with.
///
/// Always as a download and never inline: the content type is whatever the
/// uploading client said it was, so a stored `text/html` served inline would run
/// as a page on this origin. `attachment` and `nosniff` together are what stop
/// that.
pub(super) async fn download_attachment(
    State(store): State<AppState>,
    actor: Actor,
    Path((service, layer_id, oid, attachment_id)): Path<(String, String, String, String)>,
) -> Result<Response, EsriError> {
    let oid = number(&oid, "an object id")?;
    let wanted = number(&attachment_id, "an attachmentId")?;
    let layer = resolve(&store, &actor, &service, &layer_id).await?;
    let feature = feature_of(&store, &layer, oid).await?;
    let held = store.list_attachments(feature, layer.branch_id).await?;
    let id = find(&held, wanted, &layer, oid)?;
    let attachment = store.get_attachment(id).await?;

    Ok((
        StatusCode::OK,
        [
            (
                "content-type",
                header_text(&attachment.content_type, DEFAULT_CONTENT_TYPE),
            ),
            (
                "content-disposition",
                format!("attachment; filename=\"{}\"", filename(&attachment.name)),
            ),
            ("content-length", attachment.size_bytes.to_string()),
            ("x-content-type-options", "nosniff".to_string()),
        ],
        Bytes::from(attachment.data),
    )
        .into_response())
}

/// Parameters a `queryAttachments` may carry that change which attachments
/// answer. None can be honored, and a client that sent one believes the answer
/// is narrower than it is, so each is refused by name. `gdbVersion` is refused
/// on every other route here for the same reason: this service has one version
/// of the data and answering from it would not be the version that was asked for.
const UNSUPPORTED_ATTACHMENT_QUERY: [&str; 3] = ["definitionExpression", "keywords", "gdbVersion"];

pub(super) async fn query_attachments_get(
    State(store): State<AppState>,
    actor: Actor,
    Path((service, layer_id)): Path<(String, String)>,
    Query(params): Query<Vec<(String, String)>>,
) -> Result<Json<Value>, EsriError> {
    run_query_attachments(store, actor, service, layer_id, Params(params)).await
}

pub(super) async fn query_attachments_post(
    State(store): State<AppState>,
    actor: Actor,
    Path((service, layer_id)): Path<(String, String)>,
    Form(params): Form<Vec<(String, String)>>,
) -> Result<Json<Value>, EsriError> {
    run_query_attachments(store, actor, service, layer_id, Params(params)).await
}

/// Many features' attachments in one answer, grouped by the object id that owns
/// them.
///
/// A feature with no attachments is left out rather than answered as an empty
/// group, which is what Esri's own service does and what verne's extractor
/// reads. An object id no feature carries is refused, as it is on the
/// single-feature listing: a batch that quietly dropped one id would tell a
/// client that feature has no attachments.
async fn run_query_attachments(
    store: AppState,
    actor: Actor,
    service: String,
    layer_id: String,
    params: Params,
) -> Result<Json<Value>, EsriError> {
    require_esri_json(&params)?;
    for name in UNSUPPORTED_ATTACHMENT_QUERY {
        if params.asks_for(name) {
            return Err(EsriError::bad_request(format!(
                "{name} is not supported in this version of the service"
            )));
        }
    }
    let Some(raw) = params
        .get("objectIds")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Err(EsriError::bad_request(
            "queryAttachments needs objectIds, a comma-separated list of object ids",
        ));
    };
    let ids = numbers(raw, "objectIds")?;

    let layer = resolve(&store, &actor, &service, &layer_id).await?;
    let held = features_by_oid(&store, &layer, &ids).await?;
    let mut groups = Vec::new();
    for oid in &ids {
        let (feature_id, _) = held.get(oid).ok_or_else(|| unknown_oid(&layer, *oid))?;
        let attachments = store.list_attachments(*feature_id, layer.branch_id).await?;
        if attachments.is_empty() {
            continue;
        }
        groups.push(json!({
            "parentObjectId": oid,
            "attachmentInfos": attachments.iter().map(info).collect::<Vec<_>>(),
        }));
    }
    Ok(Json(json!({"attachmentGroups": groups})))
}

// ─── Writes ─────────────────────────────────────────────────────────

pub(super) async fn add_attachment(
    State(store): State<AppState>,
    actor: Actor,
    Path((service, layer_id, oid)): Path<(String, String, String)>,
    Query(query): Query<Vec<(String, String)>>,
    body: Result<Multipart, MultipartRejection>,
) -> Result<Json<Value>, EsriError> {
    let oid = number(&oid, "an object id")?;
    let (layer, feature) = writable_feature(&store, &actor, &service, &layer_id, oid).await?;
    let upload = Upload::read(body, query).await?;
    require_esri_json(&upload.params)?;
    require_no_edit_parameters(&upload.params)?;

    let id = store_attachment(&store, &actor, &layer, feature, upload).await?;
    Ok(Json(json!({"addAttachmentResult": written(id)})))
}

/// Replace an attachment's name, type and bytes.
///
/// The store has no update for an attachment, so this writes the new row and
/// deletes the old one, which mints a new uuid and therefore a new derived id.
/// The result carries that new id, which is what an Esri client reads back off
/// an `updateAttachmentResult`.
///
/// The write comes first and the delete second on purpose: a delete that fails
/// after the write leaves a copy the caller can delete, while a write that
/// failed after the delete would have lost the file.
pub(super) async fn update_attachment(
    State(store): State<AppState>,
    actor: Actor,
    Path((service, layer_id, oid)): Path<(String, String, String)>,
    Query(query): Query<Vec<(String, String)>>,
    body: Result<Multipart, MultipartRejection>,
) -> Result<Json<Value>, EsriError> {
    let oid = number(&oid, "an object id")?;
    let (layer, feature) = writable_feature(&store, &actor, &service, &layer_id, oid).await?;
    let upload = Upload::read(body, query).await?;
    require_esri_json(&upload.params)?;
    require_no_edit_parameters(&upload.params)?;

    let Some(raw) = upload
        .params
        .get("attachmentId")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Err(EsriError::bad_request(
            "updateAttachment needs attachmentId, the id of the attachment to replace",
        ));
    };
    let wanted = number(raw, "an attachmentId")?;
    let held = store.list_attachments(feature, layer.branch_id).await?;
    let replaced = find(&held, wanted, &layer, oid)?;

    let id = store_attachment(&store, &actor, &layer, feature, upload).await?;
    store
        .delete_attachment(replaced)
        .await
        .map_err(EsriError::refused)?;
    Ok(Json(json!({"updateAttachmentResult": written(id)})))
}

/// Delete named attachments of one feature, all or none.
///
/// Every id is resolved before anything is deleted, so a batch naming one id
/// this feature does not carry takes nothing: an Esri client reports a partial
/// result per row and this cannot, exactly as `applyEdits` cannot.
pub(super) async fn delete_attachments(
    State(store): State<AppState>,
    actor: Actor,
    Path((service, layer_id, oid)): Path<(String, String, String)>,
    Form(params): Form<Vec<(String, String)>>,
) -> Result<Json<Value>, EsriError> {
    let params = Params(params);
    require_esri_json(&params)?;
    require_no_edit_parameters(&params)?;
    let oid = number(&oid, "an object id")?;
    let Some(raw) = params
        .get("attachmentIds")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Err(EsriError::bad_request(
            "deleteAttachments needs attachmentIds, a comma-separated list of attachment ids",
        ));
    };
    let wanted = numbers(raw, "attachmentIds")?;

    let (layer, feature) = writable_feature(&store, &actor, &service, &layer_id, oid).await?;
    let held = store.list_attachments(feature, layer.branch_id).await?;
    let targets: Vec<(i64, Uuid)> = wanted
        .iter()
        .map(|id| find(&held, *id, &layer, oid).map(|found| (*id, found)))
        .collect::<Result<_, _>>()?;

    let mut results = Vec::new();
    for (id, target) in targets {
        store
            .delete_attachment(target)
            .await
            .map_err(EsriError::refused)?;
        results.push(json!({"objectId": id, "success": true}));
    }
    Ok(Json(json!({"deleteAttachmentResults": results})))
}

/// The same parameters an `applyEdits` is refused for, because they mean the
/// same thing here: a version of the data this service does not have, and a
/// session that does not hold an edit open.
fn require_no_edit_parameters(params: &Params) -> Result<(), EsriError> {
    for name in UNSUPPORTED_EDITS {
        if params.asks_for(name) {
            return Err(EsriError::bad_request(format!(
                "{name} is not supported in this version of the service"
            )));
        }
    }
    Ok(())
}

/// The row an upload becomes, through the same store method the native upload
/// route calls.
async fn store_attachment(
    store: &AppState,
    actor: &Actor,
    layer: &Layer,
    feature: Uuid,
    upload: Upload,
) -> Result<Uuid, EsriError> {
    let id = Uuid::now_v7();
    let attachment = Attachment {
        id,
        feature_id: Some(feature),
        branch_id: Some(layer.branch_id),
        dataset_id: None,
        name: upload.name,
        content_type: upload.content_type,
        size_bytes: upload.data.len() as i64,
        data: upload.data,
        thumbnail: None,
        metadata: json!({}),
        created_by: actor.or_body("arcgis").to_string(),
        created_at: OffsetDateTime::now_utc(),
    };
    store
        .create_attachment(&attachment)
        .await
        .map_err(EsriError::refused)?;
    Ok(id)
}

// ─── Multipart ──────────────────────────────────────────────────────

/// The file an upload carries, and the parameters sent beside it.
struct Upload {
    name: String,
    content_type: String,
    data: Vec<u8>,
    /// The query string and the body's own text fields together: a client may
    /// put `f` in either, and `attachmentId` arrives as a field.
    params: Params,
}

impl Upload {
    /// The one file part and every text field of a `multipart/form-data` body.
    ///
    /// Read into memory rather than streamed, because the store takes the bytes
    /// as one value. [`MAX_ATTACHMENT_BYTES`] is what bounds that, and the body
    /// limit on the route bounds what is read before this can check it.
    async fn read(
        body: Result<Multipart, MultipartRejection>,
        query: Vec<(String, String)>,
    ) -> Result<Upload, EsriError> {
        let mut body = body.map_err(|rejection| {
            EsriError::bad_request(format!(
                "this operation takes a multipart/form-data body with a '{FILE_PART}' file part: {}",
                rejection.body_text()
            ))
        })?;

        let mut params = query;
        let mut file: Option<(String, String, Vec<u8>)> = None;
        while let Some(field) = body.next_field().await.map_err(multipart_error)? {
            let field_name = field.name().unwrap_or_default().to_string();
            if !field_name.eq_ignore_ascii_case(FILE_PART) {
                let text = field.text().await.map_err(multipart_error)?;
                params.push((field_name, text));
                continue;
            }
            if file.is_some() {
                return Err(EsriError::bad_request(format!(
                    "the body carries more than one '{FILE_PART}' part, and an attachment is one \
                     file"
                )));
            }
            let name = field.file_name().unwrap_or_default().to_string();
            let content_type = field
                .content_type()
                .unwrap_or(DEFAULT_CONTENT_TYPE)
                .to_string();
            let data = field.bytes().await.map_err(multipart_error)?;
            if data.len() > MAX_ATTACHMENT_BYTES {
                return Err(oversize());
            }
            file = Some((name, content_type, data.to_vec()));
        }

        let Some((name, content_type, data)) = file else {
            return Err(EsriError::bad_request(format!(
                "the body carries no '{FILE_PART}' part, which is the file to attach"
            )));
        };
        if name.trim().is_empty() {
            return Err(EsriError::bad_request(format!(
                "the '{FILE_PART}' part carries no filename, and an attachment is stored under its \
                 name"
            )));
        }
        if name.len() > MAX_NAME_LEN {
            return Err(EsriError::bad_request(format!(
                "the filename is {} bytes, and this service stores at most {MAX_NAME_LEN}",
                name.len()
            )));
        }

        Ok(Upload {
            name,
            content_type,
            data,
            params: Params(params),
        })
    }
}

/// A body over the cap is the caller's answer and names the cap. It is reached
/// two ways: the route's body limit cuts a body off mid-stream, and a file that
/// fits inside that limit is measured here.
fn oversize() -> EsriError {
    EsriError {
        code: 413,
        message: format!(
            "an attachment may be at most {} MiB",
            MAX_ATTACHMENT_BYTES / (1024 * 1024)
        ),
    }
}

fn multipart_error(error: axum::extract::multipart::MultipartError) -> EsriError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return oversize();
    }
    EsriError::bad_request(format!("the multipart body cannot be read: {error}"))
}

// ─── Header hygiene ─────────────────────────────────────────────────

/// A stored value as a header value, or the fallback when it holds something no
/// header may carry. An attachment's name and type are whatever the client that
/// uploaded it sent, and the native route takes both as free JSON text, so
/// neither is trusted to be header-safe here.
fn header_text(value: &str, fallback: &str) -> String {
    if value.is_empty() || value.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return fallback.to_string();
    }
    value.to_string()
}

/// The filename a `content-disposition` names, as its quoted-string may hold it.
/// A quote, a backslash or a control character would end the quoted string or
/// the header itself, so each is dropped rather than escaped: this is the label
/// a save dialog shows, and the store keeps the name it was given.
fn filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .collect();
    if cleaned.trim().is_empty() {
        return FILE_PART.to_string();
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same uuid derives the same id every time, which is what lets a client
    /// write one down.
    #[test]
    fn a_uuid_always_derives_the_same_id() {
        let id = Uuid::parse_str("018f3a2b-7c4d-7e1f-8a9b-0c1d2e3f4a5b").unwrap();
        let first = derived_id(id);
        assert_eq!(first, derived_id(id));
        // 48 bits, so inside the integers a double holds exactly
        assert!(first >= 0, "{first}");
        assert!(first < 1 << 48, "{first}");
        assert!(first < 1 << 53, "{first}");
    }

    /// Not the uuid's own leading bytes: those are a millisecond timestamp in
    /// v7, so two attachments uploaded in the same millisecond would collide and
    /// neither could be named.
    #[test]
    fn uuids_sharing_their_leading_bytes_derive_different_ids() {
        let mut bytes = *Uuid::now_v7().as_bytes();
        let first = Uuid::from_bytes(bytes);
        // everything a v7 timestamp covers, left alone
        bytes[15] ^= 0xff;
        let second = Uuid::from_bytes(bytes);
        assert_eq!(first.as_bytes()[..6], second.as_bytes()[..6]);
        assert_ne!(derived_id(first), derived_id(second));
    }

    #[test]
    fn a_global_id_is_the_uuid_in_braces_upper_case() {
        let id = Uuid::parse_str("018f3a2b-7c4d-7e1f-8a9b-0c1d2e3f4a5b").unwrap();
        assert_eq!(
            global_id(id),
            "{018F3A2B-7C4D-7E1F-8A9B-0C1D2E3F4A5B}".to_string()
        );
    }

    /// A layer stands in for the messages, which name it.
    fn layer_of(oid: &str) -> Layer {
        Layer {
            name: "roads".into(),
            geometry: "esriGeometryPoint",
            dataset_id: Uuid::nil(),
            branch_id: Uuid::nil(),
            fields: Vec::new(),
            oid: Oid::Property(oid.to_string()),
        }
    }

    fn meta_of(id: Uuid) -> AttachmentMeta {
        AttachmentMeta {
            id,
            feature_id: Some(Uuid::nil()),
            branch_id: Some(Uuid::nil()),
            dataset_id: None,
            name: "photo.jpg".into(),
            content_type: "image/jpeg".into(),
            size_bytes: 3,
            metadata: json!({}),
            created_by: "test".into(),
            created_at: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn an_id_that_names_one_attachment_finds_it() {
        let id = Uuid::now_v7();
        let held = vec![meta_of(id), meta_of(Uuid::now_v7())];
        let layer = layer_of("OBJECTID");
        assert_eq!(find(&held, derived_id(id), &layer, 7).unwrap(), id);
    }

    /// An id no attachment on the feature carries is refused by name, so a
    /// delete cannot be aimed by a number that means nothing.
    #[test]
    fn an_unknown_id_is_refused_by_name() {
        let held = vec![meta_of(Uuid::now_v7())];
        let layer = layer_of("OBJECTID");
        let error = find(&held, 1234, &layer, 7).unwrap_err();
        assert_eq!(error.code, 400, "{}", error.message);
        assert!(error.message.contains("1234"), "{}", error.message);
        assert!(error.message.contains("roads"), "{}", error.message);
    }

    /// Two attachments whose derived ids collide: the number names neither, and
    /// the request is refused rather than aimed at the first of them, which on a
    /// delete would take the wrong file. Forced here by listing one attachment
    /// twice, which is what a collision looks like from `find`.
    #[test]
    fn a_collision_is_refused_and_names_the_native_api() {
        let id = Uuid::now_v7();
        let held = vec![meta_of(id), meta_of(id)];
        let layer = layer_of("OBJECTID");
        let error = find(&held, derived_id(id), &layer, 7).unwrap_err();
        assert_eq!(error.code, 409, "{}", error.message);
        assert!(
            error.message.contains("/api/v1/attachments"),
            "{}",
            error.message
        );
    }

    #[test]
    fn an_id_list_keeps_its_order_and_drops_repeats() {
        assert_eq!(numbers("3, 1,3 ,2", "objectIds").unwrap(), vec![3, 1, 2]);
        assert!(numbers("3,x", "objectIds").is_err());
        assert!(numbers(" , ", "objectIds").is_err());
    }

    /// Nothing a client uploaded can end a header or start a new one.
    #[test]
    fn a_header_value_cannot_be_broken_by_a_stored_name_or_type() {
        assert_eq!(
            filename("in\"voice\\.pdf"),
            "invoice.pdf",
            "a quote or a backslash ends the quoted string"
        );
        assert_eq!(filename("a\r\nX-Evil: 1"), "aX-Evil: 1");
        assert_eq!(filename("\r\n"), FILE_PART);
        assert_eq!(header_text("image/png", DEFAULT_CONTENT_TYPE), "image/png");
        assert_eq!(
            header_text("text/html\r\nX-Evil: 1", DEFAULT_CONTENT_TYPE),
            DEFAULT_CONTENT_TYPE
        );
        assert_eq!(header_text("", DEFAULT_CONTENT_TYPE), DEFAULT_CONTENT_TYPE);
    }

    #[test]
    fn the_oversize_refusal_names_the_cap() {
        let error = oversize();
        assert_eq!(error.code, 413);
        assert!(error.message.contains("32 MiB"), "{}", error.message);
    }
}
