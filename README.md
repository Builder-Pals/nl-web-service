# nl-web-service

## Production deployment

This repository deploys the application only. The application listens on
`0.0.0.0:8080` and joins the external Docker network `web` as
`native-web-service`.

TLS termination and public ports `80` and `443` are owned by the separate
private `/opt/caddy` infrastructure project. Do not start a Caddy service from
this repository.

This project is responsible for providing web services for Native Legacy.

## `GET /healthz`

Returns 200 OK if the service has successfully started.

## Sandbox APIs

The purpose of sandboxing APIs is to let the web service parse Roblox binaries, and output sandboxed copies.

These endpoints require authentication to use:

```curl
Authorization: Bearer <SERVICE_API_TOKEN>
OR
x-api-key: <SERVICE_API_TOKEN>
```

### `GET /v1/sandbox_game/{place_id OR nla_id}`

#### Uncopylocked Games

Games that have been left open-source are available through the proxy! Games uploaded after 2017 will be rejected.

#### Archived Games

Copies of games that have archived `.rbxl` files available will have the 'best' available versions served.
An exact snapshot can be selected by using its Native Level Archive record ID, for example
`GET /v1/sandbox_game/nla_9e4f05af76b5c21ba1bca1db7d20868e`.

### `GET /v1/sandbox/{asset_id}`

#### Assets

Assets uploaded by Roblox - such as gears, or old toolbox models that contain scripts - can be proxied to restore their functionality.
