use std::{collections::HashSet, time::Duration};

use bytes::Bytes;
use futures_util::StreamExt;
use rand::Rng;
use reqwest::{multipart, Client, Response, StatusCode};
use serde_json::{json, Value};
use tokio::time::sleep;

use crate::{config::Config, error::AppError};

const LIMIT: usize = 20 * 1024 * 1024;

#[derive(Clone)]
pub struct RobloxClient {
    client: Client,
    config: Config,
}

#[derive(Debug)]
pub struct SourceMetadata {
    pub revision: String,
}

#[derive(Debug)]
pub enum Operation {
    Pending,
    Complete(u64),
    Failed(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Moderation {
    Pending,
    Approved,
    Rejected(String),
}

impl RobloxClient {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let test_base = !config
            .roblox_base_url
            .starts_with("https://apis.roblox.com");
        let client = Client::builder()
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() > 5 {
                    return attempt.error("too many redirects");
                }
                let host = attempt.url().host_str().unwrap_or_default();
                let approved = host == "roblox.com"
                    || host.ends_with(".roblox.com")
                    || host.ends_with(".rbxcdn.com");
                if (attempt.url().scheme() == "https" && approved) || test_base {
                    attempt.follow()
                } else {
                    attempt.error("unapproved Roblox redirect")
                }
            }))
            .build()?;
        Ok(Self { client, config })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.config.roblox_base_url)
    }

    pub async fn validate_source(&self, id: u64) -> Result<SourceMetadata, AppError> {
        let response = self
            // Creator Store details are public. Attaching an Assets-scoped Open
            // Cloud key makes Roblox authorize the request against the key and
            // returns 403 when it has no separate Creator Store permission.
            .send(|| self.source_metadata_request(id))
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(AppError::NotFound);
        }
        let value = checked_json(response).await?;
        let creator =
            find_value(&value, &["creator", "creatorDetails", "creator_details"]).unwrap_or(&value);
        let creator_id = find_u64(
            creator,
            &["creatorId", "creator_id", "userId", "user_id", "id"],
        );
        let creator_type = find_string(creator, &["creatorType", "creator_type", "type"]);
        let asset_type = find_string(&value, &["assetType", "asset_type", "assetTypeName"]);
        let is_model = asset_type.as_deref().is_some_and(|s| {
            s.eq_ignore_ascii_case("model") || s.eq_ignore_ascii_case("ASSET_TYPE_MODEL")
        }) || find_u64(&value, &["assetTypeId", "asset_type_id"]) == Some(10);
        let is_user = creator_type
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("user"))
            .unwrap_or(true);
        if creator_id != Some(1) || !is_user || !is_model {
            return Err(AppError::Forbidden);
        }
        let revision_value = find_value(
            &value,
            &[
                "revisionId",
                "revision_id",
                "updatedUtc",
                "updateTime",
                "update_time",
                "updated",
            ],
        )
        .cloned()
        .unwrap_or_else(|| Value::String(id.to_string()));
        Ok(SourceMetadata {
            revision: revision_value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| revision_value.to_string()),
        })
    }

    fn source_metadata_request(&self, id: u64) -> reqwest::RequestBuilder {
        self.client
            .get(self.url(&format!("/toolbox-service/v2/assets/{id}")))
    }

    pub async fn download(&self, id: u64) -> Result<Bytes, AppError> {
        let response = self
            .send(|| {
                self.client
                    .get(self.url(&format!("/asset-delivery-api/v1/assetId/{id}")))
                    .header("x-api-key", &self.config.roblox_api_key)
            })
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(AppError::NotFound);
        }
        if let Some(length) = response.content_length() {
            if length > LIMIT as u64 {
                return Err(AppError::TooLarge);
            }
        }
        if !response.status().is_success() {
            return Err(upstream_status(response.status()));
        }
        let mut stream = response.bytes_stream();
        let mut data = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(map_reqwest)?;
            if data.len() + chunk.len() > LIMIT {
                return Err(AppError::TooLarge);
            }
            data.extend_from_slice(&chunk);
        }
        Ok(data.into())
    }

    pub async fn upload(&self, source_id: u64, bytes: Vec<u8>) -> Result<String, AppError> {
        let request = json!({"assetType":"Model","displayName":format!("Sandboxed Roblox model {source_id}"),"description":format!("Sandboxed compatibility copy of Roblox asset {source_id}."),"creationContext":{"creator":{"groupId":self.config.creator_group_id.to_string()}}});
        let part = multipart::Part::bytes(bytes)
            .file_name("model.rbxm")
            .mime_str("model/x-rbxm")
            .map_err(|e| AppError::Upstream(e.to_string()))?;
        let form = multipart::Form::new()
            .text("request", request.to_string())
            .part("fileContent", part);
        let response = self
            .client
            .post(self.url("/assets/v1/assets"))
            .header("x-api-key", &self.config.roblox_api_key)
            .multipart(form)
            .send()
            .await
            .map_err(map_reqwest)?;
        let value = checked_json(response).await?;
        find_string(&value, &["path", "operationId", "operation_id"])
            .map(|s| s.trim_start_matches("operations/").to_owned())
            .ok_or_else(|| {
                AppError::Upstream("upload response did not contain an operation ID".into())
            })
    }

    pub async fn operation(&self, id: &str) -> Result<Operation, AppError> {
        let response = self
            .send(|| {
                self.client
                    .get(self.url(&format!("/assets/v1/operations/{id}")))
                    .header("x-api-key", &self.config.roblox_api_key)
            })
            .await?;
        let value = checked_json(response).await?;
        if value.get("done").and_then(Value::as_bool) != Some(true) {
            return Ok(Operation::Pending);
        }
        if let Some(error) = value.get("error") {
            return Ok(Operation::Failed(error.to_string()));
        }
        find_u64(
            value.get("response").unwrap_or(&value),
            &["assetId", "asset_id"],
        )
        .map(Operation::Complete)
        .ok_or_else(|| AppError::Upstream("completed upload contained no asset ID".into()))
    }

    pub async fn moderation(&self, asset_id: u64) -> Result<Moderation, AppError> {
        let response = self
            .send(|| {
                self.client
                    .get(self.url(&format!(
                        "/assets/v1/assets/{asset_id}?readMask=moderationResult"
                    )))
                    .header("x-api-key", &self.config.roblox_api_key)
            })
            .await?;
        let value = checked_json(response).await?;
        let state = find_string(&value, &["moderationState", "moderation_state"])
            .unwrap_or_default()
            .to_ascii_uppercase();
        if state.contains("APPROVED") {
            Ok(Moderation::Approved)
        } else if state.contains("REJECTED") || state.contains("BLOCKED") {
            Ok(Moderation::Rejected(state))
        } else {
            Ok(Moderation::Pending)
        }
    }

    async fn send<F>(&self, build: F) -> Result<Response, AppError>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut attempted = HashSet::new();
        for attempt in 0..=self.config.retry_count {
            let response = build().send().await.map_err(map_reqwest)?;
            let status = response.status();
            if status != StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
                return Ok(response);
            }
            attempted.insert(status);
            if attempt == self.config.retry_count {
                return Err(if status == StatusCode::TOO_MANY_REQUESTS {
                    AppError::RateLimited
                } else {
                    upstream_status(status)
                });
            }
            let jitter = rand::rng().random_range(0..200);
            sleep(Duration::from_millis((200_u64 << attempt.min(5)) + jitter)).await;
        }
        Err(AppError::Upstream(format!(
            "retry exhausted: {attempted:?}"
        )))
    }
}

