use std::{collections::HashSet, time::Duration};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use rand::Rng;
use reqwest::{header::HeaderMap, multipart, Client, Response, StatusCode, Url};
use serde_json::{json, Value};
use tokio::time::sleep;

use crate::{config::Config, error::AppError};

const LIMIT: usize = 20 * 1024 * 1024;

#[derive(Clone)]
pub struct RobloxClient {
    client: Client,
    probe_client: Client,
    config: Config,
}

#[derive(Debug)]
pub struct SourceMetadata {
    pub revision: String,
}

#[derive(Debug)]
pub struct GameMetadata {
    pub revision: String,
    pub name: String,
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
        let probe_client = Client::builder()
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            probe_client,
            config,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.config.roblox_base_url)
    }

    pub async fn validate_source(&self, id: u64) -> Result<SourceMetadata, AppError> {
        let mut response = self
            // Creator Store details are public. Attaching an Assets-scoped Open
            // Cloud key makes Roblox authorize the request against the key and
            // returns 403 when it has no separate Creator Store permission.
            .send(|| self.source_metadata_request(id))
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            // Catalog assets such as classic Roblox-authored gears are not in
            // Creator Store search, but do have public Economy metadata.
            response = self.send(|| self.catalog_metadata_request(id)).await?;
        }
        let value = checked_json(response).await?;
        source_metadata(&value, id)
    }

    pub async fn validate_game(&self, place_id: u64) -> Result<GameMetadata, AppError> {
        let universe_response = self.send(|| self.game_universe_request(place_id)).await?;
        if metadata_unavailable(universe_response.status()) {
            return self.validate_unlisted_game(place_id).await;
        }
        let universe = checked_json(universe_response).await?;
        let Some(universe_id) = find_u64(&universe, &["universeId", "universe_id"]) else {
            return self.validate_unlisted_game(place_id).await;
        };
        let details_response = self.send(|| self.game_details_request(universe_id)).await?;
        if metadata_unavailable(details_response.status()) {
            return self.validate_unlisted_game(place_id).await;
        }
        let details = checked_json(details_response).await?;
        match game_metadata(&details, place_id) {
            Err(AppError::NotFound) => self.validate_unlisted_game(place_id).await,
            result => result,
        }
    }

    async fn validate_unlisted_game(&self, place_id: u64) -> Result<GameMetadata, AppError> {
        if place_id >= 100_000_000 {
            return Err(AppError::IneligibleGame);
        }
        let mut response = self
            .send(|| self.unlisted_game_probe_request(place_id))
            .await?;
        if metadata_unavailable(response.status()) {
            response = self
                .send(|| self.authenticated_asset_request(place_id))
                .await?;
        }
        unlisted_game_metadata(place_id, response.status(), response.headers())
    }

    fn unlisted_game_probe_request(&self, place_id: u64) -> reqwest::RequestBuilder {
        let url = if self
            .config
            .roblox_base_url
            .starts_with("https://apis.roblox.com")
        {
            format!("https://assetdelivery.roblox.com/v1/asset/?id={place_id}")
        } else {
            self.url(&format!("/v1/asset/?id={place_id}"))
        };
        self.probe_client.get(url)
    }

    fn game_universe_request(&self, place_id: u64) -> reqwest::RequestBuilder {
        self.client
            .get(self.url(&format!("/universes/v1/places/{place_id}/universe")))
    }

    fn game_details_request(&self, universe_id: u64) -> reqwest::RequestBuilder {
        let url = if self
            .config
            .roblox_base_url
            .starts_with("https://apis.roblox.com")
        {
            format!("https://games.roblox.com/v1/games?universeIds={universe_id}")
        } else {
            self.url(&format!("/v1/games?universeIds={universe_id}"))
        };
        self.client.get(url)
    }

    fn source_metadata_request(&self, id: u64) -> reqwest::RequestBuilder {
        self.client
            .get(self.url(&format!("/toolbox-service/v2/assets/{id}")))
    }

    fn catalog_metadata_request(&self, id: u64) -> reqwest::RequestBuilder {
        let url = if self
            .config
            .roblox_base_url
            .starts_with("https://apis.roblox.com")
        {
            format!("https://economy.roblox.com/v2/assets/{id}/details")
        } else {
            // Keep integration tests on their configured mock server.
            self.url(&format!("/v2/assets/{id}/details"))
        };
        self.client.get(url)
    }

    pub async fn download(&self, id: u64) -> Result<Bytes, AppError> {
        // Sources have already been restricted to public Roblox-authored
        // assets. This endpoint redirects directly to their model content;
        // Open Cloud's assetId endpoint instead returns a JSON location record.
        let mut response = self.send(|| self.public_asset_request(id)).await?;
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            let delivery = self.send(|| self.authenticated_asset_request(id)).await?;
            let value = checked_json(delivery).await?;
            let location = find_string(&value, &["location"]).ok_or_else(|| {
                AppError::Upstream("asset delivery response had no location".into())
            })?;
            let location = approved_asset_location(&location)?;
            response = self.send(|| self.client.get(location.clone())).await?;
        }
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

    fn public_asset_request(&self, id: u64) -> reqwest::RequestBuilder {
        let url = if self
            .config
            .roblox_base_url
            .starts_with("https://apis.roblox.com")
        {
            format!("https://assetdelivery.roblox.com/v1/asset/?id={id}")
        } else {
            self.url(&format!("/v1/asset/?id={id}"))
        };
        self.client.get(url)
    }

    fn authenticated_asset_request(&self, id: u64) -> reqwest::RequestBuilder {
        self.client
            .get(self.url(&format!("/asset-delivery-api/v1/assetId/{id}")))
            .header("x-api-key", &self.config.roblox_api_key)
    }

    pub async fn upload(&self, source_id: u64, bytes: Vec<u8>) -> Result<String, AppError> {
        self.upload_model(
            bytes,
            format!("Sandboxed Roblox model {source_id}"),
            format!("Sandboxed compatibility copy of Roblox asset {source_id}."),
        )
        .await
    }

    pub async fn upload_game(&self, place_id: u64, bytes: Vec<u8>) -> Result<String, AppError> {
        self.upload_model(
            bytes,
            format!("Sandboxed Roblox game {place_id}"),
            format!("Sandboxed compatibility package of Roblox game {place_id}."),
        )
        .await
    }

    async fn upload_model(
        &self,
        bytes: Vec<u8>,
        display_name: String,
        description: String,
    ) -> Result<String, AppError> {
        let request = json!({"assetType":"Model","displayName":display_name,"description":description,"creationContext":{"creator":{"groupId":self.config.creator_group_id.to_string()}}});
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

fn approved_asset_location(location: &str) -> Result<Url, AppError> {
    let url = Url::parse(location)
        .map_err(|_| AppError::Upstream("asset delivery returned an invalid location".into()))?;
    let host = url.host_str().unwrap_or_default();
    let approved =
        host == "roblox.com" || host.ends_with(".roblox.com") || host.ends_with(".rbxcdn.com");
    if url.scheme() != "https" || !approved {
        return Err(AppError::Upstream(
            "asset delivery returned an unapproved location".into(),
        ));
    }
    Ok(url)
}

fn game_metadata(value: &Value, place_id: u64) -> Result<GameMetadata, AppError> {
    let game = value
        .get("data")
        .and_then(Value::as_array)
        .and_then(|data| data.first())
        .ok_or(AppError::NotFound)?;
    let root_place_id = find_u64(game, &["rootPlaceId", "root_place_id"]);
    // Roblox represents some downloadable, unlisted games with a successful
    // response containing a synthetic zero-valued record. This is unavailable
    // metadata, not affirmative evidence that the place is copylocked.
    if find_u64(game, &["id"]) == Some(0) && root_place_id == Some(0) {
        return Err(AppError::NotFound);
    }
    let copying_allowed = find_value(game, &["copyingAllowed", "copying_allowed"])
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let created = find_string(game, &["created"]).ok_or(AppError::IneligibleGame)?;
    let created = DateTime::parse_from_rfc3339(&created)
        .map_err(|_| AppError::IneligibleGame)?
        .with_timezone(&Utc);
    let cutoff = DateTime::parse_from_rfc3339("2018-01-01T00:00:00Z")
        .expect("valid cutoff")
        .with_timezone(&Utc);
    if root_place_id != Some(place_id) || !copying_allowed || created >= cutoff {
        return Err(AppError::IneligibleGame);
    }
    let name = find_string(game, &["name"]).unwrap_or_else(|| "Unnamed Game".into());
    let revision = find_string(game, &["updated"])
        .or_else(|| find_u64(game, &["id"]).map(|id| id.to_string()))
        .unwrap_or_else(|| place_id.to_string());
    Ok(GameMetadata { revision, name })
}

fn metadata_unavailable(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
    )
}

