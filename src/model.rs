use serde::Serialize;
use sqlx::FromRow;

#[derive(Clone, Debug, FromRow)]
pub struct Workflow {
    pub source_asset_id: i64,
    pub source_revision: String,
    pub sandboxed_asset_id: Option<i64>,
    pub operation_id: Option<String>,
    pub state: String,
    pub failure_message: Option<String>,
    pub validated_at: i64,
}

#[derive(Debug, Serialize)]
pub struct SandboxResponse {
    pub source_asset_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandboxed_asset_id: Option<u64>,
    pub status: String,
    pub cached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
}

#[derive(Clone, Debug, FromRow)]
pub struct GameWorkflow {
    pub source_place_id: i64,
    pub source_revision: String,
    pub source_name: String,
    pub sandboxed_asset_id: Option<i64>,
    pub operation_id: Option<String>,
    pub state: String,
    pub failure_message: Option<String>,
    pub validated_at: i64,
}

#[derive(Debug, Serialize)]
pub struct GameSandboxResponse {
    pub source_place_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandboxed_asset_id: Option<u64>,
    pub status: String,
    pub cached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
}
