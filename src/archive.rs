use std::{collections::HashMap, sync::Arc, time::Duration};

use futures_util::StreamExt;
use reqwest::{header, Client, StatusCode, Url};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::{sync::RwLock, time::sleep};
use tracing::{info, warn};

use crate::{config::Config, error::AppError};

#[derive(Clone)]
pub struct ArchiveClient {
    inner: Arc<Inner>,
}

struct Inner {
    http: Client,
    pool: SqlitePool,
    index_url: String,
    blob_base_url: Url,
    refresh_interval: Duration,
    max_source_bytes: usize,
    state: RwLock<State>,
}

#[derive(Default)]
struct State {
    index: ArchiveIndex,
    etag: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ArchiveIndex {
    pub schema_version: u32,
    #[serde(default)]
    pub places: HashMap<String, ArchivePlace>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ArchivePlace {
    pub universe_id: u64,
    pub preferred: ArchiveVariant,
    pub variants: Vec<ArchiveVariant>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ArchiveVariant {
    pub record_id: String,
    pub title: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub path: String,
}

impl ArchiveClient {
    pub async fn new(config: &Config, pool: SqlitePool) -> anyhow::Result<Self> {
        let blob_base_url = Url::parse(&config.archive_blob_base_url)?;
        let client = Self {
            inner: Arc::new(Inner {
                http: Client::builder()
                    .timeout(config.request_timeout)
                    .user_agent("Builder-Pals/nl-web-service")
                    .build()?,
                pool,
                index_url: config.archive_index_url.clone(),
                blob_base_url,
                refresh_interval: config.archive_refresh,
                max_source_bytes: config.archive_max_source_bytes,
                state: RwLock::new(State::default()),
            }),
        };
        client.load_cached().await?;
        if let Err(error) = client.refresh().await {
            warn!(error = %error, "archive catalog refresh failed; retaining cached index");
        }
        Ok(client)
    }

    pub fn spawn_refresh(&self) {
        let client = self.clone();
        tokio::spawn(async move {
            loop {
                sleep(client.inner.refresh_interval).await;
                if let Err(error) = client.refresh().await {
                    warn!(error = %error, "archive catalog refresh failed");
                }
            }
        });
    }

    pub async fn resolve(&self, place_id: u64) -> Option<ArchiveVariant> {
        self.inner
            .state
            .read()
            .await
            .index
            .places
            .get(&place_id.to_string())
            .map(|place| place.preferred.clone())
    }

    pub async fn download(&self, variant: &ArchiveVariant) -> Result<Vec<u8>, AppError> {
        validate_variant(variant).map_err(AppError::ArchiveIntegrity)?;
        if variant.size_bytes > self.inner.max_source_bytes as u64 {
            return Err(AppError::ArchiveIntegrity(format!(
                "archive record {} exceeds the configured source limit",
                variant.record_id
            )));
        }
        let url = self
            .inner
            .blob_base_url
            .join(&variant.path)
            .map_err(|error| AppError::ArchiveIntegrity(error.to_string()))?;
        if url.scheme() != self.inner.blob_base_url.scheme()
            || url.host_str() != self.inner.blob_base_url.host_str()
            || url.port_or_known_default() != self.inner.blob_base_url.port_or_known_default()
        {
            return Err(AppError::ArchiveIntegrity(
                "archive blob escaped the configured origin".into(),
            ));
        }
        let response = self
            .inner
            .http
            .get(url)
            .send()
            .await
            .map_err(|error| AppError::Upstream(error.to_string()))?;
        if !response.status().is_success() {
            return Err(AppError::Upstream(format!(
                "archive blob returned {}",
                response.status()
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length > self.inner.max_source_bytes as u64)
        {
            return Err(AppError::ArchiveIntegrity(
                "archive blob exceeded the configured source limit".into(),
            ));
        }
        let mut bytes = Vec::with_capacity(variant.size_bytes as usize);
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| AppError::Upstream(error.to_string()))?;
            if bytes.len() + chunk.len() > self.inner.max_source_bytes {
                return Err(AppError::ArchiveIntegrity(
                    "archive blob exceeded the configured source limit".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() as u64 != variant.size_bytes {
            return Err(AppError::ArchiveIntegrity(format!(
                "archive record {} size mismatch",
                variant.record_id
            )));
        }
        let actual = hex_sha256(&bytes);
        if actual != variant.sha256 {
            return Err(AppError::ArchiveIntegrity(format!(
                "archive record {} SHA-256 mismatch",
                variant.record_id
            )));
        }
        Ok(bytes)
    }

    async fn load_cached(&self) -> anyhow::Result<()> {
        let cached = sqlx::query_as::<_, (Vec<u8>, Option<String>)>(
            "SELECT body,etag FROM archive_catalog_cache WHERE id=1",
        )
        .fetch_optional(&self.inner.pool)
        .await?;
        if let Some((body, etag)) = cached {
            let index = parse_index(&body).map_err(anyhow::Error::msg)?;
            *self.inner.state.write().await = State { index, etag };
            info!("loaded cached archive catalog");
        }
        Ok(())
    }

    async fn refresh(&self) -> anyhow::Result<()> {
        let etag = self.inner.state.read().await.etag.clone();
        let mut request = self.inner.http.get(&self.inner.index_url);
        if let Some(etag) = etag {
            request = request.header(header::IF_NONE_MATCH, etag);
        }
        let response = request.send().await?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(());
        }
        let response = response.error_for_status()?;
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.bytes().await?.to_vec();
        let index = parse_index(&body).map_err(anyhow::Error::msg)?;
        sqlx::query("INSERT INTO archive_catalog_cache(id,body,etag,updated_at) VALUES(1,?,?,unixepoch()) ON CONFLICT(id) DO UPDATE SET body=excluded.body,etag=excluded.etag,updated_at=excluded.updated_at")
            .bind(&body)
            .bind(&etag)
            .execute(&self.inner.pool)
            .await?;
        let count = index.places.len();
        *self.inner.state.write().await = State { index, etag };
        info!(places = count, "refreshed archive catalog");
        Ok(())
    }
}

fn parse_index(body: &[u8]) -> Result<ArchiveIndex, String> {
    let index: ArchiveIndex =
        serde_json::from_slice(body).map_err(|error| format!("invalid archive index: {error}"))?;
    if index.schema_version != 1 {
        return Err(format!(
            "unsupported archive schema version {}",
            index.schema_version
        ));
    }
    for (place_id, place) in &index.places {
        let parsed: u64 = place_id
            .parse()
            .map_err(|_| format!("invalid archive place ID {place_id}"))?;
        if parsed == 0 || place.universe_id == 0 || place.variants.is_empty() {
            return Err(format!("invalid archive place entry {place_id}"));
        }
        validate_variant(&place.preferred)?;
        if !place
            .variants
            .iter()
            .any(|variant| variant.record_id == place.preferred.record_id)
        {
            return Err(format!("preferred variant missing for place {place_id}"));
        }
        for variant in &place.variants {
            validate_variant(variant)?;
        }
    }
    Ok(index)
}

fn validate_variant(variant: &ArchiveVariant) -> Result<(), String> {
    if variant.record_id.is_empty()
        || variant.title.is_empty()
        || variant.sha256.len() != 64
        || !variant.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        || variant.size_bytes == 0
        || variant.path.starts_with('/')
        || variant.path.starts_with('\\')
        || variant.path.split('/').any(|segment| segment == "..")
        || !variant.path.starts_with("levels/sha256/")
    {
        return Err(format!("invalid archive variant {}", variant.record_id));
    }
    Ok(())
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, db};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn variant(path: &str) -> ArchiveVariant {
        ArchiveVariant {
            record_id: "nla_fixture".into(),
            title: "Fixture".into(),
            sha256: "a".repeat(64),
            size_bytes: 10,
            path: path.into(),
        }
    }

    #[test]
    fn rejects_unsafe_archive_paths() {
        assert!(validate_variant(&variant("levels/sha256/aa/file.rbxl")).is_ok());
        assert!(validate_variant(&variant("../secret")).is_err());
        assert!(validate_variant(&variant("https://example.com/file")).is_err());
    }

    #[test]
    fn rejects_preferred_variant_not_in_variants() {
        let body = serde_json::json!({
            "schema_version": 1,
            "places": {
                "123": {
                    "universe_id": 456,
                    "preferred": {
                        "record_id": "preferred",
                        "title": "Fixture",
                        "sha256": "a".repeat(64),
                        "size_bytes": 10,
                        "path": "levels/sha256/aa/file.rbxl"
                    },
                    "variants": [{
                        "record_id": "other",
                        "title": "Fixture",
                        "sha256": "a".repeat(64),
                        "size_bytes": 10,
                        "path": "levels/sha256/aa/file.rbxl"
                    }]
                }
            }
        });
        assert!(parse_index(&serde_json::to_vec(&body).unwrap()).is_err());
    }

    #[tokio::test]
    async fn fetches_verifies_and_reuses_cached_catalog() {
        let payload = b"<roblox></roblox>".to_vec();
        let sha256 = hex_sha256(&payload);
        let index = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "places": {
                "123": {
                    "universe_id": 456,
                    "preferred": {
                        "record_id": "nla_fixture",
                        "title": "Fixture",
                        "sha256": sha256,
                        "size_bytes": payload.len(),
                        "path": "levels/sha256/aa/file.rbxlx"
                    },
                    "variants": [{
                        "record_id": "nla_fixture",
                        "title": "Fixture",
                        "sha256": sha256,
                        "size_bytes": payload.len(),
                        "path": "levels/sha256/aa/file.rbxlx"
                    }]
                }
            }
        }))
        .unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_payload = payload.clone();
        tokio::spawn(async move {
            for request_number in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 4096];
                let read = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let (status, body, content_type) = if path == "/index" && request_number == 0 {
                    ("200 OK", index.as_slice(), "application/json")
                } else if path == "/index" {
                    ("503 Service Unavailable", &b"unavailable"[..], "text/plain")
                } else {
                    (
                        "200 OK",
                        server_payload.as_slice(),
                        "application/octet-stream",
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(body).await.unwrap();
            }
        });

        let base = format!("http://{address}/");
        let config = Config {
            bind_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            service_token: "a".repeat(32),
            roblox_api_key: "key".into(),
            creator_group_id: 1,
            database_url: "sqlite::memory:?cache=shared".into(),
            cache_ttl: Duration::from_secs(600),
            request_timeout: Duration::from_secs(2),
            polling_window: Duration::from_secs(1),
            polling_interval: Duration::from_millis(10),
            retry_count: 0,
            roblox_base_url: base.trim_end_matches('/').into(),
            archive_index_url: format!("{base}index"),
            archive_blob_base_url: base,
            archive_refresh: Duration::from_secs(900),
            archive_max_source_bytes: 64,
        };
        let pool = db::connect("sqlite::memory:?cache=shared").await.unwrap();
        let client = ArchiveClient::new(&config, pool.clone()).await.unwrap();
        let variant = client.resolve(123).await.unwrap();
        assert_eq!(client.download(&variant).await.unwrap(), payload);

        let cached = ArchiveClient::new(&config, pool).await.unwrap();
        assert!(cached.resolve(123).await.is_some());
        let mut mismatched = variant.clone();
        mismatched.sha256 = "b".repeat(64);
        assert!(matches!(
            cached.download(&mismatched).await,
            Err(AppError::ArchiveIntegrity(_))
        ));
        let mut oversized = variant;
        oversized.size_bytes = 65;
        assert!(matches!(
            cached.download(&oversized).await,
            Err(AppError::ArchiveIntegrity(_))
        ));
    }
}
