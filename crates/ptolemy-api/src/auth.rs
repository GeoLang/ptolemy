// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! JWT authentication and RBAC middleware.
//!
//! Claims are `sub`/`exp`/`role` (HS256), the same shape tiletopia and collecta
//! use, so one platform secret mints tokens every service accepts.
//!
//! Reads are anonymous by product decision; everything that mutates needs a
//! token. [`classify`] is the single place that decides which is which, so a
//! new route is write-gated by default rather than by remembering to add a
//! layer.

use axum::{
    Json,
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{DecodingKey, Validation, decode};
use serde::{Deserialize, Serialize};

/// Shortest HS256 secret we accept, matching collecta.
pub const MIN_SECRET_LEN: usize = 32;

/// JWT claims structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject (user ID)
    pub sub: String,
    /// Expiration time (UNIX timestamp)
    pub exp: usize,
    /// Role name: `admin`, `editor` or `viewer`
    pub role: String,
}

impl Claims {
    /// Parsed role, or `None` when the token carries a role we don't know.
    pub fn parsed_role(&self) -> Option<Role> {
        Role::parse(&self.role)
    }

    pub fn can_write(&self) -> bool {
        self.parsed_role().is_some_and(|r| r.can_write())
    }

    pub fn can_admin(&self) -> bool {
        self.parsed_role().is_some_and(|r| r.can_admin())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Editor,
    Viewer,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Editor => "editor",
            Role::Viewer => "viewer",
        }
    }

    /// Unknown role strings are rejected rather than defaulted, so a typo
    /// cannot silently grant write access.
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "admin" => Some(Role::Admin),
            "editor" => Some(Role::Editor),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }

    pub fn can_write(&self) -> bool {
        matches!(self, Role::Admin | Role::Editor)
    }

    pub fn can_admin(&self) -> bool {
        matches!(self, Role::Admin)
    }
}

/// What a request needs before it reaches a handler, when auth is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// No token needed.
    Public,
    /// Valid token with role `editor` or `admin`.
    Write,
    /// Valid token with role `admin`.
    Admin,
}

/// POST endpoints that only query, so they follow the same public rule as GET.
/// Kept deliberately short: anything not listed is write-gated.
const PUBLIC_QUERY_SUFFIXES: [&str; 3] = [
    "/features/intersects",
    "/features/within",
    "/features/filter",
];

/// Decide what a request needs. Reads (GET/HEAD/OPTIONS) and the query-shaped
/// POSTs are public; privilege and delivery-config changes are admin-only;
/// everything else that mutates needs write access.
pub fn classify(method: &Method, path: &str) -> Access {
    if *method == Method::GET || *method == Method::HEAD || *method == Method::OPTIONS {
        return Access::Public;
    }

    if PUBLIC_QUERY_SUFFIXES.iter().any(|s| path.ends_with(s)) {
        return Access::Public;
    }

    // granting permissions, org membership, webhook delivery and peer
    // replication all hand out access or exfiltrate data, so admin only
    if path.contains("/permissions")
        || path.contains("/webhooks")
        || path.starts_with("/api/v1/orgs")
        || path.starts_with("/api/v1/replication/peers")
    {
        return Access::Admin;
    }

    Access::Write
}

/// Configuration for JWT validation.
#[derive(Clone)]
pub struct AuthConfig {
    pub secret: String,
    pub enabled: bool,
}

