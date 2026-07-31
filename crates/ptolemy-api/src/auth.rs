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
    extract::{FromRequestParts, MatchedPath, Request, State},
    http::{HeaderMap, Method, StatusCode, Uri, header, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{DecodingKey, Validation, decode};
use ptolemy_storage::{Reader, Writer};
use serde::{Deserialize, Serialize};

/// Shortest HS256 secret we accept, matching collecta.
pub const MIN_SECRET_LEN: usize = 32;

/// Path prefix of the websocket endpoints (`/ws/branches/{id}`, `/ws/rooms/{id}`).
pub const WS_PREFIX: &str = "/ws/";

/// The ArcGIS FeatureServer frontend's root. It is read-only, so every route
/// under it is a read whatever method it takes.
const ARCGIS_PREFIX: &str = "/arcgis/rest/services";

/// Subprotocol name that marks a WebSocket handshake as carrying a bearer token.
/// See [`request_token`] for the full contract.
pub const BEARER_SUBPROTOCOL: &str = "bearer";

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
    /// Any valid token. For routes whose authorization is per-dataset and so
    /// cannot be decided from the path: the handler does it.
    Authenticated,
    /// Valid token with role `editor` or `admin`.
    Write,
    /// Valid token with role `admin`.
    Admin,
}

/// Bearer token for a request: the `Authorization` header, or on a websocket
/// path a `Sec-WebSocket-Protocol: bearer, <jwt>` offer.
///
/// The subprotocol form is there because a browser cannot set the Authorization
/// header on a WebSocket handshake. It is preferred over a query parameter
/// because proxies do not log request headers. It is scoped to [`WS_PREFIX`], so
/// nowhere else does a subprotocol act as a credential. Mirrors tiletopia's
/// contract so one platform token works against both services.
pub fn request_token<'a>(headers: &'a HeaderMap, uri: &'a Uri) -> Option<&'a str> {
    let header_token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let token = match header_token {
        Some(token) => Some(token),
        None if uri.path().starts_with(WS_PREFIX) => subprotocol_token(headers),
        None => None,
    };
    token.filter(|t| !t.is_empty())
}

/// Token out of a `Sec-WebSocket-Protocol: bearer, <jwt>` offer. Order is fixed:
/// the marker first, the token second, both in one header value, which is what
/// `new WebSocket(url, ["bearer", jwt])` sends.
fn subprotocol_token(headers: &HeaderMap) -> Option<&str> {
    let offered = headers.get("Sec-WebSocket-Protocol")?.to_str().ok()?;
    let mut entries = offered.split(',').map(str::trim);
    if entries.next()? != BEARER_SUBPROTOCOL {
        return None;
    }
    entries.next()
}

/// The route template a request matched, such as
/// `/api/v1/datasets/{id}/tags/{tag}`.
///
/// **Every path-based policy decision has to be made on this and never on the
/// raw request path.** A route whose template ends in a free-text parameter
/// (`{tag}`, `{user_id}`, `{name}`) lets the caller put any single segment they
/// like exactly where a raw-path match would look. Keying on the raw path made
/// `DELETE /api/v1/datasets/{id}/tags/trace` read as the compute-only `/trace`
/// endpoint and skip the write ladder, and `.../tags/permissions` read as grant
/// management and drop the required role from editor to any. A template comes
/// from the route tables in this crate, so nothing in it is caller-controlled.
///
/// `None` means no route matched: axum sets `MatchedPath` while routing and
/// leaves it unset only on the fallback. Every router here is mounted with
/// `nest`, `merge` or `route`, all of which register in the outer matcher, so a
/// request that will reach a handler always carries one. `matched_route_is_known`
/// in the integration tests pins that.
pub fn route_template(extensions: &axum::http::Extensions) -> Option<&str> {
    extensions.get::<MatchedPath>().map(MatchedPath::as_str)
}

