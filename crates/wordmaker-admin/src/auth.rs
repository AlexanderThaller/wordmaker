//! Sign-in through an OIDC (`OpenID` Connect) provider (Keycloak, Authentik,
//! Google, GitLab, …), so the interface can be reached from more than loopback.
//!
//! The authorization code flow with PKCE: `/auth/login` sends the browser to
//! the provider, `/auth/callback` takes the code back, has the provider's ID
//! token checked by `openidconnect` and, if its verified email address is on
//! the allowlist, starts a session. Sessions live in memory, so a restart
//! signs everyone out. A sign-in under way keeps nothing here: its PKCE
//! verifier and nonce wait in a signed cookie of the browser that started
//! it, so anonymous visitors cannot fill a table and lock others out.

use std::{
    collections::HashMap,
    fmt,
    path::PathBuf,
    sync::{
        Arc,
        Mutex,
        PoisonError,
    },
    time::{
        Duration,
        Instant,
        SystemTime,
        UNIX_EPOCH,
    },
};

use anyhow::Context;
use axum::{
    Router,
    extract::{
        Query,
        Request,
        State,
    },
    http::{
        HeaderMap,
        StatusCode,
        header,
    },
    middleware::Next,
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
use axum_extra::extract::cookie::{
    Cookie,
    CookieJar,
    SameSite,
};
use hmac::{
    Hmac,
    Mac,
};
use openidconnect::{
    AccessToken,
    AuthorizationCode,
    ClientId,
    ClientSecret,
    CsrfToken,
    EndpointMaybeSet,
    EndpointNotSet,
    EndpointSet,
    HttpRequest,
    HttpResponse,
    IssuerUrl,
    Nonce,
    OAuth2TokenResponse,
    PkceCodeChallenge,
    PkceCodeVerifier,
    RedirectUrl,
    Scope,
    SubjectIdentifier,
    TokenResponse,
    core::{
        CoreAuthenticationFlow,
        CoreClient,
        CoreProviderMetadata,
        CoreUserInfoClaims,
    },
};
use serde::Deserialize;
use sha2::Sha256;

use crate::{
    app::{
        API_GUARD_HEADER,
        Admin,
    },
    proxy,
    token::read_secret_file,
};

const SESSION_COOKIE: &str = "wordmaker_admin_session";
/// Ties a callback to the browser that started the sign-in, so nobody can
/// sign someone else in to their own account. One per sign-in, named
/// `<LOGIN_COOKIE>_<state>`, so sign-ins started in two tabs both work.
const LOGIN_COOKIE: &str = "wordmaker_admin_login";
const SESSION_TTL: Duration = Duration::from_hours(12);
const LOGIN_TTL: Duration = Duration::from_mins(10);
/// How long the provider's metadata and keys are trusted before they are
/// fetched again, which picks up rotated keys without a restart.
const DISCOVERY_TTL: Duration = Duration::from_mins(5);

/// How to sign in through an OIDC provider. Without an issuer,
/// nobody is asked to sign in.
#[derive(clap::Args, Debug, Default)]
pub struct OidcArgs {
    /// Issuer URL of the OIDC provider, e.g.
    /// `https://accounts.google.com` or
    /// `https://keycloak.example/realms/main`. Turns on sign-in.
    #[arg(long, env = "WORDMAKER_ADMIN_OIDC_ISSUER")]
    oidc_issuer: Option<String>,

    /// Client ID registered with the provider.
    #[arg(long, env = "WORDMAKER_ADMIN_OIDC_CLIENT_ID")]
    oidc_client_id: Option<String>,

    /// Client secret, for providers that registered a confidential client.
    #[arg(
        long,
        env = "WORDMAKER_ADMIN_OIDC_CLIENT_SECRET",
        hide_env_values = true,
        conflicts_with = "oidc_client_secret_file"
    )]
    oidc_client_secret: Option<String>,

    /// File holding the client secret. Surrounding whitespace is ignored.
    #[arg(
        long,
        env = "WORDMAKER_ADMIN_OIDC_CLIENT_SECRET_FILE",
        value_name = "PATH"
    )]
    oidc_client_secret_file: Option<PathBuf>,

    /// URL the browser reaches this interface at, e.g.
    /// `https://admin.example`. The provider sends people back to
    /// `<URL>/auth/callback`, which must be registered with it.
    #[arg(long, env = "WORDMAKER_ADMIN_PUBLIC_URL")]
    public_url: Option<String>,

    /// Who may sign in: email addresses, or `@example.com` for everyone with
    /// a verified address there.
    #[arg(
        long = "allowed-user",
        env = "WORDMAKER_ADMIN_ALLOWED_USERS",
        value_delimiter = ','
    )]
    allowed_users: Vec<String>,
}