async fn checked_json(response: Response) -> Result<Value, AppError> {
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Err(AppError::NotFound);
    }
    if !status.is_success() {
        return Err(upstream_status(status));
    }
    response.json().await.map_err(map_reqwest)
}
fn upstream_status(status: StatusCode) -> AppError {
    if status == StatusCode::TOO_MANY_REQUESTS {
        AppError::RateLimited
    } else {
        AppError::Upstream(format!("HTTP {status}"))
    }
}
fn map_reqwest(error: reqwest::Error) -> AppError {
    if error.is_timeout() {
        AppError::Timeout
    } else {
        AppError::Upstream(error.to_string())
    }
}
fn find_value<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    match value {
        Value::Object(map) => keys
            .iter()
            .find_map(|k| map.get(*k))
            .or_else(|| map.values().find_map(|v| find_value(v, keys))),
        Value::Array(a) => a.iter().find_map(|v| find_value(v, keys)),
        _ => None,
    }
}
fn find_string(value: &Value, keys: &[&str]) -> Option<String> {
    find_value(value, keys).and_then(|v| {
        v.as_str()
            .map(str::to_owned)
            .or_else(|| v.as_u64().map(|x| x.to_string()))
    })
}
fn find_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    find_value(value, keys).and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn config() -> Config {
        Config {
            bind_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
            service_token: "a".repeat(32),
            roblox_api_key: "upload-only-key".into(),
            creator_group_id: 1,
            database_url: "sqlite::memory:".into(),
            cache_ttl: Duration::from_secs(600),
            request_timeout: Duration::from_secs(20),
            polling_window: Duration::from_secs(60),
            polling_interval: Duration::from_secs(2),
            retry_count: 3,
            roblox_base_url: "https://apis.roblox.com".into(),
        }
    }

    #[test]
    fn public_source_metadata_request_does_not_send_open_cloud_key() {
        let client = RobloxClient::new(config()).unwrap();
        let request = client.source_metadata_request(123).build().unwrap();

        assert_eq!(
            request.url().as_str(),
            "https://apis.roblox.com/toolbox-service/v2/assets/123"
        );
        assert!(!request.headers().contains_key("x-api-key"));
    }
}
