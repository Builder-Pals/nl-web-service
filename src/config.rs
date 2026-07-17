use std::{env, net::SocketAddr, time::Duration};

use anyhow::{bail, Context, Result};

#[derive(Clone, Debug)]
pub struct Config {
    pub bind_address: SocketAddr,
    pub service_token: String,
    pub roblox_api_key: String,
    pub release_tag: String,
    pub creator_group_id: u64,
    pub database_url: String,
    pub cache_ttl: Duration,
    pub request_timeout: Duration,
    pub polling_window: Duration,
    pub polling_interval: Duration,
    pub retry_count: usize,
    pub roblox_base_url: String,
    pub archive_index_url: String,
    pub archive_blob_base_url: String,
    pub archive_refresh: Duration,
    pub archive_max_source_bytes: usize,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let required = |name: &str| env::var(name).with_context(|| format!("{name} is required"));
        let service_token = required("SERVICE_API_TOKEN")?;
        if service_token.len() < 32 {
            bail!("SERVICE_API_TOKEN must be at least 32 characters")
        }
        Ok(Self {
            bind_address: required("BIND_ADDRESS")?
                .parse()
                .context("invalid BIND_ADDRESS")?,
            service_token,
            roblox_api_key: required("ROBLOX_OPEN_CLOUD_API_KEY")?,
            release_tag: env::var("RELEASE_TAG").unwrap_or_else(|_| "dev".into()),
            creator_group_id: required("ROBLOX_CREATOR_GROUP_ID")?
                .parse()
                .context("invalid ROBLOX_CREATOR_GROUP_ID")?,
            database_url: required("DATABASE_URL")?,
            cache_ttl: seconds("CACHE_TTL_SECONDS", 600)?,
            request_timeout: seconds("REQUEST_TIMEOUT_SECONDS", 20)?,
            polling_window: seconds("POLLING_WINDOW_SECONDS", 60)?,
            polling_interval: seconds("POLLING_INTERVAL_SECONDS", 2)?,
            retry_count: env::var("RETRY_COUNT")
                .unwrap_or_else(|_| "3".into())
                .parse()
                .context("invalid RETRY_COUNT")?,
            roblox_base_url: env::var("ROBLOX_BASE_URL")
                .unwrap_or_else(|_| "https://apis.roblox.com".into())
                .trim_end_matches('/')
                .into(),
            archive_index_url: env::var("ARCHIVE_INDEX_URL").unwrap_or_else(|_| {
                "https://raw.githubusercontent.com/Builder-Pals/native-level-archive/main/place-index-v1.json".into()
            }),
            archive_blob_base_url: ensure_trailing_slash(
                &env::var("ARCHIVE_BLOB_BASE_URL").unwrap_or_else(|_| {
                    "https://raw.githubusercontent.com/Builder-Pals/native-level-archive/main/".into()
                }),
            ),
            archive_refresh: seconds("ARCHIVE_REFRESH_SECONDS", 900)?,
            archive_max_source_bytes: bytes("ARCHIVE_MAX_SOURCE_BYTES", 64 * 1024 * 1024)?,
        })
    }
}

fn bytes(name: &str, default: usize) -> Result<usize> {
    env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .with_context(|| format!("invalid {name}"))
}

fn ensure_trailing_slash(value: &str) -> String {
    format!("{}/", value.trim_end_matches('/'))
}

fn seconds(name: &str, default: u64) -> Result<Duration> {
    Ok(Duration::from_secs(
        env::var(name)
            .unwrap_or_else(|_| default.to_string())
            .parse()
            .with_context(|| format!("invalid {name}"))?,
    ))
}