impl OidcArgs {
    /// The sign-in settings, or `None` when no issuer is set.
    pub fn resolve(self) -> anyhow::Result<Option<OidcConfig>> {
        let Some(issuer) = self.oidc_issuer.filter(|issuer| !issuer.trim().is_empty()) else {
            anyhow::ensure!(
                self.oidc_client_id.is_none() && self.allowed_users.is_empty(),
                "OIDC settings given without WORDMAKER_ADMIN_OIDC_ISSUER (--oidc-issuer)"
            );
            return Ok(None);
        };
        let issuer = IssuerUrl::new(issuer.trim().to_owned()).context("invalid OIDC issuer URL")?;
        let client_id = self
            .oidc_client_id
            .filter(|id| !id.trim().is_empty())
            .context("set WORDMAKER_ADMIN_OIDC_CLIENT_ID (--oidc-client-id) for sign-in")?;
        let client_secret = match (self.oidc_client_secret, self.oidc_client_secret_file) {
            (Some(secret), _) => Some(secret.trim().to_owned()),
            (None, Some(path)) => Some(read_secret_file(&path, "OIDC client secret")?),
            (None, None) => None,
        }
        .filter(|secret| !secret.is_empty());

        let public_url = self
            .public_url
            .context("set WORDMAKER_ADMIN_PUBLIC_URL (--public-url) for sign-in")?;
        let public_url = openidconnect::url::Url::parse(public_url.trim_end_matches('/'))
            .context("invalid public URL")?;
        anyhow::ensure!(
            matches!(public_url.scheme(), "http" | "https") && public_url.host_str().is_some(),
            "the public URL must be an http:// or https:// URL, got {public_url}"
        );
        let redirect_url = RedirectUrl::new(format!(
            "{}/auth/callback",
            public_url.as_str().trim_end_matches('/')
        ))?;

        let allowed_users: Vec<String> = self
            .allowed_users
            .iter()
            .map(|user| user.trim().to_lowercase())
            .filter(|user| !user.is_empty())
            .collect();
        anyhow::ensure!(
            !allowed_users.is_empty(),
            "set WORDMAKER_ADMIN_ALLOWED_USERS (--allowed-user) to say who may sign in"
        );

        Ok(Some(OidcConfig {
            issuer,
            client_id: ClientId::new(client_id.trim().to_owned()),
            client_secret: client_secret.map(ClientSecret::new),
            redirect_url,
            public_url,
            allowed_users,
        }))
    }
}

#[derive(Debug)]
pub struct OidcConfig {
    issuer: IssuerUrl,
    client_id: ClientId,
    client_secret: Option<ClientSecret>,
    redirect_url: RedirectUrl,
    public_url: openidconnect::url::Url,
    allowed_users: Vec<String>,
}

impl OidcConfig {
    /// Where the browser reaches this interface.
    pub fn public_url(&self) -> &openidconnect::url::Url {
        &self.public_url
    }
}

/// The provider's client as discovery sets it up: an authorization
/// endpoint for sure, token and userinfo endpoints if it names them.
type Provider = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// The signed-in user, as the allowlist knows them.
#[derive(Debug, Clone)]
pub struct User(pub String);

/// Sign-in and the sessions it starts.
#[derive(Clone)]
pub struct Auth(Arc<Inner>);

struct Inner {
    config: OidcConfig,
    http: reqwest::Client,
    /// The provider as last discovered, and when.
    provider: Mutex<Option<(Instant, Arc<Provider>)>>,
    /// Signs the login cookies; made at start, so a restart also drops the
    /// sign-ins under way.
    login_key: String,
    sessions: Mutex<HashMap<String, Session>>,
}

/// A sign-in under way, as its cookie carries it.
#[derive(Debug, PartialEq)]
struct PendingLogin {
    pkce_verifier: String,
    nonce: String,
    /// Seconds since the Unix epoch.
    started: u64,
}

struct Session {
    user: User,
    expires: Instant,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("issuer", &self.0.config.issuer)
            .field("client_id", &self.0.config.client_id)
            .finish_non_exhaustive()
    }
}

