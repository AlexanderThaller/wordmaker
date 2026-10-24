//! OAuth 2.1 for the MCP endpoint, as the MCP authorization spec asks for:
//! protected resource and authorization server metadata, clients known by
//! a client ID metadata document or by dynamic registration, the
//! authorization code flow with PKCE, token refresh and revocation.
//!
//! Signing in means entering `WORDMAKER_TOKEN` on the authorization page.
//! Codes and tokens are stored as hashes keyed with that token, so changing
//! it signs every client out.

mod client_metadata;
mod store;

use std::{
    fmt::Write,
    net::SocketAddr,
};

use axum::{
    Extension,
    Form,
    Json,
    Router,
    body::Bytes,
    extract::{
        ConnectInfo,
        Query,
        State,
    },
    http::{
        HeaderMap,
        HeaderValue,
        Method,
        StatusCode,
        header,
    },
    response::{
        Html,
        IntoResponse,
        Redirect,
        Response,
    },
    routing::{
        get,
        post,
    },
};
use base64::Engine;
use chrono::{
    TimeDelta,
    Utc,
};
pub use client_metadata::ClientCache;
use client_metadata::{
    ClientInfo,
    is_metadata_url,
    redirect_matches,
    valid_redirect_uri,
};
use serde::Deserialize;
use serde_json::json;
use sha2::{
    Digest,
    Sha256,
};
use subtle::ConstantTimeEq;
use tower_http::cors::{
    Any,
    CorsLayer,
};

use crate::{
    app::AppState,
    config::Config,
    domain::settings,
    error::AppError,
    site::escape,
};

const ACCESS_TOKEN_TTL: TimeDelta = TimeDelta::hours(1);
const REFRESH_TOKEN_TTL: TimeDelta = TimeDelta::days(30);
const CODE_TTL: TimeDelta = TimeDelta::minutes(5);
/// Dynamic registration needs no credentials; this bounds what it can fill.
/// Clients that never got tokens make way for new ones.
const MAX_REGISTERED_CLIENTS: i64 = 500;
/// How long a registered client may go without getting tokens.
const UNUSED_CLIENT_TTL: TimeDelta = TimeDelta::days(1);

pub fn router() -> Router<AppState> {
    // Metadata, registration and the token endpoint are called from
    // browser based clients too.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers(Any);
    let api = Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(server_metadata),
        )
        .route("/oauth/register", post(register))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
        .layer(cors);
    Router::new()
        .route("/oauth/authorize", get(authorize).post(decide))
        .merge(api)
}

fn issuer(config: &Config) -> &str {
    config.base_url.trim_end_matches('/')
}

/// The MCP endpoint, which access tokens are issued for.
fn resource(config: &Config) -> String {
    format!("{}/mcp", issuer(config))
}

/// The `WWW-Authenticate` value of a 401 from `/mcp`, pointing clients at
/// the metadata that tells them how to sign in.
pub fn challenge(config: &Config, invalid_token: bool) -> HeaderValue {
    let mut value = format!(
        "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\"",
        issuer(config)
    );
    if invalid_token {
        value.push_str(", error=\"invalid_token\"");
    }
    HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static("Bearer"))
}

/// Whether `token` is an unexpired access token for the MCP endpoint.
pub async fn access_token_valid(state: &AppState, token: &str) -> Result<bool, AppError> {
    let grant = store::find_token(&state.pool, store::Kind::Access, &hash(state, token)).await?;
    Ok(grant.is_some_and(|grant| grant.resource == resource(&state.config)))
}

async fn resource_metadata(State(state): State<AppState>) -> Response {
    let config = &state.config;
    Json(json!({
        "resource": resource(config),
        "authorization_servers": [issuer(config)],
        "bearer_methods_supported": ["header"],
        "resource_name": "WordMaker",
    }))
    .into_response()
}

