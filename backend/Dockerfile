# syntax=docker/dockerfile:1

# --- Сборка ---
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
# Кэш загруженных библиотек и промежуточных файлов переживает пересборки образа.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked && cp target/release/budmaster-server /budmaster-server

# --- Запуск ---
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home budmaster
COPY --from=builder /budmaster-server /usr/local/bin/budmaster-server
# Сервер работает не от root.
USER budmaster
EXPOSE 8080
ENTRYPOINT ["budmaster-server"]
CMD ["serve"]