impl Auth {
    /// Checks that the provider answers, so a typo shows at startup rather
    /// than at the first sign-in.
    pub async fn new(config: OidcConfig) -> anyhow::Result<Self> {
        let auth = Self(Arc::new(Inner {
            config,
            http: reqwest::Client::builder()
                // Following redirects would let the provider's metadata point
                // requests anywhere.
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(15))
                .build()?,
            provider: Mutex::default(),
            login_key: CsrfToken::new_random_len(32).secret().clone(),
            sessions: Mutex::default(),
        }));
        auth.provider().await.with_context(|| {
            format!(
                "cannot fetch the OpenID Connect configuration of {}",
                auth.0.config.issuer.as_str()
            )
        })?;
        Ok(auth)
    }

    /// The provider's metadata and keys, fetched again once they are older
    /// than [`DISCOVERY_TTL`].
    async fn provider(&self) -> anyhow::Result<Arc<Provider>> {
        if let Some((fetched, provider)) = &*lock(&self.0.provider)
            && fetched.elapsed() < DISCOVERY_TTL
        {
            return Ok(provider.clone());
        }
        let config = &self.0.config;
        let metadata =
            CoreProviderMetadata::discover_async(config.issuer.clone(), &self.http_client())
                .await?;
        let provider = Arc::new(
            CoreClient::from_provider_metadata(
                metadata,
                config.client_id.clone(),
                config.client_secret.clone(),
            )
            .set_redirect_uri(config.redirect_url.clone()),
        );
        *lock(&self.0.provider) = Some((Instant::now(), provider.clone()));
        Ok(provider)
    }

    fn http_client(&self) -> impl Fn(HttpRequest) -> HttpFuture + Send + Sync + use<> {
        let client = self.0.http.clone();
        move |request| Box::pin(send(client.clone(), request))
    }

    fn secure_cookies(&self) -> bool {
        self.0.config.public_url.scheme() == "https"
    }

    fn allowed(&self, email: &str) -> bool {
        let email = email.to_lowercase();
        let domain = email.rsplit_once('@').map(|(_, domain)| domain);
        self.0
            .config
            .allowed_users
            .iter()
            .any(|allowed| match allowed.strip_prefix('@') {
                Some(allowed_domain) => domain == Some(allowed_domain),
                None => *allowed == email,
            })
    }

    /// The user a session cookie belongs to, if it is still good.
    fn user(&self, jar: &CookieJar) -> Option<User> {
        let id = jar.get(SESSION_COOKIE)?.value();
        let mut sessions = lock(&self.0.sessions);
        match sessions.get(id) {
            Some(session) if session.expires > Instant::now() => Some(session.user.clone()),
            Some(_) => {
                sessions.remove(id);
                None
            }
            None => None,
        }
    }

    fn start_session(&self, user: User) -> String {
        let id = CsrfToken::new_random_len(32).secret().clone();
        let now = Instant::now();
        let mut sessions = lock(&self.0.sessions);
        sessions.retain(|_, session| session.expires > now);
        sessions.insert(
            id.clone(),
            Session {
                user,
                expires: now + SESSION_TTL,
            },
        );
        id
    }

    fn cookie(&self, name: impl Into<String>, value: String, max_age: Duration) -> Cookie<'static> {
        Cookie::build((name.into(), value))
            .path("/")
            .http_only(true)
            .secure(self.secure_cookies())
            .same_site(SameSite::Lax)
            .max_age(max_age.try_into().unwrap_or(cookie::time::Duration::ZERO))
            .build()
    }

    fn removal(name: impl Into<String>) -> Cookie<'static> {
        Cookie::build((name.into(), "")).path("/").build()
    }

    fn login_mac(&self, state: &str, login: &PendingLogin) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.0.login_key.as_bytes())
            .expect("HMAC takes keys of any length");
        mac.update(
            format!(
                "{state}.{}.{}.{}",
                login.pkce_verifier, login.nonce, login.started
            )
            .as_bytes(),
        );
        mac
    }

    /// The login cookie's value: the sign-in and a MAC over it and `state`.
    fn seal_login(&self, state: &str, login: &PendingLogin) -> String {
        let tag = self.login_mac(state, login).finalize().into_bytes();
        format!(
            "{}.{}.{}.{}",
            login.pkce_verifier,
            login.nonce,
            login.started,
            hex(&tag)
        )
    }

    /// The sign-in a login cookie carries, if this server made it for
    /// `state` and it has not expired.
    fn open_login(&self, state: &str, value: &str) -> Option<PendingLogin> {
        let mut parts = value.split('.');
        let (Some(pkce_verifier), Some(nonce), Some(started), Some(tag), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return None;
        };
        let login = PendingLogin {
            pkce_verifier: pkce_verifier.to_owned(),
            nonce: nonce.to_owned(),
            started: started.parse().ok()?,
        };
        self.login_mac(state, &login)
            .verify_slice(&unhex(tag)?)
            .ok()?;
        let age = unix_now().checked_sub(login.started)?;
        (age < LOGIN_TTL.as_secs()).then_some(login)
    }
}