async fn server_metadata(State(state): State<AppState>) -> Response {
    let issuer = issuer(&state.config);
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/oauth/authorize"),
        "token_endpoint": format!("{issuer}/oauth/token"),
        "registration_endpoint": format!("{issuer}/oauth/register"),
        "revocation_endpoint": format!("{issuer}/oauth/revoke"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "revocation_endpoint_auth_methods_supported": ["none"],
        "client_id_metadata_document_supported": true,
        "authorization_response_iss_parameter_supported": true,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Dynamic client registration (RFC 7591)

#[derive(Deserialize)]
struct Registration {
    #[serde(default)]
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
}

async fn register(State(state): State<AppState>, body: Bytes) -> Response {
    let invalid = |description: &str| {
        oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            description,
        )
    };
    let Ok(registration) = serde_json::from_slice::<Registration>(&body) else {
        return invalid("the body must be a JSON client metadata object");
    };
    if registration.redirect_uris.is_empty() || registration.redirect_uris.len() > 10 {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "between one and ten redirect_uris are required",
        );
    }
    if let Some(uri) = registration
        .redirect_uris
        .iter()
        .find(|uri| !valid_redirect_uri(uri))
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            &format!("redirect URI not allowed: {uri}"),
        );
    }
    if registration
        .grant_types
        .iter()
        .flatten()
        .any(|g| g != "authorization_code" && g != "refresh_token")
    {
        return invalid("only the authorization_code and refresh_token grants are supported");
    }
    if registration
        .response_types
        .iter()
        .flatten()
        .any(|r| r != "code")
    {
        return invalid("only the code response type is supported");
    }
    let client_name = registration
        .client_name
        .map(|name| name.trim().chars().take(100).collect::<String>())
        .filter(|name| !name.is_empty());

    let client = store::RegisteredClient {
        client_id: uuid::Uuid::new_v4().to_string(),
        client_name,
        redirect_uris: registration.redirect_uris,
    };
    match store::insert_client(
        &state.pool,
        &client,
        MAX_REGISTERED_CLIENTS,
        Utc::now() - UNUSED_CLIENT_TTL,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => return invalid("too many registered clients"),
        Err(err) => return server_error(&err),
    }
    (
        StatusCode::CREATED,
        no_store(),
        Json(json!({
            "client_id": client.client_id,
            "client_id_issued_at": Utc::now().timestamp(),
            "client_name": client.client_name,
            "redirect_uris": client.redirect_uris,
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Authorization endpoint

#[derive(Debug, Default, Deserialize)]
struct AuthorizeParams {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    scope: Option<String>,
    resource: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DecisionForm {
    #[serde(flatten)]
    params: AuthorizeParams,
    token: Option<String>,
    decision: Option<String>,
}

/// A checked authorization request.
struct Request {
    client: ClientInfo,
    redirect_uri: String,
    code_challenge: String,
    state: Option<String>,
}

/// Check an authorization request. Problems are shown to the user, never
/// sent to the client: before the user has approved, a redirect would let
/// anyone use this site to send browsers to any URI they registered (RFC
/// 9700, section 4.11.2).
async fn check(state: &AppState, params: &AuthorizeParams) -> Result<Request, String> {
    let client_id = params
        .client_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or("The request does not name a client.")?;
    let client = if is_metadata_url(client_id) {
        state
            .oauth_clients
            .fetch(client_id)
            .await
            .map_err(|err| format!("The client could not be verified: {err}."))?
            .as_ref()
            .clone()
    } else {
        let client = store::get_client(&state.pool, client_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "could not look up an OAuth client");
                "Could not look up the client."
            })?
            .ok_or("This client is not registered.")?;
        ClientInfo {
            client_id: client.client_id,
            client_name: client.client_name,
            redirect_uris: client.redirect_uris,
            verified: false,
        }
    };
    let redirect_uri = match params.redirect_uri.as_deref() {
        Some(uri) if redirect_matches(&client.redirect_uris, uri) => uri.to_owned(),
        None if client.redirect_uris.len() == 1 => client.redirect_uris[0].clone(),
        _ => return Err("The redirect URI does not belong to this client.".into()),
    };
    // Clients registered before the rules got stricter may still hold one.
    if !valid_redirect_uri(&redirect_uri) {
        return Err("The redirect URI of this client is not allowed.".into());
    }

    if params.response_type.as_deref() != Some("code") {
        return Err("The client asked for an unsupported response type.".into());
    }
    let code_challenge = params
        .code_challenge
        .as_deref()
        .filter(|c| (43..=128).contains(&c.len()))
        .filter(|_| params.code_challenge_method.as_deref() == Some("S256"))
        .ok_or("The client did not use PKCE with S256, which is required.")?;
    if let Some(requested) = &params.resource
        && !names_resource(&state.config, requested)
    {
        return Err("The client asked for access to an unknown resource.".into());
    }
    Ok(Request {
        client,
        redirect_uri,
        code_challenge: code_challenge.to_owned(),
        state: params.state.clone(),
    })
}

/// Whether a `resource` parameter names our MCP endpoint (or the site,
/// which hosts nothing else a token could be for).
fn names_resource(config: &Config, requested: &str) -> bool {
    let requested = requested.trim_end_matches('/').to_lowercase();
    requested == resource(config).to_lowercase() || requested == issuer(config).to_lowercase()
}

async fn authorize(
    State(state): State<AppState>,
    Query(params): Query<AuthorizeParams>,
) -> Response {
    match check(&state, &params).await {
        Ok(request) => login_page(&state, &params, &request, None).await,
        Err(message) => refuse(&message),
    }
}

async fn decide(
    State(state): State<AppState>,
    connect: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Form(form): Form<DecisionForm>,
) -> Response {
    // Everything the page carried is checked again; it came back from the
    // browser and may have been changed.
    let request = match check(&state, &form.params).await {
        Ok(request) => request,
        Err(message) => return refuse(&message),
    };
    if form.decision.as_deref() != Some("approve") {
        return deny(&state, &request, &headers);
    }
    let peer = connect.map(|Extension(ConnectInfo(peer))| peer.ip());
    if !state.guesses.allowed(peer) {
        let mut response = login_page(
            &state,
            &form.params,
            &request,
            Some("Too many wrong tokens. Wait a minute, then try again."),
        )
        .await;
        *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        return response;
    }
    let presented = form.token.unwrap_or_default();
    if !bool::from(presented.trim().as_bytes().ct_eq(state.token.as_bytes())) {
        state.guesses.failed(peer);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let mut response = login_page(
            &state,
            &form.params,
            &request,
            Some("That is not the admin token."),
        )
        .await;
        *response.status_mut() = StatusCode::UNAUTHORIZED;
        return response;
    }

    let code = random_token();
    let stored = store::AuthCode {
        client_id: request.client.client_id.clone(),
        redirect_uri: request.redirect_uri.clone(),
        code_challenge: request.code_challenge,
        resource: resource(&state.config),
        expires_at: Utc::now() + CODE_TTL,
    };
    if let Err(err) = store::insert_code(&state.pool, &hash(&state, &code), &stored).await {
        return server_error(&err);
    }
    redirect_to_client(
        &state,
        &request.redirect_uri,
        &[("code", code.as_str())],
        request.state.as_deref(),
    )
}

fn refuse(message: &str) -> Response {
    let mut body = String::from("<h1>Cannot sign in</h1>\n");
    let _ = writeln!(body, "<p>{}</p>", escape(message));
    let mut response = page("Cannot sign in", &body);
    *response.status_mut() = StatusCode::BAD_REQUEST;
    response
}

/// Tell the client that the user said no. Only the form posted from our
/// own page is answered with a redirect. Anyone can post the deny form
/// from elsewhere without knowing the admin token, so other requests get
/// a link instead of an automatic bounce to the client's URI.
fn deny(state: &AppState, request: &Request, headers: &HeaderMap) -> Response {
    let Some(url) = client_url(
        state,
        &request.redirect_uri,
        &[
            ("error", "access_denied"),
            ("error_description", "the request was denied"),
        ],
        request.state.as_deref(),
    ) else {
        return refuse("The redirect URI is not valid.");
    };
    let same_origin = headers
        .get("sec-fetch-site")
        .is_some_and(|site| site == "same-origin");
    if same_origin {
        return Redirect::to(url.as_str()).into_response();
    }
    let mut body = String::from("<h1>Request denied</h1>\n");
    let _ = writeln!(
        body,
        "<p>Nothing was shared.</p>\n<p><a href=\"{}\">Return to {}</a></p>",
        escape(url.as_str()),
        escape(&destination(&request.redirect_uri)),
    );
    page("Request denied", &body)
}

/// Send the browser back to the client with `params`.
fn redirect_to_client(
    state: &AppState,
    redirect_uri: &str,
    params: &[(&str, &str)],
    client_state: Option<&str>,
) -> Response {
    match client_url(state, redirect_uri, params, client_state) {
        Some(url) => Redirect::to(url.as_str()).into_response(),
        None => refuse("The redirect URI is not valid."),
    }
}

/// The client's redirect URI with `params`, the client's `state` and our
/// issuer (RFC 9207), so the client can tell who answered.
fn client_url(
    state: &AppState,
    redirect_uri: &str,
    params: &[(&str, &str)],
    client_state: Option<&str>,
) -> Option<url::Url> {
    let mut url = url::Url::parse(redirect_uri).ok()?;
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in params {
            query.append_pair(key, value);
        }
        if let Some(client_state) = client_state {
            query.append_pair("state", client_state);
        }
        query.append_pair("iss", issuer(&state.config));
    }
    Some(url)
}

/// Where a redirect URI leads, as shown to the user.
fn destination(redirect_uri: &str) -> String {
    url::Url::parse(redirect_uri)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| redirect_uri.to_owned())
}

async fn login_page(
    state: &AppState,
    params: &AuthorizeParams,
    request: &Request,
    error: Option<&str>,
) -> Response {
    let site = settings::get(&state.pool)
        .await
        .map_or_else(|_| "WordMaker".into(), |s| s.site_title);
    let client_name = request
        .client
        .client_name
        .clone()
        .unwrap_or_else(|| "An MCP client".into());
    let destination = destination(&request.redirect_uri);
    let identity = if request.client.verified {
        format!(
            "Identified by <code>{}</code>.",
            escape(&request.client.client_id)
        )
    } else {
        "The name was given by the client itself and is not verified.".to_owned()
    };

    let mut body = String::new();
    let _ = write!(
        body,
        "<h1>Connect {client}</h1>\n<p><strong>{client}</strong> wants to manage the pages, media \
         and settings of <strong>{site}</strong>.</p>\n<p class=\"small\">{identity} After you \
         approve, you are sent back to <strong>{destination}</strong>; only continue if you \
         recognise it.</p>\n<form method=\"post\" action=\"/oauth/authorize\">\n",
        client = escape(&client_name),
        site = escape(&site),
        destination = escape(&destination),
    );
    for (name, value) in [
        ("response_type", &params.response_type),
        ("client_id", &params.client_id),
        ("redirect_uri", &params.redirect_uri),
        ("code_challenge", &params.code_challenge),
        ("code_challenge_method", &params.code_challenge_method),
        ("state", &params.state),
        ("scope", &params.scope),
        ("resource", &params.resource),
    ] {
        if let Some(value) = value {
            let _ = writeln!(
                body,
                "<input type=\"hidden\" name=\"{name}\" value=\"{}\">",
                escape(value)
            );
        }
    }
    body.push_str(
        "<label for=\"token\">Admin token (<code>WORDMAKER_TOKEN</code>)</label>\n<input \
         id=\"token\" type=\"password\" name=\"token\" autocomplete=\"current-password\" \
         autofocus>\n",
    );
    if let Some(error) = error {
        let _ = writeln!(
            body,
            "<p class=\"error\" role=\"alert\">{}</p>",
            escape(error)
        );
    }
    body.push_str(
        "<div class=\"buttons\"><button name=\"decision\" value=\"approve\">Approve</button> \
         <button name=\"decision\" value=\"deny\" \
         class=\"secondary\">Deny</button></div>\n</form>\n",
    );
    page(&format!("Connect {client_name}"), &body)
}

/// A standalone page of the sign-in flow. It must never be framed, cached
/// or leak its URL, which carries the request.
fn page(title: &str, body: &str) -> Response {
    // Built in short pieces: rustfmt's string wrapping once split a `\n`
    // here into a visible backslash and "n".
    let mut html = String::from("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n");
    html.push_str("<meta charset=\"utf-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    let _ = writeln!(html, "<title>{}</title>", escape(title));
    let _ = writeln!(html, "<style>{STYLE}</style>\n</head>\n<body>\n<main>");
    let _ = writeln!(html, "{body}</main>\n</body>\n</html>");
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (
                header::CONTENT_SECURITY_POLICY,
                // No form-action: browsers apply it to the redirect that
                // follows the form, which goes to the client.
                "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
            ),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        Html(html),
    )
        .into_response()
}

const STYLE: &str = "\
:root { color-scheme: light dark; font-family: system-ui, sans-serif; line-height: 1.5; }
body { margin: 0; padding: 1rem; }
main { max-width: 28rem; margin: 10vh auto; }
h1 { font-size: 1.5rem; }
.small { font-size: .9rem; opacity: .8; }
label { display: block; margin-top: 1.5rem; font-weight: 600; }
input[type=password] { width: 100%; box-sizing: border-box; padding: .6rem; font: inherit; \
                     margin-top: .4rem; }
.error { color: #c0392b; }
.buttons { margin-top: 1.5rem; display: flex; gap: .75rem; }
button { font: inherit; padding: .6rem 1.4rem; border: 0; border-radius: .3rem; background: #111; \
                     color: #fff; cursor: pointer; }
button.secondary { background: transparent; color: inherit; border: 1px solid currentColor; }
@media (prefers-color-scheme: dark) { button { background: #eee; color: #111; } }
";

// ---------------------------------------------------------------------------
// Token endpoint

#[derive(Debug, Deserialize)]
struct TokenRequest {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    resource: Option<String>,
}

async fn token(State(state): State<AppState>, Form(request): Form<TokenRequest>) -> Response {
    if let Err(err) = store::purge_expired(&state.pool).await {
        tracing::warn!(error = %err, "could not purge expired OAuth codes and tokens");
    }
    let result = match request.grant_type.as_deref() {
        Some("authorization_code") => redeem_code(&state, &request).await,
        Some("refresh_token") => refresh(&state, &request).await,
        _ => Err(TokenError::new(
            "unsupported_grant_type",
            "use authorization_code or refresh_token",
        )),
    };
    result.unwrap_or_else(IntoResponse::into_response)
}

struct TokenError {
    error: &'static str,
    description: String,
    server: Option<AppError>,
}

impl TokenError {
    fn new(error: &'static str, description: &str) -> Self {
        Self {
            error,
            description: description.to_owned(),
            server: None,
        }
    }

    fn invalid_grant() -> Self {
        Self::new(
            "invalid_grant",
            "the code or refresh token is invalid, expired or was issued to another client",
        )
    }
}

impl From<AppError> for TokenError {
    fn from(err: AppError) -> Self {
        Self {
            error: "server_error",
            description: String::new(),
            server: Some(err),
        }
    }
}

impl IntoResponse for TokenError {
    fn into_response(self) -> Response {
        match self.server {
            Some(err) => server_error(&err),
            None => oauth_error(StatusCode::BAD_REQUEST, self.error, &self.description),
        }
    }
}

fn required<'a>(value: Option<&'a String>, name: &str) -> Result<&'a str, TokenError> {
    value
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| TokenError::new("invalid_request", &format!("{name} is required")))
}

async fn redeem_code(state: &AppState, request: &TokenRequest) -> Result<Response, TokenError> {
    let code = required(request.code.as_ref(), "code")?;
    let client_id = required(request.client_id.as_ref(), "client_id")?;
    let verifier = required(request.code_verifier.as_ref(), "code_verifier")?;

    let grant_id = uuid::Uuid::new_v4().to_string();
    let mut tx = state.pool.begin().await.map_err(AppError::from)?;
    let stored = match store::redeem_code(&mut tx, &hash(state, code), &grant_id).await? {
        store::Redeemed::Fresh(stored) => stored,
        store::Redeemed::Unknown => return Err(TokenError::invalid_grant()),
        store::Redeemed::Replayed { grant_id } => {
            // The code was intercepted, or the client is confused; either
            // way the tokens it was exchanged for are not to be trusted
            // (RFC 6749, section 4.1.2).
            store::revoke_grant(&mut tx, &grant_id).await?;
            tx.commit().await.map_err(AppError::from)?;
            tracing::warn!(
                grant_id,
                "authorization code replayed; OAuth sign-in revoked"
            );
            return Err(TokenError::invalid_grant());
        }
    };
    // The code is spent even if the request turns out to be wrong.
    if let Err(err) = check_code(state, request, &stored, client_id, verifier) {
        tx.commit().await.map_err(AppError::from)?;
        return Err(err);
    }

    store::mark_client_used(&mut tx, &stored.client_id).await?;
    let grant = store::TokenGrant {
        grant_id,
        client_id: stored.client_id,
        resource: stored.resource,
    };
    let response = issue(state, &mut tx, &grant).await?;
    tx.commit().await.map_err(AppError::from)?;
    Ok(response)
}

fn check_code(
    state: &AppState,
    request: &TokenRequest,
    stored: &store::AuthCode,
    client_id: &str,
    verifier: &str,
) -> Result<(), TokenError> {
    let redirect_ok = request
        .redirect_uri
        .as_deref()
        .is_none_or(|uri| uri == stored.redirect_uri);
    if stored.expires_at < Utc::now() || stored.client_id != client_id || !redirect_ok {
        return Err(TokenError::invalid_grant());
    }
    if !(43..=128).contains(&verifier.len())
        || !bool::from(
            pkce_challenge(verifier)
                .as_bytes()
                .ct_eq(stored.code_challenge.as_bytes()),
        )
    {
        return Err(TokenError::new(
            "invalid_grant",
            "code_verifier does not match the code_challenge",
        ));
    }
    check_resource(state, request, &stored.resource)
}

async fn refresh(state: &AppState, request: &TokenRequest) -> Result<Response, TokenError> {
    let refresh_token = required(request.refresh_token.as_ref(), "refresh_token")?;
    let client_id = required(request.client_id.as_ref(), "client_id")?;
    // Using up the token and issuing its successors happen in one
    // transaction; anything that fails before the commit leaves the token
    // as it was.
    let mut tx = state.pool.begin().await.map_err(AppError::from)?;
    let grant = match store::use_refresh_token(&mut tx, &hash(state, refresh_token)).await? {
        store::Refresh::Fresh(grant) if grant.client_id == client_id => grant,
        store::Refresh::Fresh(_) | store::Refresh::Unknown => {
            return Err(TokenError::invalid_grant());
        }
        store::Refresh::Reused { grant_id } => {
            // Either the client or someone who stole the token already
            // used it; there is no telling which, so the sign-in ends
            // (RFC 9700, section 4.14.2).
            store::revoke_grant(&mut tx, &grant_id).await?;
            tx.commit().await.map_err(AppError::from)?;
            tracing::warn!(grant_id, "refresh token reused; OAuth sign-in revoked");
            return Err(TokenError::invalid_grant());
        }
    };
    check_resource(state, request, &grant.resource)?;
    // Refresh tokens are rotated: issuing new ones removes the old pair.
    let response = issue(state, &mut tx, &grant).await?;
    tx.commit().await.map_err(AppError::from)?;
    Ok(response)
}

fn check_resource(
    state: &AppState,
    request: &TokenRequest,
    granted: &str,
) -> Result<(), TokenError> {
    match &request.resource {
        Some(requested)
            if !names_resource(&state.config, requested) || granted != resource(&state.config) =>
        {
            Err(TokenError::new(
                "invalid_target",
                "the token was not issued for this resource",
            ))
        }
        _ => Ok(()),
    }
}

async fn issue(
    state: &AppState,
    conn: &mut sqlx::SqliteConnection,
    grant: &store::TokenGrant,
) -> Result<Response, TokenError> {
    let access_token = random_token();
    let refresh_token = random_token();
    let now = Utc::now();
    store::replace_tokens(
        conn,
        grant,
        &[
            (
                store::Kind::Access,
                &hash(state, &access_token),
                now + ACCESS_TOKEN_TTL,
            ),
            (
                store::Kind::Refresh,
                &hash(state, &refresh_token),
                now + REFRESH_TOKEN_TTL,
            ),
        ],
    )
    .await?;
    Ok((
        no_store(),
        Json(json!({
            "access_token": access_token,
            "token_type": "Bearer",
            "expires_in": ACCESS_TOKEN_TTL.num_seconds(),
            "refresh_token": refresh_token,
        })),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Revocation (RFC 7009)

#[derive(Debug, Deserialize)]
struct RevokeRequest {
    token: Option<String>,
}

async fn revoke(State(state): State<AppState>, Form(request): Form<RevokeRequest>) -> Response {
    let Some(token) = request.token.filter(|t| !t.is_empty()) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "token is required",
        );
    };
    // Revoking either token of a sign-in ends the whole sign-in. Unknown
    // tokens are not an error, so nobody learns which tokens exist.
    match store::revoke(&state.pool, &hash(&state, &token)).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => server_error(&err),
    }
}

// ---------------------------------------------------------------------------
// Helpers

fn random_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Codes and tokens are stored hashed together with the admin token.
fn hash(state: &AppState, value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(state.token.as_bytes());
    hasher.update([0]);
    hasher.update(value.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize())
}

fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn no_store() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        no_store(),
        Json(json!({ "error": error, "error_description": description })),
    )
        .into_response()
}