fn unlisted_game_metadata(
    place_id: u64,
    status: StatusCode,
    headers: &HeaderMap,
) -> Result<GameMetadata, AppError> {
    let downloadable = status.is_success()
        || (status.is_redirection() && headers.contains_key(reqwest::header::LOCATION));
    let asset_type = headers
        .get("roblox-assettypeid")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if !downloadable || asset_type != Some(9) {
        return Err(if status == StatusCode::NOT_FOUND {
            AppError::NotFound
        } else {
            AppError::IneligibleGame
        });
    }
    let revision = headers
        .get("roblox-assetversionnumber")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| place_id.to_string());
    Ok(GameMetadata {
        revision,
        name: format!("Unlisted Game {place_id}"),
    })
}

fn source_metadata(value: &Value, id: u64) -> Result<SourceMetadata, AppError> {
    let creator =
        find_value(value, &["creator", "creatorDetails", "creator_details"]).unwrap_or(value);
    let creator_id = find_u64(
        creator,
        &["creatorId", "creator_id", "userId", "user_id", "id"],
    );
    let creator_type = find_string(creator, &["creatorType", "creator_type", "type"]);
    let asset_type = find_string(value, &["assetType", "asset_type", "assetTypeName"]);
    let is_supported_type = asset_type.as_deref().is_some_and(|s| {
        s.eq_ignore_ascii_case("model")
            || s.eq_ignore_ascii_case("ASSET_TYPE_MODEL")
            || s.eq_ignore_ascii_case("gear")
            || s.eq_ignore_ascii_case("ASSET_TYPE_GEAR")
    }) || matches!(
        find_u64(value, &["assetTypeId", "asset_type_id"]),
        Some(10 | 19)
    );
    let is_user = creator_type
        .as_deref()
        .map(|s| s.eq_ignore_ascii_case("user"))
        .unwrap_or(true);
    if creator_id != Some(1) || !is_user || !is_supported_type {
        return Err(AppError::Forbidden);
    }
    let revision_value = find_value(
        value,
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

async fn checked_json(response: Response) -> Result<Value, AppError> {
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Err(AppError::NotFound);
    }
    if !status.is_success() {
        let detail = response.text().await.map_err(map_reqwest)?;
        let detail = detail.chars().take(2048).collect::<String>();
        return Err(if detail.is_empty() {
            upstream_status(status)
        } else {
            AppError::Upstream(format!("HTTP {status}: {detail}"))
        });
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
            .find_map(|key| {
                map.iter()
                    .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
                    .map(|(_, value)| value)
            })
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

    #[test]
    fn public_catalog_metadata_request_does_not_send_open_cloud_key() {
        let client = RobloxClient::new(config()).unwrap();
        let request = client.catalog_metadata_request(18426536).build().unwrap();

        assert_eq!(
            request.url().as_str(),
            "https://economy.roblox.com/v2/assets/18426536/details"
        );
        assert!(!request.headers().contains_key("x-api-key"));
    }

    #[test]
    fn accepts_pascal_case_roblox_gear_metadata() {
        let value = json!({
            "AssetId": 18426536,
            "AssetTypeId": 19,
            "Creator": {"Id": 1, "CreatorType": "User"},
            "Updated": "2016-12-06T01:12:37.19Z"
        });

        let metadata = source_metadata(&value, 18426536).unwrap();
        assert_eq!(metadata.revision, "2016-12-06T01:12:37.19Z");
    }

    #[test]
    fn public_asset_request_does_not_send_open_cloud_key() {
        let client = RobloxClient::new(config()).unwrap();
        let request = client.public_asset_request(18426536).build().unwrap();

        assert_eq!(
            request.url().as_str(),
            "https://assetdelivery.roblox.com/v1/asset/?id=18426536"
        );
        assert!(!request.headers().contains_key("x-api-key"));
    }

    #[test]
    fn authenticated_asset_request_uses_open_cloud_delivery() {
        let client = RobloxClient::new(config()).unwrap();
        let request = client
            .authenticated_asset_request(23886929)
            .build()
            .unwrap();

        assert_eq!(
            request.url().as_str(),
            "https://apis.roblox.com/asset-delivery-api/v1/assetId/23886929"
        );
        assert!(request.headers().contains_key("x-api-key"));
    }

    #[test]
    fn asset_delivery_locations_are_restricted_to_roblox_cdn() {
        assert!(approved_asset_location("https://fts.rbxcdn.com/content").is_ok());
        assert!(approved_asset_location("http://fts.rbxcdn.com/content").is_err());
        assert!(approved_asset_location("https://rbxcdn.com.example/content").is_err());
    }

    #[test]
    fn accepts_uncopylocked_pre_2018_root_place_from_any_creator() {
        let value = json!({"data":[{
            "id": 13058,
            "rootPlaceId": 1818,
            "name": "Classic: Crossroads",
            "creator": {"id": 999, "type": "User"},
            "copyingAllowed": true,
            "created": "2007-05-01T01:07:04.78Z",
            "updated": "2024-01-29T22:05:10.417Z"
        }]});
        let metadata = game_metadata(&value, 1818).unwrap();
        assert_eq!(metadata.name, "Classic: Crossroads");
        assert_eq!(metadata.revision, "2024-01-29T22:05:10.417Z");
    }

    #[test]
    fn rejects_copylocked_new_and_non_root_games() {
        let fixture = |copying_allowed, created: &str, root_place_id| {
            json!({"data":[{
                "rootPlaceId": root_place_id,
                "copyingAllowed": copying_allowed,
                "created": created
            }]})
        };
        assert!(matches!(
            game_metadata(&fixture(false, "2007-01-01T00:00:00Z", 1818), 1818),
            Err(AppError::IneligibleGame)
        ));
        assert!(matches!(
            game_metadata(&fixture(true, "2018-01-01T00:00:00Z", 1818), 1818),
            Err(AppError::IneligibleGame)
        ));
        assert!(matches!(
            game_metadata(&fixture(true, "2007-01-01T00:00:00Z", 999), 1818),
            Err(AppError::IneligibleGame)
        ));
    }

    #[test]
    fn accepts_downloadable_unlisted_place_metadata() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::LOCATION,
            "https://cdn.example/game".parse().unwrap(),
        );
        headers.insert("roblox-assettypeid", "9".parse().unwrap());
        headers.insert("roblox-assetversionnumber", "42".parse().unwrap());

        let metadata = unlisted_game_metadata(999_999, StatusCode::FOUND, &headers).unwrap();
        assert_eq!(metadata.name, "Unlisted Game 999999");
        assert_eq!(metadata.revision, "42");
    }

    #[test]
    fn treats_redacted_game_details_as_unavailable_metadata() {
        let value = json!({"data":[{
            "id": 0,
            "rootPlaceId": 0,
            "name": "[TITLE UNAVAILABLE]",
            "copyingAllowed": false,
            "created": "0001-01-01T05:51:00Z"
        }]});

        assert!(matches!(
            game_metadata(&value, 25415),
            Err(AppError::NotFound)
        ));
    }

    #[test]
    fn rejects_non_place_and_non_downloadable_unlisted_assets() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::LOCATION,
            "https://cdn.example/asset".parse().unwrap(),
        );
        headers.insert("roblox-assettypeid", "10".parse().unwrap());
        assert!(matches!(
            unlisted_game_metadata(123, StatusCode::FOUND, &headers),
            Err(AppError::IneligibleGame)
        ));
        assert!(matches!(
            unlisted_game_metadata(123, StatusCode::FORBIDDEN, &HeaderMap::new()),
            Err(AppError::IneligibleGame)
        ));
    }

    #[test]
    fn unlisted_game_probe_is_public() {
        let client = RobloxClient::new(config()).unwrap();
        let request = client.unlisted_game_probe_request(1818).build().unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://assetdelivery.roblox.com/v1/asset/?id=1818"
        );
        assert!(!request.headers().contains_key("x-api-key"));
    }

    #[test]
    fn authenticated_delivery_headers_identify_unlisted_places() {
        let mut headers = HeaderMap::new();
        headers.insert("roblox-assettypeid", "9".parse().unwrap());
        headers.insert("roblox-assetversionnumber", "20".parse().unwrap());

        let metadata = unlisted_game_metadata(13_969, StatusCode::OK, &headers).unwrap();
        assert_eq!(metadata.name, "Unlisted Game 13969");
        assert_eq!(metadata.revision, "20");
    }
}
