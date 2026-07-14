use anyhow::Result;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    SqlitePool,
};
use std::str::FromStr;

use crate::model::Workflow;

pub async fn connect(url: &str) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    Ok(pool)
}

pub async fn get(pool: &SqlitePool, id: u64) -> Result<Option<Workflow>> {
    Ok(
        sqlx::query_as::<_, Workflow>("SELECT * FROM workflows WHERE source_asset_id = ?")
            .bind(id as i64)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn begin(pool: &SqlitePool, id: u64, revision: &str, now: i64) -> Result<()> {
    sqlx::query("INSERT INTO workflows(source_asset_id,source_revision,state,validated_at,attempted_at) VALUES(?,?,'uploading',?,?) ON CONFLICT(source_asset_id) DO UPDATE SET source_revision=excluded.source_revision,sandboxed_asset_id=NULL,operation_id=NULL,state='uploading',failure_code=NULL,failure_message=NULL,validated_at=excluded.validated_at,attempted_at=excluded.attempted_at,completed_at=NULL")
        .bind(id as i64).bind(revision).bind(now).bind(now).execute(pool).await?;
    Ok(())
}

pub async fn touch(pool: &SqlitePool, id: u64, now: i64) -> Result<()> {
    sqlx::query("UPDATE workflows SET validated_at=? WHERE source_asset_id=?")
        .bind(now)
        .bind(id as i64)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update(
    pool: &SqlitePool,
    id: u64,
    state: &str,
    asset: Option<u64>,
    operation: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE workflows SET state=?, sandboxed_asset_id=COALESCE(?,sandboxed_asset_id), operation_id=COALESCE(?,operation_id), attempted_at=unixepoch(), completed_at=CASE WHEN ?='approved' THEN unixepoch() ELSE completed_at END WHERE source_asset_id=?")
        .bind(state).bind(asset.map(|x| x as i64)).bind(operation).bind(state).bind(id as i64).execute(pool).await?;
    Ok(())
}

pub async fn fail(pool: &SqlitePool, id: u64, code: &str, message: &str) -> Result<()> {
    sqlx::query("UPDATE workflows SET state='failed',failure_code=?,failure_message=?,attempted_at=unixepoch(),completed_at=unixepoch() WHERE source_asset_id=?")
        .bind(code).bind(message).bind(id as i64).execute(pool).await?;
    Ok(())
}
