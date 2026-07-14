cd /opt/native-web-service
docker compose up -d --force-recreate caddy
docker compose logs -f caddy