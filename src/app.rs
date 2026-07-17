use std::{sync::Arc, time::Instant};

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use chrono::Utc;
use dashmap::DashMap;
use sqlx::SqlitePool;
use subtle::ConstantTimeEq;
use tokio::{sync::Mutex, time::sleep};

use crate::{
    archive::{ArchiveClient, ArchiveVariant},
    config::Config,
    db,
    error::AppError,
    model::{GameSandboxResponse, GameWorkflow, SandboxResponse, Workflow},
    roblox::{Moderation, Operation, RobloxClient},
    transform,
};

#[derive(Clone)]
pub struct AppState {
    config: Config,
    pool: SqlitePool,
    roblox: RobloxClient,
    archive: ArchiveClient,
    locks: Arc<DashMap<u64, Arc<Mutex<()>>>>,
    game_locks: Arc<DashMap<u64, Arc<Mutex<()>>>>,
}

impl AppState {
    pub async fn new(config: Config, pool: SqlitePool) -> anyhow::Result<Self> {
        let archive = ArchiveClient::new(&config, pool.clone()).await?;
        archive.spawn_refresh();
        Ok(Self {
            roblox: RobloxClient::new(config.clone())?,
            archive,
            config,
            pool,
            locks: Arc::new(DashMap::new()),
            game_locks: Arc::new(DashMap::new()),
        })
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/sandbox/{asset_id}", get(sandbox))
        .route("/v1/sandbox_game/{place_id}", get(sandbox_game))
        .with_state(state)
}

async fn sandbox_game(
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    authenticate(&headers, &state.config.service_token)?;
    let id: u64 = raw_id.parse().map_err(|_| AppError::InvalidId)?;
    if id == 0 {
        return Err(AppError::InvalidId);
    }
    let lock = state
        .game_locks
        .entry(id)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let _guard = lock.lock().await;
    let result = run_game(&state, id).await;
    state
        .game_locks
        .remove_if(&id, |_, value| Arc::strong_count(value) <= 2);
    result
}

async fn run_game(
    state: &AppState,
    id: u64,
) -> Result<(StatusCode, HeaderMap, Json<GameSandboxResponse>), AppError> {
    let now = Utc::now().timestamp();
    if let Some(row) = db::get_game(&state.pool, id).await? {
        if row.state == "approved"
            && now - row.validated_at < state.config.cache_ttl.as_secs() as i64
        {
            return game_response(&row, true);
        }
    }

    let archive = state.archive.resolve(id).await;
    let (revision, name) = if let Some(variant) = &archive {
        (format!("archive:{}", variant.sha256), variant.title.clone())
    } else {
        let metadata = state.roblox.validate_game(id).await?;
        (metadata.revision, metadata.name)
    };
    let current = db::get_game(&state.pool, id).await?;
    match current {
        Some(ref row) if row.source_revision == revision => {
            db::touch_game(&state.pool, id, now).await?;
            if row.state == "approved" {
                let mut fresh = row.clone();
                fresh.validated_at = now;
                return game_response(&fresh, true);
            }
            if row.state == "failed" {
                return Err(AppError::Upstream(
                    row.failure_message
                        .clone()
                        .unwrap_or_else(|| "previous game workflow failed".into()),
                ));
            }
        }
        _ => db::begin_game(&state.pool, id, &revision, &name, archive.as_ref(), now).await?,
    }

    let deadline = Instant::now() + state.config.polling_window;
    loop {
        let row = db::get_game(&state.pool, id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("game workflow disappeared"))?;
        match row.state.as_str() {
            "uploading" if row.operation_id.is_none() => {
                let input = if row.source_kind == "archive" {
                    state.archive.download(&archive_variant(&row)?).await?
                } else {
                    state.roblox.download(id).await?.to_vec()
                };
                let name = row.source_name.clone();
                let output =
                    tokio::task::spawn_blocking(move || transform::package_game(&input, &name))
                        .await
                        .map_err(|e| anyhow::anyhow!(e))??;
                let operation = state.roblox.upload_game(id, output).await?;
                db::update_game(&state.pool, id, "uploading", None, Some(&operation)).await?;
            }
            "uploading" => match state
                .roblox
                .operation(row.operation_id.as_deref().unwrap())
                .await?
            {
                Operation::Pending => {}
                Operation::Complete(asset_id) => {
                    db::update_game(&state.pool, id, "moderating", Some(asset_id), None).await?
                }
                Operation::Failed(message) => {
                    db::fail_game(&state.pool, id, "upload_rejected", &message).await?;
                    return Err(AppError::Upstream(message));
                }
            },
            "moderating" => match state
                .roblox
                .moderation(row.sandboxed_asset_id.expect("moderating game asset") as u64)
                .await?
            {
                Moderation::Pending => {}
                Moderation::Approved => {
                    db::update_game(&state.pool, id, "approved", None, None).await?
                }
                Moderation::Rejected(message) => {
                    db::fail_game(&state.pool, id, "moderation_rejected", &message).await?;
                    return Err(AppError::Upstream(format!(
                        "moderation rejected: {message}"
                    )));
                }
            },
            "approved" => return game_response(&row, false),
            "failed" => {
                return Err(AppError::Upstream(
                    row.failure_message
                        .unwrap_or_else(|| "game workflow failed".into()),
                ))
            }
            other => return Err(anyhow::anyhow!("unknown game workflow state {other}").into()),
        }
        if Instant::now() >= deadline {
            let row = db::get_game(&state.pool, id)
                .await?
                .expect("game workflow exists");
            return game_response(&row, false);
        }
        sleep(state.config.polling_interval).await;
    }
}

fn game_response(
    row: &GameWorkflow,
    cached: bool,
) -> Result<(StatusCode, HeaderMap, Json<GameSandboxResponse>), AppError> {
    let complete = row.state == "approved";
    let mut headers = HeaderMap::new();
    if !complete {
        headers.insert(header::RETRY_AFTER, "10".parse().unwrap());
    }
    Ok((
        if complete {
            StatusCode::OK
        } else {
            StatusCode::ACCEPTED
        },
        headers,
        Json(GameSandboxResponse {
            source_place_id: row.source_place_id as u64,
            sandboxed_asset_id: row.sandboxed_asset_id.map(|id| id as u64),
            status: row.state.clone(),
            cached,
            source_kind: row.source_kind.clone(),
            archive_record_id: row.archive_record_id.clone(),
            archive_sha256: row.archive_sha256.clone(),
            retry_after_seconds: (!complete).then_some(10),
        }),
    ))
}

fn archive_variant(row: &GameWorkflow) -> Result<ArchiveVariant, AppError> {
    let missing = || AppError::ArchiveIntegrity("archive workflow metadata is incomplete".into());
    Ok(ArchiveVariant {
        record_id: row.archive_record_id.clone().ok_or_else(missing)?,
        title: row.source_name.clone(),
        sha256: row.archive_sha256.clone().ok_or_else(missing)?,
        size_bytes: row.archive_size.ok_or_else(missing)? as u64,
        path: row.archive_path.clone().ok_or_else(missing)?,
    })
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    match sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&state.pool)
        .await
    {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({"status":"ok"}))),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"status":"unhealthy"})),
        ),
    }
}

