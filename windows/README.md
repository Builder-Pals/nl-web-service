# Windows development

## Prerequisites

- Docker Desktop using the WSL 2 backend
- Git for Windows (provides Git Bash)

Start Docker Desktop and wait until its engine is running.

## Configure

Create `.env` in the repository root from `.env.example`. For Docker, ensure it
contains:

```dotenv
BIND_ADDRESS=0.0.0.0:8080
DATABASE_URL=sqlite:///data/cache.db
```

Fill in the service token, Roblox Open Cloud key, and creator group ID as well.
The service token must be at least 32 characters.

## Run

From Git Bash in the repository root:

```bash
./scripts/dev.sh
```

Alternatively, from PowerShell:

```powershell
& 'C:\Program Files\Git\bin\bash.exe' ./scripts/dev.sh
```

The first build can take several minutes. Test the running service from another
terminal:

```powershell
curl.exe -i http://127.0.0.1:8080/healthz

curl.exe -i `
  -H "Authorization: Bearer YOUR_SERVICE_TOKEN" `
  http://127.0.0.1:8080/v1/sandbox/ASSET_ID
```

## Manage

```powershell
# Follow application logs
docker logs -f $(docker ps -q --filter publish=8080)

# Stop the local service
docker stop $(docker ps -q --filter publish=8080)
```

After changing Rust code, stop the existing container and run `dev.sh` again so
Docker rebuilds the image. The SQLite database persists in Docker's `app_data`
volume.
