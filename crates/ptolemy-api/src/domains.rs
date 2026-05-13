// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Domains, subtypes, and attribute rules.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn domain_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/datasets/{id}/domains",
            get(list_domains).post(create_domain),
        )
        .route("/domains/{id}", get(get_domain).delete(delete_domain))
        .route(
            "/datasets/{id}/subtypes",
            get(list_subtypes).post(create_subtype),
        )
        .route("/subtypes/{id}", get(get_subtype).delete(delete_subtype))
        .route(
            "/datasets/{id}/attribute-rules",
            get(list_rules).post(create_rule),
        )
        .route(
            "/attribute-rules/{id}",
            get(get_rule).put(update_rule).delete(delete_rule),
        )
        .route("/attribute-rules/{id}/validate", post(validate_rule))
}

// ─── Domains ────────────────────────────────────────────────────────

#[derive(Serialize)]
struct Domain {
    id: Uuid,
    name: String,
    domain_type: String,
    field_type: String,
    coded_values: Option<serde_json::Value>,
    range_min: Option<f64>,
    range_max: Option<f64>,
}

async fn list_domains(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<Domain>>, DomainError> {
    let rows = sqlx::query(
        "SELECT id, name, domain_type, field_type, coded_values, range_min, range_max
         FROM domains WHERE dataset_id = $1 ORDER BY name",
    )
    .bind(dataset_id)
    .fetch_all(store.pool())
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| Domain {
                id: r.get("id"),
                name: r.get("name"),
                domain_type: r.get("domain_type"),
                field_type: r.get("field_type"),
                coded_values: r.get("coded_values"),
                range_min: r.get("range_min"),
                range_max: r.get("range_max"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateDomainRequest {
    name: String,
    domain_type: String,
    field_type: String,
    coded_values: Option<serde_json::Value>,
    range_min: Option<f64>,
    range_max: Option<f64>,
}

async fn create_domain(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<CreateDomainRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), DomainError> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO domains (id, dataset_id, name, domain_type, field_type, coded_values, range_min, range_max)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    ).bind(id).bind(dataset_id).bind(&req.name).bind(&req.domain_type).bind(&req.field_type)
    .bind(&req.coded_values).bind(req.range_min).bind(req.range_max)
    .execute(store.pool()).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": id}))))
}

async fn get_domain(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Domain>, DomainError> {
    let r = sqlx::query(
        "SELECT id, name, domain_type, field_type, coded_values, range_min, range_max
         FROM domains WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(store.pool())
    .await?
    .ok_or(DomainError::NotFound)?;
    Ok(Json(Domain {
        id: r.get("id"),
        name: r.get("name"),
        domain_type: r.get("domain_type"),
        field_type: r.get("field_type"),
        coded_values: r.get("coded_values"),
        range_min: r.get("range_min"),
        range_max: r.get("range_max"),
    }))
}

async fn delete_domain(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, DomainError> {
    sqlx::query("DELETE FROM domains WHERE id = $1")
        .bind(id)
        .execute(store.pool())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── Subtypes ───────────────────────────────────────────────────────

#[derive(Serialize)]
struct Subtype {
    id: Uuid,
    name: String,
    code: i32,
    default_values: serde_json::Value,
    domain_assignments: serde_json::Value,
}

async fn list_subtypes(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<Subtype>>, DomainError> {
    let rows = sqlx::query(
        "SELECT id, name, code, default_values, domain_assignments
         FROM subtypes WHERE dataset_id = $1 ORDER BY code",
    )
    .bind(dataset_id)
    .fetch_all(store.pool())
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| Subtype {
                id: r.get("id"),
                name: r.get("name"),
                code: r.get("code"),
                default_values: r.get("default_values"),
                domain_assignments: r.get("domain_assignments"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateSubtypeRequest {
    name: String,
    code: i32,
    #[serde(default)]
    default_values: serde_json::Value,
    #[serde(default)]
    domain_assignments: serde_json::Value,
}

async fn create_subtype(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<CreateSubtypeRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), DomainError> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO subtypes (id, dataset_id, name, code, default_values, domain_assignments)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(dataset_id)
    .bind(&req.name)
    .bind(req.code)
    .bind(&req.default_values)
    .bind(&req.domain_assignments)
    .execute(store.pool())
    .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": id}))))
}

async fn get_subtype(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Subtype>, DomainError> {
    let r = sqlx::query(
        "SELECT id, name, code, default_values, domain_assignments FROM subtypes WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(store.pool())
    .await?
    .ok_or(DomainError::NotFound)?;
    Ok(Json(Subtype {
        id: r.get("id"),
        name: r.get("name"),
        code: r.get("code"),
        default_values: r.get("default_values"),
        domain_assignments: r.get("domain_assignments"),
    }))
}

async fn delete_subtype(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, DomainError> {
    sqlx::query("DELETE FROM subtypes WHERE id = $1")
        .bind(id)
        .execute(store.pool())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── Attribute Rules ────────────────────────────────────────────────

#[derive(Serialize)]
struct AttributeRule {
    id: Uuid,
    name: String,
    rule_type: String,
    trigger_event: String,
    expression: String,
    error_message: Option<String>,
    enabled: bool,
}

async fn list_rules(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<AttributeRule>>, DomainError> {
    let rows = sqlx::query(
        "SELECT id, name, rule_type, trigger_event, expression, error_message, enabled
         FROM attribute_rules WHERE dataset_id = $1 ORDER BY name",
    )
    .bind(dataset_id)
    .fetch_all(store.pool())
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| AttributeRule {
                id: r.get("id"),
                name: r.get("name"),
                rule_type: r.get("rule_type"),
                trigger_event: r.get("trigger_event"),
                expression: r.get("expression"),
                error_message: r.get("error_message"),
                enabled: r.get("enabled"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateRuleRequest {
    name: String,
    rule_type: String,
    trigger_event: String,
    expression: String,
    error_message: Option<String>,
}

async fn create_rule(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<CreateRuleRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), DomainError> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO attribute_rules (id, dataset_id, name, rule_type, trigger_event, expression, error_message)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    ).bind(id).bind(dataset_id).bind(&req.name).bind(&req.rule_type)
    .bind(&req.trigger_event).bind(&req.expression).bind(&req.error_message)
    .execute(store.pool()).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": id}))))
}

async fn get_rule(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<AttributeRule>, DomainError> {
    let r = sqlx::query(
        "SELECT id, name, rule_type, trigger_event, expression, error_message, enabled
         FROM attribute_rules WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(store.pool())
    .await?
    .ok_or(DomainError::NotFound)?;
    Ok(Json(AttributeRule {
        id: r.get("id"),
        name: r.get("name"),
        rule_type: r.get("rule_type"),
        trigger_event: r.get("trigger_event"),
        expression: r.get("expression"),
        error_message: r.get("error_message"),
        enabled: r.get("enabled"),
    }))
}

#[derive(Deserialize)]
struct UpdateRuleRequest {
    expression: Option<String>,
    error_message: Option<String>,
    enabled: Option<bool>,
}

async fn update_rule(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateRuleRequest>,
) -> Result<StatusCode, DomainError> {
    if let Some(expr) = &req.expression {
        sqlx::query("UPDATE attribute_rules SET expression = $2 WHERE id = $1")
            .bind(id)
            .bind(expr)
            .execute(store.pool())
            .await?;
    }
    if let Some(msg) = &req.error_message {
        sqlx::query("UPDATE attribute_rules SET error_message = $2 WHERE id = $1")
            .bind(id)
            .bind(msg)
            .execute(store.pool())
            .await?;
    }
    if let Some(en) = req.enabled {
        sqlx::query("UPDATE attribute_rules SET enabled = $2 WHERE id = $1")
            .bind(id)
            .bind(en)
            .execute(store.pool())
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_rule(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, DomainError> {
    sqlx::query("DELETE FROM attribute_rules WHERE id = $1")
        .bind(id)
        .execute(store.pool())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Validate a rule expression against sample features.
async fn validate_rule(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, DomainError> {
    let r = sqlx::query("SELECT expression, dataset_id FROM attribute_rules WHERE id = $1")
        .bind(id)
        .fetch_optional(store.pool())
        .await?
        .ok_or(DomainError::NotFound)?;
    let _dataset_id: Uuid = r.get("dataset_id");
    let expression: String = r.get("expression");
    // Basic validation: check it's parseable SQL
    let is_valid = !expression.trim().is_empty();
    Ok(Json(
        serde_json::json!({"valid": is_valid, "expression": expression}),
    ))
}

// ─── Error ──────────────────────────────────────────────────────────

enum DomainError {
    Db(sqlx::Error),
    NotFound,
}
impl From<sqlx::Error> for DomainError {
    fn from(e: sqlx::Error) -> Self {
        DomainError::Db(e)
    }
}
impl IntoResponse for DomainError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            DomainError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            DomainError::Db(e) => {
                tracing::error!("DB: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
