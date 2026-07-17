use anyhow::Result;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    SqlitePool,
};
use std::str::FromStr;

use crate::archive::ArchiveVariant;
use crate::model::{GameWorkflow, Workflow};

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

pub async fn get_game(pool: &SqlitePool, id: u64) -> Result<Option<GameWorkflow>> {
    Ok(
        sqlx::query_as::<_, GameWorkflow>("SELECT * FROM game_workflows WHERE source_place_id = ?")
            .bind(id as i64)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn begin_game(
    pool: &SqlitePool,
    id: u64,
    revision: &str,
    name: &str,
    archive: Option<&ArchiveVariant>,
    now: i64,
) -> Result<()> {
    let source_kind = if archive.is_some() {
        "archive"
    } else {
        "roblox"
    };
    sqlx::query("INSERT INTO game_workflows(source_place_id,source_revision,source_name,source_kind,archive_record_id,archive_sha256,archive_path,archive_size,state,validated_at,attempted_at) VALUES(?,?,?,?,?,?,?,?,'uploading',?,?) ON CONFLICT(source_place_id) DO UPDATE SET source_revision=excluded.source_revision,source_name=excluded.source_name,source_kind=excluded.source_kind,archive_record_id=excluded.archive_record_id,archive_sha256=excluded.archive_sha256,archive_path=excluded.archive_path,archive_size=excluded.archive_size,sandboxed_asset_id=NULL,operation_id=NULL,state='uploading',failure_code=NULL,failure_message=NULL,validated_at=excluded.validated_at,attempted_at=excluded.attempted_at,completed_at=NULL")
        .bind(id as i64)
        .bind(revision)
        .bind(name)
        .bind(source_kind)
        .bind(archive.map(|value| value.record_id.as_str()))
        .bind(archive.map(|value| value.sha256.as_str()))
        .bind(archive.map(|value| value.path.as_str()))
        .bind(archive.map(|value| value.size_bytes as i64))
        .bind(now)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn touch_game(pool: &SqlitePool, id: u64, now: i64) -> Result<()> {
    sqlx::query("UPDATE game_workflows SET validated_at=? WHERE source_place_id=?")
        .bind(now)
        .bind(id as i64)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_game(
    pool: &SqlitePool,
    id: u64,
    state: &str,
    asset: Option<u64>,
    operation: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE game_workflows SET state=?, sandboxed_asset_id=COALESCE(?,sandboxed_asset_id), operation_id=COALESCE(?,operation_id), attempted_at=unixepoch(), completed_at=CASE WHEN ?='approved' THEN unixepoch() ELSE completed_at END WHERE source_place_id=?")
        .bind(state).bind(asset.map(|x| x as i64)).bind(operation).bind(state).bind(id as i64).execute(pool).await?;
    Ok(())
}

pub async fn fail_game(pool: &SqlitePool, id: u64, code: &str, message: &str) -> Result<()> {
    sqlx::query("UPDATE game_workflows SET state='failed',failure_code=?,failure_message=?,attempted_at=unixepoch(),completed_at=unixepoch() WHERE source_place_id=?")
        .bind(code).bind(message).bind(id as i64).execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn model_and_game_workflows_with_the_same_id_are_independent() {
        let pool = connect("sqlite::memory:?cache=shared").await.unwrap();
        begin(&pool, 1818, "model-revision", 1).await.unwrap();
        begin_game(&pool, 1818, "game-revision", "Crossroads", None, 1)
            .await
            .unwrap();

        let model = get(&pool, 1818).await.unwrap().unwrap();
        let game = get_game(&pool, 1818).await.unwrap().unwrap();
        assert_eq!(model.source_revision, "model-revision");
        assert_eq!(game.source_revision, "game-revision");
        assert_eq!(game.source_name, "Crossroads");
        assert_eq!(game.source_kind, "roblox");
    }

    #[tokio::test]
    async fn archive_workflow_persists_integrity_metadata() {
        let pool = connect("sqlite::memory:?cache=shared").await.unwrap();
        let archive = ArchiveVariant {
            record_id: "nla_fixture".into(),
            title: "Archived Crossroads".into(),
            sha256: "a".repeat(64),
            size_bytes: 1234,
            path: "levels/sha256/aa/file.rbxlx".into(),
        };
        begin_game(
            &pool,
            1818,
            &format!("archive:{}", archive.sha256),
            &archive.title,
            Some(&archive),
            1,
        )
        .await
        .unwrap();

        let game = get_game(&pool, 1818).await.unwrap().unwrap();
        assert_eq!(game.source_kind, "archive");
        assert_eq!(game.archive_record_id.as_deref(), Some("nla_fixture"));
        assert_eq!(
            game.archive_sha256.as_deref(),
            Some(archive.sha256.as_str())
        );
        assert_eq!(game.archive_path.as_deref(), Some(archive.path.as_str()));
        assert_eq!(game.archive_size, Some(1234));
    }
}