/// Redacted so a stray `{:?}` cannot put the signing secret in a log line.
impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("enabled", &self.enabled)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl AuthConfig {
    /// Permissive env read, used when building a router in tests and tools: a
    /// missing secret means auth off. The serve path must use
    /// [`AuthConfig::from_env_strict`] instead so a missing secret refuses to
    /// start rather than silently opening every write endpoint.
    pub fn from_env() -> Self {
        let secret = std::env::var("PTOLEMY_JWT_SECRET").unwrap_or_default();
        let enabled = !secret.is_empty();
        Self { secret, enabled }
    }

    /// Fail-closed env read for the serve path.
    pub fn from_env_strict() -> Result<Self, String> {
        let secret = std::env::var("PTOLEMY_JWT_SECRET").ok();
        let disabled = std::env::var("PTOLEMY_AUTH_DISABLED").as_deref() == Ok("true");
        Self::resolve(secret.as_deref(), disabled)
    }

    /// Resolve a startup config from its two inputs. Errors carry no secret
    /// material, only its length.
    pub fn resolve(secret: Option<&str>, auth_disabled: bool) -> Result<Self, String> {
        if auth_disabled {
            // also on stderr: a warning this important must not depend on RUST_LOG
            const MSG: &str = "PTOLEMY_AUTH_DISABLED=true: authentication is OFF, every write endpoint is open to anonymous callers";
            eprintln!("WARNING: {MSG}");
            tracing::warn!("{MSG}");
            return Ok(Self::disabled());
        }

        let secret = secret.unwrap_or_default();
        if secret.is_empty() {
            return Err(
                "PTOLEMY_JWT_SECRET is not set. Set it to 32+ random bytes shared with the other \
                 platform services, or set PTOLEMY_AUTH_DISABLED=true to run without auth."
                    .into(),
            );
        }
        if secret.len() < MIN_SECRET_LEN {
            return Err(format!(
                "PTOLEMY_JWT_SECRET is {} bytes, need at least {MIN_SECRET_LEN}",
                secret.len()
            ));
        }
        Ok(Self {
            secret: secret.to_string(),
            enabled: true,
        })
    }

    pub fn enabled(secret: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
            enabled: true,
        }
    }

    pub fn disabled() -> Self {
        Self {
            secret: String::new(),
            enabled: false,
        }
    }
}

fn deny(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({"error": message}))).into_response()
}

/// Middleware that validates the `Authorization: Bearer <jwt>` header and
/// enforces the role [`classify`] asks for. Claims are put in the request
/// extensions for handlers that want the caller identity.
/// If auth is disabled, all requests pass through.
pub async fn auth_middleware(
    State(config): State<AuthConfig>,
    request: Request,
    next: Next,
) -> Response {
    if !config.enabled {
        return next.run(request).await;
    }

    let access = classify(request.method(), request.uri().path());
    if access == Access::Public {
        return next.run(request).await;
    }

    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let Some(token) = token else {
        return deny(StatusCode::UNAUTHORIZED, "missing bearer token");
    };

    // the decode error is not echoed back: it distinguishes "expired" from
    // "bad signature", which helps an attacker more than a caller
    let key = DecodingKey::from_secret(config.secret.as_bytes());
    let Ok(data) = decode::<Claims>(token, &key, &Validation::default()) else {
        return deny(StatusCode::UNAUTHORIZED, "invalid or expired token");
    };
    let claims = data.claims;

    let allowed = match access {
        Access::Public => true,
        Access::Write => claims.can_write(),
        Access::Admin => claims.can_admin(),
    };
    if !allowed {
        return deny(
            StatusCode::FORBIDDEN,
            match access {
                Access::Admin => "admin role required",
                _ => "editor or admin role required",
            },
        );
    }

    let mut request = request;
    request.extensions_mut().insert(claims);
    next.run(request).await
}

