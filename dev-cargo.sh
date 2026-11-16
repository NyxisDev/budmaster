#!/usr/bin/env bash
# Запускает cargo в контейнере: Rust на компьютере не нужен.
#   ./dev-cargo.sh check
#   ./dev-cargo.sh test        (нужна запущенная база: docker compose up -d postgres)
set -euo pipefail
export MSYS_NO_PATHCONV=1
root="$(cd "$(dirname "$0")" && (pwd -W 2>/dev/null || pwd))"
docker run --rm \
  -v "$root/backend:/app" \
  -v budmaster_cargo:/usr/local/cargo/registry \
  -v budmaster_target:/target \
  -e CARGO_TARGET_DIR=/target \
  -e DATABASE_URL="${TEST_DATABASE_URL:-}" \
  --network "${CARGO_NETWORK:-bridge}" \
  -w /app rust:1-bookworm cargo "$@"