async fn sandbox(
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    authenticate(&headers, &state.config.service_token)?;
    let id: u64 = raw_id.parse().map_err(|_| AppError::InvalidId)?;
    if id == 0 {
        return Err(AppError::InvalidId);
    }
    let lock = state
        .locks
        .entry(id)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let _guard = lock.lock().await;
    let result = run(&state, id).await;
    state
        .locks
        .remove_if(&id, |_, value| Arc::strong_count(value) <= 2);
    result
}

async fn run(
    state: &AppState,
    id: u64,
) -> Result<(StatusCode, HeaderMap, Json<SandboxResponse>), AppError> {
    let now = Utc::now().timestamp();
    if let Some(row) = db::get(&state.pool, id).await? {
        if row.state == "approved"
            && now - row.validated_at < state.config.cache_ttl.as_secs() as i64
        {
            return response(&row, true);
        }
    }

    let metadata = state.roblox.validate_source(id).await?;
    let current = db::get(&state.pool, id).await?;
    match current {
        Some(ref row) if row.source_revision == metadata.revision => {
            db::touch(&state.pool, id, now).await?;
            if row.state == "approved" {
                let mut fresh = row.clone();
                fresh.validated_at = now;
                return response(&fresh, true);
            }
            if row.state == "failed" {
                return Err(AppError::Upstream(
                    row.failure_message
                        .clone()
                        .unwrap_or_else(|| "previous workflow failed".into()),
                ));
            }
        }
        _ => db::begin(&state.pool, id, &metadata.revision, now).await?,
    }

    let deadline = Instant::now() + state.config.polling_window;
    loop {
        let row = db::get(&state.pool, id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("workflow disappeared"))?;
        match row.state.as_str() {
            "uploading" if row.operation_id.is_none() => {
                let input = state.roblox.download(id).await?;
                let output = tokio::task::spawn_blocking(move || transform::sandbox(&input))
                    .await
                    .map_err(|e| anyhow::anyhow!(e))??;
                let operation = state.roblox.upload(id, output).await?;
                db::update(&state.pool, id, "uploading", None, Some(&operation)).await?;
            }
            "uploading" => match state
                .roblox
                .operation(row.operation_id.as_deref().unwrap())
                .await?
            {
                Operation::Pending => {}
                Operation::Complete(asset_id) => {
                    db::update(&state.pool, id, "moderating", Some(asset_id), None).await?
                }
                Operation::Failed(message) => {
                    db::fail(&state.pool, id, "upload_rejected", &message).await?;
                    return Err(AppError::Upstream(message));
                }
            },
            "moderating" => match state
                .roblox
                .moderation(row.sandboxed_asset_id.expect("moderating asset") as u64)
                .await?
            {
                Moderation::Pending => {}
                Moderation::Approved => db::update(&state.pool, id, "approved", None, None).await?,
                Moderation::Rejected(message) => {
                    db::fail(&state.pool, id, "moderation_rejected", &message).await?;
                    return Err(AppError::Upstream(format!(
                        "moderation rejected: {message}"
                    )));
                }
            },
            "approved" => return response(&row, false),
            "failed" => {
                return Err(AppError::Upstream(
                    row.failure_message
                        .unwrap_or_else(|| "workflow failed".into()),
                ))
            }
            other => return Err(anyhow::anyhow!("unknown workflow state {other}").into()),
        }
        if Instant::now() >= deadline {
            let row = db::get(&state.pool, id).await?.expect("workflow exists");
            return response(&row, false);
        }
        sleep(state.config.polling_interval).await;
    }
}

