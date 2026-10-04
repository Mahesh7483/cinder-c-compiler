#!/usr/bin/env bash
# Start (or reuse) the long-lived Linux dev container used to build and test
# Cinder. The repo is bind-mounted at /work; the cargo target dir and registry
# live in named volumes so incremental builds stay fast.
#
#   scripts/dev-up.sh            start / reuse the container
#   scripts/dx.sh <command...>   run a command inside it, from /work
#   scripts/dev-down.sh          stop and remove it (volumes are kept)
set -euo pipefail
export MSYS_NO_PATHCONV=1
NAME=${CINDER_DEV_CONTAINER:-cinder-dev}
IMAGE=${CINDER_DEV_IMAGE:-rust:1-slim-bookworm}
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && (pwd -W 2>/dev/null || pwd))

if docker ps --format '{{.Names}}' | grep -qx "$NAME"; then
  echo "$NAME already running"; exit 0
fi
if docker ps -a --format '{{.Names}}' | grep -qx "$NAME"; then
  docker start "$NAME" >/dev/null; echo "$NAME started"; exit 0
fi
docker volume create cinder-target >/dev/null
docker volume create cinder-cargo >/dev/null
# --init: a real PID 1 that reaps orphans. The server tests run hostile programs (fork bombs, setsid
# escapes); their killed children are reparented to PID 1 and, if nobody reaps them, stay zombies that
# keep counting against their uid's process limit and make later test runs fail with EAGAIN.
docker run -d --init --name "$NAME" \
  -v "$ROOT:/work" \
  -v cinder-target:/target \
  -v cinder-cargo:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/target \
  -e CARGO_TERM_COLOR=never \
  -w /work "$IMAGE" sleep infinity >/dev/null
docker exec "$NAME" rustup component add rustfmt clippy >/dev/null 2>&1 || true
echo "$NAME created"
