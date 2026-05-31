// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! JWT authentication and RBAC middleware.

use axum::{
    Json,
    extract::Request,
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{DecodingKey, Validation, decode};
use serde::{Deserialize, Serialize};

/// JWT claims structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject (user ID)
    pub sub: String,
    /// User's display name
    pub name: String,
    /// Roles assigned to the user
    pub roles: Vec<Role>,
    /// Expiration time (UNIX timestamp)
    pub exp: u64,
    /// Issued at (UNIX timestamp)
    pub iat: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    Editor,
    Viewer,
}

impl Role {
    pub fn can_write(&self) -> bool {
        matches!(self, Role::Admin | Role::Editor)
    }

    pub fn can_admin(&self) -> bool {
        matches!(self, Role::Admin)
    }
}

/// Configuration for JWT validation.
#[derive(Clone)]
pub struct AuthConfig {
    pub secret: String,
    pub enabled: bool,
}

impl AuthConfig {
    pub fn from_env() -> Self {
        let secret = std::env::var("PTOLEMY_JWT_SECRET").unwrap_or_default();
        let enabled = !secret.is_empty();
        Self { secret, enabled }
    }

    pub fn disabled() -> Self {
        Self {
            secret: String::new(),
            enabled: false,
        }
    }
}

/// Middleware that validates JWT tokens or API keys from request headers.
/// Checks Authorization: Bearer <jwt> first, then X-API-Key header.
/// If auth is disabled (no PTOLEMY_JWT_SECRET), all requests pass through.
pub async fn auth_middleware(request: Request, next: Next) -> Response {
    let config = AuthConfig::from_env();

    if !config.enabled {
        return next.run(request).await;
    }

    // Skip auth for health endpoint
    if request.uri().path() == "/api/v1/health" {
        return next.run(request).await;
    }

    // Try JWT Bearer token first
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    if let Some(token) = token {
        let key = DecodingKey::from_secret(config.secret.as_bytes());
        let validation = Validation::default();

        return match decode::<Claims>(token, &key, &validation) {
            Ok(data) => {
                let mut request = request;
                request.extensions_mut().insert(data.claims);
                next.run(request).await
            }
            Err(e) => (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": format!("invalid token: {e}")})),
            )
                .into_response(),
        };
    }

    // Try API key (X-API-Key header)
    let api_key = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok());

    if let Some(_api_key) = api_key {
        // API key validation is done by the api_key_middleware (requires DB access).
        // If we reach here, the key header is present — let it through to be validated
        // by the stateful layer. Attach a marker so downstream knows to check.
        let mut request = request;
        request.extensions_mut().insert(ApiKeyPresent);
        return next.run(request).await;
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error": "missing authorization header"})),
    )
        .into_response()
}

/// Marker type indicating an API key was presented (needs DB validation).
#[derive(Clone)]
pub struct ApiKeyPresent;

/// Require write permission (admin or editor role).
pub async fn require_write(request: Request, next: Next) -> Response {
    let config = AuthConfig::from_env();
    if !config.enabled {
        return next.run(request).await;
    }

    let claims = request.extensions().get::<Claims>();
    match claims {
        Some(c) if c.roles.iter().any(|r| r.can_write()) => next.run(request).await,
        Some(_) => (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "write permission required"})),
        )
            .into_response(),
        None => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "not authenticated"})),
        )
            .into_response(),
    }
}

/// Generate a JWT token (for testing/admin use).
pub fn generate_token(
    secret: &str,
    sub: &str,
    name: &str,
    roles: Vec<Role>,
    ttl_secs: u64,
) -> String {
    use jsonwebtoken::{EncodingKey, Header, encode};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let claims = Claims {
        sub: sub.to_string(),
        name: name.to_string(),
        roles,
        exp: now + ttl_secs,
        iat: now,
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

/// Generate a JWT token using the configured secret (for OIDC callback).
/// Returns Err if PTOLEMY_JWT_SECRET is not set.
pub fn generate_token_from_env(sub: &str, name: &str, roles: &[Role]) -> Result<String, String> {
    let config = AuthConfig::from_env();
    if !config.enabled {
        return Err("JWT secret not configured (set PTOLEMY_JWT_SECRET)".into());
    }
    Ok(generate_token(
        &config.secret,
        sub,
        name,
        roles.to_vec(),
        86400,
    ))
}