fn authenticate(headers: &HeaderMap, token: &str) -> Result<(), AppError> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    let api_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let expected = token.as_bytes();
    let valid = bearer.as_bytes().ct_eq(expected) | api_key.as_bytes().ct_eq(expected);
    if valid.into() {
        Ok(())
    } else {
        Err(AppError::Unauthorized)
    }
}

fn response(
    row: &Workflow,
    cached: bool,
) -> Result<(StatusCode, HeaderMap, Json<SandboxResponse>), AppError> {
    let complete = row.state == "approved";
    let mut headers = HeaderMap::new();
    if !complete {
        headers.insert(header::RETRY_AFTER, "10".parse().unwrap());
    }
    Ok((
        if complete {
            StatusCode::OK
        } else {
            StatusCode::ACCEPTED
        },
        headers,
        Json(SandboxResponse {
            source_asset_id: row.source_asset_id as u64,
            sandboxed_asset_id: row.sandboxed_asset_id.map(|x| x as u64),
            status: row.state.clone(),
            cached,
            retry_after_seconds: (!complete).then_some(10),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_bearer_token() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert!(authenticate(&headers, "abc").is_ok());
        assert!(authenticate(&headers, "abcd").is_err());
    }

    #[test]
    fn accepts_x_api_key() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "abc".parse().unwrap());
        assert!(authenticate(&headers, "abc").is_ok());
        assert!(authenticate(&headers, "abcd").is_err());
    }

    #[test]
    fn rejects_missing_or_malformed_credentials() {
        assert!(authenticate(&HeaderMap::new(), "abc").is_err());

        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "abc".parse().unwrap());
        assert!(authenticate(&headers, "abc").is_err());
    }
}