/// The login cookie's name for the sign-in `state`; `None` for a state
/// this interface would not have made.
fn login_cookie(state: &str) -> Option<String> {
    let made_here = !state.is_empty()
        && state.len() <= 64
        && state
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    made_here.then(|| format!("{LOGIN_COOKIE}_{state}"))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

type HttpFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<HttpResponse, reqwest::Error>> + Send + 'static>>;

/// Carries `openidconnect`'s requests with this crate's reqwest.
async fn send(
    client: reqwest::Client,
    request: HttpRequest,
) -> Result<HttpResponse, reqwest::Error> {
    let response = client.execute(request.try_into()?).await?;
    let status = response.status();
    let headers = response.headers().clone();
    let mut converted = HttpResponse::new(response.bytes().await?.to_vec());
    *converted.status_mut() = status;
    *converted.headers_mut() = headers;
    Ok(converted)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // The maps stay consistent even if a holder panicked.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn router() -> Router<Admin> {
    Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", post(logout))
        .route("/auth/signed-out", get(signed_out))
}

/// Lets requests with a session through, with the [`User`] attached. The
/// page itself sends people to sign in; anything else gets a 401.
pub async fn require_session(
    State(admin): State<Admin>,
    jar: CookieJar,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(auth) = &admin.auth else {
        return next.run(request).await;
    };
    if let Some(user) = auth.user(&jar) {
        request.extensions_mut().insert(user);
        return next.run(request).await;
    }
    if request.uri().path() == "/" {
        return Redirect::to("/auth/login").into_response();
    }
    proxy::error(
        StatusCode::UNAUTHORIZED,
        "signed out; reload the page to sign in again",
    )
}

async fn login(State(admin): State<Admin>, jar: CookieJar) -> Response {
    let Some(auth) = &admin.auth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if auth.user(&jar).is_some() {
        return Redirect::to("/").into_response();
    }
    let provider = match auth.provider().await {
        Ok(provider) => provider,
        Err(err) => {
            tracing::warn!(error = %err, "OpenID Connect discovery failed");
            return page(
                StatusCode::BAD_GATEWAY,
                "Sign-in unavailable",
                "The sign-in provider cannot be reached right now.",
            );
        }
    };

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = provider
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".to_owned()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    let Some(name) = login_cookie(state.secret()) else {
        return page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Sign-in unavailable",
            "The sign-in could not be started.",
        );
    };
    let login = PendingLogin {
        pkce_verifier: pkce_verifier.secret().clone(),
        nonce: nonce.secret().clone(),
        started: unix_now(),
    };
    let value = auth.seal_login(state.secret(), &login);
    let jar = jar.add(auth.cookie(name, value, LOGIN_TTL));
    (jar, Redirect::to(url.as_str())).into_response()
}

#[derive(Debug, Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn callback(
    State(admin): State<Admin>,
    jar: CookieJar,
    Query(params): Query<CallbackParams>,
) -> Response {
    let Some(auth) = &admin.auth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let name = params.state.as_deref().and_then(login_cookie);
    let finished = finish_login(auth, &jar, name.as_deref(), params).await;
    // The sign-in is over either way.
    let jar = match name {
        Some(name) if jar.get(&name).is_some() => jar.remove(Auth::removal(name)),
        _ => jar,
    };
    match finished {
        Ok(user) => {
            tracing::info!(user = %user.0, "signed in");
            let session = auth.start_session(user);
            let jar = jar.add(auth.cookie(SESSION_COOKIE, session, SESSION_TTL));
            (jar, Redirect::to("/")).into_response()
        }
        Err(Refusal(status, message)) => {
            (jar, page(status, "Sign-in failed", &message)).into_response()
        }
    }
}

/// Why a sign-in did not work out, for the person trying.
#[derive(Debug)]
struct Refusal(StatusCode, String);

fn refused(status: StatusCode, message: &str) -> Refusal {
    Refusal(status, message.to_owned())
}

/// The provider's side went wrong; the details are for the log only.
fn provider_failed(err: &dyn fmt::Display) -> Refusal {
    tracing::warn!(error = %err, "OpenID Connect sign-in failed");
    refused(
        StatusCode::BAD_GATEWAY,
        "The sign-in provider's answer could not be verified.",
    )
}