fn server_error(err: &AppError) -> Response {
    tracing::error!(error = %err, "OAuth request failed");
    oauth_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "internal error",
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::Arc,
    };

    use axum::body::Body;
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;

    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const REDIRECT: &str = "https://claude.ai/api/mcp/auth_callback";

    async fn app() -> Router {
        app_with(Arc::default()).await
    }

    async fn app_with(guesses: Arc<crate::token::Guesses>) -> Router {
        let config = Config {
            database_url: String::new(),
            media_dir: std::env::temp_dir().join("wordmaker-oauth-test"),
            themes_dir: std::env::temp_dir().join("wordmaker-oauth-test-themes"),
            base_url: "https://site.example/".into(),
            max_upload_bytes: 1024,
        };
        let state = AppState {
            pool: crate::db::connect_memory().await,
            config: Arc::new(config),
            token: "admin-secret".into(),
            themes: Arc::default(),
            images: Arc::default(),
            oauth_clients: Arc::default(),
            guesses,
            analytics: None,
        };
        crate::app::router(state, vec!["site.example".into()])
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: String,
    }

    impl Reply {
        fn json(&self) -> Value {
            serde_json::from_str(&self.body).unwrap()
        }

        fn location(&self) -> url::Url {
            url::Url::parse(self.headers[header::LOCATION].to_str().unwrap()).unwrap()
        }
    }

    async fn call(app: &Router, request: axum::http::Request<Body>) -> Reply {
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn get(app: &Router, uri: &str) -> Reply {
        call(
            app,
            axum::http::Request::get(uri).body(Body::empty()).unwrap(),
        )
        .await
    }

    async fn post_form(app: &Router, uri: &str, form: &[(&str, &str)]) -> Reply {
        post_form_from(app, uri, form, None).await
    }

    /// Post a form as a browser would, from a page of `fetch_site`
    /// (`Sec-Fetch-Site`).
    async fn post_form_from(
        app: &Router,
        uri: &str,
        form: &[(&str, &str)],
        fetch_site: Option<&str>,
    ) -> Reply {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        let mut request = axum::http::Request::post(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(site) = fetch_site {
            request = request.header("sec-fetch-site", site);
        }
        call(app, request.body(Body::from(body)).unwrap()).await
    }

    async fn register(app: &Router, redirect_uri: &str) -> Reply {
        call(
            app,
            axum::http::Request::post("/oauth/register")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "redirect_uris": [redirect_uri] }).to_string(),
                ))
                .unwrap(),
        )
        .await
    }

    async fn register_client(app: &Router) -> String {
        let reply = call(
            app,
            axum::http::Request::post("/oauth/register")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "client_name": "Claude", "redirect_uris": [REDIRECT] }).to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        reply.json()["client_id"].as_str().unwrap().to_owned()
    }

    fn authorize_params(client_id: &str) -> Vec<(&'static str, String)> {
        vec![
            ("response_type", "code".into()),
            ("client_id", client_id.into()),
            ("redirect_uri", REDIRECT.into()),
            ("code_challenge", pkce_challenge(VERIFIER)),
            ("code_challenge_method", "S256".into()),
            ("state", "xyz".into()),
            ("resource", "https://site.example/mcp".into()),
        ]
    }

    /// Sign in through the form and return the authorization code.
    async fn sign_in(app: &Router, client_id: &str) -> String {
        let mut form = authorize_params(client_id);
        form.push(("token", "admin-secret".into()));
        form.push(("decision", "approve".into()));
        let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let reply = post_form(app, "/oauth/authorize", &form).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        let location = reply.location();
        assert!(location.as_str().starts_with(REDIRECT), "{location}");
        let query: HashMap<_, _> = location.query_pairs().into_owned().collect();
        assert_eq!(query["state"], "xyz");
        assert_eq!(query["iss"], "https://site.example");
        query["code"].clone()
    }

    async fn mcp_status(app: &Router, token: &str) -> Reply {
        call(
            app,
            axum::http::Request::post("/mcp")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::HOST, "site.example")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .body(Body::from(
                    json!({
                        "jsonrpc": "2.0", "id": 1, "method": "initialize",
                        "params": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "clientInfo": { "name": "test", "version": "1" }
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
    }

    #[tokio::test]
    async fn metadata_and_challenge() {
        let app = app().await;
        let reply = get(&app, "/.well-known/oauth-protected-resource/mcp").await;
        assert_eq!(reply.json()["resource"], "https://site.example/mcp");
        assert_eq!(
            reply.json()["authorization_servers"][0],
            "https://site.example"
        );
        let reply = get(&app, "/.well-known/oauth-authorization-server").await;
        let metadata = reply.json();
        assert_eq!(metadata["issuer"], "https://site.example");
        assert_eq!(
            metadata["token_endpoint"],
            "https://site.example/oauth/token"
        );
        assert_eq!(metadata["client_id_metadata_document_supported"], true);

        let reply = mcp_status(&app, "nope").await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        let challenge = reply.headers[header::WWW_AUTHENTICATE].to_str().unwrap();
        assert!(
            challenge.starts_with(
                "Bearer resource_metadata=\"https://site.example/.well-known/oauth-protected-resource/mcp\""
            ),
            "{challenge}"
        );
    }

    #[tokio::test]
    async fn full_flow() {
        let app = app().await;
        let client_id = register_client(&app).await;

        // The login page shows who is asking.
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(authorize_params(&client_id))
            .finish();
        let reply = get(&app, &format!("/oauth/authorize?{query}")).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert!(reply.body.contains("Connect Claude"), "{}", reply.body);
        assert!(reply.body.contains("claude.ai"), "{}", reply.body);
        assert_eq!(reply.headers[header::X_FRAME_OPTIONS], "DENY");
        assert!(reply.body.contains("<main>\n<h1>"), "{}", reply.body);

        let code = sign_in(&app, &client_id).await;
        let reply = post_form(
            &app,
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("client_id", &client_id),
                ("redirect_uri", REDIRECT),
                ("code_verifier", VERIFIER),
                ("resource", "https://site.example/mcp"),
            ],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
        let tokens = reply.json();
        let access = tokens["access_token"].as_str().unwrap().to_owned();
        let refresh = tokens["refresh_token"].as_str().unwrap().to_owned();
        assert_eq!(tokens["token_type"], "Bearer");

        assert_eq!(mcp_status(&app, &access).await.status, StatusCode::OK);
        // OAuth tokens are for /mcp only.
        let reply = call(
            &app,
            axum::http::Request::get("/api/v1/pages")
                .header(header::AUTHORIZATION, format!("Bearer {access}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

        // Refreshing rotates both tokens.
        let reply = post_form(
            &app,
            "/oauth/token",
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
                ("client_id", &client_id),
            ],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        let new_access = reply.json()["access_token"].as_str().unwrap().to_owned();
        let new_refresh = reply.json()["refresh_token"].as_str().unwrap().to_owned();
        assert_eq!(
            mcp_status(&app, &access).await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(mcp_status(&app, &new_access).await.status, StatusCode::OK);

        // Revoking the refresh token ends the sign-in.
        let reply = post_form(&app, "/oauth/revoke", &[("token", &new_refresh)]).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            mcp_status(&app, &new_access).await.status,
            StatusCode::UNAUTHORIZED
        );
    }

    /// Exchange a fresh code for tokens; returns (access, refresh).
    async fn tokens(app: &Router, client_id: &str) -> (String, String) {
        let code = sign_in(app, client_id).await;
        let reply = redeem(app, client_id, &code).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        let tokens = reply.json();
        (
            tokens["access_token"].as_str().unwrap().to_owned(),
            tokens["refresh_token"].as_str().unwrap().to_owned(),
        )
    }

    async fn redeem(app: &Router, client_id: &str, code: &str) -> Reply {
        post_form(
            app,
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("client_id", client_id),
                ("redirect_uri", REDIRECT),
                ("code_verifier", VERIFIER),
            ],
        )
        .await
    }

    async fn refresh_with(app: &Router, client_id: &str, refresh: &str) -> Reply {
        post_form(
            app,
            "/oauth/token",
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
                ("client_id", client_id),
            ],
        )
        .await
    }

    #[tokio::test]
    async fn refresh_token_reuse_revokes_the_sign_in() {
        let app = app().await;
        let client_id = register_client(&app).await;
        let (_, refresh) = tokens(&app, &client_id).await;

        let reply = refresh_with(&app, &client_id, &refresh).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        let access = reply.json()["access_token"].as_str().unwrap().to_owned();
        let next_refresh = reply.json()["refresh_token"].as_str().unwrap().to_owned();
        assert_eq!(mcp_status(&app, &access).await.status, StatusCode::OK);

        // The old token again: whoever holds the new pair loses it too.
        let reply = refresh_with(&app, &client_id, &refresh).await;
        assert_eq!(reply.json()["error"], "invalid_grant");
        assert_eq!(
            mcp_status(&app, &access).await.status,
            StatusCode::UNAUTHORIZED
        );
        let reply = refresh_with(&app, &client_id, &next_refresh).await;
        assert_eq!(reply.json()["error"], "invalid_grant");

        // A refresh token presented by another client is not used up.
        let (_, refresh) = tokens(&app, &client_id).await;
        let reply = refresh_with(&app, "someone-else", &refresh).await;
        assert_eq!(reply.json()["error"], "invalid_grant");
        let reply = refresh_with(&app, &client_id, &refresh).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    }

    #[tokio::test]
    async fn code_replay_revokes_the_sign_in() {
        let app = app().await;
        let client_id = register_client(&app).await;
        let code = sign_in(&app, &client_id).await;
        let reply = redeem(&app, &client_id, &code).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        let access = reply.json()["access_token"].as_str().unwrap().to_owned();
        let refresh = reply.json()["refresh_token"].as_str().unwrap().to_owned();
        assert_eq!(mcp_status(&app, &access).await.status, StatusCode::OK);

        let reply = redeem(&app, &client_id, &code).await;
        assert_eq!(reply.json()["error"], "invalid_grant");
        assert_eq!(
            mcp_status(&app, &access).await.status,
            StatusCode::UNAUTHORIZED
        );
        let reply = refresh_with(&app, &client_id, &refresh).await;
        assert_eq!(reply.json()["error"], "invalid_grant");
    }

    #[tokio::test]
    async fn sign_in_is_throttled_after_wrong_tokens() {
        let guesses = Arc::new(crate::token::Guesses::default());
        let app = app_with(guesses.clone()).await;
        let client_id = register_client(&app).await;
        let (access, _) = tokens(&app, &client_id).await;
        // Wrong tokens elsewhere, e.g. on /api, count here too.
        for _ in 0..20 {
            guesses.failed(None);
        }
        // Signed-in clients are not locked out, but the admin token is no
        // longer checked.
        assert_eq!(mcp_status(&app, &access).await.status, StatusCode::OK);
        assert_eq!(
            mcp_status(&app, "admin-secret").await.status,
            StatusCode::TOO_MANY_REQUESTS
        );
        let mut form = authorize_params(&client_id);
        form.push(("token", "admin-secret".into()));
        form.push(("decision", "approve".into()));
        let pairs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let reply = post_form(&app, "/oauth/authorize", &pairs).await;
        assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(!reply.headers.contains_key(header::LOCATION));
        assert!(
            reply.body.contains("Too many wrong tokens"),
            "{}",
            reply.body
        );
    }

    #[tokio::test]
    async fn wrong_admin_token_and_denial() {
        let app = app().await;
        let client_id = register_client(&app).await;
        let mut form = authorize_params(&client_id);
        form.push(("token", "guess".into()));
        form.push(("decision", "approve".into()));
        let pairs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let reply = post_form(&app, "/oauth/authorize", &pairs).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert!(reply.body.contains("not the admin token"), "{}", reply.body);

        let mut form = authorize_params(&client_id);
        form.push(("decision", "deny".into()));
        let pairs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let reply = post_form_from(&app, "/oauth/authorize", &pairs, Some("same-origin")).await;
        let query: HashMap<_, _> = reply.location().query_pairs().into_owned().collect();
        assert_eq!(query["error"], "access_denied");
        assert_eq!(query["state"], "xyz");

        // A denial posted from anywhere else is not bounced on to the
        // client, since anyone could send it.
        for site in [None, Some("cross-site")] {
            let reply = post_form_from(&app, "/oauth/authorize", &pairs, site).await;
            assert_eq!(reply.status, StatusCode::OK);
            assert!(!reply.headers.contains_key(header::LOCATION));
            assert!(reply.body.contains("error=access_denied"), "{}", reply.body);
        }
    }

    #[tokio::test]
    async fn refuses_bad_requests() {
        let app = app().await;
        let client_id = register_client(&app).await;

        // Unknown redirect URIs are never redirected to.
        let mut params = authorize_params(&client_id);
        params[2].1 = "https://evil.example/cb".into();
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params)
            .finish();
        let reply = get(&app, &format!("/oauth/authorize?{query}")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(!reply.headers.contains_key(header::LOCATION));

        // PKCE is required, and errors before the user approved are shown
        // instead of being sent to the client.
        let params: Vec<_> = authorize_params(&client_id)
            .into_iter()
            .filter(|(k, _)| !k.starts_with("code_challenge"))
            .collect();
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params)
            .finish();
        let reply = get(&app, &format!("/oauth/authorize?{query}")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(!reply.headers.contains_key(header::LOCATION));
        assert!(reply.body.contains("PKCE"), "{}", reply.body);
        let reply = get(
            &app,
            &format!("/oauth/authorize?client_id={client_id}&redirect_uri={REDIRECT}"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(!reply.headers.contains_key(header::LOCATION));

        // A wrong verifier gets no tokens.
        let code = sign_in(&app, &client_id).await;
        let reply = post_form(
            &app,
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("client_id", &client_id),
                (
                    "code_verifier",
                    "wrong-verifier-wrong-verifier-wrong-verifier-123",
                ),
            ],
        )
        .await;
        assert_eq!(reply.json()["error"], "invalid_grant");

        // Registration refuses redirect URIs that are not safe.
        for uri in [
            "http://evil.example/cb",
            "ms-msdt:/id",
            "search-ms:query=x",
            "intent://x#Intent;end",
        ] {
            let reply = register(&app, uri).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(reply.json()["error"], "invalid_redirect_uri");
        }
        let reply = register(&app, "com.example.app:/oauth/callback").await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    }
}
