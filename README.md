# nl-web-service

This project is responsible for providing web services for Native Legacy.

## `/healthz`

Returns 200 OK if the service has successfully started.

## Sandbox APIs

The purpose of sandboxing APIs is to let the web service parse Roblox binaries, and output sandboxed copies.

### `GET sandbox_game/{place_id}`

#### Uncopylocked Games

Games that have been left open-source are available through the proxy! Games uploaded after 2017 will be rejected.

#### Archived Games

Copies of games that have archived `.rbxl` files available will have the best copies available served.

### `GET sandbox/{asset_id}`

#### Assets

Assets uploaded by Roblox - such as gears, or old toolbox models that contain scripts - can be proxied to restore their functionality.
