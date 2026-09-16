cd /opt/native-web-service
docker network inspect "${PROXY_NETWORK:-web}" >/dev/null 2>&1 || docker network create "${PROXY_NETWORK:-web}"
git pull --ff-only
docker compose up -d --build
docker image prune -f