async fn finish_login(
    auth: &Auth,
    jar: &CookieJar,
    cookie: Option<&str>,
    params: CallbackParams,
) -> Result<User, Refusal> {
    // The state names the sign-in, and only the browser that started it
    // holds its cookie.
    let state = params.state.unwrap_or_default();
    let login = cookie
        .and_then(|name| jar.get(name))
        .and_then(|cookie| auth.open_login(&state, cookie.value()))
        .ok_or_else(|| {
            refused(
                StatusCode::BAD_REQUEST,
                "This sign-in has expired or was started elsewhere.",
            )
        })?;

    if let Some(error) = params.error {
        let detail = params.error_description.unwrap_or(error);
        return Err(refused(
            StatusCode::FORBIDDEN,
            &format!("The provider refused: {detail}"),
        ));
    }
    let code = params
        .code
        .ok_or_else(|| refused(StatusCode::BAD_REQUEST, "The provider sent no code."))?;

    let http = auth.http_client();
    let provider = auth.provider().await.map_err(|err| provider_failed(&err))?;
    let tokens = provider
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|err| provider_failed(&err))?
        .set_pkce_verifier(PkceCodeVerifier::new(login.pkce_verifier))
        .request_async(&http)
        .await
        .map_err(|err| provider_failed(&err))?;

    let id_token = tokens
        .id_token()
        .ok_or_else(|| provider_failed(&"no ID token in the token response"))?;
    // The access token came straight from the token endpoint, so its
    // `at_hash` need not be checked (OIDC Core 3.1.3.8).
    let claims = id_token
        .claims(&provider.id_token_verifier(), &Nonce::new(login.nonce))
        .map_err(|err| provider_failed(&err))?;

    let email = match claims.email() {
        Some(email) => Some((email.to_string(), claims.email_verified())),
        // Some providers keep the address out of the ID token.
        None => user_info(&provider, &http, tokens.access_token(), claims.subject())
            .await
            .map_err(|err| provider_failed(&err))?,
    };
    let Some((email, Some(true))) = email else {
        return Err(refused(
            StatusCode::FORBIDDEN,
            "The provider did not vouch for an email address of yours.",
        ));
    };
    if !auth.allowed(&email) {
        tracing::warn!(%email, "sign-in refused: not on the allowlist");
        return Err(refused(
            StatusCode::FORBIDDEN,
            &format!("{email} may not use this interface."),
        ));
    }
    Ok(User(email))
}

/// The email address and whether it is verified, from the userinfo
/// endpoint.
async fn user_info(
    provider: &Provider,
    http: &(impl Fn(HttpRequest) -> HttpFuture + Sync),
    access_token: &AccessToken,
    subject: &SubjectIdentifier,
) -> anyhow::Result<Option<(String, Option<bool>)>> {
    let Ok(request) = provider.user_info(access_token.clone(), Some(subject.clone())) else {
        // No userinfo endpoint.
        return Ok(None);
    };
    let claims: CoreUserInfoClaims = request.request_async(http).await?;
    Ok(claims
        .email()
        .map(|email| (email.to_string(), claims.email_verified())))
}

async fn logout(State(admin): State<Admin>, headers: HeaderMap, jar: CookieJar) -> Response {
    if !headers.contains_key(API_GUARD_HEADER) {
        return proxy::error(
            StatusCode::FORBIDDEN,
            &format!("signing out needs the {API_GUARD_HEADER} header"),
        );
    }
    if let (Some(auth), Some(session)) = (&admin.auth, jar.get(SESSION_COOKIE)) {
        lock(&auth.0.sessions).remove(session.value());
    }
    (
        jar.remove(Auth::removal(SESSION_COOKIE)),
        StatusCode::NO_CONTENT,
    )
        .into_response()
}

async fn signed_out() -> Response {
    page(
        StatusCode::OK,
        "Signed out",
        "You are signed out of the WordMaker admin interface.",
    )
}