/// Generate a JWT token (for testing/admin use).
pub fn generate_token(secret: &str, sub: &str, role: Role, ttl_secs: u64) -> String {
    use jsonwebtoken::{EncodingKey, Header, encode};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let claims = Claims {
        sub: sub.to_string(),
        exp: (now + ttl_secs) as usize,
        role: role.as_str().to_string(),
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
pub fn generate_token_from_env(sub: &str, role: Role) -> Result<String, String> {
    let config = AuthConfig::from_env();
    if !config.enabled {
        return Err("JWT secret not configured (set PTOLEMY_JWT_SECRET)".into());
    }
    Ok(generate_token(&config.secret, sub, role, 86400))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn resolve_rejects_missing_secret() {
        let err = AuthConfig::resolve(None, false).unwrap_err();
        assert!(err.contains("PTOLEMY_JWT_SECRET is not set"));
        assert!(AuthConfig::resolve(Some(""), false).is_err());
    }

    #[test]
    fn resolve_rejects_short_secret() {
        let err = AuthConfig::resolve(Some("too-short"), false).unwrap_err();
        assert!(err.contains("need at least 32"));
        // the error must not leak the secret itself
        assert!(!err.contains("too-short"));
    }

    #[test]
    fn resolve_accepts_long_secret() {
        let cfg = AuthConfig::resolve(Some(SECRET), false).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.secret, SECRET);
    }

    #[test]
    fn resolve_allows_explicit_opt_out() {
        let cfg = AuthConfig::resolve(None, true).unwrap();
        assert!(!cfg.enabled);
    }

    #[test]
    fn classify_reads_are_public() {
        assert_eq!(
            classify(&Method::GET, "/api/v1/branches/x/features"),
            Access::Public
        );
        assert_eq!(classify(&Method::HEAD, "/api/v1/datasets"), Access::Public);
        assert_eq!(classify(&Method::GET, "/api/v1/health"), Access::Public);
    }

    #[test]
    fn classify_spatial_queries_are_public() {
        assert_eq!(
            classify(&Method::POST, "/api/v1/branches/x/features/intersects"),
            Access::Public
        );
        assert_eq!(
            classify(&Method::POST, "/api/v1/branches/x/features/within"),
            Access::Public
        );
        assert_eq!(
            classify(&Method::POST, "/api/v1/branches/x/features/filter"),
            Access::Public
        );
    }

    #[test]
    fn classify_writes_need_write_access() {
        for (method, path) in [
            (Method::POST, "/api/v1/branches/x/commit"),
            (Method::POST, "/api/v1/datasets"),
            (Method::POST, "/api/v1/qgis/branches/x/sync"),
            (Method::POST, "/api/v1/branches/x/merge/y"),
            (Method::POST, "/api/v1/conflicts/x/resolve"),
            (Method::POST, "/api/v1/branches/x/geoprocessing/split"),
            (Method::DELETE, "/api/v1/attachments/x"),
            (Method::PUT, "/api/v1/reviews/x/approve"),
        ] {
            assert_eq!(classify(&method, path), Access::Write, "{method} {path}");
        }
    }

    #[test]
    fn classify_admin_ops_need_admin() {
        for (method, path) in [
            (Method::POST, "/api/v1/datasets/x/webhooks"),
            (Method::DELETE, "/api/v1/webhooks/x"),
            (Method::POST, "/api/v1/datasets/x/permissions"),
            (Method::DELETE, "/api/v1/branches/x/permissions/u"),
            (Method::POST, "/api/v1/orgs"),
            (Method::DELETE, "/api/v1/orgs/x/members/u"),
            (Method::POST, "/api/v1/replication/peers"),
        ] {
            assert_eq!(classify(&method, path), Access::Admin, "{method} {path}");
        }
    }

    #[test]
    fn unknown_role_cannot_write() {
        let claims = Claims {
            sub: "u".into(),
            exp: 0,
            role: "Editor".into(), // wrong case, not a known role
        };
        assert!(!claims.can_write());
        assert!(!claims.can_admin());
    }

    /// tiletopia mints exactly this claim shape; decoding it as our own
    /// [`Claims`] is what makes one platform secret work across services.
    #[test]
    fn decodes_tiletopia_claims() {
        #[derive(Serialize)]
        struct TiletopiaClaims {
            sub: String,
            exp: usize,
            role: String,
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as usize;
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &TiletopiaClaims {
                sub: "6f1c2d3e".into(),
                exp: now + 3600,
                role: "editor".into(),
            },
            &jsonwebtoken::EncodingKey::from_secret(SECRET.as_bytes()),
        )
        .unwrap();

        let decoded = decode::<Claims>(
            &token,
            &DecodingKey::from_secret(SECRET.as_bytes()),
            &Validation::default(),
        )
        .unwrap()
        .claims;
        assert_eq!(decoded.sub, "6f1c2d3e");
        assert_eq!(decoded.parsed_role(), Some(Role::Editor));
        assert!(decoded.can_write());
        assert!(!decoded.can_admin());
    }

    #[test]
    fn generated_token_round_trips() {
        let token = generate_token(SECRET, "u1", Role::Admin, 60);
        let claims = decode::<Claims>(
            &token,
            &DecodingKey::from_secret(SECRET.as_bytes()),
            &Validation::default(),
        )
        .unwrap()
        .claims;
        assert_eq!(claims.role, "admin");
        assert!(claims.can_admin());
    }
}
