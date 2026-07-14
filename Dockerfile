FROM rust:1.88-bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --locked --release

FROM debian:bookworm-slim
RUN useradd --create-home --uid 10001 app && mkdir /data && chown app:app /data
COPY --from=builder /app/target/release/native-web-service /usr/local/bin/native-web-service
USER app
ENV BIND_ADDRESS=0.0.0.0:8080 DATABASE_URL=sqlite:///data/cache.db
EXPOSE 8080
VOLUME ["/data"]
ENTRYPOINT ["native-web-service"]