/// A small page of its own, for when the interface is out of reach.
fn page(status: StatusCode, title: &str, message: &str) -> Response {
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>{title} · WordMaker</title>
<style>
  body {{ font: 16px/1.5 system-ui, sans-serif; max-width: 32rem; margin: 15vh auto; padding: 0 16px; color: #1f2328; background: #fff; }}
  a {{ color: #2d6cdf; }}
  @media (prefers-color-scheme: dark) {{ body {{ color: #e6e6e6; background: #17181a; }} a {{ color: #7aa7ff; }} }}
</style>
</head>
<body>
<h1>{title}</h1>
<p>{message}</p>
<p><a href="/auth/login">Sign in</a></p>
</body>
</html>"#,
        title = escape(title),
        message = escape(message),
    );
    (status, [(header::CACHE_CONTROL, "no-store")], Html(html)).into_response()
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use axum::{
        Form,
        Json,
        body::Body,
        http::Method,
    };
    use chrono::{
        TimeDelta,
        Utc,
    };
    use http_body_util::BodyExt;
    use openidconnect::{
        Audience,
        EmptyAdditionalClaims,
        EndUserEmail,
        StandardClaims,
        core::{
            CoreHmacKey,
            CoreIdToken,
            CoreIdTokenClaims,
            CoreJwsSigningAlgorithm,
        },
    };
    use serde_json::json;
    use tower::ServiceExt;

    use super::*;
    use crate::{
        app,
        proxy::Upstream,
    };

    const CLIENT_SECRET: &str = "client-secret-of-some-length";

    /// A stand-in OIDC provider. Its codes are `email|verified|nonce`, so a
    /// test says who signs in by the code it hands back.
    async fn fake_provider() -> String {
        #[derive(Deserialize)]
        struct TokenForm {
            code: String,
            code_verifier: Option<String>,
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let discovery = json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["HS256"],
        });
        let token_issuer = issuer.clone();
        let token = move |Form(form): Form<TokenForm>| async move {
            assert!(form.code_verifier.is_some(), "PKCE verifier missing");
            let mut parts = form.code.splitn(3, '|');
            let (email, verified, nonce) = (
                parts.next().unwrap().to_owned(),
                parts.next().unwrap() == "true",
                parts.next().unwrap().to_owned(),
            );
            let access_token = AccessToken::new("access-token".into());
            let claims = CoreIdTokenClaims::new(
                IssuerUrl::new(token_issuer).unwrap(),
                vec![Audience::new("admin".into())],
                Utc::now() + TimeDelta::minutes(5),
                Utc::now(),
                StandardClaims::new(SubjectIdentifier::new("user-1".into()))
                    .set_email(Some(EndUserEmail::new(email)))
                    .set_email_verified(Some(verified)),
                EmptyAdditionalClaims {},
            )
            .set_nonce(Some(Nonce::new(nonce)));
            let id_token = CoreIdToken::new(
                claims,
                &CoreHmacKey::new(CLIENT_SECRET),
                CoreJwsSigningAlgorithm::HmacSha256,
                Some(&access_token),
                None,
            )
            .unwrap();
            Json(json!({
                "access_token": access_token.secret(),
                "token_type": "Bearer",
                "id_token": id_token.to_string(),
            }))
        };
        let provider = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || async move { Json(discovery) }),
            )
            .route("/jwks", get(|| async { Json(json!({ "keys": [] })) }))
            .route("/token", post(token));
        tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
        issuer
    }

    fn args(issuer: &str) -> OidcArgs {
        OidcArgs {
            oidc_issuer: Some(issuer.to_owned()),
            oidc_client_id: Some("admin".into()),
            oidc_client_secret: Some(CLIENT_SECRET.into()),
            oidc_client_secret_file: None,
            public_url: Some("http://localhost:3001/".into()),
            allowed_users: vec!["Alice@Example.com".into(), "@team.example".into()],
        }
    }

    async fn admin() -> Router {
        let issuer = fake_provider().await;
        let auth = Auth::new(args(&issuer).resolve().unwrap().unwrap())
            .await
            .unwrap();
        app::router(Admin {
            upstream: Upstream::new("http://127.0.0.1:9", "secret".into()).unwrap(),
            allowed_hosts: vec!["localhost".to_owned()].into(),
            auth: Some(auth),
        })
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: String,
    }

    impl Reply {
        fn location(&self) -> &str {
            self.headers[header::LOCATION].to_str().unwrap()
        }

        /// The `name=value` part of the cookie set under `name`.
        fn cookie(&self, name: &str) -> Option<String> {
            self.headers
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .filter_map(|value| value.split(';').next())
                .find(|pair| pair.starts_with(&format!("{name}=")))
                .map(str::to_owned)
        }

        /// The `name=value` part of the login cookie this reply sets.
        fn login_cookie(&self) -> Option<String> {
            self.headers
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .filter_map(|value| value.split(';').next())
                .find(|pair| pair.starts_with(&format!("{LOGIN_COOKIE}_")))
                .map(str::to_owned)
        }
    }

    async fn call(app: &Router, method: Method, uri: &str, cookie: Option<&str>) -> Reply {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "localhost:3001")
            .header(API_GUARD_HEADER, "1");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    /// Starts a sign-in and comes back from the provider as `email`; returns
    /// the callback's answer.
    async fn sign_in_as(app: &Router, email: &str, verified: bool) -> Reply {
        let login = call(app, Method::GET, "/auth/login", None).await;
        come_back_as(
            app,
            &login,
            email,
            verified,
            login.login_cookie().as_deref(),
        )
        .await
    }

    /// Comes back from the provider to the sign-in `login` started, with
    /// `cookie`.
    async fn come_back_as(
        app: &Router,
        login: &Reply,
        email: &str,
        verified: bool,
        cookie: Option<&str>,
    ) -> Reply {
        assert_eq!(login.status, StatusCode::SEE_OTHER, "{}", login.body);
        let url = openidconnect::url::Url::parse(login.location()).unwrap();
        let param = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .unwrap()
                .1
                .into_owned()
        };
        assert_eq!(param("code_challenge_method"), "S256");
        assert_eq!(param("redirect_uri"), "http://localhost:3001/auth/callback");
        assert!(param("scope").contains("email"));

        let code = format!("{email}|{verified}|{}", param("nonce"));
        let callback = openidconnect::url::form_urlencoded::Serializer::new(String::new())
            .append_pair("code", &code)
            .append_pair("state", &param("state"))
            .finish();
        call(
            app,
            Method::GET,
            &format!("/auth/callback?{callback}"),
            cookie,
        )
        .await
    }

    #[tokio::test]
    async fn signs_in_and_out() {
        let app = admin().await;

        let page = call(&app, Method::GET, "/", None).await;
        assert_eq!(page.status, StatusCode::SEE_OTHER);
        assert_eq!(page.location(), "/auth/login");

        let reply = sign_in_as(&app, "alice@example.com", true).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        assert_eq!(reply.location(), "/");
        let session = reply.cookie(SESSION_COOKIE).unwrap();
        let set_cookie = reply
            .headers
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap())
            .find(|value| value.starts_with(&session))
            .unwrap();
        assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
        assert!(set_cookie.contains("SameSite=Lax"), "{set_cookie}");

        let page = call(&app, Method::GET, "/", Some(&session)).await;
        assert_eq!(page.status, StatusCode::OK);
        let config = call(&app, Method::GET, "/config.json", Some(&session)).await;
        let config: serde_json::Value = serde_json::from_str(&config.body).unwrap();
        assert_eq!(config["user"], "alice@example.com");

        let logout = call(&app, Method::POST, "/auth/logout", Some(&session)).await;
        assert_eq!(logout.status, StatusCode::NO_CONTENT);
        let page = call(&app, Method::GET, "/", Some(&session)).await;
        assert_eq!(page.status, StatusCode::SEE_OTHER, "the session is gone");
    }

    #[tokio::test]
    async fn only_allowed_and_verified_addresses_get_in() {
        let app = admin().await;

        let reply = sign_in_as(&app, "bob@team.example", true).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);

        for (email, verified) in [
            ("mallory@example.com", true),
            ("bob@team.example.evil", true),
            ("alice@example.com", false),
        ] {
            let reply = sign_in_as(&app, email, verified).await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{email}");
            assert!(reply.cookie(SESSION_COOKIE).is_none(), "{email}");
        }
    }

    #[tokio::test]
    async fn callbacks_must_come_back_to_the_browser_that_started() {
        let app = admin().await;
        let login = call(&app, Method::GET, "/auth/login", None).await;
        let url = openidconnect::url::Url::parse(login.location()).unwrap();
        let (_, state) = url.query_pairs().find(|(key, _)| key == "state").unwrap();

        // Someone else's browser, without the sign-in cookie.
        let reply = call(
            &app,
            Method::GET,
            &format!("/auth/callback?code=x&state={state}"),
            None,
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(reply.cookie(SESSION_COOKIE).is_none());
    }

    #[tokio::test]
    async fn sign_ins_keep_no_state_here() {
        let app = admin().await;
        // Far more sign-ins than any table would have held.
        for _ in 0..1500 {
            let login = call(&app, Method::GET, "/auth/login", None).await;
            assert_eq!(login.status, StatusCode::SEE_OTHER, "{}", login.body);
        }
        let reply = sign_in_as(&app, "alice@example.com", true).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        assert!(reply.cookie(SESSION_COOKIE).is_some());
        let removed = reply.login_cookie().unwrap();
        assert!(
            removed.ends_with('='),
            "the login cookie is cleared: {removed}"
        );
    }

    #[tokio::test]
    async fn parallel_sign_ins_both_work() {
        let app = admin().await;
        let first = call(&app, Method::GET, "/auth/login", None).await;
        let second = call(&app, Method::GET, "/auth/login", None).await;
        let cookies = format!(
            "{}; {}",
            first.login_cookie().unwrap(),
            second.login_cookie().unwrap()
        );
        for login in [&second, &first] {
            let reply = come_back_as(&app, login, "alice@example.com", true, Some(&cookies)).await;
            assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        }
    }

    #[tokio::test]
    async fn login_cookies_cannot_be_altered_or_moved() {
        let app = admin().await;
        let first = call(&app, Method::GET, "/auth/login", None).await;
        let second = call(&app, Method::GET, "/auth/login", None).await;
        let cookie = first.login_cookie().unwrap();

        // Another sign-in's cookie under this one's name.
        let (name, _) = cookie.split_once('=').unwrap();
        let other = second.login_cookie().unwrap();
        let (_, other) = other.split_once('=').unwrap();
        let moved = format!("{name}={other}");
        // A changed verifier.
        let altered = cookie.replacen('=', "=x", 1);
        for cookie in [moved, altered] {
            let reply = come_back_as(&app, &first, "alice@example.com", true, Some(&cookie)).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{cookie}");
            assert!(reply.cookie(SESSION_COOKIE).is_none());
        }
    }

    #[test]
    fn login_cookies_expire() {
        let auth = Auth(Arc::new(Inner {
            config: args("https://id.example").resolve().unwrap().unwrap(),
            http: reqwest::Client::new(),
            provider: Mutex::default(),
            login_key: "key".into(),
            sessions: Mutex::default(),
        }));
        let fresh = PendingLogin {
            pkce_verifier: "verifier".into(),
            nonce: "nonce".into(),
            started: unix_now(),
        };
        let sealed = auth.seal_login("state", &fresh);
        assert_eq!(auth.open_login("state", &sealed), Some(fresh));
        assert_eq!(auth.open_login("other", &sealed), None);

        let old = PendingLogin {
            started: unix_now() - LOGIN_TTL.as_secs(),
            pkce_verifier: "verifier".into(),
            nonce: "nonce".into(),
        };
        assert_eq!(
            auth.open_login("state", &auth.seal_login("state", &old)),
            None
        );

        assert_eq!(
            login_cookie("abc-_9").as_deref(),
            Some("wordmaker_admin_login_abc-_9")
        );
        assert_eq!(login_cookie(""), None);
        assert_eq!(login_cookie("a=b"), None);
    }

    #[tokio::test]
    async fn without_a_session_only_sign_in_is_open() {
        let app = admin().await;
        for path in [
            "/api/v1/pages",
            "/config.json",
            "/app.js",
            "/media/logo.png",
        ] {
            let reply = call(&app, Method::GET, path, None).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{path}");
        }
        let reply = call(&app, Method::GET, "/auth/signed-out", None).await;
        assert_eq!(reply.status, StatusCode::OK);

        let reply = call(
            &app,
            Method::GET,
            "/api/v1/pages",
            Some(&format!("{SESSION_COOKIE}=made-up")),
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn settings_are_checked() {
        assert!(OidcArgs::default().resolve().unwrap().is_none());

        let without_issuer = OidcArgs {
            oidc_issuer: None,
            ..args("https://id.example")
        };
        assert!(
            without_issuer.resolve().is_err(),
            "settings without an issuer"
        );

        let nobody = OidcArgs {
            allowed_users: vec![" ".into()],
            ..args("https://id.example")
        };
        let err = nobody.resolve().unwrap_err().to_string();
        assert!(err.contains("ALLOWED_USERS"), "{err}");

        let no_public_url = OidcArgs {
            public_url: None,
            ..args("https://id.example")
        };
        assert!(no_public_url.resolve().is_err());

        let config = args("https://id.example").resolve().unwrap().unwrap();
        assert_eq!(
            config.redirect_url.as_str(),
            "http://localhost:3001/auth/callback"
        );
        assert_eq!(config.allowed_users, ["alice@example.com", "@team.example"]);
    }

    #[test]
    fn error_pages_escape_what_they_show() {
        assert_eq!(
            escape(r#"<a href="x">&'"#),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }
}