/// POST endpoints that only query, so they follow the same public rule as GET.
/// Kept deliberately short: anything not listed is write-gated.
const PUBLIC_QUERY_SUFFIXES: [&str; 3] = [
    "/features/intersects",
    "/features/within",
    "/features/filter",
];

/// Whole subtrees whose mutating methods only compute. Every route under them
/// reads features and returns a derived geometry without persisting anything.
const COMPUTE_ONLY_SEGMENTS: [&str; 2] = ["/geoprocessing/", "/3d/"];

/// The rest of the endpoints that take a POST body because the request is too
/// big for a query string but write nothing. See [`needs_write_grant`].
const COMPUTE_ONLY_SUFFIXES: [&str; 17] = [
    // network analysis over an existing graph
    "/trace",
    "/shortest-path",
    "/astar",
    "/isochrone",
    "/tsp",
    // point cloud
    "/query",
    "/profile",
    // rule and topology checks
    "/validate",
    // similarity
    "/similarity/search",
    "/similarity/cluster",
    // hex and projection maths that touch no branch
    "/h3/compact",
    "/transform",
    // trajectory pair maths
    "/trajectories/nearest",
    // vertical slices that return a computed answer and tell the caller to
    // commit it themselves
    "/parcels/split",
    "/parcels/merge",
    "/surveys/compare",
    "/coverage/simulate",
];

/// Whether the per-dataset write ladder applies to this request.
///
/// `route` is a [`route_template`], never a raw path. The exemption lists are
/// matched against it, so an entry can only ever collide with a literal segment
/// this crate wrote, not with a value a caller supplied.
///
/// Default is yes for every mutating method, so a route added tomorrow is
/// guarded without its author doing anything. The exceptions are listed above
/// and each one has to be opted in by hand.
///
/// The failure modes are deliberately asymmetric. Forgetting to list a new
/// compute endpoint denies a read-only caller and shows up as a 403 the first
/// time someone without a write grant uses it. Forgetting anything about a new
/// write endpoint leaves it guarded. Only the loud mistake is possible in the
/// direction that matters.
pub fn needs_write_grant(method: &Method, route: &str) -> bool {
    if !matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        return false;
    }

    // Grant management has its own, stricter gate in rbac.rs: an admin grant on
    // the dataset. Running the ladder here as well would deny a dataset admin
    // managing permissions on a branch whose rows do not include them, which is
    // exactly the case they need it for.
    if route.contains("/permissions") {
        return false;
    }

    if COMPUTE_ONLY_SEGMENTS.iter().any(|s| route.contains(s)) {
        return false;
    }
    if PUBLIC_QUERY_SUFFIXES.iter().any(|s| route.ends_with(s)) {
        return false;
    }
    if COMPUTE_ONLY_SUFFIXES.iter().any(|s| route.ends_with(s)) {
        return false;
    }
    // an evacuation route is pure arithmetic; POST /incidents commits a feature
    if route.ends_with("/incidents/evacuate") {
        return false;
    }
    // simplifying a trajectory reports what the simplification would do without
    // storing it, while simplifying a topology edits the topology in place
    if route.contains("/trajectories/") && route.ends_with("/simplify") {
        return false;
    }

    true
}

