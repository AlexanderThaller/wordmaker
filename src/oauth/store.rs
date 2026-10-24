//! Registered clients, authorization codes and tokens. Codes and tokens are
//! looked up by their hash; the values themselves are never stored.

use chrono::{
    DateTime,
    Utc,
};
use sqlx::{
    SqliteConnection,
    SqlitePool,
};

use crate::error::AppResult;

#[derive(Debug, Clone)]
pub struct RegisteredClient {
    pub client_id: String,
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
}

pub async fn insert_client(pool: &SqlitePool, client: &RegisteredClient) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO oauth_clients (client_id, client_name, redirect_uris, created_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind(&client.client_id)
    .bind(&client.client_name)
    .bind(serde_json::to_string(&client.redirect_uris).unwrap_or_default())
    .bind(Utc::now())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn count_clients(pool: &SqlitePool) -> AppResult<i64> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM oauth_clients")
        .fetch_one(pool)
        .await?;
    Ok(count)
}

pub async fn get_client(pool: &SqlitePool, client_id: &str) -> AppResult<Option<RegisteredClient>> {
    let row: Option<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT client_id, client_name, redirect_uris FROM oauth_clients WHERE client_id = ?",
    )
    .bind(client_id)
    .fetch_optional(pool)
    .await?;
    Ok(
        row.map(|(client_id, client_name, redirect_uris)| RegisteredClient {
            client_id,
            client_name,
            redirect_uris: serde_json::from_str(&redirect_uris).unwrap_or_default(),
        }),
    )
}

/// What an authorization code was issued for.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AuthCode {
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub resource: String,
    pub expires_at: DateTime<Utc>,
}

pub async fn insert_code(pool: &SqlitePool, code_hash: &str, code: &AuthCode) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO oauth_codes
             (code_hash, client_id, redirect_uri, code_challenge, resource, expires_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(code_hash)
    .bind(&code.client_id)
    .bind(&code.redirect_uri)
    .bind(&code.code_challenge)
    .bind(&code.resource)
    .bind(code.expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove a code and return it; a code can be redeemed only once.
pub async fn take_code(pool: &SqlitePool, code_hash: &str) -> AppResult<Option<AuthCode>> {
    let code = sqlx::query_as(
        "DELETE FROM oauth_codes WHERE code_hash = ?
         RETURNING client_id, redirect_uri, code_challenge, resource, expires_at",
    )
    .bind(code_hash)
    .fetch_optional(pool)
    .await?;
    Ok(code)
}

/// One sign-in of a client, shared by the tokens issued for it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TokenGrant {
    pub grant_id: String,
    pub client_id: String,
    pub resource: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Access,
    Refresh,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Access => "access",
            Self::Refresh => "refresh",
        }
    }
}

/// Replace whatever live tokens `grant` has with the given ones. Used
/// refresh tokens stay behind, to recognise their reuse.
pub async fn replace_tokens(
    conn: &mut SqliteConnection,
    grant: &TokenGrant,
    tokens: &[(Kind, &str, DateTime<Utc>)],
) -> AppResult<()> {
    sqlx::query("DELETE FROM oauth_tokens WHERE grant_id = ? AND used = 0")
        .bind(&grant.grant_id)
        .execute(&mut *conn)
        .await?;
    for (kind, token_hash, expires_at) in tokens {
        sqlx::query(
            "INSERT INTO oauth_tokens
                 (token_hash, kind, grant_id, client_id, resource, expires_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(token_hash)
        .bind(kind.as_str())
        .bind(&grant.grant_id)
        .bind(&grant.client_id)
        .bind(&grant.resource)
        .bind(expires_at)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// What a refresh token turned out to be when it was presented.
#[derive(Debug)]
pub enum Refresh {
    /// Unused and unexpired; it is now marked used.
    Fresh(TokenGrant),
    /// Used before: someone else has, or had, a copy of it.
    Reused {
        grant_id: String,
    },
    Unknown,
}

/// Use up a refresh token. Marking it used is a single statement, so of
/// two requests with the same token only one gets `Fresh`.
pub async fn use_refresh_token(
    conn: &mut SqliteConnection,
    token_hash: &str,
) -> AppResult<Refresh> {
    let now = Utc::now();
    let fresh: Option<(String, String, String)> = sqlx::query_as(
        "UPDATE oauth_tokens SET used = 1
         WHERE token_hash = ? AND kind = 'refresh' AND used = 0 AND expires_at > ?
         RETURNING grant_id, client_id, resource",
    )
    .bind(token_hash)
    .bind(now)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some((grant_id, client_id, resource)) = fresh {
        return Ok(Refresh::Fresh(TokenGrant {
            grant_id,
            client_id,
            resource,
        }));
    }
    let reused: Option<(String,)> = sqlx::query_as(
        "SELECT grant_id FROM oauth_tokens
         WHERE token_hash = ? AND kind = 'refresh' AND used = 1 AND expires_at > ?",
    )
    .bind(token_hash)
    .bind(now)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(reused.map_or(Refresh::Unknown, |(grant_id,)| Refresh::Reused { grant_id }))
}

/// Revoke every token of a grant.
pub async fn revoke_grant(conn: &mut SqliteConnection, grant_id: &str) -> AppResult<()> {
    sqlx::query("DELETE FROM oauth_tokens WHERE grant_id = ?")
        .bind(grant_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The grant behind an unexpired, unused token of the given kind.
pub async fn find_token(
    pool: &SqlitePool,
    kind: Kind,
    token_hash: &str,
) -> AppResult<Option<TokenGrant>> {
    let row: Option<(String, String, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT grant_id, client_id, resource, expires_at FROM oauth_tokens
         WHERE token_hash = ? AND kind = ? AND used = 0",
    )
    .bind(token_hash)
    .bind(kind.as_str())
    .fetch_optional(pool)
    .await?;
    Ok(row.filter(|(.., expires_at)| *expires_at > Utc::now()).map(
        |(grant_id, client_id, resource, _)| TokenGrant {
            grant_id,
            client_id,
            resource,
        },
    ))
}

/// Revoke every token of the grant `token_hash` belongs to.
pub async fn revoke(pool: &SqlitePool, token_hash: &str) -> AppResult<()> {
    sqlx::query(
        "DELETE FROM oauth_tokens WHERE grant_id IN
             (SELECT grant_id FROM oauth_tokens WHERE token_hash = ?)",
    )
    .bind(token_hash)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn purge_expired(pool: &SqlitePool) -> AppResult<()> {
    let now = Utc::now();
    sqlx::query("DELETE FROM oauth_codes WHERE expires_at < ?")
        .bind(now)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM oauth_tokens WHERE expires_at < ?")
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}
