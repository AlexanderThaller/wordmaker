use std::{
    str::FromStr,
    time::Duration,
};

use sqlx::{
    Sqlite,
    SqlitePool,
    Transaction,
    sqlite::{
        SqliteConnectOptions,
        SqliteJournalMode,
        SqlitePoolOptions,
    },
};

pub async fn connect(url: &str) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await?;

    sqlx::migrate!().run(&pool).await?;
    Ok(pool)
}

/// A transaction that holds the write lock from the start. A deferred
/// `BEGIN` that reads first fails at once with "database is locked" when it
/// later writes after another connection committed; the busy timeout does
/// not apply then. Use this for every transaction that reads and then writes.
pub async fn begin_write(pool: &SqlitePool) -> sqlx::Result<Transaction<'static, Sqlite>> {
    pool.begin_with("BEGIN IMMEDIATE").await
}

/// A fresh in-memory database. Limited to one connection, since every
/// connection to `:memory:` would otherwise see its own empty database.
#[cfg(test)]
pub async fn connect_memory() -> SqlitePool {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::migrate!().run(&pool).await.unwrap();
    pool
}