/// Decide what a request needs. Reads (GET/HEAD/OPTIONS) and the query-shaped
/// POSTs are public; privilege and delivery-config changes are admin-only;
/// everything else that mutates needs write access.
///
/// `route` is a [`route_template`]. Callers may fall back to the raw path only
/// when no route matched, where the answer decides which error the client sees
/// and nothing else: no handler runs on that path.
pub fn classify(method: &Method, route: &str) -> Access {
    // A websocket handshake is a GET, so without this it would fall into the
    // read-is-public rule below and both sockets would accept anonymous callers.
    // /ws/rooms/{id} relays whatever a peer sends to every other peer in the
    // room, so an anonymous socket is a write, not a read.
    if route.starts_with(WS_PREFIX) {
        return Access::Authenticated;
    }

    // Grant management is authorized per dataset, not per role: the holder of an
    // `admin` grant manages their own dataset. The route alone cannot decide that,
    // so any valid token gets through to the handler, which enforces
    // instance-admin-or-dataset-admin and answers 403 otherwise. This still has
    // to sit above the read-is-public rule, or an anonymous GET would leak the ACL.
    if route.contains("/permissions") {
        return Access::Authenticated;
    }

    // webhook config, org membership and audit are ACL/config that both hand out
    // access and exfiltrate data, and have no per-dataset owner to delegate to,
    // so they stay admin-only for every method.
    if route.contains("/webhooks")
        || route.starts_with("/api/v1/orgs")
        || route.starts_with("/api/v1/audit")
    {
        return Access::Admin;
    }

    // /metrics leaks traffic shape and the non-uuid path identifiers the label
    // normalizer keeps (topology names, room ids). The platform compose publishes
    // ptolemy's port straight to the host, so a proxy allowlist can't cover it.
    if route == "/metrics" {
        return Access::Admin;
    }

    if *method == Method::GET || *method == Method::HEAD || *method == Method::OPTIONS {
        // Two reads that are diagnostics and bulk replication, not map data, so
        // the anonymous-viewer decision does not cover them.

        // dataset event history is webhook delivery diagnostics: it carries the
        // payloads that were sent and the traffic shape. lrs has
        // /routes/{id}/events, map data with the same suffix, so the dataset
        // prefix is part of the match. Emitting an event stays a normal write.
        if route.starts_with("/api/v1/datasets/") && route.ends_with("/events") {
            return Access::Admin;
        }
        // the change feed dumps every change on a branch for a replication peer,
        // and registering a peer is already admin-only
        if route.starts_with("/api/v1/replication/feed") {
            return Access::Admin;
        }
        return Access::Public;
    }

    // A PostGIS topology is a Postgres schema, not rows inside a dataset:
    // CreateTopology issues DDL and the route's `{id}` is discarded. There is no
    // owner for the write ladder to check, so these stay admin-only until a
    // topology is bound to a dataset. Reads already returned above.
    if route.ends_with("/topologies")
        || route.ends_with("/add-face")
        || (route.starts_with("/api/v1/topologies/") && route.ends_with("/simplify"))
    {
        return Access::Admin;
    }

    if PUBLIC_QUERY_SUFFIXES.iter().any(|s| route.ends_with(s)) {
        return Access::Public;
    }

    // the FeatureServer query reads features and takes a POST body only because
    // an object id list is too long for a URL. Nothing under /arcgis writes.
    if route.starts_with(ARCGIS_PREFIX) && route.ends_with("/query") {
        return Access::Public;
    }

    // the point cloud spatial query and elevation profile only SELECT, so they
    // are reads that happen to take a POST body. The prefix is part of the match
    // because /query and /profile are generic enough to catch a future write.
    if route.starts_with("/api/v1/pointclouds/")
        && (route.ends_with("/query") || route.ends_with("/profile"))
    {
        return Access::Public;
    }

    // registering a replication peer hands out a data feed, so admin only;
    // listing peers (GET) stays public under the read rule above
    if route.starts_with("/api/v1/replication/peers") {
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

    let mut request = request;
    // per-dataset enforcement is off in dev mode, so downstream needs to know
    // auth ran at all, not just whether this request carried a token
    request.extensions_mut().insert(AuthEnabled);

    // the matched template, so a caller cannot put a policy keyword in a
    // free-text segment. Falling back to the raw path is only reachable when no
    // route matched, where the fallback answers 404 and no handler runs, so the
    // decision picks the error the client sees and grants nothing.
    let access = {
        let route = route_template(request.extensions()).unwrap_or_else(|| request.uri().path());
        classify(request.method(), route)
    };
    let token = request_token(request.headers(), request.uri()).map(str::to_owned);
    let key = DecodingKey::from_secret(config.secret.as_bytes());
    let claims = token
        .as_deref()
        .and_then(|t| decode::<Claims>(t, &key, &Validation::default()).ok())
        .map(|data| data.claims);

    if access == Access::Public {
        // a public route ignores a bad token rather than rejecting it, keeping
        // anonymous reads working; a good one still identifies the caller so
        // private-dataset reads can be allowed
        if let Some(claims) = claims {
            request.extensions_mut().insert(claims);
        }
        return next.run(request).await;
    }

    if token.is_none() {
        return deny(StatusCode::UNAUTHORIZED, "missing bearer token");
    }
    // the decode error is not echoed back: it distinguishes "expired" from
    // "bad signature", which helps an attacker more than a caller
    let Some(claims) = claims else {
        return deny(StatusCode::UNAUTHORIZED, "invalid or expired token");
    };

    let allowed = match access {
        Access::Public => true,
        // an unknown role string is still not a role, so it gets nothing
        Access::Authenticated => claims.parsed_role().is_some(),
        Access::Write => claims.can_write(),
        Access::Admin => claims.can_admin(),
    };
    if !allowed {
        return deny(
            StatusCode::FORBIDDEN,
            match access {
                Access::Admin => "admin role required",
                Access::Authenticated => "unknown role",
                _ => "editor or admin role required",
            },
        );
    }

    request.extensions_mut().insert(claims);
    next.run(request).await
}

/// Marker put in the request extensions when auth is enabled. Absent means dev
/// mode, where per-dataset permissions and visibility are not enforced at all.
#[derive(Debug, Clone, Copy)]
pub struct AuthEnabled;

/// The caller: who to record in an audit field (`author`, `created_by`,
/// `granted_by`, …) and who to check permission rows against. Claims are absent
/// when auth is off, and on a public route when the request carried no usable
/// token.
#[derive(Debug, Clone)]
pub struct Actor {
    claims: Option<Claims>,
    auth_enabled: bool,
}

impl Actor {
    /// The token subject wins over whatever the body says, so a caller cannot
    /// attribute a write to someone else. With auth off the body value stands,
    /// which keeps dev and CLI flows working.
    pub fn or_body<'a>(&'a self, body: &'a str) -> &'a str {
        self.id().unwrap_or(body)
    }

    /// The verified caller id, or `None` when there is no token to trust.
    pub fn id(&self) -> Option<&str> {
        self.claims.as_ref().map(|c| c.sub.as_str())
    }

    /// The verified caller id only when auth is on, which is when a permission
    /// row means anything. Used for the dataset creator auto-grant.
    pub fn enforced_id(&self) -> Option<&str> {
        if self.auth_enabled { self.id() } else { None }
    }

    pub fn is_instance_admin(&self) -> bool {
        self.claims.as_ref().is_some_and(Claims::can_admin)
    }

    pub fn claims(&self) -> Option<&Claims> {
        self.claims.as_ref()
    }

    /// Whether per-dataset permissions apply at all. False in dev mode, where
    /// there is no verified identity to check them against.
    pub fn enforces(&self) -> bool {
        self.auth_enabled
    }

    /// The identity a dataset listing is filtered by.
    pub fn reader(&self) -> Reader {
        Reader {
            bypass: !self.auth_enabled || self.is_instance_admin(),
            id: self.id().map(str::to_owned),
        }
    }

    /// The identity a write is checked against.
    pub fn writer(&self) -> Writer {
        if !self.auth_enabled {
            return Writer::Unenforced;
        }
        match &self.claims {
            None => Writer::Anonymous,
            Some(claims) => Writer::user(claims.sub.clone(), claims.can_admin()),
        }
    }

    /// The caller as seen from a layer, which has the whole request rather than
    /// its parts. Same construction the extractor uses, so a middleware and a
    /// handler always agree on who is calling.
    pub fn from_extensions(extensions: &axum::http::Extensions) -> Self {
        Actor {
            claims: extensions.get::<Claims>().cloned(),
            auth_enabled: extensions.get::<AuthEnabled>().is_some(),
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Actor {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Actor::from_extensions(&parts.extensions))
    }
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
    fn classify_pointcloud_queries_are_public() {
        assert_eq!(
            classify(&Method::POST, "/api/v1/pointclouds/x/query"),
            Access::Public
        );
        assert_eq!(
            classify(&Method::POST, "/api/v1/pointclouds/x/profile"),
            Access::Public
        );
        // adding a patch is still a write, and the suffixes do not travel
        assert_eq!(
            classify(&Method::POST, "/api/v1/pointclouds/x/patches"),
            Access::Write
        );
        assert_eq!(
            classify(&Method::POST, "/api/v1/branches/x/query"),
            Access::Write
        );
    }

    #[test]
    fn classify_writes_need_write_access() {
        for (method, path) in [
            (Method::POST, "/api/v1/branches/x/commit"),
            (Method::POST, "/api/v1/datasets"),
            (Method::POST, "/api/v1/qgis/branches/x/sync"),
            (Method::POST, "/api/v1/branches/x/merge/y"),
            (Method::POST, "/api/v1/branches/x/merge/y/resolve"),
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
            (Method::POST, "/api/v1/orgs"),
            (Method::DELETE, "/api/v1/orgs/x/members/u"),
            (Method::POST, "/api/v1/replication/peers"),
        ] {
            assert_eq!(classify(&method, path), Access::Admin, "{method} {path}");
        }
    }

    /// The fix: config/ACL/membership/audit reads must be admin, not public,
    /// even though they are GETs.
    #[test]
    fn classify_sensitive_reads_are_admin() {
        for path in [
            "/api/v1/datasets/x/webhooks",
            "/api/v1/orgs",
            "/api/v1/orgs/x/members",
            "/api/v1/audit",
        ] {
            assert_eq!(classify(&Method::GET, path), Access::Admin, "GET {path}");
        }
    }

    /// Delivery history and the replication feed are reads, but diagnostics and
    /// bulk change data rather than map data.
    #[test]
    fn classify_diagnostic_reads_are_admin() {
        for path in ["/api/v1/datasets/x/events", "/api/v1/replication/feed/x"] {
            assert_eq!(classify(&Method::GET, path), Access::Admin, "GET {path}");
            assert_eq!(classify(&Method::HEAD, path), Access::Admin, "HEAD {path}");
        }

        // lrs route events share the suffix but are map data
        assert_eq!(
            classify(&Method::GET, "/api/v1/routes/x/events"),
            Access::Public
        );
        // emitting an event is still an ordinary write, not admin
        assert_eq!(
            classify(&Method::POST, "/api/v1/datasets/x/events"),
            Access::Write
        );
    }

    /// Grant management cannot be decided from the path: a dataset admin manages
    /// their own dataset, so the handler authorizes and any valid token gets in.
    /// It must still never be Public, or an anonymous GET would dump the ACL.
    #[test]
    fn classify_permission_routes_need_a_token_not_a_role() {
        for (method, path) in [
            (Method::GET, "/api/v1/datasets/x/permissions"),
            (Method::POST, "/api/v1/datasets/x/permissions"),
            (Method::DELETE, "/api/v1/datasets/x/permissions/u"),
            (Method::GET, "/api/v1/datasets/x/permissions/u/check"),
            (Method::GET, "/api/v1/branches/x/permissions"),
            (Method::POST, "/api/v1/branches/x/permissions"),
            (Method::DELETE, "/api/v1/branches/x/permissions/u"),
            (Method::GET, "/api/v1/branches/x/permissions/u/check"),
        ] {
            assert_eq!(
                classify(&method, path),
                Access::Authenticated,
                "{method} {path}"
            );
        }
    }

    #[test]
    fn classify_metrics_is_admin() {
        assert_eq!(classify(&Method::GET, "/metrics"), Access::Admin);
        assert_eq!(classify(&Method::HEAD, "/metrics"), Access::Admin);
        // the gate is the exact scrape path, not any path containing "metrics"
        assert_eq!(
            classify(&Method::GET, "/api/v1/branches/x/quality/metrics"),
            Access::Public
        );
    }

    /// The anonymous-viewer product decision: spatial data reads stay public.
    #[test]
    fn classify_data_reads_stay_public() {
        for path in [
            "/api/v1/datasets",
            "/api/v1/datasets/x",
            "/api/v1/branches/x/features",
            "/api/v1/branches/x/tiles/1/2/3",
            "/api/v1/branches/x/export/geojson",
            "/api/v1/replication/peers",
        ] {
            assert_eq!(classify(&Method::GET, path), Access::Public, "GET {path}");
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

    fn headers_with(name: &str, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
        headers
    }

    /// A handshake is a GET, so without the /ws rule it would fall into the
    /// read-is-public branch and both sockets would take anonymous callers.
    #[test]
    fn classify_websockets_need_a_token() {
        assert_eq!(
            classify(&Method::GET, "/ws/branches/6f1c2d3e"),
            Access::Authenticated
        );
        assert_eq!(
            classify(&Method::GET, "/ws/rooms/design-review"),
            Access::Authenticated
        );
    }

    #[test]
    fn request_token_reads_authorization_header() {
        let headers = headers_with("authorization", "Bearer abc.def.ghi");
        let uri: Uri = "/api/v1/datasets".parse().unwrap();
        assert_eq!(request_token(&headers, &uri), Some("abc.def.ghi"));
    }

    #[test]
    fn request_token_reads_subprotocol_on_ws_paths() {
        let headers = headers_with("sec-websocket-protocol", "bearer, abc.def.ghi");
        let uri: Uri = "/ws/rooms/r1".parse().unwrap();
        assert_eq!(request_token(&headers, &uri), Some("abc.def.ghi"));
    }

    /// The subprotocol is a credential only on the websocket paths. Anywhere
    /// else it must be ignored, or any route could be entered with a header a
    /// browser sets from script without a preflight.
    #[test]
    fn subprotocol_is_not_a_credential_off_ws_paths() {
        let headers = headers_with("sec-websocket-protocol", "bearer, abc.def.ghi");
        let uri: Uri = "/api/v1/datasets".parse().unwrap();
        assert_eq!(request_token(&headers, &uri), None);
    }

    #[test]
    fn subprotocol_needs_the_marker_first_and_a_non_empty_token() {
        let uri: Uri = "/ws/rooms/r1".parse().unwrap();
        // marker missing
        let headers = headers_with("sec-websocket-protocol", "abc.def.ghi");
        assert_eq!(request_token(&headers, &uri), None);
        // marker not first
        let headers = headers_with("sec-websocket-protocol", "abc.def.ghi, bearer");
        assert_eq!(request_token(&headers, &uri), None);
        // marker alone
        let headers = headers_with("sec-websocket-protocol", "bearer");
        assert_eq!(request_token(&headers, &uri), None);
        // marker with an empty token
        let headers = headers_with("sec-websocket-protocol", "bearer, ");
        assert_eq!(request_token(&headers, &uri), None);
    }

    /// A real Authorization header wins, so a non-browser client is unaffected.
    #[test]
    fn authorization_header_wins_over_subprotocol() {
        let mut headers = headers_with("authorization", "Bearer real.token");
        headers.insert(
            axum::http::HeaderName::from_static("sec-websocket-protocol"),
            "bearer, other.token".parse().unwrap(),
        );
        let uri: Uri = "/ws/rooms/r1".parse().unwrap();
        assert_eq!(request_token(&headers, &uri), Some("real.token"));
    }
}
