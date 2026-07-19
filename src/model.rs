use serde::Serialize;
use sqlx::FromRow;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum CreatorType {
    User,
    Group,
}

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
    pub source_kind: String,
    pub archive_record_id: Option<String>,
    pub archive_sha256: Option<String>,
    pub archive_path: Option<String>,
    pub archive_size: Option<i64>,
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
    pub creator_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator_type: Option<CreatorType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandboxed_asset_id: Option<u64>,
    pub status: String,
    pub cached: bool,
    pub source_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_record_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
}
