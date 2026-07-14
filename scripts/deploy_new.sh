cd /opt/native-web-service
git pull --ff-only
docker compose up -d --build
docker image prune -f