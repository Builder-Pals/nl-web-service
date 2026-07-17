# nl-web-service

Sandboxes Roblox Gears by modifying their scripts, and uploads them to the Creator Store.
It can also package eligible uncopylocked Roblox games for Native Legacy.

## Setup

1. Create a Roblox Open Cloud API key owned by an account able to manage the target group.
2. Grant the asset read and write permissions required by Roblox's Assets APIs.
   Creator Store source metadata is public and does not require an additional key
   permission.
3. Copy `.env.example` to `.env`, use a cryptographically random service token of at least 32 characters, and fill in the Roblox key and group ID.
4. Create the SQLite parent directory when using a path such as `sqlite://data/cache.db`.
5. Export the variables and run `cargo run --release`.

TLS is intentionally terminated by a reverse proxy or load balancer. Do not expose the private HTTP listener directly to the internet.

## API

```text
GET /v1/sandbox/{asset_id}
Authorization: Bearer <SERVICE_API_TOKEN>
```

Uncopylocked root places created before 2018 can be packaged with:

```text
GET /v1/sandbox_game/{place_id}
Authorization: Bearer <SERVICE_API_TOKEN>
```

Roblox server scripts can instead authenticate with an experience secret:

```luau
local HttpService = game:GetService("HttpService")

local response = HttpService:RequestAsync({
    Url = "https://api.fizzyhex.design/v1/sandbox/123456789",
    Method = "GET",
    Headers = {
        ["x-api-key"] = HttpService:GetSecret("SANDBOX_API_KEY"),
    },
})
```

Create `SANDBOX_API_KEY` in the experience's Secrets Store with the same value as
`SERVICE_API_TOKEN`. Keep this request in a server Script and enable HTTP requests
for the experience. The `Authorization: Bearer` scheme remains available for
non-Roblox clients.

A completed request returns `200` only after Roblox reports moderation approval:

```json
{"source_asset_id":123,"sandboxed_asset_id":456,"status":"approved","cached":false}
```

The game endpoint uses the same status fields, with `source_place_id` in place
of `source_asset_id`.

Known historical place IDs are resolved through
[`Builder-Pals/native-level-archive`](https://github.com/Builder-Pals/native-level-archive)
before Roblox. The service caches the versioned place index in SQLite, downloads only relative
content-addressed paths from the configured archive origin, and verifies the declared size and
SHA-256 before parsing. Unknown IDs retain the existing live-Roblox behavior.

Game responses additionally include `source_kind` (`archive` or `roblox`). Archive-backed
responses include `archive_record_id` and `archive_sha256`; these fields are optional for backward
compatibility.

If the 60-second processing window expires, it returns `202` with `Retry-After: 10`. Call the same URL again to resume polling without duplicating the upload.

`GET /healthz` is unauthenticated and checks SQLite connectivity.

## Configuration

The variables shown in `.env.example` are supported. `ROBLOX_BASE_URL` is additionally available for integration testing against a mock server and should not be set in production.

`ARCHIVE_INDEX_URL` and `ARCHIVE_BLOB_BASE_URL` select the catalog and fixed download origin.
`ARCHIVE_REFRESH_SECONDS` defaults to `900`, and `ARCHIVE_MAX_SOURCE_BYTES` defaults to
`67108864` (64 MiB). The
archive source limit is separate from Roblox's 20 MiB packaged-upload limit. If refresh fails, the
last valid SQLite-cached index remains active.

The service accepts binary or XML Roblox files, including gzip-wrapped public
delivery responses, and enforces Roblox's 20 MB asset upload ceiling. Model and
Gear sources must be authored by Roblox user ID `1`. Game sources can have any
creator, but must be an uncopylocked root place whose experience was created
before 2018. When Roblox no longer exposes a game's public metadata, a
downloadable Place asset with an ID below `1000000` is also eligible.

## Operational notes

- Mount the SQLite file on persistent storage. WAL mode creates adjacent `-wal` and `-shm` files.
- One process replica is supported. Per-asset locks are in memory; multiple replicas can create duplicate uploads.
- A failed source revision stays failed until Roblox reports a different revision.

## Checks

```text
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --locked
```
