// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! OpenID Connect (OIDC) SSO integration.
//!
//! When configured via environment variables, Ptolemy can authenticate users
//! against an external OIDC provider (Keycloak, Auth0, Google, Azure AD, etc.)
//!
//! Environment variables:
//!   PTOLEMY_OIDC_ISSUER_URL   — The OIDC issuer URL (e.g. https://keycloak.example.com/realms/ptolemy)
//!   PTOLEMY_OIDC_CLIENT_ID    — OAuth2 client ID
//!   PTOLEMY_OIDC_CLIENT_SECRET — OAuth2 client secret
//!   PTOLEMY_OIDC_REDIRECT_URL — Callback URL (e.g. http://localhost:3000/auth/oidc/callback)

use axum::{
    Json, Router,
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};

/// OIDC configuration loaded from environment.
#[derive(Clone, Debug)]
pub struct OidcConfig {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: String,
    pub enabled: bool,
}

impl OidcConfig {
    pub fn from_env() -> Self {
        let issuer_url = std::env::var("PTOLEMY_OIDC_ISSUER_URL").unwrap_or_default();
        let client_id = std::env::var("PTOLEMY_OIDC_CLIENT_ID").unwrap_or_default();
        let client_secret = std::env::var("PTOLEMY_OIDC_CLIENT_SECRET").unwrap_or_default();
        let redirect_url = std::env::var("PTOLEMY_OIDC_REDIRECT_URL").unwrap_or_default();

        let enabled = !issuer_url.is_empty() && !client_id.is_empty();

        Self {
            issuer_url,
            client_id,
            client_secret,
            redirect_url,
            enabled,
        }
    }
}

/// Routes for OIDC login flow.
pub fn oidc_routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/auth/oidc/login", get(oidc_login))
        .route("/auth/oidc/callback", get(oidc_callback))
        .route("/auth/oidc/config", get(oidc_config_info))
}

#[derive(Serialize)]
struct OidcStatus {
    enabled: bool,
    issuer_url: Option<String>,
}

async fn oidc_config_info() -> Json<OidcStatus> {
    let config = OidcConfig::from_env();
    Json(OidcStatus {
        enabled: config.enabled,
        issuer_url: if config.enabled {
            Some(config.issuer_url)
        } else {
            None
        },
    })
}

async fn oidc_login() -> Response {
    let config = OidcConfig::from_env();
    if !config.enabled {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "OIDC not configured"})),
        )
            .into_response();
    }

    // Build authorization URL
    // In production this would use openidconnect crate's discovery,
    // but we construct it from the well-known endpoint pattern.
    let auth_url = format!(
        "{}/protocol/openid-connect/auth?client_id={}&redirect_uri={}&response_type=code&scope=openid%20profile%20email",
        config.issuer_url,
        urlencoding::encode(&config.client_id),
        urlencoding::encode(&config.redirect_url),
    );

    Redirect::temporary(&auth_url).into_response()
}

#[derive(Deserialize)]
struct CallbackParams {
    code: Option<String>,
    error: Option<String>,
}

#[derive(Serialize)]
struct TokenResponse {
    access_token: String,
    user: UserInfo,
}

#[derive(Debug, Serialize, Deserialize)]
struct UserInfo {
    sub: String,
    name: Option<String>,
    email: Option<String>,
}

async fn oidc_callback(Query(params): Query<CallbackParams>) -> Response {
    let config = OidcConfig::from_env();
    if !config.enabled {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "OIDC not configured"})),
        )
            .into_response();
    }

    if let Some(error) = params.error {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": error})),
        )
            .into_response();
    }

    let Some(code) = params.code else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing authorization code"})),
        )
            .into_response();
    };

    // Exchange code for token
    let token_url = format!("{}/protocol/openid-connect/token", config.issuer_url);

    let client = reqwest::Client::new();
    let token_resp = client
        .post(&token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("client_id", &config.client_id),
            ("client_secret", &config.client_secret),
            ("redirect_uri", &config.redirect_url),
        ])
        .send()
        .await;

    let token_resp = match token_resp {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!("OIDC token exchange failed: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": "token exchange failed"})),
            )
                .into_response();
        }
    };

    if !token_resp.status().is_success() {
        let body = token_resp.text().await.unwrap_or_default();
        tracing::error!("OIDC token endpoint returned error: {body}");
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": "token exchange rejected"})),
        )
            .into_response();
    }

    let oidc_token: serde_json::Value = match token_resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Failed to parse OIDC token response: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": "invalid token response"})),
            )
                .into_response();
        }
    };

    // Fetch user info
    let userinfo_url = format!("{}/protocol/openid-connect/userinfo", config.issuer_url);

    let access_token = oidc_token["access_token"].as_str().unwrap_or_default();
    let userinfo_resp = client
        .get(&userinfo_url)
        .bearer_auth(access_token)
        .send()
        .await;

    let user_info: UserInfo = match userinfo_resp {
        Ok(resp) if resp.status().is_success() => resp.json().await.unwrap_or(UserInfo {
            sub: "unknown".into(),
            name: None,
            email: None,
        }),
        _ => UserInfo {
            sub: "unknown".into(),
            name: None,
            email: None,
        },
    };

    // Generate a Ptolemy JWT for the user
    let ptolemy_token = crate::auth::generate_token_from_env(
        &user_info.sub,
        user_info.name.as_deref().unwrap_or(&user_info.sub),
        &[crate::auth::Role::Editor],
    );

    match ptolemy_token {
        Ok(token) => (
            StatusCode::OK,
            Json(TokenResponse {
                access_token: token,
                user: user_info,
            }),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "failed to generate session token"})),
        )
            .into_response(),
    }
}
