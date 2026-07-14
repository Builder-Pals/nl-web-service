use std::{env, net::SocketAddr, time::Duration};

use anyhow::{bail, Context, Result};

#[derive(Clone, Debug)]
pub struct Config {
    pub bind_address: SocketAddr,
    pub service_token: String,
    pub roblox_api_key: String,
    pub creator_group_id: u64,
    pub database_url: String,
    pub cache_ttl: Duration,
    pub request_timeout: Duration,
    pub polling_window: Duration,
    pub polling_interval: Duration,
    pub retry_count: usize,
    pub roblox_base_url: String,
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
        })
    }
}

fn seconds(name: &str, default: u64) -> Result<Duration> {
    Ok(Duration::from_secs(
        env::var(name)
            .unwrap_or_else(|_| default.to_string())
            .parse()
            .with_context(|| format!("invalid {name}"))?,
    ))
}
