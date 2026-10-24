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

/// Register `client`, keeping at most `max` clients. Unused clients (that
/// never got tokens and have no sign-in under way) registered before
/// `stale` are dropped; if there is still no room, the oldest unused ones
/// registered before `fresh` make way. Newer ones may be waiting for their
/// user to approve, so they are never pushed out by registrations that
/// follow. Returns false when no client can make way.
pub async fn insert_client(
    pool: &SqlitePool,
    client: &RegisteredClient,
    max: i64,
    stale: DateTime<Utc>,
    fresh: DateTime<Utc>,
) -> AppResult<bool> {
    // Starting with a write takes the database's write lock, so the count
    // below cannot change before the insert.
    let mut tx = pool.begin().await?;
    sqlx::query(
        "DELETE FROM oauth_clients
         WHERE used_at IS NULL AND created_at < ? AND NOT EXISTS
             (SELECT 1 FROM oauth_codes WHERE oauth_codes.client_id = oauth_clients.client_id)",
    )
    .bind(stale)
    .execute(&mut *tx)
    .await?;
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM oauth_clients")
        .fetch_one(&mut *tx)
        .await?;
    if count >= max {
        sqlx::query(
            "DELETE FROM oauth_clients WHERE client_id IN
                 (SELECT client_id FROM oauth_clients
                  WHERE used_at IS NULL AND created_at < ? AND NOT EXISTS
                      (SELECT 1 FROM oauth_codes
                       WHERE oauth_codes.client_id = oauth_clients.client_id)
                  ORDER BY created_at LIMIT ?)",
        )
        .bind(fresh)
        .bind(count - max + 1)
        .execute(&mut *tx)
        .await?;
    }
    let inserted = sqlx::query(
        "INSERT INTO oauth_clients (client_id, client_name, redirect_uris, created_at)
         SELECT ?, ?, ?, ? WHERE (SELECT COUNT(*) FROM oauth_clients) < ?",
    )
    .bind(&client.client_id)
    .bind(&client.client_name)
    .bind(serde_json::to_string(&client.redirect_uris).unwrap_or_default())
    .bind(Utc::now())
    .bind(max)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    tx.commit().await?;
    Ok(inserted)
}

/// Note that a registered client got tokens, which keeps it registered.
pub async fn mark_client_used(conn: &mut SqliteConnection, client_id: &str) -> AppResult<()> {
    sqlx::query("UPDATE oauth_clients SET used_at = ? WHERE client_id = ? AND used_at IS NULL")
        .bind(Utc::now())
        .bind(client_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
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
    /// Whether the authorization request named `redirect_uri` itself.
    pub redirect_uri_sent: bool,
    pub code_challenge: String,
    pub resource: String,
    pub expires_at: DateTime<Utc>,
}

pub async fn insert_code(pool: &SqlitePool, code_hash: &str, code: &AuthCode) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO oauth_codes
             (code_hash, client_id, redirect_uri, redirect_uri_sent, code_challenge, resource,
              expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(code_hash)
    .bind(&code.client_id)
    .bind(&code.redirect_uri)
    .bind(code.redirect_uri_sent)
    .bind(&code.code_challenge)
    .bind(&code.resource)
    .bind(code.expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// What an authorization code turned out to be when it was redeemed.
#[derive(Debug)]
pub enum Redeemed {
    /// Not redeemed before; it now belongs to the new grant.
    Fresh(AuthCode),
    /// Redeemed before, for `grant_id`.
    Replayed {
        grant_id: String,
    },
    Unknown,
}

/// Redeem a code for `grant_id`; a code can be redeemed only once. The code
/// stays until it expires, to recognise replays.
pub async fn redeem_code(
    conn: &mut SqliteConnection,
    code_hash: &str,
    grant_id: &str,
) -> AppResult<Redeemed> {
    let fresh = sqlx::query_as(
        "UPDATE oauth_codes SET grant_id = ? WHERE code_hash = ? AND grant_id IS NULL
         RETURNING client_id, redirect_uri, redirect_uri_sent, code_challenge, resource,
                   expires_at",
    )
    .bind(grant_id)
    .bind(code_hash)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(code) = fresh {
        return Ok(Redeemed::Fresh(code));
    }
    let replayed: Option<(String,)> = sqlx::query_as(
        "SELECT grant_id FROM oauth_codes WHERE code_hash = ? AND grant_id IS NOT NULL",
    )
    .bind(code_hash)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(
        replayed.map_or(Redeemed::Unknown, |(grant_id,)| Redeemed::Replayed {
            grant_id,
        }),
    )
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

/// Drop expired codes and tokens, including the used ones kept to spot
/// reuse.
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

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;

    fn client(id: &str) -> RegisteredClient {
        RegisteredClient {
            client_id: id.into(),
            client_name: None,
            redirect_uris: vec!["https://app.example/cb".into()],
        }
    }

    async fn ids(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar("SELECT client_id FROM oauth_clients ORDER BY created_at")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn registration_makes_room_from_unused_clients() {
        let pool = crate::db::connect_memory().await;
        let long_ago = Utc::now() - TimeDelta::days(1);
        let soon = Utc::now() + TimeDelta::days(1);
        for id in ["a", "b", "c"] {
            assert!(
                insert_client(&pool, &client(id), 3, long_ago, soon)
                    .await
                    .unwrap()
            );
        }
        // A full table drops the oldest client that was never used.
        assert!(
            insert_client(&pool, &client("d"), 3, long_ago, soon)
                .await
                .unwrap()
        );
        assert_eq!(ids(&pool).await, ["b", "c", "d"]);

        // Clients in use, or with a sign-in under way, stay.
        mark_client_used(&mut pool.acquire().await.unwrap(), "b")
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO oauth_codes
                 (code_hash, client_id, redirect_uri, code_challenge, \
             resource, expires_at)
             VALUES ('h', 'c', 'https://app.example/cb', 'x', 'r', ?)",
        )
        .bind(Utc::now())
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            insert_client(&pool, &client("e"), 3, long_ago, soon)
                .await
                .unwrap()
        );
        assert_eq!(ids(&pool).await, ["b", "c", "e"]);
        mark_client_used(&mut pool.acquire().await.unwrap(), "e")
            .await
            .unwrap();
        assert!(
            !insert_client(&pool, &client("f"), 3, long_ago, soon)
                .await
                .unwrap()
        );

        // Unused clients expire.
        sqlx::query("DELETE FROM oauth_codes")
            .execute(&pool)
            .await
            .unwrap();
        let later = Utc::now() + TimeDelta::seconds(1);
        assert!(
            insert_client(&pool, &client("g"), 3, later, soon)
                .await
                .unwrap()
        );
        assert_eq!(ids(&pool).await, ["b", "e", "g"]);
    }

    #[tokio::test]
    async fn new_clients_are_not_pushed_out() {
        let pool = crate::db::connect_memory().await;
        let long_ago = Utc::now() - TimeDelta::days(1);
        let fresh = Utc::now() - TimeDelta::minutes(15);
        for id in ["a", "b"] {
            assert!(
                insert_client(&pool, &client(id), 2, long_ago, fresh)
                    .await
                    .unwrap()
            );
        }
        // Both may still be waiting for their user to approve.
        assert!(
            !insert_client(&pool, &client("c"), 2, long_ago, fresh)
                .await
                .unwrap()
        );
        assert_eq!(ids(&pool).await, ["a", "b"]);
    }
}
